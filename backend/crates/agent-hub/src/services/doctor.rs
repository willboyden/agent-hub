//! Port of services/doctor.py: the read-only host-exposure report (GET /doctor, `hubctl doctor`). Never contains
//! secrets. Everything host-facing is injected (DoctorFs, env, home, uid) so tests run offline. A broken check reports
//! an `info` finding `check-<name>` (detail = the Python exception type name) instead of failing the report.
//! Owner: WS-F.
//!
//! Python-compatibility notes: settings files are parsed with a port of `json.loads` (NaN/Infinity literals, lone
//! surrogate escapes and big ints are accepted like Python; nesting deeper than 9998 containers is "malformed", the
//! RecursionError depth of CPython 3.13 at top level); paths are `pathlib.PurePosixPath` strings; the systemd
//! `ReadWritePaths` scan reproduces `re.finditer(r"^\s*ReadWritePaths\s*=\s*(.+)$", text, re.M)` exactly.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use hub_core::py::text::{is_space, split_ws, strip};
use hub_core::value::PyValue;
use serde_json::{Map, Value, json};

use crate::config::Settings;

pub const STATE_PATHS: [&str; 4] = [
    "~/.local/share/agent-hub",
    "~/.config/agent-hub",
    "~/.local/share/agent-knowledge",
    "~/.config/agent-knowledge",
];
pub const POLICY_GLOB: &str = "**/hub/policy/**";
/// Client params the claude-code checks read; an absent one skips its check with an `info: not configured` finding.
pub const CLAUDE_PARAMS: [&str; 3] = ["settings_template", "settings_rendered", "network_allowlist_key"];
pub const DOCS_POINTER: &str = "docs/SECURITY.md";
pub const MAX_READ: usize = 1_000_000;
pub const LOOPBACK_DOMAINS: [&str; 6] = [
    "localhost",
    "127.0.0.1",
    "::1",
    "[::1]",
    "0.0.0.0",
    "host.docker.internal",
];
/// `json.loads` raises RecursionError beyond this many nested containers (CPython 3.13, measured at top level).
pub const JSON_MAX_DEPTH: usize = 9998;

/// stat result the checks use (`st_mode` with the file type bits, `st_uid`, `st_size`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoctorStat {
    pub mode: u32,
    pub uid: u32,
    pub size: u64,
}

/// Host reads behind a port (RealDoctorFs for real).
pub trait DoctorFs: Send + Sync {
    fn read_text(&self, path: &Path) -> Option<String>;
    fn stat(&self, path: &Path) -> Option<DoctorStat>;
    fn glob(&self, directory: &Path, pattern: &str) -> Vec<PathBuf>;
}

/// The real filesystem: `read_text` refuses files over MAX_READ, decodes UTF-8 with replacement and translates
/// newlines like Python's text mode; `stat` is an lstat; `glob` matches one directory's entry names.
pub struct RealDoctorFs;

impl DoctorFs for RealDoctorFs {
    fn read_text(&self, path: &Path) -> Option<String> {
        // follows links (client configs are often symlinked dotfiles); a FIFO or device is never opened blocking
        let bytes = super::fs::read_regular(path, true, MAX_READ as u64).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        // universal newlines: "\r\n" and "\r" become "\n"
        Some(text.replace("\r\n", "\n").replace('\r', "\n"))
    }
    fn stat(&self, path: &Path) -> Option<DoctorStat> {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path).ok().map(|m| DoctorStat {
            mode: m.mode(),
            uid: m.uid(),
            size: m.size(),
        })
    }
    fn glob(&self, directory: &Path, pattern: &str) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(directory) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = rd
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| hub_core::py::fnmatchcase(n, pattern))
            .map(|n| directory.join(n))
            .collect();
        out.sort();
        out
    }
}

pub struct DoctorContext<'a> {
    pub cfg: &'a Settings,
    pub fs: &'a dyn DoctorFs,
    pub env: &'a std::collections::HashMap<String, String>,
    pub home: PathBuf,
    pub app_root: PathBuf,
    pub uid: u32,
    /// `[{id, adapter, roots, params, manage}]`
    pub clients: Vec<Value>,
    /// the /knowledge/health summary, if it was reachable
    pub knowledge: Option<Value>,
    /// client id -> missing floor denies (None = unknown)
    pub permission_gaps: &'a dyn Fn(&str) -> Option<Vec<String>>,
    pub floor_read: Vec<String>,
    pub floor_write: Vec<String>,
    /// SkillEvalService.doctor_info(); None = not checked
    pub skill_evaluator: Option<Value>,
}

/// `finding(fid, sev, title, detail="", fix="")`.
pub fn finding(fid: &str, sev: &str, title: &str, detail: &str, fix: &str) -> Value {
    json!({"id": fid, "severity": sev, "title": title, "detail": detail, "fix": fix})
}

/// The Python exception type name a check raised (`type(exc).__name__`).
pub type Exc = &'static str;
/// What one check returns.
pub type Found = Result<Vec<Value>, Exc>;
/// One check, by its Python name.
pub type Check = (&'static str, fn(&DoctorContext<'_>) -> Found);

// ---- Python object model over JSON ---------------------------------------------------------------------------------
fn py_str(v: &Value) -> String {
    PyValue::from_json(v).py_str()
}

/// Python truthiness of a JSON value.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `d.get(k)` on a value that must be a dict (`AttributeError` otherwise).
fn get<'a>(d: &'a Value, k: &str) -> Result<Option<&'a Value>, Exc> {
    match d {
        Value::Object(m) => Ok(m.get(k)),
        _ => Err("AttributeError"),
    }
}

/// `d.get(k, {})` used as a dict: missing = empty, present but not a dict = `AttributeError` on use.
fn sub_dict<'a>(d: &'a Value, k: &str) -> Result<Option<&'a Map<String, Value>>, Exc> {
    match get(d, k)? {
        None => Ok(None),
        Some(Value::Object(m)) => Ok(Some(m)),
        Some(_) => Err("AttributeError"),
    }
}

/// `d[k]` (`KeyError` when missing).
fn item<'a>(d: &'a Value, k: &str) -> Result<&'a Value, Exc> {
    get(d, k)?.ok_or("KeyError")
}

/// `cl.get("params", {}).get(key)`.
fn param<'a>(cl: &'a Value, key: &str) -> Result<Option<&'a Value>, Exc> {
    Ok(sub_dict(cl, "params")?.and_then(|m| m.get(key)))
}

/// `isinstance(v, str) and v.strip()` -> the str.
fn nonblank(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !strip(s).is_empty())
}

// ---- pathlib.PurePosixPath -------------------------------------------------------------------------------------------
/// A `PurePosixPath`: root (`""`, `"/"` or `"//"`) and its parts (no empty or `.` parts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurePath {
    root: String,
    parts: Vec<String>,
}

