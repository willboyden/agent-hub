//! Port of backend/src/agent_hub/adapters/_util.py: shared pure helpers for the client adapters (names and safe relative
//! paths, diagnostics, frontmatter render/parse, hub marker blocks, JSON/YAML key helpers, verify slice digests, capped
//! discover reads, skill/instruction renderers, sandbox wrapper). Owner: WS-A. Security control S-ADAPTER-PATHS.
//!
//! Byte-exact serialisers (PORTING.md "Emitted file formats"): [`dump_json`] = `json.dumps(doc, indent=2,
//! sort_keys=True, ensure_ascii=False) + "\n"`; [`dump_yaml`] = `yaml.safe_dump(doc, sort_keys=True, ...)`;
//! [`render_frontmatter`] = `"---\n" + safe_dump(fields, sort_keys=False, ...) + "---\n\n" + body.rstrip() + "\n"`.
//! [`find_region`]/[`slice_digest`] MUST agree with services/merge.rs (tests/test_markers.rs feeds both).
//!
//! The second half of this file holds the Python behaviours the adapters lean on when they read untrusted client files:
//! truthiness, iteration of any value, `sorted()` ordering (and its `TypeError`), `json.loads` (NaN/Infinity, big ints,
//! lone surrogates, duplicate keys, the C scanner's 9998-level nesting limit) and canonical `json.dumps(sort_keys=True,
//! separators=(",", ":"), default=str)`.

use std::collections::HashSet;
use std::sync::LazyLock;

use hub_core::py::{float, json as pyjson, os as pyos, text};
use hub_core::value::PyValue;
use hub_core::yaml::{self, DumpOpts};
use hub_core::{re, yamlsafe};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::base::{
    AdapterError, AdapterResult, Artifact, CAPABILITIES, Check, ClientConfig, Diag, Expected, FileSystem, Files,
    InstructionItem, SkillItem,
};

pub const MAX_DESC_WARN: usize = 1024;
pub const MAX_DESC_ERR: usize = 8192;
pub const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_READ_FILE_BYTES: usize = 512 * 1024;
pub const MAX_READ_TOTAL_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_BODY_CHARS: usize = 512 * 1024;
pub const MAX_DISCOVER_FILES: usize = 400;

pub const MD_BEGIN: &str = "<!-- agent-hub:begin (managed block; edits inside are overwritten) -->";
pub const MD_END: &str = "<!-- agent-hub:end -->";
pub const HASH_BEGIN: &str = "# agent-hub:begin (managed block; edits inside are overwritten)";
pub const HASH_END: &str = "# agent-hub:end";

pub const READ_ONLY_DROPS: [&str; 3] = ["write", "edit", "notebook"];

/// A parsed frontmatter mapping: `{cap_str(k, 200): v}` in document order.
pub type Frontmatter = IndexMap<String, PyValue>;

static NAME_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[a-z0-9][a-z0-9._-]{0,63}$"));
static DRIVE_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z]:"));
// Python `\s` (str patterns) is `str.isspace()`, which also covers \x1c-\x1f; Rust's `\s` does not.
static MARKER_LIKE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)(<!--[\s\x1c-\x1f]*agent-hub:)|(^[\s\x1c-\x1f]*#[\s\x1c-\x1f]*agent-hub:)"));

/// `UnsafePath(ValueError)`: a name or path that could escape its root.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct UnsafePath(pub String);

impl From<UnsafePath> for AdapterError {
    fn from(e: UnsafePath) -> Self {
        AdapterError::new("UnsafePath", e.0)
    }
}

/// `MarkerInjection(ValueError)`: a body line looks like a hub block marker.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("content contains a line that looks like an agent-hub block marker")]
pub struct MarkerInjection;

// ---- names and paths ---------------------------------------------------------------------------------------------------
/// `valid_name(name)`: `[a-z0-9][a-z0-9._-]{0,63}`, fullmatch.
pub fn valid_name(name: &str) -> bool {
    NAME_RE.is_match(name)
}

/// `safe_relpath(*parts)`: join relative path parts with `/`; refuse anything that could leave the root.
pub fn safe_relpath(parts: &[&str]) -> Result<String, UnsafePath> {
    let mut out: Vec<&str> = Vec::new();
    for part in parts {
        if part.is_empty() {
            return Err(UnsafePath("empty path part".into()));
        }
        if part.contains('\0') || part.contains('\\') || part.starts_with('/') || DRIVE_RE.is_match(part) {
            return Err(UnsafePath(format!("unsafe path part {}", text::repr(part))));
        }
        for seg in part.split('/') {
            if seg.is_empty() || seg == "." || seg == ".." {
                return Err(UnsafePath(format!("unsafe path segment in {}", text::repr(part))));
            }
            if seg.chars().any(|c| (c as u32) < 32) {
                return Err(UnsafePath("control character in path".into()));
            }
            out.push(seg);
        }
    }
    Ok(out.join("/"))
}

/// `fs_path(cfg, root, rel)`: the absolute path for the FileSystem port (`None` when the root is not configured).
pub fn fs_path(cfg: &ClientConfig, root: &str, rel: &str) -> Result<Option<String>, UnsafePath> {
    let Some(base) = cfg.roots.get(root).filter(|b| !b.is_empty()) else {
        return Ok(None);
    };
    Ok(Some(pyos::join(base, &safe_relpath(&[rel])?)))
}

// ---- diagnostics -------------------------------------------------------------------------------------------------------
/// `unsupported_severity(cfg)`: strict clients make every lossy mapping an error, others a warning.
pub fn unsupported_severity(cfg: &ClientConfig) -> &'static str {
    if cfg.strict { "error" } else { "warn" }
}

pub fn diag(sev: &str, code: &str, message: impl Into<String>, item: Option<&str>) -> Diag {
    Diag {
        severity: sev.into(),
        code: code.into(),
        message: message.into(),
        item: item.map(str::to_string),
    }
}

/// `oneline(text, item, out)`: `" ".join(text.split())`, with an info Diag when that changed anything.
pub fn oneline(s: &str, item: &str, out: &mut Vec<Diag>) -> String {
    let flat = text::collapse_ws(s);
    if flat != s {
        out.push(diag(
            "info",
            "description_normalized",
            "description whitespace/newlines collapsed to one line",
            Some(item),
        ));
    }
    flat
}

/// `check_description(desc, item, out)`: false (and an error Diag) when the description is too large to accept.
pub fn check_description(desc: &str, item: &str, out: &mut Vec<Diag>) -> bool {
    let n = text::char_len(desc);
    if n > MAX_DESC_ERR {
        out.push(diag(
            "error",
            "description_too_long",
            format!("description is {n} chars (limit {MAX_DESC_ERR})"),
            Some(item),
        ));
        return false;
    }
    if n > MAX_DESC_WARN {
        out.push(diag(
            "warn",
            "description_long",
            format!("description is {n} chars; clients may truncate above {MAX_DESC_WARN}"),
            Some(item),
        ));
    }
    true
}

