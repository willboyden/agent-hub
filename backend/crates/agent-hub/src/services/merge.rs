//! Port of services/merge.py: the merge engine for one target file: `own`, `json_keys`, `yaml_keys`, `block`. Pure
//! (bytes in, bytes out). Every mode reduces the file to a *slice* (the part the hub owns): the whole file, the managed
//! keys, or the marker region. Conflict detection compares slice hashes, so unmanaged content elsewhere in the file may
//! change freely. `content=None` on a Desired means "the hub no longer wants this slice" (removal).
//! `find_region` MUST agree with adapters/_util.rs. Owner: WS-B2.
//!
//! Documents are Python values ([`PyValue`]): JSON goes through [`pyjson`], a port of CPython's `json.loads`/`json.dumps`
//! (NaN/Infinity literals, unbounded ints, duplicate keys, the `indent=2` layout, `sort_keys`/`default=str` for the slice
//! digest), YAML through `hub_core::yamlsafe` (load) and `hub_core::yaml::dump` (`safe_dump(sort_keys=False,
//! allow_unicode=True, default_flow_style=False, width=100000)`).

use hub_core::common::sha256;
use hub_core::py::text::{rstrip, splitlines, strip};
use hub_core::value::PyValue;
use hub_core::yaml::DumpOpts;
use hub_core::{HubResult, Problem};

pub const MD_BEGIN_PREFIX: &str = "<!-- agent-hub:begin";
pub const MD_END: &str = "<!-- agent-hub:end -->";
pub const HASH_BEGIN_PREFIX: &str = "# agent-hub:begin";
pub const HASH_END: &str = "# agent-hub:end";

fn marks(style: &str) -> (&'static str, &'static str) {
    if style == "md" {
        (MD_BEGIN_PREFIX, MD_END)
    } else {
        (HASH_BEGIN_PREFIX, HASH_END)
    }
}

/// `find_region(text, style)`: Ok(Some((start, end))) | Ok(None) (no region) | Err("broken") (begin without end).
/// Offsets are BYTE offsets into `text` (Python returns code-point offsets; both slice the same region). `end`
/// includes the end marker's line break.
pub fn find_region(text: &str, style: &str) -> Result<Option<(usize, usize)>, &'static str> {
    let (begin, end) = marks(style);
    let mut pos = 0usize;
    let mut start: Option<usize> = None;
    for line in splitlines(text, true) {
        if start.is_none() && line.starts_with(begin) {
            start = Some(pos);
        } else if let Some(s) = start
            && rstrip(line) == end
        {
            return Ok(Some((s, pos + line.len())));
        }
        pos += line.len();
    }
    if start.is_some() { Err("broken") } else { Ok(None) }
}

/// One artifact's desired state for a live file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub root: String,
    pub path: String,
    pub kind: String,
    pub merge: String,
    pub source_ids: Vec<String>,
    pub mode: u32,
    pub managed_keys: Vec<String>,
    /// None: remove the hub's slice
    pub content: Option<Vec<u8>>,
    pub managed: bool,
}

impl Desired {
    /// "root:path"
    pub fn key(&self) -> String {
        format!("{}:{}", self.root, self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Merged {
    /// bytes to write; None: nothing to write / delete the file
    pub new: Option<Vec<u8>>,
    /// digest of the hub-owned slice as found (None: absent)
    pub slice_live: Option<String>,
    /// digest of the slice after the merge (None: absent)
    pub slice_new: Option<String>,
    /// non-empty: the live file cannot be merged safely
    pub unparseable: String,
    pub empty_after: bool,
    pub block_style: String,
}

/// A mapping document: insertion-ordered (key, value) pairs.
pub type Doc = Vec<(PyValue, PyValue)>;

/// Why `_load_doc` failed: the Python exception class (its `__name__` is part of the conflict reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocError {
    UnicodeDecodeError,
    JsonDecodeError,
    YamlError,
    /// "top level is not a mapping"
    ValueError,
    /// nesting deeper than Python's recursion limit: Python raises RecursionError, which merge.py does NOT catch
    RecursionError,
}

impl DocError {
    pub fn class(self) -> &'static str {
        match self {
            DocError::UnicodeDecodeError => "UnicodeDecodeError",
            DocError::JsonDecodeError => "JSONDecodeError",
            DocError::YamlError => "YamlError",
            DocError::ValueError => "ValueError",
            DocError::RecursionError => "RecursionError",
        }
    }
}

/// `_load_doc(raw, fmt)`: `{}` for an empty/blank file or a `null` document; ValueError for a non-mapping.
pub fn load_doc(raw: &[u8], fmt: &str) -> Result<Doc, DocError> {
    let text = std::str::from_utf8(raw).map_err(|_| DocError::UnicodeDecodeError)?;
    if strip(text).is_empty() {
        return Ok(Vec::new());
    }
    let data = if fmt == "json" {
        pyjson::loads(text)?
    } else {
        hub_core::yamlsafe::load(text, 8 * 1024 * 1024).map_err(|_| DocError::YamlError)?
    };
    match data {
        PyValue::None => Ok(Vec::new()),
        PyValue::Dict(d) => Ok(d),
        _ => Err(DocError::ValueError),
    }
}

fn key_is(k: &PyValue, p: &str) -> bool {
    k.as_str() == Some(p)
}

/// `get_dotted(doc, path)`: (found, value).
pub fn get_dotted<'a>(doc: &'a [(PyValue, PyValue)], path: &str) -> Option<&'a PyValue> {
    let mut parts = path.split('.');
    let first = parts.next()?;
    let mut cur = doc.iter().find(|(k, _)| key_is(k, first)).map(|(_, v)| v)?;
    for p in parts {
        let d = cur.as_dict()?;
        cur = d.iter().find(|(k, _)| key_is(k, p)).map(|(_, v)| v)?;
    }
    Some(cur)
}