impl PurePath {
    pub fn new(s: &str) -> Self {
        let root = if s.starts_with("//") && !s.starts_with("///") {
            "//"
        } else if s.starts_with('/') {
            "/"
        } else {
            ""
        };
        let parts = s
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .map(str::to_string)
            .collect();
        Self {
            root: root.into(),
            parts,
        }
    }
    pub fn of(p: &Path) -> Self {
        Self::new(&p.to_string_lossy())
    }
    pub fn is_absolute(&self) -> bool {
        !self.root.is_empty()
    }
    /// `self / other`.
    pub fn join(&self, other: &str) -> Self {
        let o = Self::new(other);
        if o.is_absolute() {
            return o;
        }
        let mut parts = self.parts.clone();
        parts.extend(o.parts);
        Self {
            root: self.root.clone(),
            parts,
        }
    }
    pub fn parent(&self) -> Self {
        let mut parts = self.parts.clone();
        parts.pop();
        Self {
            root: self.root.clone(),
            parts,
        }
    }
    pub fn name(&self) -> &str {
        self.parts.last().map_or("", String::as_str)
    }
    /// `with_name(name)`: `ValueError` for a path without a name.
    pub fn with_name(&self, name: &str) -> Result<Self, Exc> {
        if self.parts.is_empty() {
            return Err("ValueError");
        }
        let mut p = self.parent();
        p.parts.push(name.into());
        Ok(p)
    }
    /// `self in other.parents`.
    pub fn is_parent_of(&self, other: &Self) -> bool {
        self.root == other.root && self.parts.len() < other.parts.len() && other.parts.starts_with(&self.parts)
    }
    pub fn path(&self) -> PathBuf {
        PathBuf::from(self.to_string())
    }
}

impl std::fmt::Display for PurePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = format!("{}{}", self.root, self.parts.join("/"));
        f.write_str(if s.is_empty() { "." } else { &s })
    }
}

/// `_expand(p, home)`: `home / p[2:]` for `~/...`, else `Path(p)`.
fn expand(p: &str, home: &PurePath) -> PurePath {
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PurePath::new(p),
    }
}

// ---- json.loads ------------------------------------------------------------------------------------------------------
enum Frame {
    Arr(Vec<PyValue>),
    Obj(Vec<(PyValue, PyValue)>, String),
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

fn hex4(b: &[u8], i: usize) -> Option<u32> {
    let s = b.get(i..i + 4)?;
    if !s.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u32::from_str_radix(std::str::from_utf8(s).ok()?, 16).ok()
}

/// A JSON string body starting after the opening quote: (text, index after the closing quote). Strict like Python:
/// control characters are refused; a lone surrogate escape becomes U+FFFD (Rust strings cannot hold it).
fn parse_string(s: &str, start: usize) -> Option<(String, usize)> {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = start;
    loop {
        let run_end = (i..b.len()).find(|&j| b[j] == b'"' || b[j] == b'\\' || b[j] < 0x20)?;
        out.push_str(s.get(i..run_end)?);
        i = run_end;
        match b[i] {
            b'"' => return Some((out, i + 1)),
            b'\\' => {
                let esc = *b.get(i + 1)?;
                i += 2;
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
                        let mut c = hex4(b, i)?;
                        i += 4;
                        if (0xD800..0xDC00).contains(&c) && i + 6 < b.len() && b[i] == b'\\' && b[i + 1] == b'u' {
                            let c2 = hex4(b, i + 2)?;
                            if (0xDC00..0xE000).contains(&c2) {
                                c = 0x10000 + ((c - 0xD800) << 10) + (c2 - 0xDC00);
                                i += 6;
                            }
                        }
                        out.push(char::from_u32(c).unwrap_or('\u{FFFD}'));
                    }
                    _ => return None,
                }
            }
            _ => return None, // a raw control character
        }
    }
}

/// A JSON number at `i` (Python's NUMBER_RE): an int when it has no fraction or exponent (refused over 4300 digits,
/// Python's int-from-str limit), else a float.
fn parse_number(s: &str, i: usize) -> Option<(PyValue, usize)> {
    let b = s.as_bytes();
    let mut j = i;
    if b.get(j) == Some(&b'-') {
        j += 1;
    }
    match b.get(j) {
        Some(b'0') => j += 1,
        Some(b'1'..=b'9') => {
            while b.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
        }
        _ => return None,
    }
    let int_end = j;
    let mut is_float = false;
    if b.get(j) == Some(&b'.') && b.get(j + 1).is_some_and(u8::is_ascii_digit) {
        j += 1;
        while b.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        is_float = true;
    }
    if matches!(b.get(j), Some(b'e' | b'E')) {
        let mut k = j + 1;
        if matches!(b.get(k), Some(b'+' | b'-')) {
            k += 1;
        }
        if b.get(k).is_some_and(u8::is_ascii_digit) {
            while b.get(k).is_some_and(u8::is_ascii_digit) {
                k += 1;
            }
            j = k;
            is_float = true;
        }
    }
    let text = &s[i..j];
    if is_float {
        return Some((PyValue::Float(text.parse::<f64>().ok()?), j));
    }
    let digits = &s[i..int_end];
    if digits.trim_start_matches('-').len() > 4300 {
        return None;
    }
    Some(match digits.parse::<i64>() {
        Ok(v) => (PyValue::Int(v), j),
        Err(_) => (PyValue::BigInt(digits.to_string()), j),
    })
}

/// `json.loads(text)`; None where Python raises ValueError or RecursionError.
pub fn json_loads(s: &str) -> Option<PyValue> {
    if s.starts_with('\u{feff}') {
        return None;
    }
    let b = s.as_bytes();
    let mut i = skip_ws(b, 0);
    let mut stack: Vec<Frame> = Vec::new();
    let result: PyValue;
    'value: loop {
        // one value at i (whitespace already skipped)
        let mut val: PyValue = match b.get(i) {
            Some(b'[' | b'{') if stack.len() >= JSON_MAX_DEPTH => return None,
            Some(b'[') => {
                i = skip_ws(b, i + 1);
                if b.get(i) == Some(&b']') {
                    i += 1;
                    PyValue::List(Vec::new())
                } else {
                    stack.push(Frame::Arr(Vec::new()));
                    continue 'value;
                }
            }
            Some(b'{') => {
                i = skip_ws(b, i + 1);
                match b.get(i) {
                    Some(b'}') => {
                        i += 1;
                        PyValue::Dict(Vec::new())
                    }
                    Some(b'"') => {
                        let (k, ni) = parse_string(s, i + 1)?;
                        i = skip_ws(b, ni);
                        if b.get(i) != Some(&b':') {
                            return None;
                        }
                        i = skip_ws(b, i + 1);
                        stack.push(Frame::Obj(Vec::new(), k));
                        continue 'value;
                    }
                    _ => return None,
                }
            }
            Some(b'"') => {
                let (st, ni) = parse_string(s, i + 1)?;
                i = ni;
                PyValue::Str(st)
            }
            Some(b'n') if b[i..].starts_with(b"null") => {
                i += 4;
                PyValue::None
            }
            Some(b't') if b[i..].starts_with(b"true") => {
                i += 4;
                PyValue::Bool(true)
            }
            Some(b'f') if b[i..].starts_with(b"false") => {
                i += 5;
                PyValue::Bool(false)
            }
            Some(b'N') if b[i..].starts_with(b"NaN") => {
                i += 3;
                PyValue::Float(f64::NAN)
            }
            Some(b'I') if b[i..].starts_with(b"Infinity") => {
                i += 8;
                PyValue::Float(f64::INFINITY)
            }
            Some(b'-') if b[i..].starts_with(b"-Infinity") => {
                i += 9;
                PyValue::Float(f64::NEG_INFINITY)
            }
            Some(b'-' | b'0'..=b'9') => {
                let (v, ni) = parse_number(s, i)?;
                i = ni;
                v
            }
            _ => return None,
        };
        // hand the finished value to its container (closing containers as they complete)
        loop {
            match stack.last_mut() {
                None => {
                    result = val;
                    break 'value;
                }
                Some(Frame::Arr(items)) => {
                    items.push(val);
                    i = skip_ws(b, i);
                    match b.get(i) {
                        Some(b',') => {
                            i = skip_ws(b, i + 1);
                            continue 'value;
                        }
                        Some(b']') => {
                            i += 1;
                            let Some(Frame::Arr(items)) = stack.pop() else {
                                return None;
                            };
                            val = PyValue::List(items);
                        }
                        _ => return None,
                    }
                }
                Some(Frame::Obj(pairs, key)) => {
                    let k = PyValue::Str(std::mem::take(key));
                    match pairs.iter_mut().find(|(pk, _)| *pk == k) {
                        Some(slot) => slot.1 = val,
                        None => pairs.push((k, val)),
                    }
                    i = skip_ws(b, i);
                    match b.get(i) {
                        Some(b',') => {
                            i = skip_ws(b, i + 1);
                            if b.get(i) != Some(&b'"') {
                                return None;
                            }
                            let (k, ni) = parse_string(s, i + 1)?;
                            i = skip_ws(b, ni);
                            if b.get(i) != Some(&b':') {
                                return None;
                            }
                            i = skip_ws(b, i + 1);
                            *key = k;
                            continue 'value;
                        }
                        Some(b'}') => {
                            i += 1;
                            let Some(Frame::Obj(pairs, _)) = stack.pop() else {
                                return None;
                            };
                            val = PyValue::Dict(pairs);
                        }
                        _ => return None,
                    }
                }
            }
        }
    }
    (skip_ws(b, i) == b.len()).then_some(result)
}