/// `dedupe(names, kind, out)`: names that occur more than once (each reported once, as an error).
pub fn dedupe<'a>(names: impl IntoIterator<Item = &'a str>, kind: &str, out: &mut Vec<Diag>) -> HashSet<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut dup: HashSet<String> = HashSet::new();
    for n in names {
        if seen.contains(n) && !dup.contains(n) {
            dup.insert(n.to_string());
            out.push(diag(
                "error",
                "duplicate_name",
                format!("duplicate {kind} name {}", text::repr(n)),
                Some(n),
            ));
        }
        seen.insert(n);
    }
    dup
}

// ---- hashing / bytes ---------------------------------------------------------------------------------------------------
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn to_bytes(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

/// `bytes.decode("utf-8", errors="replace")`.
pub fn decode_replace(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `cap_str(value, limit)`: `str()` of a parsed value with a length cap; `None` (or an absent key), lists and dicts
/// give "".
pub fn cap_str(value: Option<&PyValue>, limit: usize) -> String {
    match value {
        None | Some(PyValue::None | PyValue::List(_) | PyValue::Dict(_)) => String::new(),
        Some(v) => text::take_chars(&v.py_str(), limit).to_string(),
    }
}

// ---- frontmatter -------------------------------------------------------------------------------------------------------
/// `render_frontmatter(fields, body)`: `---` YAML `---` body. Key order is the caller's.
pub fn render_frontmatter(fields: &PyValue, body: &str) -> String {
    let head = yaml::dump(fields, &DumpOpts::FRONTMATTER);
    format!("---\n{head}---\n\n{}\n", text::rstrip(body))
}

/// `parse_frontmatter(text)`: the tolerant inverse of [`render_frontmatter`]; no or a broken frontmatter gives
/// `({}, text)`. The YAML goes through the hardened loader (no aliases/anchors, size/depth/node caps). Only
/// `yamlsafe.YamlError` is caught, as in Python: an exception the loader lets escape (`ValueError` for `2024-02-30`,
/// `KeyError`, `RecursionError`, ...) is returned as `Err`.
pub fn parse_frontmatter(s: &str) -> AdapterResult<(Frontmatter, String)> {
    let none = || Ok((Frontmatter::new(), s.to_string()));
    if !s.starts_with("---") {
        return none();
    }
    let lines: Vec<&str> = s.split('\n').collect();
    if text::strip(lines[0]) != "---" {
        return none();
    }
    for i in 1..lines.len() {
        if text::rstrip(lines[i]) == "---" {
            let data = match yamlsafe::load_exact(&lines[1..i].join("\n"), yamlsafe::DEFAULT_MAX_BYTES) {
                Ok(d) => d,
                Err(yamlsafe::LoadError::Yaml(_)) => return none(),
                Err(yamlsafe::LoadError::Python { exc_type, message }) => {
                    return Err(AdapterError::new(exc_type, message));
                }
            };
            let pairs = match data {
                PyValue::None => Vec::new(),
                PyValue::Dict(d) => d,
                _ => return none(),
            };
            let mut fm = Frontmatter::new();
            for (k, v) in pairs {
                fm.insert(cap_str(Some(&k), 200), v);
            }
            let body = lines[i + 1..].join("\n");
            return Ok((fm, body.trim_start_matches('\n').to_string()));
        }
    }
    none()
}

// ---- hub blocks --------------------------------------------------------------------------------------------------------
/// `looks_like_marker(line)`: deliberately broader than the merge engine's exact rule.
pub fn looks_like_marker(line: &str) -> bool {
    MARKER_LIKE.is_match(line)
}

fn begin_end(style: &str) -> (&'static str, &'static str) {
    if style == "md" {
        (MD_BEGIN, MD_END)
    } else {
        (HASH_BEGIN, HASH_END)
    }
}

/// `wrap_block(body, style)`: the body between hub markers; refuses a body line that looks like a marker.
pub fn wrap_block(body: &str, style: &str) -> Result<String, MarkerInjection> {
    if text::splitlines(body, false).iter().any(|ln| looks_like_marker(ln)) {
        return Err(MarkerInjection);
    }
    let (begin, end) = begin_end(style);
    Ok(format!("{begin}\n{}\n{end}\n", text::strip(body)))
}

/// `find_region(text, style)` in BYTE offsets (Python returns code point offsets of the same span): `Ok(Some(span))`,
/// `Ok(None)` (no region) or `Err("broken")` (a begin line without an end line).
pub fn find_region(s: &str, style: &str) -> Result<Option<(usize, usize)>, &'static str> {
    let (begin, end) = match style {
        "md" => ("<!-- agent-hub:begin", MD_END),
        _ => ("# agent-hub:begin", HASH_END),
    };
    let mut pos = 0usize;
    let mut start: Option<usize> = None;
    for line in text::splitlines(s, true) {
        match start {
            None if line.starts_with(begin) => start = Some(pos),
            Some(st) if text::rstrip(line) == end => return Ok(Some((st, pos + line.len()))),
            _ => {}
        }
        pos += line.len();
    }
    if start.is_some() { Err("broken") } else { Ok(None) }
}

/// `extract_block(text, style)`: the marker region (always ending in a newline), or `None`.
pub fn extract_block(s: &str, style: &str) -> Option<String> {
    let Ok(Some((a, b))) = find_region(s, style) else {
        return None;
    };
    let region = &s[a..b];
    Some(if region.ends_with('\n') {
        region.to_string()
    } else {
        format!("{region}\n")
    })
}

/// `replace_block(existing, block, style)`: what the applier does for merge='block'.
pub fn replace_block(existing: &str, block: &str, style: &str) -> String {
    match extract_block(existing, style) {
        None => format!(
            "{}{}{block}",
            existing.trim_end_matches('\n'),
            if text::strip(existing).is_empty() { "" } else { "\n\n" }
        ),
        Some(old) => existing.replacen(old.trim_end_matches('\n'), block.trim_end_matches('\n'), 1),
    }
}

/// Instructions sorted by `(order, id)`.
pub fn sorted_instructions(items: &[InstructionItem]) -> Vec<&InstructionItem> {
    let mut v: Vec<&InstructionItem> = items.iter().collect();
    v.sort_by(|a, b| (a.order, &a.id).cmp(&(b.order, &b.id)));
    v
}

/// `render_instructions(items, headings)`: sorted by `(order, id)`; deterministic.
pub fn render_instructions(items: &[InstructionItem], headings: bool) -> String {
    sorted_instructions(items)
        .iter()
        .map(|it| {
            let head = if headings {
                format!("## {}\n\n", it.title)
            } else {
                String::new()
            };
            format!("{head}{}", text::strip(&it.body))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

// ---- json / yaml key helpers -------------------------------------------------------------------------------------------
/// `dump_json(doc)`: `json.dumps(doc, indent=2, sort_keys=True, ensure_ascii=False) + "\n"`.
pub fn dump_json(doc: &Value) -> Vec<u8> {
    let mut s = pyjson::dumps(doc, &pyjson::Opts::adapter());
    s.push('\n');
    s.into_bytes()
}

/// [`dump_json`] of a Python value (a generic spec's templates and static values can hold anything YAML loads):
/// `TypeError` for values `json.dumps` cannot encode, as Python raises.
pub fn dump_json_py(doc: &PyValue) -> AdapterResult<Vec<u8>> {
    let mut out = String::new();
    json_py_write(&mut out, doc, 0)?;
    out.push('\n');
    Ok(out.into_bytes())
}

fn json_py_newline(out: &mut String, level: usize) {
    out.push('\n');
    out.push_str(&"  ".repeat(level));
}

fn json_py_write(out: &mut String, v: &PyValue, level: usize) -> AdapterResult<()> {
    match v {
        PyValue::None => out.push_str("null"),
        PyValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        PyValue::Int(i) => out.push_str(&i.to_string()),
        PyValue::BigInt(s) => out.push_str(s),
        PyValue::Float(f) => out.push_str(&float::json(*f)),
        PyValue::Str(s) => pyjson::write_str(out, s, false),
        PyValue::List(items) if items.is_empty() => out.push_str("[]"),
        PyValue::List(items) => {
            out.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_py_newline(out, level + 1);
                json_py_write(out, it, level + 1)?;
            }
            json_py_newline(out, level);
            out.push(']');
        }
        PyValue::Dict(d) if d.is_empty() => out.push_str("{}"),
        PyValue::Dict(d) => {
            let items = sorted_items(d)?;
            out.push('{');
            for (i, (k, val)) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_py_newline(out, level + 1);
                pyjson::write_str(out, &canon_key(k)?, false);
                out.push_str(": ");
                json_py_write(out, val, level + 1)?;
            }
            json_py_newline(out, level);
            out.push('}');
        }
        other => {
            return Err(AdapterError::new(
                "TypeError",
                format!("Object of type {} is not JSON serializable", other.type_name()),
            ));
        }
    }
    Ok(())
}

/// `dump_yaml(doc)`: `yaml.safe_dump(doc, sort_keys=True, allow_unicode=True, default_flow_style=False, width=100000)`.
pub fn dump_yaml(doc: &PyValue) -> Vec<u8> {
    yaml::dump(doc, &DumpOpts::ADAPTER).into_bytes()
}

/// `set_dotted(doc, path, value)` on a JSON object.
pub fn set_dotted(doc: &mut serde_json::Map<String, Value>, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let (last, parents) = parts.split_last().unwrap_or((&"", &[]));
    let mut cur = doc;
    for p in parents {
        if !cur.get(*p).is_some_and(Value::is_object) {
            cur.insert((*p).to_string(), Value::Object(serde_json::Map::new()));
        }
        let Some(Value::Object(next)) = cur.get_mut(*p) else {
            return;
        };
        cur = next;
    }
    cur.insert((*last).to_string(), value);
}

/// `get_dotted(doc, path)` on a JSON value (`None` for an absent path; a JSON null is returned as `Some(Null)`).
pub fn get_dotted<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = doc;
    for p in path.split('.') {
        cur = cur.as_object()?.get(p)?;
    }
    Some(cur)
}

/// `del_dotted(doc, path)` on a JSON object.
pub fn del_dotted(doc: &mut serde_json::Map<String, Value>, path: &str) {
    let parts: Vec<&str> = path.split('.').collect();
    let (last, parents) = parts.split_last().unwrap_or((&"", &[]));
    let mut cur = doc;
    for p in parents {
        let Some(Value::Object(next)) = cur.get_mut(*p) else {
            return;
        };
        cur = next;
    }
    cur.shift_remove(*last);
}

/// `merge_keys(existing, managed_doc, managed_keys)`: replace ONLY the managed keys, keep everything else.
pub fn merge_keys(existing: &Value, managed_doc: &Value, managed_keys: &[&str]) -> Value {
    let mut out = existing.as_object().cloned().unwrap_or_default();
    for key in managed_keys {
        match get_dotted(managed_doc, key) {
            None | Some(Value::Null) => del_dotted(&mut out, key),
            Some(v) => set_dotted(&mut out, key, v.clone()),
        }
    }
    Value::Object(out)
}

/// `get_dotted` on a Python value (YAML/JSON documents read back from a client).
pub fn get_dotted_py<'a>(doc: &'a PyValue, path: &str) -> Option<&'a PyValue> {
    let mut cur = doc;
    for p in path.split('.') {
        cur = cur.get(p)?;
    }
    Some(cur)
}