/// `d[key] = value` (an existing key keeps its position).
fn dict_set(d: &mut Doc, key: &str, value: PyValue) {
    if let Some(slot) = d.iter_mut().find(|(k, _)| key_is(k, key)) {
        slot.1 = value;
    } else {
        d.push((PyValue::str(key), value));
    }
}

/// `set_dotted(doc, path, value)`: intermediate non-dicts are replaced by `{}`.
pub fn set_dotted(doc: &mut Doc, path: &str, value: PyValue) {
    let parts: Vec<&str> = path.split('.').collect();
    set_in(doc, &parts, value);
}

fn set_in(cur: &mut Doc, parts: &[&str], value: PyValue) {
    let [first, rest @ ..] = parts else { return };
    if rest.is_empty() {
        dict_set(cur, first, value);
        return;
    }
    if !cur
        .iter()
        .any(|(k, v)| key_is(k, first) && matches!(v, PyValue::Dict(_)))
    {
        dict_set(cur, first, PyValue::Dict(Vec::new()));
    }
    if let Some((_, PyValue::Dict(next))) = cur.iter_mut().find(|(k, _)| key_is(k, first)) {
        set_in(next, rest, value);
    }
}

/// `del_dotted(doc, path)`: delete a key and prune parents that this deletion left empty.
pub fn del_dotted(doc: &mut Doc, path: &str) {
    let parts: Vec<&str> = path.split('.').collect();
    del_in(doc, &parts);
}

/// Returns whether `cur` is empty after the deletion below it (only meaningful when the full chain existed).
fn del_in(cur: &mut Doc, parts: &[&str]) -> bool {
    let [first, rest @ ..] = parts else {
        return cur.is_empty();
    };
    if rest.is_empty() {
        cur.retain(|(k, _)| !key_is(k, first));
        return cur.is_empty();
    }
    let Some(idx) = cur
        .iter()
        .position(|(k, v)| key_is(k, first) && matches!(v, PyValue::Dict(_)))
    else {
        return false; // a missing or non-dict parent: nothing is deleted, nothing is pruned
    };
    let emptied = match &mut cur[idx].1 {
        PyValue::Dict(next) => del_in(next, rest),
        _ => false,
    };
    if emptied {
        cur.remove(idx);
    }
    cur.is_empty()
}

/// `_extract(doc, keys)`: `{k: value}` for every managed key present, in sorted key order.
fn extract(doc: &[(PyValue, PyValue)], keys: &[String]) -> Doc {
    let mut sorted: Vec<&String> = keys.iter().collect();
    sorted.sort();
    sorted
        .into_iter()
        .filter_map(|k| get_dotted(doc, k).map(|v| (PyValue::str(k.as_str()), v.clone())))
        .collect()
}

/// `_canon(obj)`: sha256 of `json.dumps(obj, sort_keys=True, separators=(",", ":"), default=str)`.
pub fn canon(obj: &PyValue) -> HubResult<String> {
    let text = pyjson::dumps(obj, &pyjson::Opts::canon())?;
    Ok(sha256(text.as_bytes()))
}

fn dump_doc(doc: &Doc, fmt: &str) -> HubResult<Vec<u8>> {
    let v = PyValue::Dict(doc.clone());
    if fmt == "json" {
        Ok((pyjson::dumps(&v, &pyjson::Opts::merge())? + "\n").into_bytes())
    } else {
        Ok(hub_core::yaml::dump(&v, &DumpOpts::FRONTMATTER).into_bytes())
    }
}

fn merge_keys(ds: &[Desired], live: Option<&[u8]>, fmt: &str, adopt_broken: bool) -> HubResult<Merged> {
    let mut keys: Vec<String> = ds.iter().flat_map(|d| d.managed_keys.iter().cloned()).collect();
    keys.sort();
    keys.dedup();
    let mut m = Merged::default();
    let mut doc: Doc = match live.map(|l| load_doc(l, fmt)) {
        None => Vec::new(),
        Some(Ok(d)) => d,
        Some(Err(DocError::RecursionError)) => return Err(Problem::internal()),
        Some(Err(e)) => {
            if !adopt_broken {
                m.unparseable = format!("live file is not a mergeable {fmt} document ({})", e.class());
                return Ok(m);
            }
            Vec::new()
        }
    };
    let live_slice = extract(&doc, &keys);
    m.slice_live = if live.is_some() && !live_slice.is_empty() {
        Some(canon(&PyValue::Dict(live_slice))?)
    } else {
        None
    };
    for d in ds {
        let src = match d.content.as_deref().map(|c| load_doc(c, fmt)) {
            None => Vec::new(),
            Some(Ok(s)) => s,
            Some(Err(DocError::RecursionError)) => return Err(Problem::internal()),
            Some(Err(e)) => {
                m.unparseable = format!("rendered artifact is not a valid {fmt} document ({})", e.class());
                return Ok(m);
            }
        };
        for k in &d.managed_keys {
            match get_dotted(&src, k) {
                Some(v) => set_dotted(&mut doc, k, v.clone()),
                None => del_dotted(&mut doc, k),
            }
        }
    }
    let new_slice = extract(&doc, &keys);
    m.slice_new = if new_slice.is_empty() {
        None
    } else {
        Some(canon(&PyValue::Dict(new_slice))?)
    };
    m.empty_after = doc.is_empty();
    m.new = if live.is_none() && doc.is_empty() {
        None
    } else {
        Some(dump_doc(&doc, fmt)?)
    };
    Ok(m)
}