/// `_load_json(text)`: None for no text, the string `"__malformed__"` where json.loads raises.
fn load_json(text: Option<&str>) -> PyValue {
    match text {
        None => PyValue::None,
        Some(t) => json_loads(t).unwrap_or_else(|| PyValue::str("__malformed__")),
    }
}

/// `doc.get(k) if isinstance(doc, dict) else None`.
fn pget<'a>(doc: Option<&'a PyValue>, k: &str) -> Option<&'a PyValue> {
    match doc {
        Some(d @ PyValue::Dict(_)) => d.get(k),
        _ => None,
    }
}

// ---- str helpers -----------------------------------------------------------------------------------------------------
/// `str.capitalize()`: the first character in titlecase, the rest lowercased (CPython 3.13 / Unicode 15.1 titlecase
/// exceptions in TITLE_NOT_UPPER; lowercase via Rust's tables).
pub fn capitalize(s: &str) -> String {
    let Some(first) = s.chars().next() else {
        return String::new();
    };
    let mut out = match TITLE_NOT_UPPER.binary_search_by(|(c, _)| c.cmp(&first)) {
        Ok(i) => TITLE_NOT_UPPER[i].1.to_string(),
        Err(_) => first.to_uppercase().collect(),
    };
    // lowercase the WHOLE string so the final-sigma rule sees the first character as context, then drop the first
    // character's own lowercase (always its context-free mapping: nothing precedes it)
    let lowered = s.to_lowercase();
    let skip = first.to_lowercase().map(char::len_utf8).sum::<usize>();
    out.push_str(lowered.get(skip..).unwrap_or_default());
    out
}

/// `str.isdigit()`.
fn is_digit_str(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| {
            hub_core::py::text::decimal(c).is_some()
                || DIGIT_NOT_DECIMAL.iter().any(|(a, b)| (*a..=*b).contains(&u32::from(c)))
        })
}

fn join<S: AsRef<str>>(items: &[S], sep: &str) -> String {
    items.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(sep)
}

fn take<S>(items: &[S], n: usize) -> &[S] {
    &items[..items.len().min(n)]
}

// ---- individual checks -------------------------------------------------------------------------------------------
fn data_dir(c: &DoctorContext<'_>) -> PurePath {
    PurePath::of(&c.cfg.data_dir)
}

pub fn check_bootstrap_key(c: &DoctorContext<'_>) -> Found {
    let f = data_dir(c).join("bootstrap-admin.key");
    if c.fs.stat(&f.path()).is_none() {
        return Ok(vec![finding(
            "bootstrap-key",
            "ok",
            "No bootstrap admin key file at rest",
            "",
            "",
        )]);
    }
    Ok(vec![finding(
        "bootstrap-key",
        "warn",
        "The bootstrap admin key file still exists",
        &format!("{f} is readable by every process of your OS user (including unsandboxed agents)."),
        "Store the key in a password manager, then run: hubctl retire-bootstrap-key",
    )])
}

pub fn check_modes(c: &DoctorContext<'_>) -> Found {
    let d = data_dir(c);
    let mut dirs = vec![d.clone()];
    dirs.extend(["plans", "backups", "snapshots", "locks", "applies", "content"].map(|n| d.join(n)));
    dirs.push(PurePath::of(&c.cfg.content_dir));
    let files = [
        d.join("hub.db"),
        d.join("attest.key"),
        d.join("bootstrap-admin.key"),
        PurePath::of(&c.cfg.knowledge_admin_key_file),
    ];
    let mut bad: Vec<String> = Vec::new();
    let mut fixes: Vec<String> = Vec::new();
    let wanted = dirs.iter().map(|p| (p, 0o700)).chain(files.iter().map(|p| (p, 0o600)));
    for (p, want) in wanted {
        let Some(st) = c.fs.stat(&p.path()) else { continue };
        let mode = st.mode & 0o7777;
        if mode & 0o077 != 0 {
            bad.push(format!("{p} is 0o{mode:o} (expected 0o{want:o})"));
            fixes.push(format!("chmod {want:o} {p}"));
        }
    }
    if bad.is_empty() {
        return Ok(vec![finding(
            "modes",
            "ok",
            "State directories and key files are private (0700 / 0600)",
            "",
            "",
        )]);
    }
    Ok(vec![finding(
        "modes",
        "warn",
        "Some hub state is readable by other users",
        &join(take(&bad, 8), "; "),
        &join(take(&fixes, 8), "; "),
    )])
}

pub fn check_content_dir(c: &DoctorContext<'_>) -> Found {
    let home = PurePath::of(&c.home);
    let mut roots: Vec<String> = Vec::new();
    for cl in &c.clients {
        if let Some(m) = sub_dict(cl, "roots")? {
            for r in m.values() {
                let r = r.as_str().ok_or("AttributeError")?;
                roots.push(expand(r, &home).to_string());
            }
        }
    }
    let mut problems: Vec<String> = Vec::new();
    if let Err(p) = super::contentguard::check_location(&c.cfg.content_dir, &roots, &c.app_root) {
        problems.push(
            p.detail
                .split(". Keep the content repo")
                .next()
                .unwrap_or("")
                .to_string(),
        );
    }
    if let Some(st) = c.fs.stat(&c.cfg.content_dir)
        && st.uid != c.uid
    {
        problems.push(format!(
            "{} is not owned by the current user",
            PurePath::of(&c.cfg.content_dir)
        ));
    }
    if problems.is_empty() {
        return Ok(vec![finding(
            "content-dir",
            "ok",
            "Content repo is outside every client root and the app tree, and owned by you",
            "",
            "",
        )]);
    }
    Ok(vec![finding(
        "content-dir",
        "crit",
        "The content repository is not in a safe place",
        &problems.join("; "),
        "hubctl migrate-content --to ~/.local/share/agent-hub/content --yes",
    )])
}