// ---- capability mapping ------------------------------------------------------------------------------------------------
/// `ordered_caps(caps)`: canonical capabilities in canonical order.
pub fn ordered_caps<'a>(caps: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let have: HashSet<&str> = caps.into_iter().collect();
    CAPABILITIES
        .iter()
        .filter(|c| have.contains(*c))
        .map(|c| (*c).to_string())
        .collect()
}

/// `unknown_caps(caps)`: sorted, de-duplicated.
pub fn unknown_caps(caps: &[String]) -> Vec<String> {
    sorted_set(caps.iter().filter(|c| !CAPABILITIES.contains(&c.as_str())).cloned())
}

/// `effective_caps(caps, read_only)`.
pub fn effective_caps<'a>(caps: impl IntoIterator<Item = &'a str>, read_only: bool) -> Vec<String> {
    let mut have = ordered_caps(caps);
    if read_only {
        have.retain(|c| !READ_ONLY_DROPS.contains(&c.as_str()));
    }
    have
}

// ---- verify ------------------------------------------------------------------------------------------------------------
/// `_canon_digest(obj)`: sha256 of `json.dumps(obj, sort_keys=True, separators=(",", ":"), default=str)`.
pub fn canon_digest(obj: &PyValue) -> AdapterResult<String> {
    Ok(sha256_hex(canon_dumps(obj)?.as_bytes()))
}