/// `_style(content)`: md when the block starts (after ASCII whitespace) with the md begin marker.
fn style_of(content: &[u8]) -> &'static str {
    let start = content
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
        .unwrap_or(content.len());
    if content[start..].starts_with(MD_BEGIN_PREFIX.as_bytes()) {
        "md"
    } else {
        "hash"
    }
}

fn merge_block(ds: &[Desired], live: Option<&[u8]>, adopt_broken: bool) -> HubResult<Merged> {
    let mut m = Merged::default();
    let Some(d) = ds.first() else {
        return Err(Problem::internal());
    };
    let style = match &d.content {
        Some(c) => style_of(c),
        None if d.path.ends_with(".md") || d.path.ends_with(".markdown") => "md",
        None => "hash",
    };
    m.block_style = style.to_string();
    let mut text = live
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .unwrap_or_default();
    let found = match find_region(&text, style) {
        Ok(f) => f,
        Err(_) => {
            if !adopt_broken {
                m.unparseable = "hub begin marker without a matching end marker".into();
                return Ok(m);
            }
            // adopt: drop the broken tail (from the FIRST occurrence of the begin prefix, as `text.index`)
            let cut = text.find(marks(style).0).unwrap_or(text.len());
            text.truncate(cut);
            None
        }
    };
    let region = found.map(|(a, b)| {
        let mut r = text[a..b].to_string();
        if !r.ends_with('\n') {
            r.push('\n');
        }
        r
    });
    m.slice_live = region.map(|r| sha256(r.as_bytes()));
    let new_text = match &d.content {
        None => {
            m.slice_new = None;
            let t = match found {
                None => text.clone(),
                Some((a, b)) => {
                    let (before, after) = (&text[..a], &text[b..]);
                    let sep = if !strip(before).is_empty() && !strip(after).is_empty() {
                        "\n\n"
                    } else if !strip(before).is_empty() {
                        "\n"
                    } else {
                        ""
                    };
                    format!(
                        "{}{sep}{}",
                        before.trim_end_matches('\n'),
                        after.trim_start_matches('\n')
                    )
                }
            };
            if strip(&t).is_empty() { String::new() } else { t }
        }
        Some(c) => {
            // Python: `d.content.decode("utf-8")` (strict); a rendered block that is not UTF-8 is an unexpected error
            let mut block = String::from_utf8(c.clone()).map_err(|_| Problem::internal())?;
            if !block.ends_with('\n') {
                block.push('\n');
            }
            m.slice_new = Some(sha256(block.as_bytes()));
            match found {
                Some((a, b)) => format!("{}{block}{}", &text[..a], &text[b..]),
                None => format!(
                    "{}{}{block}",
                    text.trim_end_matches('\n'),
                    if strip(&text).is_empty() { "" } else { "\n\n" }
                ),
            }
        }
    };
    m.empty_after = strip(&new_text).is_empty();
    m.new = if live.is_none() && d.content.is_none() {
        None
    } else {
        Some(new_text.into_bytes())
    };
    Ok(m)
}

/// `merge_file(ds, live, adopt_broken=False)`.
pub fn merge_file(ds: &[Desired], live: Option<&[u8]>, adopt_broken: bool) -> HubResult<Merged> {
    let Some(d) = ds.first() else {
        return Err(Problem::internal());
    };
    match d.merge.as_str() {
        "own" => {
            let mut m = Merged {
                slice_live: live.map(sha256),
                ..Merged::default()
            };
            if let Some(c) = &d.content {
                m.new = Some(c.clone());
                m.slice_new = Some(sha256(c));
            }
            Ok(m)
        }
        "json_keys" => merge_keys(ds, live, "json", adopt_broken),
        "yaml_keys" => merge_keys(ds, live, "yaml", adopt_broken),
        "block" => merge_block(ds, live, adopt_broken),
        other => Ok(Merged {
            unparseable: format!("unknown merge mode {}", hub_core::py::repr(other)),
            ..Merged::default()
        }),
    }
}

/// CPython's `json` module over [`PyValue`]: `loads` (json.decoder/scanner semantics) and `dumps` (the encoder's
/// layouts the hub uses).
pub mod pyjson {
    use hub_core::py::float;
    use hub_core::py::json::write_str;
    use hub_core::value::PyValue;
    use hub_core::{HubResult, Problem};