pub fn check_network_exposure(c: &DoctorContext<'_>) -> Found {
    let mut out = Vec::new();
    if c.cfg.trust_loopback {
        out.push(finding(
            "trust-loopback",
            "crit",
            "HUB_TRUST_LOOPBACK is ON",
            "Any local process, including an agent with a shell, is admin without a key.",
            "Unset HUB_TRUST_LOOPBACK (or set it to false) and restart the hub.",
        ));
    } else {
        out.push(finding(
            "trust-loopback",
            "ok",
            "Every request needs a bearer key (loopback trust is off)",
            "",
            "",
        ));
    }
    let host = &c.cfg.host;
    if !crate::config::is_loopback_host(host) {
        out.push(finding(
            "bind",
            "crit",
            &format!("The hub is bound to {host}, not loopback"),
            "The API is reachable from other machines.",
            "Set HUB_HOST=127.0.0.1.",
        ));
    } else {
        out.push(finding("bind", "ok", "The hub listens on loopback only", "", ""));
    }
    Ok(out)
}

/// The exact lines missing from a Claude Code settings document. Tolerates any malformed shape.
pub fn claude_settings_gaps(doc: &PyValue) -> Vec<String> {
    let mut lines = Vec::new();
    let sandbox = pget(Some(doc), "sandbox");
    let creds = pget(sandbox, "credentials");
    let files = pget(creds, "files");
    let have: BTreeSet<String> = match files {
        Some(PyValue::List(l)) => l
            .iter()
            .filter(|e| matches!(e, PyValue::Dict(_)))
            .map(|e| e.get("path").map_or_else(|| "None".to_string(), PyValue::py_str))
            .collect(),
        _ => BTreeSet::new(),
    };
    let perms = pget(Some(doc), "permissions");
    let denies: Vec<&str> = match pget(perms, "deny") {
        Some(PyValue::List(l)) => l.iter().filter_map(PyValue::as_str).collect(),
        _ => Vec::new(),
    };
    for p in STATE_PATHS {
        if !have.contains(p) {
            lines.push(format!(
                "sandbox.credentials.files += {{\"path\": \"{p}\", \"mode\": \"deny\"}}"
            ));
        }
        for tool in ["Read", "Write", "Edit"] {
            let pre = format!("{tool}(");
            if !denies.iter().any(|d| d.starts_with(&pre) && d.contains(p)) {
                lines.push(format!("permissions.deny += \"{tool}({p}/**)\""));
            }
        }
    }
    for tool in ["Write", "Edit"] {
        let pre = format!("{tool}(");
        if !denies.iter().any(|d| d.starts_with(&pre) && d.contains("hub/policy")) {
            lines.push(format!("permissions.deny += \"{tool}({POLICY_GLOB})\""));
        }
    }
    lines
}

/// `_primary_root(c, cl)`: roots["project"] or the first root.
fn primary_root(c: &DoctorContext<'_>, cl: &Value) -> Result<Option<PurePath>, Exc> {
    let Some(roots) = sub_dict(cl, "roots")? else {
        return Ok(None);
    };
    let r = match roots.get("project").filter(|v| truthy(Some(v))) {
        Some(v) => Some(v),
        None => roots.values().next(),
    };
    Ok(r.and_then(Value::as_str).map(|r| expand(r, &PurePath::of(&c.home))))
}

/// `_param_path(c, cl, key, default_rel)`: a configured path param (absolute, ~, or relative to the client's primary
/// root); the default only where one is documented.
fn param_path(
    c: &DoctorContext<'_>,
    cl: &Value,
    key: &str,
    default_rel: Option<&str>,
) -> Result<Option<PurePath>, Exc> {
    let v = param(cl, key)?;
    let root = primary_root(c, cl)?;
    if let Some(v) = nonblank(v) {
        let p = expand(strip(v), &PurePath::of(&c.home));
        return Ok(if p.is_absolute() {
            Some(p)
        } else {
            root.map(|r| r.join(&p.to_string()))
        });
    }
    Ok(match (default_rel, root) {
        (Some(d), Some(r)) if !d.is_empty() => Some(r.join(d)),
        _ => None,
    })
}

/// `_dotted(doc, key)`.
fn dotted<'a>(doc: &'a PyValue, key: &str) -> Option<&'a PyValue> {
    let mut cur = doc;
    for part in key.split('.') {
        match cur {
            PyValue::Dict(_) => cur = cur.get(part)?,
            _ => return None,
        }
    }
    Some(cur)
}

/// `rerender_hint(cl)`: the client's own hint, else the generic advice.
pub fn rerender_hint(cl: &Value) -> Result<String, Exc> {
    Ok(match nonblank(param(cl, "rerender_hint")?) {
        Some(h) => h.to_string(),
        None => "then re-render the live config from the template with your own sync process".into(),
    })
}

fn client_id(cl: &Value) -> Result<String, Exc> {
    item(cl, "id").map(py_str)
}

fn adapter_is(cl: &Value, id: &str) -> Result<bool, Exc> {
    Ok(get(cl, "adapter")?.and_then(Value::as_str) == Some(id))
}

pub fn check_params_missing(c: &DoctorContext<'_>) -> Found {
    let mut miss = Vec::new();
    for cl in &c.clients {
        if adapter_is(cl, "claude-code")? {
            let mut absent = Vec::new();
            for k in CLAUDE_PARAMS {
                if nonblank(param(cl, k)?).is_none() {
                    absent.push(k);
                }
            }
            if !absent.is_empty() {
                miss.push(format!("{}: {}", client_id(cl)?, absent.join(", ")));
            }
        }
    }
    if miss.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![finding(
        "doctor-params",
        "info",
        "Some doctor checks are not configured",
        &miss.join("; "),
        "Add these keys under `params:` in the client's yaml (see docs/SECURITY.md). Until then the matching checks are skipped rather than guessed.",
    )])
}