/// `slice_digest(raw, merge, managed_keys)`: digest of the hub-owned slice of a live file, identical to the merge
/// engine's `slice_live` (own: whole file; block: the marker region; *_keys: canonical JSON of the managed keys
/// present). `Err` only where Python raises (a managed value whose dict keys cannot be sorted).
pub fn slice_digest(raw: &[u8], merge: &str, managed_keys: &[String]) -> AdapterResult<Option<String>> {
    if merge == "own" {
        return Ok(Some(sha256_hex(raw)));
    }
    let s = decode_replace(raw);
    if merge == "block" {
        let blk = extract_block(&s, "md").or_else(|| extract_block(&s, "hash"));
        return Ok(blk.map(|b| sha256_hex(b.as_bytes())));
    }
    let doc = if merge == "json_keys" {
        match json_loads(&s) {
            Ok(d) => d,
            Err(_) => return Ok(None),
        }
    } else {
        match yamlsafe::load_exact(&s, 8 * 1024 * 1024) {
            Ok(d) => d,
            // `except (ValueError, yamlsafe.YamlError)`
            Err(yamlsafe::LoadError::Yaml(_)) => return Ok(None),
            Err(yamlsafe::LoadError::Python { exc_type, .. })
                if exc_type == "ValueError" || exc_type == "UnicodeDecodeError" =>
            {
                return Ok(None);
            }
            Err(yamlsafe::LoadError::Python { exc_type, message }) => return Err(AdapterError::new(exc_type, message)),
        }
    };
    if !matches!(doc, PyValue::Dict(_)) {
        return Ok(None);
    }
    let mut keys: Vec<&String> = managed_keys.iter().collect();
    keys.sort();
    let mut found: Vec<(PyValue, PyValue)> = Vec::new();
    for k in keys {
        if let Some(v) = get_dotted_py(&doc, k).filter(|v| !v.is_none()) {
            match found.iter_mut().find(|(fk, _)| fk.as_str() == Some(k.as_str())) {
                Some(slot) => slot.1 = v.clone(),
                None => found.push((PyValue::str(k.clone()), v.clone())),
            }
        }
    }
    if found.is_empty() {
        return Ok(None);
    }
    canon_digest(&PyValue::Dict(found)).map(Some)
}

/// `verify_expected(cfg, expected, fs)`: files present with the delivered content (whole-file hash first, then, for
/// key-merged and block artifacts, only the hub-owned slice). A mismatch is never reported ok.
pub fn verify_expected(cfg: &ClientConfig, expected: &[Expected], fs: &dyn FileSystem) -> AdapterResult<Vec<Check>> {
    let mut sorted: Vec<&Expected> = expected.iter().collect();
    sorted.sort_by(|a, b| (&a.root, &a.path).cmp(&(&b.root, &b.path)));
    let mut checks = Vec::new();
    let check = |name: &str, ok: bool, detail: String| Check {
        name: name.to_string(),
        ok,
        detail,
    };
    for e in sorted {
        let name = format!("{}:{}", e.root, e.path);
        let p = match fs_path(cfg, &e.root, &e.path) {
            Err(exc) => {
                checks.push(check(&name, false, format!("unsafe path: {exc}")));
                continue;
            }
            Ok(None) => {
                checks.push(check(
                    &name,
                    false,
                    format!("root {} not configured", text::repr(&e.root)),
                ));
                continue;
            }
            Ok(Some(p)) => p,
        };
        if !fs.exists(&p) {
            checks.push(check(&name, false, "file missing".into()));
            continue;
        }
        let raw = match fs.read_bytes(&p) {
            Ok(r) => r,
            Err(exc) => {
                checks.push(check(&name, false, format!("unreadable: {}", pyos::error_name(&exc))));
                continue;
            }
        };
        if sha256_hex(&raw) == e.sha256 {
            checks.push(check(&name, true, "hash matches".into()));
            continue;
        }
        let merge = e.merge.as_str();
        if merge != "own" && slice_digest(&raw, merge, &e.managed_keys)?.as_deref() == Some(e.sha256.as_str()) {
            checks.push(check(&name, true, format!("managed {merge} slice matches")));
        } else if merge != "own" {
            checks.push(check(
                &name,
                false,
                format!("managed {merge} slice differs from what the hub delivered"),
            ));
        } else {
            checks.push(check(&name, false, "hash differs from the delivered content".into()));
        }
    }
    Ok(checks)
}

// ---- discover helpers --------------------------------------------------------------------------------------------------
/// The Python exception an `OSError` from the FileSystem port surfaces as (an unhandled `listdir` failure escapes
/// discover() in Python too).
pub fn os_error(e: &std::io::Error, path: &str) -> AdapterError {
    let msg = if e.raw_os_error().is_some() {
        pyos::error_str(e, Some(path))
    } else {
        e.to_string()
    };
    AdapterError::new(pyos::error_name(e), msg)
}

/// `sorted(fs.listdir(path))`, with an escaping OSError.
pub fn listdir_sorted(fs: &dyn FileSystem, path: &str) -> AdapterResult<Vec<String>> {
    let mut v = fs.listdir(path).map_err(|e| os_error(&e, path))?;
    v.sort();
    Ok(v)
}

/// `size_of(fs, path)`: the size BEFORE reading, when the port offers it; `None` = unknown.
pub fn size_of(fs: &dyn FileSystem, path: &str) -> Option<u64> {
    match fs.size(path) {
        Some(Ok(n)) => Some(n),
        _ => None,
    }
}

/// `read_capped(fs, path, notes, limit)`: one file, skipped (with a note) when over `limit`.
pub fn read_capped(fs: &dyn FileSystem, path: &str, notes: &mut Vec<String>, limit: usize) -> Option<Vec<u8>> {
    if let Some(n) = size_of(fs, path)
        && n > limit as u64
    {
        notes.push(format!(
            "skipped {}: {n} bytes exceeds the {limit} byte read cap",
            pyos::basename(path)
        ));
        return None;
    }
    let data = fs.read_bytes(path).ok()?;
    if data.len() > limit {
        notes.push(format!(
            "skipped {}: {} bytes exceeds the {limit} byte read cap",
            pyos::basename(path),
            data.len()
        ));
        return None;
    }
    Some(data)
}

/// `read_text(fs, path, notes)`: capped text read ("" when unreadable or over the cap).
pub fn read_text(fs: &dyn FileSystem, path: &str, notes: &mut Vec<String>) -> String {
    read_capped(fs, path, notes, MAX_READ_FILE_BYTES)
        .map(|d| decode_replace(&d))
        .unwrap_or_default()
}

struct TreeWalk<'a> {
    fs: &'a dyn FileSystem,
    base: &'a str,
    limit: usize,
    out: Files,
    total: usize,
}

fn tree_walk(w: &mut TreeWalk<'_>, rel: &str, sink: &mut Vec<String>) -> AdapterResult<()> {
    let cur = if rel.is_empty() {
        w.base.to_string()
    } else {
        pyos::join(w.base, rel)
    };
    for name in listdir_sorted(w.fs, &cur)? {
        if w.out.len() >= w.limit {
            if !sink.iter().any(|n| n.contains("file limit")) {
                sink.push(format!("stopped reading {}: {} file limit reached", w.base, w.limit));
            }
            return Ok(());
        }
        let r = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let p = pyos::join(w.base, &r);
        if w.fs.is_dir(&p) {
            if matches!(name.as_str(), "__pycache__" | ".git" | "node_modules") {
                continue;
            }
            tree_walk(w, &r, sink)?;
            continue;
        }
        let Some(data) = read_capped(w.fs, &p, sink, MAX_READ_FILE_BYTES) else {
            continue;
        };
        if w.total + data.len() > MAX_READ_TOTAL_BYTES {
            sink.push(format!(
                "skipped {r}: total read cap of {MAX_READ_TOTAL_BYTES} bytes reached"
            ));
            continue;
        }
        w.total += data.len();
        w.out.insert(r, data);
    }
    Ok(())
}