    use super::DocError;

    /// Python's default recursion limit is 1000 frames; deeper JSON raises RecursionError.
    const MAX_DEPTH: usize = 990;

    struct P<'a> {
        s: &'a [u8],
        t: &'a str,
        i: usize,
    }

    fn ws(p: &mut P<'_>) {
        while p.i < p.s.len() && matches!(p.s[p.i], b' ' | b'\t' | b'\n' | b'\r') {
            p.i += 1;
        }
    }

    type R<T> = Result<T, DocError>;

    fn value(p: &mut P<'_>, depth: usize) -> R<PyValue> {
        let s = p.s;
        let Some(&c) = s.get(p.i) else {
            return Err(DocError::JsonDecodeError);
        };
        let rest = &s[p.i..];
        match c {
            b'"' => {
                p.i += 1;
                Ok(PyValue::Str(string(p)?))
            }
            b'{' => {
                if depth >= MAX_DEPTH {
                    return Err(DocError::RecursionError);
                }
                p.i += 1;
                object(p, depth + 1)
            }
            b'[' => {
                if depth >= MAX_DEPTH {
                    return Err(DocError::RecursionError);
                }
                p.i += 1;
                array(p, depth + 1)
            }
            b'n' if rest.starts_with(b"null") => {
                p.i += 4;
                Ok(PyValue::None)
            }
            b't' if rest.starts_with(b"true") => {
                p.i += 4;
                Ok(PyValue::Bool(true))
            }
            b'f' if rest.starts_with(b"false") => {
                p.i += 5;
                Ok(PyValue::Bool(false))
            }
            b'N' if rest.starts_with(b"NaN") => {
                p.i += 3;
                Ok(PyValue::Float(f64::NAN))
            }
            b'I' if rest.starts_with(b"Infinity") => {
                p.i += 8;
                Ok(PyValue::Float(f64::INFINITY))
            }
            b'-' if rest.starts_with(b"-Infinity") => {
                p.i += 9;
                Ok(PyValue::Float(f64::NEG_INFINITY))
            }
            _ => number(p),
        }
    }

    /// NUMBER_RE = `(-?(?:0|[1-9][0-9]*))(\.[0-9]+)?([eE][-+]?[0-9]+)?`
    fn number(p: &mut P<'_>) -> R<PyValue> {
        let s = p.s;
        let start = p.i;
        let mut i = p.i;
        if s.get(i) == Some(&b'-') {
            i += 1;
        }
        match s.get(i) {
            Some(b'0') => i += 1,
            Some(b'1'..=b'9') => {
                while s.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
            }
            _ => return Err(DocError::JsonDecodeError),
        }
        let int_end = i;
        let mut is_float = false;
        if s.get(i) == Some(&b'.') && s.get(i + 1).is_some_and(u8::is_ascii_digit) {
            i += 1;
            while s.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            is_float = true;
        }
        if matches!(s.get(i), Some(b'e' | b'E')) {
            let mut j = i + 1;
            if matches!(s.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            if s.get(j).is_some_and(u8::is_ascii_digit) {
                while s.get(j).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                }
                i = j;
                is_float = true;
            }
        }
        p.i = i;
        let text = &p.t[start..i];
        if is_float {
            return text
                .parse::<f64>()
                .map(PyValue::Float)
                .map_err(|_| DocError::JsonDecodeError);
        }
        let int_text = &p.t[start..int_end];
        Ok(match int_text.parse::<i64>() {
            Ok(v) => PyValue::Int(v),
            Err(_) => PyValue::BigInt(int_text.to_string()),
        })
    }

    fn hex4(s: &[u8], at: usize) -> R<u32> {
        let h = s.get(at..at + 4).ok_or(DocError::JsonDecodeError)?;
        let t = std::str::from_utf8(h).map_err(|_| DocError::JsonDecodeError)?;
        if !t.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(DocError::JsonDecodeError);
        }
        u32::from_str_radix(t, 16).map_err(|_| DocError::JsonDecodeError)
    }

    /// `scanstring(s, end, strict=True)` after the opening quote.
    fn string(p: &mut P<'_>) -> R<String> {
        let s = p.s;
        let mut out = String::new();
        loop {
            let chunk_start = p.i;
            while p.i < s.len() && s[p.i] != b'"' && s[p.i] != b'\\' && s[p.i] >= 0x20 {
                p.i += 1;
            }
            out.push_str(&p.t[chunk_start..p.i]);
            match s.get(p.i) {
                Some(b'"') => {
                    p.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    let esc = *s.get(p.i + 1).ok_or(DocError::JsonDecodeError)?;
                    p.i += 2;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut uni = hex4(s, p.i)?;
                            p.i += 4;
                            if (0xd800..=0xdbff).contains(&uni) && s.get(p.i..p.i + 2) == Some(b"\\u") {
                                let uni2 = hex4(s, p.i + 2)?;
                                if (0xdc00..=0xdfff).contains(&uni2) {
                                    uni = 0x10000 + (((uni - 0xd800) << 10) | (uni2 - 0xdc00));
                                    p.i += 6;
                                }
                            }
                            // a lone surrogate cannot live in a Rust String (Python keeps it): refused as a decode error
                            out.push(char::from_u32(uni).ok_or(DocError::JsonDecodeError)?);
                        }
                        _ => return Err(DocError::JsonDecodeError),
                    }
                }
                // end of input, or a control character (strict)
                _ => return Err(DocError::JsonDecodeError),
            }
        }
    }

    fn object(p: &mut P<'_>, depth: usize) -> R<PyValue> {
        let mut pairs: Vec<(PyValue, PyValue)> = Vec::new();
        let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        ws(p);
        if p.s.get(p.i) == Some(&b'}') {
            p.i += 1;
            return Ok(PyValue::Dict(pairs));
        }
        loop {
            if p.s.get(p.i) != Some(&b'"') {
                return Err(DocError::JsonDecodeError);
            }
            p.i += 1;
            let key = string(p)?;
            ws(p);
            if p.s.get(p.i) != Some(&b':') {
                return Err(DocError::JsonDecodeError);
            }
            p.i += 1;
            ws(p);
            let v = value(p, depth)?;
            match index.get(&key) {
                Some(&at) => pairs[at].1 = v, // a duplicate key keeps its first position, the last value wins
                None => {
                    index.insert(key.clone(), pairs.len());
                    pairs.push((PyValue::Str(key), v));
                }
            }
            ws(p);
            match p.s.get(p.i) {
                Some(b'}') => {
                    p.i += 1;
                    return Ok(PyValue::Dict(pairs));
                }
                Some(b',') => {
                    p.i += 1;
                    ws(p);
                }
                _ => return Err(DocError::JsonDecodeError),
            }
        }
    }

    fn array(p: &mut P<'_>, depth: usize) -> R<PyValue> {
        let mut items = Vec::new();
        ws(p);
        if p.s.get(p.i) == Some(&b']') {
            p.i += 1;
            return Ok(PyValue::List(items));
        }
        loop {
            items.push(value(p, depth)?);
            ws(p);
            match p.s.get(p.i) {
                Some(b']') => {
                    p.i += 1;
                    return Ok(PyValue::List(items));
                }
                Some(b',') => {
                    p.i += 1;
                    ws(p);
                }
                _ => return Err(DocError::JsonDecodeError),
            }
        }
    }

    /// `json.loads(text)` (str input).
    pub fn loads(text: &str) -> Result<PyValue, DocError> {
        if text.starts_with('\u{feff}') {
            return Err(DocError::JsonDecodeError);
        }
        let mut p = P {
            s: text.as_bytes(),
            t: text,
            i: 0,
        };
        ws(&mut p);
        let v = value(&mut p, 0)?;
        ws(&mut p);
        if p.i != p.s.len() {
            return Err(DocError::JsonDecodeError);
        }
        Ok(v)
    }

    /// The `json.dumps` keyword arguments merge.py uses.
    pub struct Opts {
        pub indent: Option<usize>,
        pub sort_keys: bool,
        pub ensure_ascii: bool,
        pub item_sep: &'static str,
        pub key_sep: &'static str,
        /// `default=str`
        pub default_str: bool,
    }

    impl Opts {
        /// `_canon`: `sort_keys=True, separators=(",", ":"), default=str` (ensure_ascii default True).
        pub fn canon() -> Self {
            Self {
                indent: None,
                sort_keys: true,
                ensure_ascii: true,
                item_sep: ",",
                key_sep: ":",
                default_str: true,
            }
        }
        /// `_dump_doc` (json): `indent=2, ensure_ascii=False`.
        pub fn merge() -> Self {
            Self {
                indent: Some(2),
                sort_keys: false,
                ensure_ascii: false,
                item_sep: ",",
                key_sep: ": ",
                default_str: false,
            }
        }
    }

    /// A dict key as the encoder writes it (str, or float/bool/None/int converted); TypeError otherwise.
    fn key_text(k: &PyValue) -> HubResult<String> {
        Ok(match k {
            PyValue::Str(s) => s.clone(),
            PyValue::Float(f) => float::json(*f),
            PyValue::Bool(b) => if *b { "true" } else { "false" }.into(),
            PyValue::None => "null".into(),
            PyValue::Int(i) => i.to_string(),
            PyValue::BigInt(t) => t.clone(),
            _ => return Err(Problem::internal()), // TypeError: keys must be str, int, float, bool or None
        })
    }

    /// `sorted(dct.items())` key comparison: str with str, numbers with numbers; anything else is a TypeError.
    fn cmp_keys(a: &PyValue, b: &PyValue) -> Option<std::cmp::Ordering> {
        fn num(v: &PyValue) -> Option<f64> {
            match v {
                PyValue::Bool(b) => Some(f64::from(u8::from(*b))),
                #[allow(clippy::cast_precision_loss)]
                PyValue::Int(i) => Some(*i as f64),
                PyValue::BigInt(t) => t.parse().ok(),
                PyValue::Float(f) => Some(*f),
                _ => None,
            }
        }
        match (a, b) {
            (PyValue::Str(x), PyValue::Str(y)) => Some(x.cmp(y)),
            _ => match (num(a), num(b)) {
                (Some(x), Some(y)) => x.partial_cmp(&y).or(Some(std::cmp::Ordering::Equal)),
                _ => None,
            },
        }
    }

    fn nl(out: &mut String, o: &Opts, level: usize) {
        if let Some(n) = o.indent {
            out.push('\n');
            out.push_str(&" ".repeat(n * level));
        }
    }

    fn write(out: &mut String, v: &PyValue, o: &Opts, level: usize) -> HubResult<()> {
        match v {
            PyValue::None => out.push_str("null"),
            PyValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            PyValue::Int(i) => out.push_str(&i.to_string()),
            PyValue::BigInt(t) => out.push_str(t),
            PyValue::Float(f) => out.push_str(&float::json(*f)),
            PyValue::Str(s) => write_str(out, s, o.ensure_ascii),
            PyValue::List(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return Ok(());
                }
                out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(o.item_sep);
                    }
                    nl(out, o, level + 1);
                    write(out, it, o, level + 1)?;
                }
                nl(out, o, level);
                out.push(']');
            }
            PyValue::Dict(pairs) => {
                if pairs.is_empty() {
                    out.push_str("{}");
                    return Ok(());
                }
                let mut items: Vec<&(PyValue, PyValue)> = pairs.iter().collect();
                if o.sort_keys {
                    let mut bad = false;
                    items.sort_by(|a, b| {
                        cmp_keys(&a.0, &b.0).unwrap_or_else(|| {
                            bad = true;
                            std::cmp::Ordering::Equal
                        })
                    });
                    if bad {
                        return Err(Problem::internal()); // TypeError: '<' not supported between key types
                    }
                }
                out.push('{');
                for (i, (k, val)) in items.into_iter().enumerate() {
                    if i > 0 {
                        out.push_str(o.item_sep);
                    }
                    nl(out, o, level + 1);
                    write_str(out, &key_text(k)?, o.ensure_ascii);
                    out.push_str(o.key_sep);
                    write(out, val, o, level + 1)?;
                }
                nl(out, o, level);
                out.push('}');
            }
            // not JSON types: `default=str` turns them into their str(); without it json raises TypeError
            other => {
                if !o.default_str {
                    return Err(Problem::internal());
                }
                write_str(out, &other.py_str(), o.ensure_ascii);
            }
        }
        Ok(())
    }

    /// `json.dumps(v, **opts)`; Err (500) where Python raises TypeError.
    pub fn dumps(v: &PyValue, o: &Opts) -> HubResult<String> {
        let mut out = String::new();
        write(&mut out, v, o, 0)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type R = Result<(), Box<dyn std::error::Error>>;

    fn d(merge: &str, content: Option<&[u8]>, keys: &[&str], path: &str) -> Desired {
        Desired {
            root: "home".into(),
            path: path.into(),
            kind: "mcp".into(),
            merge: merge.into(),
            source_ids: vec![],
            mode: 0o644,
            managed_keys: keys.iter().map(|k| (*k).to_string()).collect(),
            content: content.map(<[u8]>::to_vec),
            managed: true,
        }
    }

    fn s(k: &str) -> PyValue {
        PyValue::str(k)
    }

    /// tests/core/test_fs_merge.py::test_dotted_helpers
    #[test]
    fn test_dotted_helpers() {
        let mut doc: Doc = Vec::new();
        set_dotted(&mut doc, "a.b.c", PyValue::Int(1));
        assert_eq!(get_dotted(&doc, "a.b.c"), Some(&PyValue::Int(1)));
        assert_eq!(get_dotted(&doc, "a.x"), None);
        set_dotted(&mut doc, "a.keep", PyValue::Int(2));
        del_dotted(&mut doc, "a.b.c");
        assert_eq!(
            PyValue::Dict(doc.clone()),
            PyValue::Dict(vec![(s("a"), PyValue::Dict(vec![(s("keep"), PyValue::Int(2))]))])
        );
        del_dotted(&mut doc, "no.such.key");
        del_dotted(&mut doc, "a.keep");
        assert!(doc.is_empty());
    }

    /// tests/core/test_fs_merge.py::test_yaml_keys_merge_preserves_foreign
    #[test]
    fn test_yaml_keys_merge_preserves_foreign() -> R {
        let live = b"theme: dark\nagent:\n  disabled_toolsets: [old]\n  other: 1\n";
        let k = ["agent.disabled_toolsets"];
        let content: &[u8] = b"agent:\n  disabled_toolsets: [browser]\n";
        let m = merge_file(&[d("yaml_keys", Some(content), &k, "f")], Some(live), false)?;
        let new = String::from_utf8(m.new.clone().ok_or("no new")?)?;
        assert!(
            new.contains("theme: dark") && new.contains("other: 1") && new.contains("browser") && !new.contains("old")
        );
        assert_ne!(m.slice_live, m.slice_new);
        let again = merge_file(&[d("yaml_keys", Some(content), &k, "f")], Some(new.as_bytes()), false)?;
        assert_eq!(again.slice_live, again.slice_new);
        let alias = merge_file(
            &[d("yaml_keys", Some(b"x: &a 1\ny: *a\n"), &["x"], "f")],
            Some(b""),
            false,
        )?;
        assert!(!alias.unparseable.is_empty());
        let list = merge_file(&[d("yaml_keys", Some(b"- list\n"), &["x"], "f")], Some(b""), false)?;
        assert!(!list.unparseable.is_empty());
        Ok(())
    }

    /// tests/core/test_fs_merge.py::test_json_merge_edge_cases
    #[test]
    fn test_json_merge_edge_cases() -> R {
        assert_eq!(
            merge_file(&[d("json_keys", Some(b"{}"), &["a"], "f")], None, false)?.new,
            None
        );
        assert_eq!(
            merge_file(&[d("json_keys", Some(b"[1]"), &["a"], "f")], None, false)?.unparseable,
            "rendered artifact is not a valid json document (ValueError)"
        );
        assert_eq!(
            merge_file(&[d("json_keys", Some(b"{\"a\": 1}"), &["a"], "f")], Some(b"[1]"), false)?.unparseable,
            "live file is not a mergeable json document (ValueError)"
        );
        assert!(
            merge_file(&[d("json_keys", Some(b"{\"a\": 1}"), &["a"], "f")], Some(b"[1]"), true)?
                .new
                .is_some()
        );
        let m = merge_file(
            &[d("json_keys", None, &["a"], "f")],
            Some(b"{\"a\": 1, \"b\": 2}"),
            false,
        )?;
        let new = String::from_utf8(m.new.clone().ok_or("no new")?)?;
        assert!(new.contains("\"b\"") && !new.contains("\"a\"") && !m.empty_after);
        assert!(merge_file(&[d("json_keys", None, &["a"], "f")], Some(b"{\"a\": 1}"), false)?.empty_after);
        assert_eq!(
            merge_file(&[d("nonsense", Some(b"x"), &[], "f")], Some(b""), false)?.unparseable,
            "unknown merge mode 'nonsense'"
        );
        assert_eq!(
            merge_file(&[d("json_keys", Some(b"{}"), &["a"], "f")], Some(b"{ // c\n}"), false)?.unparseable,
            "live file is not a mergeable json document (JSONDecodeError)"
        );
        assert_eq!(
            merge_file(&[d("json_keys", Some(b"{}"), &["a"], "f")], Some(b"\xff"), false)?.unparseable,
            "live file is not a mergeable json document (UnicodeDecodeError)"
        );
        Ok(())
    }

    /// tests/core/test_fs_merge.py::test_hash_style_block_and_removal
    #[test]
    fn test_hash_style_block_and_removal() -> R {
        let blk: &[u8] = b"# agent-hub:begin (managed)\nkey = 1\n# agent-hub:end\n";
        let live: &[u8] = b"[core]\nx = 1\n";
        let m = merge_file(&[d("block", Some(blk), &[], "config.toml")], Some(live), false)?;
        assert_eq!(m.new, Some([live, b"\n", blk].concat()));
        assert_eq!(m.block_style, "hash");
        let removed = merge_file(&[d("block", None, &[], "config.toml")], m.new.as_deref(), false)?;
        assert_eq!((removed.new.as_deref(), removed.slice_new), (Some(live), None));
        let only = merge_file(&[d("block", None, &[], "config.toml")], Some(blk), false)?;
        assert!(only.empty_after && only.new.as_deref() == Some(&b""[..]));
        assert_eq!(merge_file(&[d("block", None, &[], "x.md")], None, false)?.new, None);
        let text = "a\n<!-- agent-hub:begin x -->\nbody\n";
        assert_eq!(find_region(text, "md"), Err("broken"));
        assert_eq!(find_region("plain\n", "md"), Ok(None));
        let adopt = merge_file(
            &[d(
                "block",
                Some(b"<!-- agent-hub:begin x -->\nnew\n<!-- agent-hub:end -->\n"),
                &[],
                "x.md",
            )],
            Some(text.as_bytes()),
            true,
        )?;
        assert!(
            adopt
                .new
                .as_deref()
                .is_some_and(|n| n.starts_with(b"a\n\n<!-- agent-hub:begin x -->\nnew"))
        );
        Ok(())
    }

    #[test]
    fn find_region_byte_offsets_with_unicode() {
        let text = "é😀\n<!-- agent-hub:begin -->\nx\n<!-- agent-hub:end -->  \nafter";
        let (a, b) = find_region(text, "md").unwrap_or_default().unwrap_or_default();
        assert_eq!(&text[a..b], "<!-- agent-hub:begin -->\nx\n<!-- agent-hub:end -->  \n");
    }

    fn b64(v: &serde_json::Value) -> Option<Vec<u8>> {
        use base64::Engine as _;
        v.as_str()
            .and_then(|t| base64::engine::general_purpose::STANDARD.decode(t).ok())
    }

    /// Every case of a golden file generated by Python's merge_file (tests/golden/gen_delivery.py).
    fn check_goldens(text: &str) -> R {
        let doc: serde_json::Value = serde_json::from_str(text)?;
        let cases = doc["cases"].as_array().ok_or("no cases")?;
        for (n, c) in cases.iter().enumerate() {
            let ds: Vec<Desired> = c["desired"]
                .as_array()
                .ok_or("desired")?
                .iter()
                .map(|x| {
                    let keys: Vec<String> = x["keys"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|k| k.as_str().map(String::from)).collect())
                        .unwrap_or_default();
                    Desired {
                        root: "home".into(),
                        path: c["path"].as_str().unwrap_or_default().into(),
                        kind: "mcp".into(),
                        merge: c["merge"].as_str().unwrap_or_default().into(),
                        source_ids: vec![],
                        mode: 0o644,
                        managed_keys: keys,
                        content: b64(&x["content"]),
                        managed: true,
                    }
                })
                .collect();
            let live = b64(&c["live"]);
            let adopt = c["adopt"].as_bool().unwrap_or(false);
            let want = &c["result"];
            let got = merge_file(&ds, live.as_deref(), adopt);
            if let Some(err) = want["error"].as_str() {
                assert!(got.is_err(), "case {n}: Python raised {err}, Rust returned {got:?}");
                continue;
            }
            let m = got.map_err(|p| format!("case {n}: {}", p.detail))?;
            assert_eq!(m.new, b64(&want["new"]), "case {n} new: {c}");
            assert_eq!(
                m.slice_live.as_deref(),
                want["slice_live"].as_str(),
                "case {n} slice_live"
            );
            assert_eq!(m.slice_new.as_deref(), want["slice_new"].as_str(), "case {n} slice_new");
            assert_eq!(
                m.unparseable,
                want["unparseable"].as_str().unwrap_or_default(),
                "case {n}"
            );
            assert_eq!(
                Some(m.empty_after),
                want["empty_after"].as_bool(),
                "case {n} empty_after"
            );
            assert_eq!(
                m.block_style,
                want["block_style"].as_str().unwrap_or_default(),
                "case {n}"
            );
        }
        Ok(())
    }

    /// json_keys / block / own merges are byte-identical to Python (228 generated cases).
    #[test]
    fn merge_file_matches_python_goldens() -> R {
        check_goldens(include_str!("../../tests/golden/merge.json"))
    }

    /// yaml_keys merges: the written bytes come from the YAML emitter (WS-Y's PyYAML port).
    #[test]
    fn merge_file_yaml_matches_python_goldens() -> R {
        check_goldens(include_str!("../../tests/golden/merge_yaml.json"))
    }

    #[test]
    fn pyjson_like_cpython() -> R {
        let v = pyjson::loads(
            r#" {"a": [1, 2.0, -0, 1e400, NaN, -Infinity, 123456789012345678901234567890], "a": "xé😀", "b": {}} "#,
        )
        .map_err(|e| e.class())?;
        let out = pyjson::dumps(&v, &pyjson::Opts::merge())?;
        assert_eq!(out, "{\n  \"a\": \"xé😀\",\n  \"b\": {}\n}");
        let v = pyjson::loads(r#"[1, 2.0, -0, 1e400, NaN, -Infinity, 123456789012345678901234567890, 1E-7]"#)
            .map_err(|e| e.class())?;
        assert_eq!(
            pyjson::dumps(&v, &pyjson::Opts::canon())?,
            "[1,2.0,0,Infinity,NaN,-Infinity,123456789012345678901234567890,1e-07]"
        );
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\":1,}",
            "01",
            "1.",
            "\"\t\"",
            "\u{feff}{}",
            "{} x",
            "tru",
            "-",
        ] {
            assert_eq!(pyjson::loads(bad).err(), Some(DocError::JsonDecodeError), "{bad:?}");
        }
        let deep = "[".repeat(2000);
        assert_eq!(pyjson::loads(&deep).err(), Some(DocError::RecursionError));
        Ok(())
    }

    fn arb_value() -> impl proptest::strategy::Strategy<Value = PyValue> {
        use proptest::prelude::*;
        let leaf = prop_oneof![
            Just(PyValue::None),
            any::<bool>().prop_map(PyValue::Bool),
            any::<i64>().prop_map(PyValue::Int),
            (-1.0e300f64..1.0e300).prop_map(PyValue::Float),
            "[ -~\u{e9}\u{1f600}\n\t\"\\\\]{0,12}".prop_map(PyValue::Str),
        ];
        leaf.prop_recursive(4, 48, 6, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..6).prop_map(PyValue::List),
                proptest::collection::btree_map("[a-z\u{e9}]{0,6}", inner, 0..6)
                    .prop_map(|m| PyValue::Dict(m.into_iter().map(|(k, v)| (PyValue::Str(k), v)).collect())),
            ]
        })
    }

    proptest::proptest! {
        /// `json.loads(json.dumps(v, indent=2, ensure_ascii=False))` gives `v` back, with ints and floats kept apart.
        #[test]
        fn pyjson_round_trips(v in arb_value()) {
            let text = pyjson::dumps(&v, &pyjson::Opts::merge()).map_err(|p| proptest::test_runner::TestCaseError::fail(p.detail))?;
            let back = pyjson::loads(&text).map_err(|e| proptest::test_runner::TestCaseError::fail(e.class()))?;
            proptest::prop_assert_eq!(back.repr(), v.repr());
            let canon = pyjson::dumps(&v, &pyjson::Opts::canon()).map_err(|p| proptest::test_runner::TestCaseError::fail(p.detail))?;
            proptest::prop_assert!(canon.is_ascii());
            let back = pyjson::loads(&canon).map_err(|e| proptest::test_runner::TestCaseError::fail(e.class()))?;
            proptest::prop_assert_eq!(back, v);
        }
    }
}