pub fn check_claude_code(c: &DoctorContext<'_>) -> Found {
    let mut out = Vec::new();
    for cl in &c.clients {
        if !adapter_is(cl, "claude-code")? {
            continue;
        }
        let cid = client_id(cl)?;
        let mut doc = PyValue::None;
        match param_path(c, cl, "settings_template", None)? {
            None => out.push(finding(
                "claude-template",
                "info",
                &format!("{cid}: settings_template is not configured"),
                "No template path is set, so its deny lists were not checked.",
                "Set the client param settings_template to the file your sync process renders the live config from.",
            )),
            Some(tpl) => {
                let txt = c.fs.read_text(&tpl.path());
                doc = load_json(txt.as_deref());
                if txt.is_none() {
                    out.push(finding(
                        "claude-template",
                        "info",
                        &format!("{cid}: the configured settings template is not readable"),
                        &tpl.to_string(),
                        "",
                    ));
                } else if !matches!(doc, PyValue::Dict(_)) {
                    out.push(finding(
                        "claude-template",
                        "warn",
                        &format!("{cid}: the settings template is not valid JSON"),
                        &tpl.to_string(),
                        "Fix the JSON syntax.",
                    ));
                    doc = PyValue::None;
                } else {
                    let gaps = claude_settings_gaps(&doc);
                    if gaps.is_empty() {
                        out.push(finding(
                            "claude-template",
                            "ok",
                            &format!("{cid}: template denies the hub/knowledge state paths and hub/policy"),
                            "",
                            "",
                        ));
                    } else {
                        out.push(finding(
                            "claude-template",
                            "warn",
                            &format!("{cid}: the settings template is missing hub deny entries"),
                            &format!("{} missing in {tpl}", gaps.len()),
                            &format!("Add to {tpl}:\n{}\n{}", join(take(&gaps, 14), "\n"), rerender_hint(cl)?),
                        ));
                    }
                }
            }
        }
        match nonblank(param(cl, "network_allowlist_key")?) {
            None => out.push(finding(
                "claude-network",
                "info",
                &format!("{cid}: network_allowlist_key is not configured"),
                "The sandbox's allowed domains were not checked.",
                "Set the client param network_allowlist_key to the dotted key of the allowed-domains list in the template.",
            )),
            Some(key) if matches!(doc, PyValue::Dict(_)) => {
                let key = strip(key);
                if let Some(PyValue::List(domains)) = dotted(&doc, key) {
                    let mut lo: Vec<String> = domains
                        .iter()
                        .map(PyValue::py_str)
                        .filter(|d| LOOPBACK_DOMAINS.contains(&d.to_lowercase().as_str()))
                        .collect();
                    lo.sort();
                    if !lo.is_empty() {
                        out.push(finding(
                            "claude-network",
                            "warn",
                            &format!("{cid}: the sandbox may reach loopback ({})", lo.join(", ")),
                            "The hub API (and every other loopback service) is then reachable from sandboxed bash.",
                            &format!("Remove the loopback entries from {key}."),
                        ));
                    } else if domains.is_empty() {
                        out.push(finding(
                            "claude-network",
                            "info",
                            &format!("{cid}: sandboxed bash has no network"),
                            "It cannot reach the hub API until a human approves a host.",
                            "",
                        ));
                    }
                }
            }
            Some(_) => {}
        }
        match param_path(c, cl, "settings_rendered", None)? {
            None => out.push(finding(
                "claude-rendered",
                "info",
                &format!("{cid}: settings_rendered is not configured"),
                "The rendered live config was not compared with the template.",
                "Set the client param settings_rendered to the live config file the client actually reads.",
            )),
            Some(live_path) => {
                let live = load_json(c.fs.read_text(&live_path.path()).as_deref());
                if matches!(live, PyValue::Dict(_)) {
                    let lgaps = claude_settings_gaps(&live);
                    if lgaps.is_empty() {
                        out.push(finding(
                            "claude-rendered",
                            "ok",
                            &format!("{cid}: the rendered live config has the deny entries"),
                            "",
                            "",
                        ));
                    } else {
                        out.push(finding(
                            "claude-rendered",
                            "info",
                            &format!("{cid}: the rendered live config lacks the current deny entries"),
                            &format!("{} entries are not in {live_path} yet.", lgaps.len()),
                            &format!("{}.", capitalize(&rerender_hint(cl)?)),
                        ));
                    }
                }
            }
        }
    }
    Ok(out)
}

pub fn check_opencode(c: &DoctorContext<'_>) -> Found {
    let mut any = false;
    for cl in &c.clients {
        any |= adapter_is(cl, "opencode")?;
    }
    if !any {
        return Ok(Vec::new());
    }
    Ok(vec![finding(
        "opencode-host",
        "info",
        "opencode runs as an unsandboxed process of your OS user",
        "It can read everything the hub protects (key files, state, content, policy). The hub cannot constrain it.",
        &format!("Run it as a separate OS user; see {DOCS_POINTER} (host-level, needs your approval)."),
    )])
}

pub fn check_ptrace(c: &DoctorContext<'_>) -> Found {
    let raw = c.fs.read_text(Path::new("/proc/sys/kernel/yama/ptrace_scope"));
    let v = strip(raw.as_deref().unwrap_or("")).to_string();
    if !is_digit_str(&v) {
        return Ok(vec![finding(
            "ptrace",
            "info",
            "ptrace_scope is unknown",
            "Yama is not exposed on this host.",
            "",
        )]);
    }
    // int(v): a digit that is not a decimal (e.g. "²") raises ValueError in Python
    let n = hub_core::py::text::int_str(&v).ok_or("ValueError")?;
    if n == 0 {
        return Ok(vec![finding(
            "ptrace",
            "info",
            "kernel.yama.ptrace_scope = 0",
            "A same-user process can attach to and read the memory of your other processes (including the hub).",
            "sudo sysctl kernel.yama.ptrace_scope=1 (host-level; needs your approval)",
        )]);
    }
    Ok(vec![finding(
        "ptrace",
        "info",
        &format!("kernel.yama.ptrace_scope = {v}"),
        "Same-user ptrace is restricted to descendants.",
        "",
    )])
}

/// The capture group of every match of `re.finditer(r"^\s*ReadWritePaths\s*=\s*(.+)$", text, re.M)` (`\s` =
/// `str.isspace`, `.` = anything but "\n", `^`/`$` at "\n" boundaries), with Python's backtracking reproduced.
pub fn read_write_paths_values(text: &str) -> Vec<String> {
    const LIT: &str = "ReadWritePaths";
    let ch: Vec<char> = text.chars().collect();
    let n = ch.len();
    // ws_end[i] = end of the whitespace run starting at i
    let mut ws_end = vec![n; n + 1];
    for i in (0..n).rev() {
        ws_end[i] = if is_space(ch[i]) { ws_end[i + 1] } else { i };
    }
    let lit: Vec<char> = LIT.chars().collect();
    let line_end = |k: usize| (k..n).find(|&j| ch[j] == '\n').unwrap_or(n);
    let mut out = Vec::new();
    let mut pos = 0;
    while pos <= n {
        let mut found: Option<(usize, usize, usize)> = None; // (group start, group end, match end)
        for s in pos..=n {
            if s != 0 && ch[s - 1] != '\n' {
                continue;
            }
            let i = ws_end[s];
            if i + lit.len() > n || ch[i..i + lit.len()] != lit[..] {
                continue;
            }
            let i = ws_end[i + lit.len()];
            if ch.get(i) != Some(&'=') {
                continue;
            }
            let j = i + 1;
            let w = ws_end[j];
            // greedy `\s*` then `.+` needs one non-"\n" char: back off from the end of the whitespace run
            let k = if w < n {
                Some(w)
            } else {
                (j..w).rev().find(|&k| ch[k] != '\n')
            };
            if let Some(k) = k {
                let e = line_end(k);
                found = Some((k, e, e));
                break;
            }
        }
        let Some((gs, ge, me)) = found else { break };
        out.push(ch[gs..ge].iter().collect());
        pos = if me == pos { me + 1 } else { me };
    }
    out
}

pub fn check_units(c: &DoctorContext<'_>) -> Found {
    let udir = c.home.join(".config").join("systemd").join("user");
    let mut units = c.fs.glob(&udir, "agent-hub*.service");
    units.extend(c.fs.glob(&udir, "agent-knowledge*.service"));
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let app = PurePath::of(&c.app_root);
    let parent = app.parent();
    let pol = app.join("policy");
    let home = PurePath::of(&c.home).to_string();
    let mut out = Vec::new();
    for u in &units {
        let text = c.fs.read_text(u).unwrap_or_default();
        let mut bad: Vec<String> = Vec::new();
        for value in read_write_paths_values(&text) {
            for tok in split_ws(&value) {
                let tok = tok
                    .trim_start_matches(['-', '+'])
                    .replace("%h", &home)
                    .replacen('~', &home, 1);
                let p = PurePath::new(&tok);
                if p == parent || p == pol || p.is_parent_of(&parent) || pol.is_parent_of(&p) {
                    bad.push(tok);
                }
            }
        }
        let name = PurePath::of(u).name().to_string();
        if bad.is_empty() {
            out.push(finding(
                &format!("unit-{name}"),
                "ok",
                &format!("{name}: ReadWritePaths does not include the hub's parent tree or hub/policy"),
                "",
                "",
            ));
        } else {
            out.push(finding(
                &format!("unit-{name}"),
                "warn",
                &format!("{name}: ReadWritePaths exposes the hub's parent tree or hub/policy"),
                &bad.join(", "),
                "Restrict ReadWritePaths to the hub/knowledge state directories only.",
            ));
        }
    }
    Ok(out)
}