/// `read_tree(fs, base, limit, notes)`: a directory tree (relpath -> bytes) through the port: at most `limit` files,
/// 512 KB per file, 16 MB in total; `__pycache__`, `.git` and `node_modules` are skipped.
pub fn read_tree(fs: &dyn FileSystem, base: &str, limit: usize, notes: &mut Vec<String>) -> AdapterResult<Files> {
    let mut w = TreeWalk {
        fs,
        base,
        limit,
        out: Files::new(),
        total: 0,
    };
    if fs.exists(base) && fs.is_dir(base) {
        tree_walk(&mut w, "", notes)?;
    }
    Ok(w.out)
}

// ---- shared renderers --------------------------------------------------------------------------------------------------
/// `skill_artifacts(skills, root, base, out, skill_filename)`: skills as `<base>/<name>/<relpath>` files (SKILL.md passed
/// through untouched; only its frontmatter name is cross-checked).
pub fn skill_artifacts(
    skills: &[SkillItem],
    root: &str,
    base: &str,
    out: &mut Vec<Diag>,
    skill_filename: &str,
) -> AdapterResult<Vec<Artifact>> {
    let mut arts = Vec::new();
    let dup = dedupe(skills.iter().map(|s| s.name.as_str()), "skill", out);
    let mut sorted: Vec<&SkillItem> = skills.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for sk in sorted {
        if dup.contains(&sk.name) {
            continue;
        }
        if !valid_name(&sk.name) {
            out.push(diag(
                "error",
                "invalid_name",
                format!("skill name {} is not a valid identifier", text::repr(&sk.name)),
                Some(&sk.name),
            ));
            continue;
        }
        if !check_description(&sk.description, &sk.name, out) {
            continue;
        }
        let Some(skill_md) = sk.files.get("SKILL.md") else {
            out.push(diag(
                "error",
                "missing_skill_md",
                "skill has no SKILL.md",
                Some(&sk.name),
            ));
            continue;
        };
        let (fm, _) = parse_frontmatter(&decode_replace(skill_md))?;
        let fm_name = fm.get("name");
        if fm_name.is_none_or(|n| n.as_str() != Some(sk.name.as_str())) {
            let shown = fm_name.map_or_else(|| "None".to_string(), PyValue::repr);
            out.push(diag(
                "warn",
                "skill_name_mismatch",
                format!("SKILL.md frontmatter name {shown} differs from the skill name"),
                Some(&sk.name),
            ));
        }
        let mut rels: Vec<&String> = sk.files.keys().collect();
        rels.sort();
        let mut staged = Vec::new();
        let mut bad = false;
        for rel in rels {
            let data = &sk.files[rel.as_str()];
            let fname = if rel == "SKILL.md" {
                skill_filename
            } else {
                rel.as_str()
            };
            let path = match safe_relpath(&[base, &sk.name, fname]) {
                Ok(p) => p,
                Err(exc) => {
                    out.push(diag(
                        "error",
                        "unsafe_path",
                        format!("skill file {}: {exc}", text::repr(rel)),
                        Some(&sk.name),
                    ));
                    bad = true;
                    break;
                }
            };
            if data.len() > MAX_FILE_BYTES {
                out.push(diag(
                    "error",
                    "file_too_large",
                    format!("skill file {} is {} bytes", text::repr(rel), data.len()),
                    Some(&sk.name),
                ));
                bad = true;
                break;
            }
            let mut a = Artifact::new(root, &path, data.clone(), "skill");
            a.source_ids = vec![sk.name.clone()];
            staged.push(a);
        }
        if !bad {
            arts.extend(staged);
        }
    }
    Ok(arts)
}

/// `discover_skills(fs, cfg, root, base, notes)`: skills at `<root>/<base>/<name>/SKILL.md`.
pub fn discover_skills(
    fs: &dyn FileSystem,
    cfg: &ClientConfig,
    root: &str,
    base: &str,
    notes: &mut Vec<String>,
) -> AdapterResult<Vec<SkillItem>> {
    let mut out = Vec::new();
    let Some(top) = fs_path(cfg, root, base)? else {
        return Ok(out);
    };
    if !fs.exists(&top) || !fs.is_dir(&top) {
        return Ok(out);
    }
    for name in listdir_sorted(fs, &top)? {
        let d = pyos::join(&top, &name);
        if !fs.is_dir(&d) {
            continue;
        }
        if !valid_name(&name) {
            notes.push(format!("skipped skill dir {}: not a valid hub name", text::repr(&name)));
            continue;
        }
        let files = read_tree(fs, &d, MAX_DISCOVER_FILES, notes)?;
        let Some(skill_md) = files.get("SKILL.md") else {
            notes.push(format!("skipped skill dir {}: no SKILL.md", text::repr(&name)));
            continue;
        };
        let (fm, _) = parse_frontmatter(&decode_replace(skill_md))?;
        out.push(SkillItem {
            name,
            description: cap_str(fm.get("description"), MAX_DESC_ERR),
            files,
            ..SkillItem::default()
        });
    }
    Ok(out)
}

/// `instruction_artifact(items, root, path, params, diags)`: CLAUDE.md / AGENTS.md (`instructions_mode` block | own).
pub fn instruction_artifact(
    items: &[InstructionItem],
    root: &str,
    path: &str,
    params: &PyValue,
    diags: &mut Vec<Diag>,
) -> Option<Artifact> {
    let ordered = sorted_instructions(items);
    let ids: Vec<String> = ordered.iter().map(|i| i.id.clone()).collect();
    let mode = params
        .get("instructions_mode")
        .cloned()
        .unwrap_or_else(|| PyValue::str("block"));
    let own = match mode.as_str() {
        Some("own") => true,
        Some("block") => false,
        _ => {
            diags.push(diag(
                "error",
                "invalid_param",
                format!("instructions_mode must be own or block, got {}", mode.repr()),
                None,
            ));
            false
        }
    };
    if own {
        let s = if ordered.len() == 1 {
            ordered[0].body.clone()
        } else {
            let parts: Vec<String> = ordered
                .iter()
                .map(|i| format!("## {}\n\n{}", i.title, text::strip(&i.body)))
                .collect();
            format!("{}\n", parts.join("\n\n"))
        };
        let mut a = Artifact::new(root, path, to_bytes(&s), "instruction");
        a.source_ids = ids;
        return Some(a);
    }
    let Ok(block) = wrap_block(&render_instructions(items, true), "md") else {
        diags.push(diag(
            "error",
            "marker_injection",
            format!("{path}: {MarkerInjection}; instructions not rendered"),
            None,
        ));
        return None;
    };
    let mut a = Artifact::new(root, path, to_bytes(&block), "instruction");
    a.source_ids = ids;
    a.merge = "block".into();
    Some(a)
}

/// `sandbox_prefix(params)`: the argv prefix (params.sandbox_wrapper) for servers that declare a sandbox_profile.
pub fn sandbox_prefix(params: &PyValue) -> Vec<String> {
    match params.get("sandbox_wrapper") {
        Some(PyValue::List(l)) => l.iter().map(PyValue::py_str).collect(),
        _ => Vec::new(),
    }
}