pub fn check_knowledge(c: &DoctorContext<'_>) -> Found {
    let k = match &c.knowledge {
        None | Some(Value::Null) => None,
        Some(k) => Some(k),
    };
    let reachable = match k {
        None => false,
        Some(k) => truthy(get(k, "reachable")?),
    };
    let Some(k) = k.filter(|_| reachable) else {
        return Ok(vec![finding(
            "knowledge",
            "info",
            "Knowledge service not reachable",
            "Its key and embedding-key checks were skipped.",
            "Start it: cd hub/knowledge && uv run agent-knowledge",
        )]);
    };
    if !truthy(get(k, "authenticated")?) {
        let src = get(k, "key_source")?.map_or_else(|| "none".to_string(), py_str);
        return Ok(vec![finding(
            "knowledge",
            "warn",
            "The hub cannot authenticate to the knowledge service",
            &format!("No valid knowledge admin key (source: {src})."),
            "Set HUB_KNOWLEDGE_ADMIN_KEY or make ~/.local/share/agent-knowledge/admin.key readable (0600).",
        )]);
    }
    let src = get(k, "key_source")?.map_or_else(|| "None".to_string(), py_str);
    Ok(vec![finding(
        "knowledge",
        "ok",
        "Knowledge service reachable and authenticated",
        &format!("key source: {src}"),
        "",
    )])
}

/// `_norm(p, home)`: canonical form for coverage comparison (~ / $HOME / the real home dir are one spelling; trailing
/// `/**`, `/*`, `/` dropped).
pub fn norm(p: &str, home: &Path) -> String {
    let home = PurePath::of(home).to_string();
    let mut p = p.replace("${HOME}", "~").replace("$HOME", "~");
    if p == home || p.starts_with(&format!("{home}/")) {
        p = format!("~{}", &p[home.len()..]);
    }
    loop {
        if let Some(t) = p.strip_suffix("/**") {
            p = t.to_string();
        } else if let Some(t) = p.strip_suffix("/*") {
            p = t.to_string();
        } else {
            break;
        }
    }
    let t = p.trim_end_matches('/');
    if t.is_empty() { "/".into() } else { t.to_string() }
}

/// `_covers(pattern, target)`: does a deny for `pattern` cover the floor entry `target` (both normalised)?
pub fn covers(pattern: &str, target: &str) -> bool {
    pattern == target || target.starts_with(&format!("{pattern}/")) || hub_core::py::fnmatchcase(target, pattern)
}

/// `re.fullmatch(r"(Read|Write|Edit)\((.*)\)", e)`.
fn tool_deny(e: &str) -> Option<(&'static str, &str)> {
    for (tool, key) in [("Read(", "read"), ("Write(", "write"), ("Edit(", "edit")] {
        if let Some(inner) = e.strip_prefix(tool).and_then(|r| r.strip_suffix(')'))
            && !inner.contains('\n')
        {
            return Some((key, inner));
        }
    }
    None
}