pub fn sandbox_missing(name: &str, out: &mut Vec<Diag>) {
    out.push(diag(
        "error",
        "sandbox_wrapper_missing",
        "server declares a sandbox_profile but params.sandbox_wrapper is not set; refusing to render it unsandboxed",
        Some(name),
    ));
}

/// `sorted(set(x))` of strings.
pub fn sorted_set<I: IntoIterator<Item = String>>(it: I) -> Vec<String> {
    let mut v: Vec<String> = it.into_iter().collect::<HashSet<_>>().into_iter().collect();
    v.sort();
    v
}

/// A JSON object (`serde_json`) as a Python dict value (lossless for JSON-born data).
pub fn py(v: &Value) -> PyValue {
    PyValue::from_json(v)
}

// =========================================================================================================================
// Python value behaviours (untrusted client files)
// =========================================================================================================================

/// `bool(x)`.
pub fn truthy(v: &PyValue) -> bool {
    match v {
        PyValue::None => false,
        PyValue::Bool(b) => *b,
        PyValue::Int(i) => *i != 0,
        PyValue::BigInt(_) | PyValue::Date { .. } | PyValue::DateTime(_) => true,
        PyValue::Float(f) => *f != 0.0,
        PyValue::Str(s) => !s.is_empty(),
        PyValue::Bytes(b) => !b.is_empty(),
        PyValue::List(l) | PyValue::Set(l) => !l.is_empty(),
        PyValue::Dict(d) => !d.is_empty(),
    }
}

/// `for x in v` (a str iterates its characters, a dict its keys, bytes their ints); `TypeError` where Python raises.
pub fn py_iter(v: &PyValue) -> AdapterResult<Vec<PyValue>> {
    match v {
        PyValue::List(l) | PyValue::Set(l) => Ok(l.clone()),
        PyValue::Str(s) => Ok(s.chars().map(|c| PyValue::Str(c.to_string())).collect()),
        PyValue::Dict(d) => Ok(d.iter().map(|(k, _)| k.clone()).collect()),
        PyValue::Bytes(b) => Ok(b.iter().map(|x| PyValue::Int(i64::from(*x))).collect()),
        other => Err(AdapterError::new(
            "TypeError",
            format!("'{}' object is not iterable", other.type_name()),
        )),
    }
}

fn lt_error(a: &PyValue, b: &PyValue) -> AdapterError {
    AdapterError::new(
        "TypeError",
        format!(
            "'<' not supported between instances of '{}' and '{}'",
            a.type_name(),
            b.type_name()
        ),
    )
}

fn num(v: &PyValue) -> Option<f64> {
    #[allow(clippy::cast_precision_loss)]
    match v {
        PyValue::Bool(b) => Some(f64::from(u8::from(*b))),
        PyValue::Int(i) => Some(*i as f64),
        PyValue::BigInt(s) => s.parse::<f64>().ok(),
        PyValue::Float(f) => Some(*f),
        _ => None,
    }
}

/// `a < b` with Python's rules for the types YAML/JSON produce (str, numbers, bytes, dates, lists); `TypeError` for
/// the rest.
pub fn py_lt(a: &PyValue, b: &PyValue) -> AdapterResult<bool> {
    if let (PyValue::Int(i), PyValue::Int(j)) = (a, b) {
        return Ok(i < j);
    }
    if let (Some(x), Some(y)) = (num(a), num(b)) {
        return Ok(x < y);
    }
    match (a, b) {
        (PyValue::Str(x), PyValue::Str(y)) => Ok(x < y),
        (PyValue::Bytes(x), PyValue::Bytes(y)) => Ok(x < y),
        (PyValue::Date { .. }, PyValue::Date { .. }) => Ok(a.isoformat() < b.isoformat()),
        (PyValue::List(x), PyValue::List(y)) => {
            for (p, q) in x.iter().zip(y.iter()) {
                if p != q {
                    return py_lt(p, q);
                }
            }
            Ok(x.len() < y.len())
        }
        _ => Err(lt_error(a, b)),
    }
}

/// `sorted(items, key=...)` with Python's comparison, as a binary insertion sort (CPython's timsort for short runs: the
/// first comparison is `items[1] < items[0]`), so an incomparable pair raises the same `TypeError`.
pub fn py_sorted_by_key<T: Clone>(items: &[T], key: impl Fn(&T) -> &PyValue) -> AdapterResult<Vec<T>> {
    let mut out: Vec<T> = Vec::with_capacity(items.len());
    for it in items {
        let (mut lo, mut hi) = (0, out.len());
        while lo < hi {
            let mid = usize::midpoint(lo, hi);
            if py_lt(key(it), key(&out[mid]))? {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        out.insert(lo, it.clone());
    }
    Ok(out)
}

/// `sorted(d.items())` of a dict (keys are unique, so only keys are compared).
pub fn sorted_items(d: &[(PyValue, PyValue)]) -> AdapterResult<Vec<(PyValue, PyValue)>> {
    py_sorted_by_key(d, |(k, _)| k)
}

// ---- json.loads ----------------------------------------------------------------------------------------------------------
/// A string as Python's `json.loads` returns it: `text` with any lone surrogate escape replaced by U+FFFD, and a flag
/// telling that it held one (such a str cannot be encoded to UTF-8, which some callers test).
#[derive(Debug, Clone, PartialEq)]
pub struct JStr {
    pub text: String,
    pub lone_surrogate: bool,
}

/// `json.loads` output.
#[derive(Debug, Clone, PartialEq)]
pub enum PyJson {
    Null,
    Bool(bool),
    Int(i64),
    BigInt(String),
    Float(f64),
    Str(JStr),
    List(Vec<PyJson>),
    /// insertion order of first occurrence, last value wins (a Python dict)
    Dict(Vec<(JStr, PyJson)>),
}

impl PyJson {
    pub fn get(&self, key: &str) -> Option<&PyJson> {
        match self {
            PyJson::Dict(d) => d
                .iter()
                .find(|(k, _)| !k.lone_surrogate && k.text == key)
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// As a [`PyValue`] (lone surrogates become U+FFFD).
    pub fn to_py(&self) -> PyValue {
        match self {
            PyJson::Null => PyValue::None,
            PyJson::Bool(b) => PyValue::Bool(*b),
            PyJson::Int(i) => PyValue::Int(*i),
            PyJson::BigInt(s) => PyValue::BigInt(s.clone()),
            PyJson::Float(f) => PyValue::Float(*f),
            PyJson::Str(s) => PyValue::Str(s.text.clone()),
            PyJson::List(l) => PyValue::List(l.iter().map(PyJson::to_py).collect()),
            PyJson::Dict(d) => {
                let mut out: Vec<(PyValue, PyValue)> = Vec::with_capacity(d.len());
                for (k, v) in d {
                    let key = PyValue::Str(k.text.clone());
                    match out.iter_mut().find(|(ok, _)| *ok == key) {
                        Some(slot) => slot.1 = v.to_py(),
                        None => out.push((key, v.to_py())),
                    }
                }
                PyValue::Dict(out)
            }
        }
    }
}

impl Drop for PyJson {
    /// Iterative: a hostile document may nest 9998 levels, which a recursive drop would not survive on a 2 MiB stack.
    fn drop(&mut self) {
        let mut stack: Vec<PyJson> = Vec::new();
        match self {
            PyJson::List(items) => stack.append(items),
            PyJson::Dict(pairs) => stack.extend(pairs.drain(..).map(|(_, v)| v)),
            _ => return,
        }
        while let Some(mut x) = stack.pop() {
            match &mut x {
                PyJson::List(items) => stack.append(items),
                PyJson::Dict(pairs) => stack.extend(pairs.drain(..).map(|(_, v)| v)),
                _ => {}
            }
        }
    }
}

/// CPython 3.13's C JSON scanner nests at most this many containers (C recursion limit 10000).
pub const JSON_MAX_DEPTH: usize = 9998;
/// [`json_loads`] builds a [`PyValue`] tree (converted and dropped recursively), so it refuses deeper documents
/// (DEVIATION D-ADAPTER-JSON-DEPTH: Python accepts up to 9998 levels; client config files are never that deep).
pub const JSON_VALUE_MAX_DEPTH: usize = 1000;

struct JParser<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
    max_depth: usize,
}

enum Frame {
    List(Vec<PyJson>),
    Dict(Vec<(JStr, PyJson)>, Option<JStr>),
}

impl JParser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn hex4(&self, at: usize) -> Option<u32> {
        let h = self.src.get(at..at + 4)?;
        if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(h, 16).ok()
    }

    fn string(&mut self) -> Result<JStr, String> {
        // at the opening quote
        self.i += 1;
        let mut out = String::new();
        let mut lone = false;
        loop {
            let Some(c) = self.peek() else {
                return Err("Unterminated string".into());
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(JStr {
                        text: out,
                        lone_surrogate: lone,
                    });
                }
                b'\\' => {
                    let Some(e) = self.s.get(self.i + 1).copied() else {
                        return Err("Unterminated string".into());
                    };
                    self.i += 2;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let Some(u1) = self.hex4(self.i) else {
                                return Err("Invalid \\uXXXX escape".into());
                            };
                            self.i += 4;
                            let mut cp = u1;
                            if (0xd800..=0xdbff).contains(&u1)
                                && self.s.get(self.i) == Some(&b'\\')
                                && self.s.get(self.i + 1) == Some(&b'u')
                                && self.i + 6 < self.s.len()
                            {
                                let Some(u2) = self.hex4(self.i + 2) else {
                                    return Err("Invalid \\uXXXX escape".into());
                                };
                                if (0xdc00..=0xdfff).contains(&u2) {
                                    cp = 0x10000 + ((u1 - 0xd800) << 10) + (u2 - 0xdc00);
                                    self.i += 6;
                                }
                            }
                            if let Some(ch) = char::from_u32(cp) {
                                out.push(ch);
                            } else {
                                lone = true;
                                out.push('\u{fffd}');
                            }
                        }
                        _ => return Err("Invalid \\escape".into()),
                    }
                }
                c if c < 0x20 => return Err("Invalid control character".into()),
                _ => {
                    let ch = self.src[self.i..].chars().next().unwrap_or('\u{fffd}');
                    out.push(ch);
                    self.i += ch.len_utf8();
                }
            }
        }
    }

    fn number(&mut self) -> Result<PyJson, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    self.i += 1;
                }
            }
            _ => return Err("Expecting value".into()),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') && self.s.get(self.i + 1).is_some_and(u8::is_ascii_digit) {
            is_float = true;
            self.i += 1;
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.i += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let mut j = self.i + 1;
            if matches!(self.s.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            if self.s.get(j).is_some_and(u8::is_ascii_digit) {
                is_float = true;
                self.i = j;
                while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    self.i += 1;
                }
            }
        }
        let lit = &self.src[start..self.i];
        if is_float {
            return lit.parse::<f64>().map(PyJson::Float).map_err(|e| e.to_string());
        }
        Ok(match lit.parse::<i64>() {
            Ok(i) => PyJson::Int(i),
            Err(_) => PyJson::BigInt(lit.to_string()),
        })
    }

    /// A scalar, or `None` when the next token opens a container.
    fn scalar(&mut self) -> Result<Option<PyJson>, String> {
        let rest = &self.s[self.i..];
        let lit = |p: &[u8]| rest.starts_with(p);
        let v = match self.peek() {
            Some(b'"') => PyJson::Str(self.string()?),
            Some(b'[' | b'{') => return Ok(None),
            Some(b'n') if lit(b"null") => {
                self.i += 4;
                PyJson::Null
            }
            Some(b't') if lit(b"true") => {
                self.i += 4;
                PyJson::Bool(true)
            }
            Some(b'f') if lit(b"false") => {
                self.i += 5;
                PyJson::Bool(false)
            }
            Some(b'N') if lit(b"NaN") => {
                self.i += 3;
                PyJson::Float(f64::NAN)
            }
            Some(b'I') if lit(b"Infinity") => {
                self.i += 8;
                PyJson::Float(f64::INFINITY)
            }
            Some(b'-') if lit(b"-Infinity") => {
                self.i += 9;
                PyJson::Float(f64::NEG_INFINITY)
            }
            Some(b'-' | b'0'..=b'9') => self.number()?,
            _ => return Err("Expecting value".into()),
        };
        Ok(Some(v))
    }

    fn colon(&mut self) -> Result<(), String> {
        self.ws();
        if self.peek() != Some(b':') {
            return Err("Expecting ':' delimiter".into());
        }
        self.i += 1;
        Ok(())
    }

    /// Iterative (an explicit stack), so 9998 levels never touch the thread's stack limit.
    fn parse(&mut self) -> Result<PyJson, String> {
        let mut stack: Vec<Frame> = Vec::new();
        let mut value: PyJson;
        'outer: loop {
            // one value; an opening bracket pushes a frame and keeps going
            loop {
                self.ws();
                if let Some(v) = self.scalar()? {
                    value = v;
                    break;
                }
                if stack.len() >= self.max_depth {
                    return Err("maximum recursion depth exceeded".into());
                }
                let open = self.s[self.i];
                self.i += 1;
                self.ws();
                if open == b'[' {
                    if self.peek() == Some(b']') {
                        self.i += 1;
                        value = PyJson::List(Vec::new());
                        break;
                    }
                    stack.push(Frame::List(Vec::new()));
                } else {
                    match self.peek() {
                        Some(b'}') => {
                            self.i += 1;
                            value = PyJson::Dict(Vec::new());
                            break;
                        }
                        Some(b'"') => {
                            let k = self.string()?;
                            self.colon()?;
                            stack.push(Frame::Dict(Vec::new(), Some(k)));
                        }
                        _ => return Err("Expecting property name enclosed in double quotes".into()),
                    }
                }
            }
            // attach it to the enclosing containers, closing them as far as the input does
            loop {
                let Some(top) = stack.last_mut() else {
                    break 'outer;
                };
                let is_list = match top {
                    Frame::List(items) => {
                        items.push(value);
                        true
                    }
                    Frame::Dict(pairs, key) => {
                        let k = key.take().unwrap_or(JStr {
                            text: String::new(),
                            lone_surrogate: false,
                        });
                        match pairs.iter_mut().find(|(pk, _)| *pk == k) {
                            Some(slot) => slot.1 = value,
                            None => pairs.push((k, value)),
                        }
                        false
                    }
                };
                self.ws();
                match (self.peek(), is_list) {
                    (Some(b','), true) => {
                        self.i += 1;
                        continue 'outer;
                    }
                    (Some(b','), false) => {
                        self.i += 1;
                        self.ws();
                        if self.peek() != Some(b'"') {
                            return Err("Expecting property name enclosed in double quotes".into());
                        }
                        let k = self.string()?;
                        self.colon()?;
                        if let Some(Frame::Dict(_, key)) = stack.last_mut() {
                            *key = Some(k);
                        }
                        continue 'outer;
                    }
                    (Some(b']'), true) | (Some(b'}'), false) => {
                        self.i += 1;
                        value = match stack.pop() {
                            Some(Frame::List(items)) => PyJson::List(items),
                            Some(Frame::Dict(pairs, _)) => PyJson::Dict(pairs),
                            None => return Err("Expecting value".into()),
                        };
                    }
                    _ => return Err("Expecting ',' delimiter".into()),
                }
            }
        }
        self.ws();
        if self.i != self.s.len() {
            return Err("Extra data".into());
        }
        Ok(value)
    }
}