type Coverage = [(&'static str, BTreeSet<String>); 3];

fn empty_coverage() -> Coverage {
    [
        ("read", BTreeSet::new()),
        ("write", BTreeSet::new()),
        ("edit", BTreeSet::new()),
    ]
}

/// `_claude_coverage(doc, home)`: {read, write, edit} -> normalised paths denied by one claude settings document
/// (credentials.files deny all three).
fn claude_coverage(doc: &PyValue, home: &Path) -> Coverage {
    let mut cov = empty_coverage();
    if !matches!(doc, PyValue::Dict(_)) {
        return cov;
    }
    if let Some(PyValue::List(deny)) = pget(pget(Some(doc), "permissions"), "deny") {
        for e in deny {
            if let Some((kind, inner)) = e.as_str().and_then(tool_deny)
                && let Some(slot) = cov.iter_mut().find(|(k, _)| *k == kind)
            {
                slot.1.insert(norm(inner, home));
            }
        }
    }
    if let Some(PyValue::List(files)) = pget(pget(pget(Some(doc), "sandbox"), "credentials"), "files") {
        for e in files {
            if let PyValue::Dict(_) = e
                && e.get("mode").and_then(PyValue::as_str) == Some("deny")
                && let Some(path) = e.get("path").and_then(PyValue::as_str)
            {
                for slot in &mut cov {
                    slot.1.insert(norm(path, home));
                }
            }
        }
    }
    cov
}

/// `claude_effective_gaps(c, cl)`: (uncovered floor entries in claude's own syntax, sources examined) over the UNION
/// of the template, the rendered live config and the project settings files.
pub fn claude_effective_gaps(c: &DoctorContext<'_>, cl: &Value) -> Result<(Vec<String>, Vec<String>), Exc> {
    let project = param_path(c, cl, "settings_project", Some(".claude/settings.json"))?;
    let local = match &project {
        Some(p) => Some(p.with_name("settings.local.json")?),
        None => None,
    };
    let candidates = [
        param_path(c, cl, "settings_template", None)?,
        param_path(c, cl, "settings_rendered", None)?,
        project,
        local,
    ];
    let mut docs: Vec<(String, PyValue)> = Vec::new();
    for p in candidates.into_iter().flatten() {
        if let Some(txt) = c.fs.read_text(&p.path()) {
            docs.push((p.to_string(), load_json(Some(&txt))));
        }
    }
    let mut cov = empty_coverage();
    for (_, d) in &docs {
        for (slot, add) in cov.iter_mut().zip(claude_coverage(d, &c.home)) {
            slot.1.extend(add.1);
        }
    }
    let mut lines: Vec<String> = Vec::new();
    let wants = c
        .floor_read
        .iter()
        .map(|g| ("read", g))
        .chain(c.floor_write.iter().flat_map(|g| [("write", g), ("edit", g)]));
    for (kind, glob) in wants {
        let target = norm(glob, &c.home);
        let have = &cov.iter().find(|(k, _)| *k == kind).map(|s| &s.1);
        if have.is_some_and(|set| set.iter().any(|p| covers(p, &target))) {
            continue;
        }
        let tool = capitalize(kind);
        let line = format!("permissions.deny += \"{tool}({glob})\"");
        if !lines.contains(&line) {
            lines.push(line);
        }
        let stem = norm(glob, &c.home);
        if !stem.starts_with("**") && glob.ends_with("/**") {
            let creds = format!("sandbox.credentials.files += {{\"path\": \"{stem}\", \"mode\": \"deny\"}}");
            if !lines.contains(&creds) {
                lines.push(creds);
            }
        }
    }
    Ok((lines, docs.into_iter().map(|(n, _)| n).collect()))
}

pub fn check_advisory_gap(c: &DoctorContext<'_>) -> Found {
    let mut out = Vec::new();
    for cl in &c.clients {
        if truthy(sub_dict(cl, "manage")?.and_then(|m| m.get("permissions"))) {
            continue;
        }
        let cid = client_id(cl)?;
        let adapter = get(cl, "adapter")?.map_or_else(String::new, py_str);
        let fid = format!("advisory-gap-{cid}");
        if adapter == "claude-code" {
            let (lines, sources) = claude_effective_gaps(c, cl)?;
            if sources.is_empty() {
                out.push(finding(
                    &fid,
                    "info",
                    &format!("{cid}: no settings files found to check the floor's deny paths against"),
                    "Neither a configured template, a rendered config nor the project settings could be read.",
                    "Set the client params settings_template / settings_rendered / settings_project.",
                ));
            } else if !lines.is_empty() {
                let more = if lines.len() > 16 { "\n..." } else { "" };
                out.push(finding(
                    &fid,
                    "warn",
                    &format!(
                        "{cid}: {} floor deny entries are not covered by its effective config",
                        lines.len()
                    ),
                    &format!("Checked the union of: {}", sources.join(", ")),
                    &format!(
                        "Add to the template (or the settings file you maintain):\n{}{more}\n{}",
                        join(take(&lines, 16), "\n"),
                        rerender_hint(cl)?
                    ),
                ));
            } else {
                out.push(finding(
                    &fid,
                    "ok",
                    &format!("{cid}: every floor deny path is covered by its effective config"),
                    &format!("Checked the union of: {}", sources.join(", ")),
                    "",
                ));
            }
        } else if adapter == "opencode" {
            if let Some(gaps) = (c.permission_gaps)(&cid).filter(|g| !g.is_empty()) {
                out.push(finding(
                    &fid,
                    "info",
                    &format!(
                        "{cid}: {} floor paths have no bash-glob deny (cannot be made complete)",
                        gaps.len()
                    ),
                    "opencode is unsandboxed and its bash-glob permissions only approximate path denies.",
                    &format!(
                        "Add what you can to opencode.json, and rely on running it as a separate OS user ({DOCS_POINTER})."
                    ),
                ));
            }
        } else if adapter.starts_with("hermes") {
            out.push(finding(
                &fid,
                "ok",
                &format!("{cid}: runs in a container with no host mounts of the hub's state"),
                "Verify the compose volumes do not mount ~/.local/share/agent-hub or the hub's parent tree read-write.",
                "",
            ));
        } else if let Some(gaps) = (c.permission_gaps)(&cid).filter(|g| !g.is_empty()) {
            let more = if gaps.len() > 8 { "\n..." } else { "" };
            out.push(finding(
                &fid,
                "warn",
                &format!("{cid}: permissions are advisory and the floor's deny paths are missing from its config"),
                &format!("{} missing", gaps.len()),
                &format!("Add to the client's config:\n{}{more}", join(take(&gaps, 8), "\n")),
            ));
        }
    }
    Ok(out)
}

pub fn check_skill_evaluator(c: &DoctorContext<'_>) -> Found {
    let Some(s) = c.skill_evaluator.as_ref().filter(|s| !s.is_null()) else {
        return Ok(Vec::new());
    };
    if !truthy(get(s, "installed")?) {
        return Ok(vec![finding(
            "skill-evaluator",
            "info",
            "SkillEvaluator is not installed (optional)",
            "Skill evaluation is unavailable: neither skillevaluator-local nor skillevaluator is on the hub's PATH or in ~/.local/bin. Evaluations are advisory; nothing else depends on them.",
            super::skilleval::INSTALL_HINT,
        )]);
    }
    let launcher = get(s, "launcher")?.map_or_else(|| "None".to_string(), py_str);
    let version = get(s, "version")?;
    // `[str(n) for n in s.get("env_names") or []]`
    let names: Vec<String> = match get(s, "env_names")? {
        v if !truthy(v) => Vec::new(),
        Some(Value::Array(a)) => a.iter().map(py_str).collect(),
        Some(Value::String(t)) => t.chars().map(String::from).collect(),
        Some(Value::Object(o)) => o.keys().cloned().collect(),
        Some(_) => return Err("TypeError"),
        None => Vec::new(),
    };
    let how = if launcher == "skillevaluator-local" {
        "The wrapper supplies the model provider settings itself; the hub passes it no key.".to_string()
    } else {
        format!(
            "Plain skillevaluator: the LLM tiers need SKILL_EVAL_* in the hub's environment ({} such variable(s) set; names only: {}). The static tier needs none.",
            names.len(),
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        )
    };
    let ver = if truthy(version) {
        format!(" {}", version.map(py_str).unwrap_or_default())
    } else {
        String::new()
    };
    Ok(vec![finding(
        "skill-evaluator",
        "info",
        &format!("SkillEvaluator is installed ({launcher}{ver})"),
        &how,
        "",
    )])
}

/// The checks in Python's `CHECKS` order.
pub const CHECKS: [Check; 12] = [
    ("check_bootstrap_key", check_bootstrap_key),
    ("check_modes", check_modes),
    ("check_content_dir", check_content_dir),
    ("check_network_exposure", check_network_exposure),
    ("check_claude_code", check_claude_code),
    ("check_params_missing", check_params_missing),
    ("check_opencode", check_opencode),
    ("check_ptrace", check_ptrace),
    ("check_units", check_units),
    ("check_knowledge", check_knowledge),
    ("check_advisory_gap", check_advisory_gap),
    ("check_skill_evaluator", check_skill_evaluator),
];

/// `run_doctor(ctx)`: `{"status": ok|attention|action_needed, "counts": {crit, warn, info, ok}, "findings": [...]}`.
pub fn run_doctor(c: &DoctorContext<'_>) -> hub_core::HubResult<Value> {
    Ok(run_doctor_with(c, &CHECKS))
}

/// `run_doctor` over an explicit check list (Python's tests monkeypatch `CHECKS`). A check that raises (or panics)
/// adds `check-<name>` info with only the exception type name, never its message.
pub fn run_doctor_with(c: &DoctorContext<'_>, checks: &[Check]) -> Value {
    let mut findings: Vec<Value> = Vec::new();
    for (name, chk) in checks {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| chk(c))).unwrap_or(Err("RuntimeError"));
        match r {
            Ok(f) => findings.extend(f),
            Err(exc) => findings.push(finding(
                &format!("check-{name}"),
                "info",
                &format!("{name} could not run"),
                exc,
                "",
            )),
        }
    }
    let count = |s: &str| findings.iter().filter(|f| f["severity"] == s).count();
    let (crit, warn, info, ok) = (count("crit"), count("warn"), count("info"), count("ok"));
    let status = if crit > 0 {
        "action_needed"
    } else if warn > 0 {
        "attention"
    } else {
        "ok"
    };
    json!({
        "status": status,
        "counts": {"crit": crit, "warn": warn, "info": info, "ok": ok},
        "findings": findings,
    })
}