/// `json.loads(text)` (CPython 3.13 C scanner semantics); `Err` = `JSONDecodeError` or `RecursionError`.
pub fn json_loads_raw(s: &str) -> Result<PyJson, String> {
    JParser {
        s: s.as_bytes(),
        src: s,
        i: 0,
        max_depth: JSON_MAX_DEPTH,
    }
    .parse()
}

/// `json.loads(text)` as a [`PyValue`] (at most [`JSON_VALUE_MAX_DEPTH`] levels).
pub fn json_loads(s: &str) -> Result<PyValue, String> {
    JParser {
        s: s.as_bytes(),
        src: s,
        i: 0,
        max_depth: JSON_VALUE_MAX_DEPTH,
    }
    .parse()
    .map(|v| v.to_py())
}

// ---- canonical json.dumps ------------------------------------------------------------------------------------------------
/// `json.dumps(obj, sort_keys=True, separators=(",", ":"), default=str)` (ensure_ascii, allow_nan).
pub fn canon_dumps(v: &PyValue) -> AdapterResult<String> {
    let mut out = String::new();
    canon_write(&mut out, v)?;
    Ok(out)
}

fn canon_key(k: &PyValue) -> AdapterResult<String> {
    Ok(match k {
        PyValue::Str(s) | PyValue::BigInt(s) => s.clone(),
        PyValue::None => "null".into(),
        PyValue::Bool(b) => if *b { "true" } else { "false" }.into(),
        PyValue::Int(i) => i.to_string(),
        PyValue::Float(f) => float::json(*f),
        other => {
            return Err(AdapterError::new(
                "TypeError",
                format!("keys must be str, int, float, bool or None, not {}", other.type_name()),
            ));
        }
    })
}

fn canon_write(out: &mut String, v: &PyValue) -> AdapterResult<()> {
    match v {
        PyValue::None => out.push_str("null"),
        PyValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        PyValue::Int(i) => out.push_str(&i.to_string()),
        PyValue::BigInt(s) => out.push_str(s),
        PyValue::Float(f) => out.push_str(&float::json(*f)),
        PyValue::Str(s) => pyjson::write_str(out, s, true),
        PyValue::List(items) => {
            out.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canon_write(out, it)?;
            }
            out.push(']');
        }
        PyValue::Dict(d) => {
            let items = sorted_items(d)?;
            out.push('{');
            for (i, (k, val)) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                pyjson::write_str(out, &canon_key(k)?, true);
                out.push(':');
                canon_write(out, val)?;
            }
            out.push('}');
        }
        // default=str
        other => pyjson::write_str(out, &other.py_str(), true),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_loads_python_semantics() {
        assert_eq!(json_loads("NaN").map(|v| v.repr()), Ok("nan".into()));
        assert_eq!(
            json_loads("123456789012345678901234567890").map(|v| v.repr()),
            Ok("123456789012345678901234567890".into())
        );
        assert_eq!(
            json_loads(r#"{"a": 1, "b": 2, "a": 3}"#).map(|v| v.repr()),
            Ok("{'a': 3, 'b': 2}".into())
        );
        assert!(json_loads("[1,]").is_err());
        assert!(json_loads("01").is_err());
        assert!(json_loads("\"a\tb\"").is_err());
        assert!(json_loads(" {} x").is_err());
        assert!(json_loads("").is_err());
        assert!(json_loads("{1: 2}").is_err());
        let s = json_loads_raw(concat!("\"", "\\ud800x", "\\ud83d\\ude00", "\""));
        assert!(matches!(&s, Ok(PyJson::Str(j)) if j.lone_surrogate && j.text == "\u{fffd}x😀"));
        let nest = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
        assert!(json_loads_raw(&nest(JSON_MAX_DEPTH)).is_ok()); // and dropped without recursion
        assert!(json_loads_raw(&nest(JSON_MAX_DEPTH + 1)).is_err());
        assert!(json_loads(&nest(JSON_VALUE_MAX_DEPTH)).is_ok());
        assert!(json_loads(&nest(JSON_VALUE_MAX_DEPTH + 1)).is_err());
        assert_eq!(json_loads("1.0e2").map(|v| v.repr()), Ok("100.0".into()));
        assert_eq!(json_loads("-0").map(|v| v.repr()), Ok("0".into()));
        assert_eq!(
            json_loads(r#"[{"a": [1, {"b": {}}]}, [], "x"]"#).map(|v| v.repr()),
            Ok("[{'a': [1, {'b': {}}]}, [], 'x']".into())
        );
    }

    #[test]
    fn canon_dumps_matches_json_dumps() {
        let v = json_loads(r#"{"b": [1, 2.5, "é", null, true], "a": {"y": 1}}"#).unwrap_or_default();
        assert_eq!(
            canon_dumps(&v).ok(),
            Some(r#"{"a":{"y":1},"b":[1,2.5,"\u00e9",null,true]}"#.into())
        );
        let mixed = PyValue::Dict(vec![
            (PyValue::Int(1), PyValue::None),
            (PyValue::str("a"), PyValue::None),
        ]);
        assert_eq!(
            canon_dumps(&mixed).err().map(|e| e.to_string()),
            Some("TypeError: '<' not supported between instances of 'str' and 'int'".into())
        );
    }
}