// ---- Unicode tables (CPython 3.13, Unicode 15.1) -----------------------------------------------------------------------
/// Characters whose `str.title()` differs from `str.upper()` (generated: `[(c, c.title()) for c in map(chr, range(0x110000)) if c.title() != c.upper()]`).
const TITLE_NOT_UPPER: [(char, &str); 135] = [
    ('\u{DF}', "Ss"),
    ('\u{1C4}', "\u{1C5}"),
    ('\u{1C5}', "\u{1C5}"),
    ('\u{1C6}', "\u{1C5}"),
    ('\u{1C7}', "\u{1C8}"),
    ('\u{1C8}', "\u{1C8}"),
    ('\u{1C9}', "\u{1C8}"),
    ('\u{1CA}', "\u{1CB}"),
    ('\u{1CB}', "\u{1CB}"),
    ('\u{1CC}', "\u{1CB}"),
    ('\u{1F1}', "\u{1F2}"),
    ('\u{1F2}', "\u{1F2}"),
    ('\u{1F3}', "\u{1F2}"),
    ('\u{587}', "\u{535}\u{582}"),
    ('\u{10D0}', "\u{10D0}"),
    ('\u{10D1}', "\u{10D1}"),
    ('\u{10D2}', "\u{10D2}"),
    ('\u{10D3}', "\u{10D3}"),
    ('\u{10D4}', "\u{10D4}"),
    ('\u{10D5}', "\u{10D5}"),
    ('\u{10D6}', "\u{10D6}"),
    ('\u{10D7}', "\u{10D7}"),
    ('\u{10D8}', "\u{10D8}"),
    ('\u{10D9}', "\u{10D9}"),
    ('\u{10DA}', "\u{10DA}"),
    ('\u{10DB}', "\u{10DB}"),
    ('\u{10DC}', "\u{10DC}"),
    ('\u{10DD}', "\u{10DD}"),
    ('\u{10DE}', "\u{10DE}"),
    ('\u{10DF}', "\u{10DF}"),
    ('\u{10E0}', "\u{10E0}"),
    ('\u{10E1}', "\u{10E1}"),
    ('\u{10E2}', "\u{10E2}"),
    ('\u{10E3}', "\u{10E3}"),
    ('\u{10E4}', "\u{10E4}"),
    ('\u{10E5}', "\u{10E5}"),
    ('\u{10E6}', "\u{10E6}"),
    ('\u{10E7}', "\u{10E7}"),
    ('\u{10E8}', "\u{10E8}"),
    ('\u{10E9}', "\u{10E9}"),
    ('\u{10EA}', "\u{10EA}"),
    ('\u{10EB}', "\u{10EB}"),
    ('\u{10EC}', "\u{10EC}"),
    ('\u{10ED}', "\u{10ED}"),
    ('\u{10EE}', "\u{10EE}"),
    ('\u{10EF}', "\u{10EF}"),
    ('\u{10F0}', "\u{10F0}"),
    ('\u{10F1}', "\u{10F1}"),
    ('\u{10F2}', "\u{10F2}"),
    ('\u{10F3}', "\u{10F3}"),
    ('\u{10F4}', "\u{10F4}"),
    ('\u{10F5}', "\u{10F5}"),
    ('\u{10F6}', "\u{10F6}"),
    ('\u{10F7}', "\u{10F7}"),
    ('\u{10F8}', "\u{10F8}"),
    ('\u{10F9}', "\u{10F9}"),
    ('\u{10FA}', "\u{10FA}"),
    ('\u{10FD}', "\u{10FD}"),
    ('\u{10FE}', "\u{10FE}"),
    ('\u{10FF}', "\u{10FF}"),
    ('\u{1F80}', "\u{1F88}"),
    ('\u{1F81}', "\u{1F89}"),
    ('\u{1F82}', "\u{1F8A}"),
    ('\u{1F83}', "\u{1F8B}"),
    ('\u{1F84}', "\u{1F8C}"),
    ('\u{1F85}', "\u{1F8D}"),
    ('\u{1F86}', "\u{1F8E}"),
    ('\u{1F87}', "\u{1F8F}"),
    ('\u{1F88}', "\u{1F88}"),
    ('\u{1F89}', "\u{1F89}"),
    ('\u{1F8A}', "\u{1F8A}"),
    ('\u{1F8B}', "\u{1F8B}"),
    ('\u{1F8C}', "\u{1F8C}"),
    ('\u{1F8D}', "\u{1F8D}"),
    ('\u{1F8E}', "\u{1F8E}"),
    ('\u{1F8F}', "\u{1F8F}"),
    ('\u{1F90}', "\u{1F98}"),
    ('\u{1F91}', "\u{1F99}"),
    ('\u{1F92}', "\u{1F9A}"),
    ('\u{1F93}', "\u{1F9B}"),
    ('\u{1F94}', "\u{1F9C}"),
    ('\u{1F95}', "\u{1F9D}"),
    ('\u{1F96}', "\u{1F9E}"),
    ('\u{1F97}', "\u{1F9F}"),
    ('\u{1F98}', "\u{1F98}"),
    ('\u{1F99}', "\u{1F99}"),
    ('\u{1F9A}', "\u{1F9A}"),
    ('\u{1F9B}', "\u{1F9B}"),
    ('\u{1F9C}', "\u{1F9C}"),
    ('\u{1F9D}', "\u{1F9D}"),
    ('\u{1F9E}', "\u{1F9E}"),
    ('\u{1F9F}', "\u{1F9F}"),
    ('\u{1FA0}', "\u{1FA8}"),
    ('\u{1FA1}', "\u{1FA9}"),
    ('\u{1FA2}', "\u{1FAA}"),
    ('\u{1FA3}', "\u{1FAB}"),
    ('\u{1FA4}', "\u{1FAC}"),
    ('\u{1FA5}', "\u{1FAD}"),
    ('\u{1FA6}', "\u{1FAE}"),
    ('\u{1FA7}', "\u{1FAF}"),
    ('\u{1FA8}', "\u{1FA8}"),
    ('\u{1FA9}', "\u{1FA9}"),
    ('\u{1FAA}', "\u{1FAA}"),
    ('\u{1FAB}', "\u{1FAB}"),
    ('\u{1FAC}', "\u{1FAC}"),
    ('\u{1FAD}', "\u{1FAD}"),
    ('\u{1FAE}', "\u{1FAE}"),
    ('\u{1FAF}', "\u{1FAF}"),
    ('\u{1FB2}', "\u{1FBA}\u{345}"),
    ('\u{1FB3}', "\u{1FBC}"),
    ('\u{1FB4}', "\u{386}\u{345}"),
    ('\u{1FB7}', "\u{391}\u{342}\u{345}"),
    ('\u{1FBC}', "\u{1FBC}"),
    ('\u{1FC2}', "\u{1FCA}\u{345}"),
    ('\u{1FC3}', "\u{1FCC}"),
    ('\u{1FC4}', "\u{389}\u{345}"),
    ('\u{1FC7}', "\u{397}\u{342}\u{345}"),
    ('\u{1FCC}', "\u{1FCC}"),
    ('\u{1FF2}', "\u{1FFA}\u{345}"),
    ('\u{1FF3}', "\u{1FFC}"),
    ('\u{1FF4}', "\u{38F}\u{345}"),
    ('\u{1FF7}', "\u{3A9}\u{342}\u{345}"),
    ('\u{1FFC}', "\u{1FFC}"),
    ('\u{FB00}', "Ff"),
    ('\u{FB01}', "Fi"),
    ('\u{FB02}', "Fl"),
    ('\u{FB03}', "Ffi"),
    ('\u{FB04}', "Ffl"),
    ('\u{FB05}', "St"),
    ('\u{FB06}', "St"),
    ('\u{FB13}', "\u{544}\u{576}"),
    ('\u{FB14}', "\u{544}\u{565}"),
    ('\u{FB15}', "\u{544}\u{56B}"),
    ('\u{FB16}', "\u{54E}\u{576}"),
    ('\u{FB17}', "\u{544}\u{56D}"),
];
/// Ranges of characters with `str.isdigit()` but not `str.isdecimal()` (superscripts, circled digits, ...).
const DIGIT_NOT_DECIMAL: [(u32, u32); 20] = [
    (0xB2, 0xB3),
    (0xB9, 0xB9),
    (0x1369, 0x1371),
    (0x19DA, 0x19DA),
    (0x2070, 0x2070),
    (0x2074, 0x2079),
    (0x2080, 0x2089),
    (0x2460, 0x2468),
    (0x2474, 0x247C),
    (0x2488, 0x2490),
    (0x24EA, 0x24EA),
    (0x24F5, 0x24FD),
    (0x24FF, 0x24FF),
    (0x2776, 0x277E),
    (0x2780, 0x2788),
    (0x278A, 0x2792),
    (0x10A40, 0x10A43),
    (0x10E60, 0x10E68),
    (0x11052, 0x1105A),
    (0x1F100, 0x1F10A),
];
