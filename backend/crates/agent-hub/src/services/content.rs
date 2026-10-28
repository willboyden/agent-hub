//! Port of services/content.py: strict, schema-validated read/write access to a content tree (the git working tree, or
//! a read-only snapshot of HEAD). Nothing here commits. Layout (PORTING.md "Content repo layout"): skills/<name>/
//! {SKILL.md, hub.yaml, extra files}, agents|instructions|memory/<id>.md (frontmatter), mcp|rules|collections|clients|
//! profiles/<id>.yaml, inbox/<client>/<id>.md, endpoints.yaml, hub.yaml. Writes: `common::atomic_write` mode 0644,
//! YAML via `yamlsafe::dump` / `join_frontmatter`. Owner: WS-B1.
//!
//! Python raises many exception types through these methods and its callers tell them apart (`except Unprocessable`,
//! `except (ValidationError, YamlError)`, `type(exc).__name__`). The locked public methods return `HubResult`; each has
//! an `*_x` twin returning [`Exc`], which keeps that distinction for the other WS-B1 services.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use hub_core::common::{atomic_write, token_hex};
use hub_core::ids::{from_plural, plural, valid_id};
use hub_core::models::{self, ClientDoc, CollectionDoc, Issue, ProfileDoc, issues_from};
use hub_core::pathsafe::{check_relpath, resolve_under};
use hub_core::pyd::{self, Model, ValidationError};
use hub_core::value::PyValue;
use hub_core::yamlsafe::{self, YamlError};
use hub_core::{HubResult, Problem};
use indexmap::IndexMap;
use serde_json::Value;

use super::attest::Attestor;
use crate::adapters::base::Files;

pub const SKILL_RESERVED: [&str; 2] = ["SKILL.md", "hub.yaml"];
pub const MAX_SKILL_FILES: usize = 200;
pub const SEED_DIRS: [&str; 10] = [
    "instructions",
    "skills",
    "agents",
    "mcp",
    "rules",
    "memory",
    "inbox",
    "collections",
    "clients",
    "profiles",
];

const FIX_HINT: &str =
    "Quote frontmatter values that contain ': ' or start with special characters, e.g. description: \"Use when: ...\"";

/// `DEFAULT_ENDPOINTS` (seeded into endpoints.yaml).
pub fn default_endpoints() -> IndexMap<String, String> {
    IndexMap::from([
        ("router".to_string(), "http://127.0.0.1:4000/v1".to_string()),
        ("knowledge".to_string(), "http://127.0.0.1:8795".to_string()),
    ])
}

// ---- Python exceptions -------------------------------------------------------------------------------------------

/// An exception escaping a content operation, as Python's callers see it.
#[derive(Debug, Clone)]
pub enum Exc {
    /// `ProblemError` (any subclass; [`py_class`] names it).
    Problem(Problem),
    /// `pydantic.ValidationError`.
    Validation(ValidationError),
    /// Any other exception: `type(exc).__name__` and `str(exc)` (`YamlError`, `ValueError`, `OSError`, `TypeError`...).
    Other { name: String, msg: String },
}

pub type XResult<T> = Result<T, Exc>;

impl Exc {
    pub fn other(name: &str, msg: impl Into<String>) -> Self {
        Exc::Other {
            name: name.to_string(),
            msg: msg.into(),
        }
    }
    /// What the HTTP layer makes of it: a ProblemError as itself, everything else 500 `internal_error`.
    pub fn into_problem(self) -> Problem {
        match self {
            Exc::Problem(p) => p,
            Exc::Validation(_) | Exc::Other { .. } => Problem::internal(),
        }
    }
    /// `type(exc).__name__`.
    pub fn type_name(&self) -> String {
        match self {
            Exc::Problem(p) => py_class(p).to_string(),
            Exc::Validation(_) => "ValidationError".into(),
            Exc::Other { name, .. } => name.clone(),
        }
    }
    /// `str(exc)`.
    pub fn message(&self) -> String {
        match self {
            Exc::Problem(p) => p.detail.clone(),
            Exc::Validation(e) => e.to_string(),
            Exc::Other { msg, .. } => msg.clone(),
        }
    }
    /// `isinstance(exc, yamlsafe.YamlError)`.
    pub fn is_yaml(&self) -> bool {
        matches!(self, Exc::Other { name, .. } if name == "YamlError")
    }
    /// `isinstance(exc, ValueError)` (YamlError, UnicodeDecodeError and ValidationError are ValueErrors).
    pub fn is_value_error(&self) -> bool {
        match self {
            Exc::Validation(_) => true,
            Exc::Other { name, .. } => matches!(name.as_str(), "ValueError" | "YamlError" | "UnicodeDecodeError"),
            Exc::Problem(_) => false,
        }
    }
    /// The problem, when it is one of the given ProblemError classes.
    pub fn problem_of(&self, classes: &[&str]) -> Option<&Problem> {
        match self {
            Exc::Problem(p) if classes.contains(&py_class(p)) => Some(p),
            _ => None,
        }
    }
}

impl From<Problem> for Exc {
    fn from(p: Problem) -> Self {
        Exc::Problem(p)
    }
}

impl From<io::Error> for Exc {
    fn from(e: io::Error) -> Self {
        Exc::Other {
            name: hub_core::py::os::error_name(&e).to_string(),
            msg: hub_core::py::os::error_str(&e, None),
        }
    }
}

impl From<yamlsafe::LoadError> for Exc {
    /// `yamlsafe.YamlError`, or the Python exception that escaped PyYAML with its own type.
    fn from(e: yamlsafe::LoadError) -> Self {
        match e {
            yamlsafe::LoadError::Yaml(y) => Exc::other("YamlError", y.0),
            yamlsafe::LoadError::Python { exc_type, message } => Exc::other(exc_type, message),
        }
    }
}

impl From<YamlError> for Exc {
    fn from(e: YamlError) -> Self {
        Exc::other("YamlError", e.0)
    }
}

impl From<ValidationError> for Exc {
    fn from(e: ValidationError) -> Self {
        Exc::Validation(e)
    }
}

/// The ProblemError subclass Python raises for a problem (`type(exc).__name__`). Status and code identify it: every
/// 503 is an `Unavailable` except the two plain `ProblemError(status=503)` (content guard, attestation key), and the
/// 429s are plain `ProblemError`s.
pub fn py_class(p: &Problem) -> &'static str {
    match p.status {
        400 => "BadRequest",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "NotFound",
        409 => "Conflict",
        422 => "Unprocessable",
        503 if !matches!(p.code.as_str(), "content_dir_unsafe" | "attest_key_unusable") => "Unavailable",
        _ => "ProblemError",
    }
}

fn unprocessable(detail: impl Into<String>) -> Problem {
    Problem::unprocessable(detail)
}

/// `[i.model_dump() for i in issues_from(exc, path)]`.
pub fn issue_dicts(exc: &ValidationError, path: &str) -> Value {
    Value::Array(
        issues_from(exc, path)
            .iter()
            .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
            .collect(),
    )
}

/// `Unprocessable(detail, errors=[...issues...])`.
pub fn invalid(detail: &str, exc: &ValidationError) -> Problem {
    unprocessable(detail).with("errors", issue_dicts(exc, ""))
}

// ---- Python value helpers ----------------------------------------------------------------------------------------

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

/// `list(x)`: the items Python's iteration yields (dict keys, str characters, bytes ints); TypeError otherwise.
pub fn py_list(v: &PyValue) -> XResult<Vec<PyValue>> {
    Ok(match v {
        PyValue::List(l) | PyValue::Set(l) => l.clone(),
        PyValue::Dict(d) => d.iter().map(|(k, _)| k.clone()).collect(),
        PyValue::Str(s) => s.chars().map(|c| PyValue::Str(c.to_string())).collect(),
        PyValue::Bytes(b) => b.iter().map(|x| PyValue::Int(i64::from(*x))).collect(),
        other => {
            return Err(Exc::other(
                "TypeError",
                format!("'{}' object is not iterable", other.type_name()),
            ));
        }
    })
}

/// `d.get(key)` on a str-keyed dict body.
pub fn dget<'a>(d: &'a [(PyValue, PyValue)], key: &str) -> Option<&'a PyValue> {
    d.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v)
}

/// `d[key] = value` (an existing key keeps its position).
pub fn dset(d: &mut Vec<(PyValue, PyValue)>, key: &str, value: PyValue) {
    match d.iter_mut().find(|(k, _)| k.as_str() == Some(key)) {
        Some(slot) => slot.1 = value,
        None => d.push((PyValue::str(key), value)),
    }
}

/// `d.pop(key, None)`.
pub fn dpop(d: &mut Vec<(PyValue, PyValue)>, key: &str) -> Option<PyValue> {
    let i = d.iter().position(|(k, _)| k.as_str() == Some(key))?;
    Some(d.remove(i).1)
}

/// `dict(payload)` of a request/caller payload (a dict).
pub fn dict_of(v: &PyValue) -> XResult<Vec<(PyValue, PyValue)>> {
    match v {
        PyValue::Dict(d) => Ok(d.clone()),
        other => Err(Exc::other(
            "TypeError",
            format!("'{}' object is not iterable", other.type_name()),
        )),
    }
}

/// The entries of a dict value (empty for anything else).
pub fn dict_items(v: PyValue) -> Vec<(PyValue, PyValue)> {
    match v {
        PyValue::Dict(d) => d,
        _ => Vec::new(),
    }
}

/// `bytes.decode("utf-8")` with CPython's UnicodeDecodeError text.
pub fn decode_utf8(b: &[u8]) -> Result<String, String> {
    match std::str::from_utf8(b) {
        Ok(s) => Ok(s.to_string()),
        Err(e) => {
            let start = e.valid_up_to();
            let lead = b.get(start).copied().unwrap_or(0);
            let (end, reason) = match e.error_len() {
                None => (b.len(), "unexpected end of data"),
                Some(n) => {
                    let reason = if n == 1 && !(0xC2..=0xF4).contains(&lead) {
                        "invalid start byte"
                    } else {
                        "invalid continuation byte"
                    };
                    (start + n, reason)
                }
            };
            Err(if end == start + 1 {
                format!("'utf-8' codec can't decode byte 0x{lead:02x} in position {start}: {reason}")
            } else {
                format!(
                    "'utf-8' codec can't decode bytes in position {start}-{}: {reason}",
                    end - 1
                )
            })
        }
    }
}

/// `Path.read_text()`'s universal newlines: `\r\n` and lone `\r` become `\n`.
pub fn universal_newlines(s: &str) -> String {
    if s.contains('\r') {
        s.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        s.to_string()
    }
}

/// `yamlsafe.load(text)`: its `YamlError` as `Exc::Other{"YamlError"}`; a Python exception that escapes PyYAML
/// (a ValueError for `2024-02-30`, a KeyError, a RecursionError) keeps its own type, so callers that catch only
/// `YamlError` let it through (-> 500), like Python.
pub fn yaml_load_x(text: &str, max_bytes: usize) -> XResult<PyValue> {
    yamlsafe::load_exact(text, max_bytes).map_err(Exc::from)
}

/// `yamlsafe.split_frontmatter(text)` with the exceptions of [`yaml_load_x`].
pub fn split_frontmatter_x(text: &str) -> XResult<(Vec<(PyValue, PyValue)>, String)> {
    yamlsafe::split_frontmatter_exact(text).map_err(Exc::from)
}

/// `Model.model_validate(yamlsafe.load(text) or {})` where the caller catches `(ValidationError, YamlError)`:
/// Ok(Err(message)) for those two (str(exc)), Err for anything else.
fn load_validate(text: &str, model: &Model) -> XResult<Result<PyValue, String>> {
    let v = match yaml_load_x(text, yamlsafe::DEFAULT_MAX_BYTES) {
        Ok(v) => v,
        Err(e) if e.is_yaml() => return Ok(Err(e.message())),
        Err(e) => return Err(e),
    };
    let v = if truthy(&v) { v } else { PyValue::Dict(Vec::new()) };
    Ok(pyd::validate(model, &v).map_err(|e| e.to_string()))
}

/// `os.stat(p).st_mtime` (CPython: `sec + nsec * 1e-9`).
pub fn st_mtime(m: &fs::Metadata) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let (sec, nsec) = (m.mtime() as f64, m.mtime_nsec() as f64);
    sec + nsec * 1e-9
}

/// `sorted(base.rglob("*"))` (Python 3.13: symlinked directories are listed, never descended; unreadable directories
/// are skipped) as (path, relative components), ordered like `Path` comparison (component lists).
pub fn rglob(base: &Path) -> Vec<(PathBuf, Vec<String>)> {
    fn walk(d: &Path, rel: &[String], out: &mut Vec<(PathBuf, Vec<String>)>) {
        let Ok(rd) = fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let mut r = rel.to_vec();
            r.push(e.file_name().to_string_lossy().into_owned());
            let is_real_dir = fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir());
            out.push((p.clone(), r.clone()));
            if is_real_dir {
                walk(&p, &r, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(base, &[], &mut out);
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

fn is_symlink(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

/// The kind's model and directory: `_MD` (frontmatter files) and `_YAML` (YAML documents).
fn md_kind(kind: &str) -> Option<(&'static str, &'static Model)> {
    match kind {
        "agent" => Some(("agents", models::agent_meta())),
        "instruction" => Some(("instructions", models::instruction_meta())),
        "memory" => Some(("memory", models::memory_meta())),
        _ => None,
    }
}

fn yaml_kind(kind: &str) -> Option<(&'static str, &'static Model)> {
    match kind {
        "mcp" => Some(("mcp", models::mcp_doc())),
        "rule" => Some(("rules", models::rule_doc())),
        _ => None,
    }
}

/// The pydantic model of an item kind's meta.
pub fn model_of(kind: &str) -> Option<&'static Model> {
    match kind {
        "skill" => Some(models::skill_meta()),
        k => md_kind(k).or_else(|| yaml_kind(k)).map(|(_, m)| m),
    }
}

fn key_of(kind: &str) -> &'static str {
    if matches!(kind, "instruction" | "rule" | "memory") {
        "id"
    } else {
        "name"
    }
}

/// `model.model_dump(mode="json", exclude_none=...)` of a validated value, as a PyValue (what yamlsafe dumps);
/// `pyd::dump_json` drops None-valued fields and extras with `exclude_none`, as pydantic does.
pub fn dump_model(model: &Model, v: &PyValue, exclude_none: bool) -> PyValue {
    PyValue::from_json(&pyd::dump_json(model, v, exclude_none))
}

/// As [`dump_model`], as JSON (API responses).
pub fn dump_model_json(model: &Model, v: &PyValue, exclude_none: bool) -> Value {
    dump_model(model, v, exclude_none).to_json()
}

/// The default `Sidecar()` (validated).
pub fn default_sidecar() -> PyValue {
    pyd::validate(models::sidecar(), &PyValue::Dict(Vec::new())).unwrap_or(PyValue::Dict(Vec::new()))
}

/// `Sidecar.model_dump(mode="json")`.
pub fn sidecar_json(side: &PyValue) -> Value {
    dump_model_json(models::sidecar(), side, false)
}

// ---- items -------------------------------------------------------------------------------------------------------

/// One content item. `meta` is the VALIDATED model dict (pyd output for the kind's model), None when invalid.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Item {
    pub kind: String,
    pub name: String,
    pub meta: Option<PyValue>,
    pub body: String,
    /// skills: every file except the hub.yaml sidecar
    pub files: Files,
    /// skills: the validated Sidecar dict
    pub sidecar: Option<PyValue>,
    pub issues: Vec<Issue>,
    pub updated: f64,
}

/// `list(x or [])` of a tags/groups/applies_to value, each element as text.
fn str_list(v: &PyValue) -> Vec<String> {
    if !truthy(v) {
        return Vec::new();
    }
    py_list(v).unwrap_or_default().iter().map(PyValue::py_str).collect()
}

impl Item {
    fn new(kind: &str, name: &str) -> Self {
        Item {
            kind: kind.into(),
            name: name.into(),
            ..Item::default()
        }
    }
    pub fn valid(&self) -> bool {
        self.meta.is_some() && self.issues.is_empty()
    }
    /// `_get(attr)`: the meta's attribute (fields and allowed extras), None without meta.
    fn get(&self, attr: &str) -> Option<&PyValue> {
        self.meta.as_ref()?.get(attr)
    }
    fn get_truthy(&self, attr: &str) -> Option<&PyValue> {
        self.get(attr).filter(|v| truthy(v))
    }
    /// `title`: meta.title or meta.description or name, first 200 chars.
    pub fn title(&self) -> String {
        let s = self
            .get_truthy("title")
            .or_else(|| self.get_truthy("description"))
            .map_or_else(|| self.name.clone(), PyValue::py_str);
        hub_core::py::text::take_chars(&s, 200).to_string()
    }
    pub fn description(&self) -> String {
        self.get_truthy("description")
            .or_else(|| self.get_truthy("title"))
            .map(PyValue::py_str)
            .unwrap_or_default()
    }
    pub fn tags(&self) -> Vec<String> {
        match &self.sidecar {
            Some(s) => s.get("tags").map(str_list).unwrap_or_default(),
            None => self.get("tags").map(str_list).unwrap_or_default(),
        }
    }
    pub fn groups(&self) -> Vec<String> {
        match &self.sidecar {
            Some(s) => s.get("groups").map(str_list).unwrap_or_default(),
            None => self.get("groups").map(str_list).unwrap_or_default(),
        }
    }
    pub fn source(&self) -> String {
        self.sidecar
            .as_ref()
            .and_then(|s| s.get("source"))
            .map(PyValue::py_str)
            .unwrap_or_default()
    }
    pub fn applies_to(&self) -> Vec<String> {
        self.get("applies_to").map(str_list).unwrap_or_default()
    }
    /// A field of a valid item's meta (`meta_as(item, Model).<field>`), as text.
    pub fn meta_str(&self, field: &str) -> String {
        self.get(field).map(PyValue::py_str).unwrap_or_default()
    }
    /// A field of a valid item's meta.
    pub fn meta_field(&self, field: &str) -> Option<&PyValue> {
        self.get(field)
    }
    /// `item.meta.model_dump(mode="json", exclude_none=True, by_alias=True)`.
    pub fn meta_json(&self) -> Option<Value> {
        let model = model_of(&self.kind)?;
        self.meta.as_ref().map(|m| dump_model_json(model, m, true))
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Catalog {
    /// (kind, name) -> item, in load order
    pub items: IndexMap<(String, String), Item>,
    pub collections: IndexMap<String, CollectionDoc>,
    pub clients: IndexMap<String, ClientDoc>,
    pub profiles: IndexMap<String, ProfileDoc>,
    pub endpoints: IndexMap<String, String>,
    pub problems: Vec<Issue>,
}

impl Catalog {
    /// Items of one kind sorted by name.
    pub fn of_kind(&self, kind: &str) -> Vec<&Item> {
        let mut v: Vec<&Item> = self
            .items
            .iter()
            .filter(|((k, _), _)| k == kind)
            .map(|(_, i)| i)
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

/// `validate_endpoints(data)`: name rules + http(s) URLs (ValueError messages are API-visible).
pub fn validate_endpoints(data: &PyValue) -> Result<IndexMap<String, String>, String> {
    let Some(d) = data.as_dict() else {
        return Err("endpoints must be a mapping".into());
    };
    let mut out = IndexMap::new();
    for (k, v) in d {
        let good = k.as_str().is_some_and(|s| {
            s.chars().next().is_some_and(char::is_alphabetic)
                && hub_core::py::text::is_alnum(&s.replace('_', "a"))
                && s == s.to_lowercase()
                && s.chars().count() <= 32
        });
        if !good {
            return Err(format!(
                "bad endpoint name: {}",
                hub_core::py::text::repr(hub_core::py::text::take_chars(&k.py_str(), 32))
            ));
        }
        let ks = k.py_str();
        match v.as_str() {
            Some(u) if (u.starts_with("http://") || u.starts_with("https://")) && u.chars().count() <= 300 => {
                out.insert(ks, u.to_string());
            }
            _ => return Err(format!("endpoint {ks} must be an http(s) URL")),
        }
    }
    Ok(out)
}

/// `kind_from_plural(plural)`.
pub fn kind_from_plural(plural: &str) -> HubResult<&'static str> {
    from_plural(plural).ok_or_else(|| Problem::bad_request(format!("unknown collection of items: {plural}")))
}

/// `base64.b64decode(s, validate=True)`: None where Python raises (binascii.Error / non-ASCII ValueError): padding is
/// required and canonical, non-zero trailing bits are accepted.
pub fn b64decode_strict(s: &str) -> Option<Vec<u8>> {
    if !s.is_ascii() {
        return None;
    }
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::RequireCanonical),
    );
    engine.decode(s).ok()
}

pub struct ContentStore {
    pub root: PathBuf,
    pub max_file: u64,
    pub max_skill: u64,
    pub attestor: Option<Arc<Attestor>>,
}

impl ContentStore {
    pub fn new(root: &Path, max_file: u64, max_skill: u64, attestor: Option<Arc<Attestor>>) -> Self {
        Self {
            root: root.to_path_buf(),
            max_file,
            max_skill,
            attestor,
        }
    }

    // -- low level ---------------------------------------------------------------------------------------------
    /// `_p(rel)`: resolved under the root; 400 `unsafe_path` otherwise.
    pub fn path(&self, rel: &str) -> HubResult<PathBuf> {
        resolve_under(&self.root, rel)
            .map_err(|e| Problem::bad_request(format!("unsafe content path: {e}")).code("unsafe_path"))
    }

    /// `_read(rel)`: UTF-8 text (universal newlines), capped at `max_file`.
    pub fn read_x(&self, rel: &str) -> XResult<String> {
        let p = self.path(rel)?;
        let st = fs::symlink_metadata(&p)?;
        if !p.is_file() || st.file_type().is_symlink() {
            return Err(Problem::not_found(format!("no such file: {rel}")).into());
        }
        if st.len() > self.max_file {
            return Err(unprocessable(format!("{rel} exceeds {} bytes", self.max_file))
                .code("file_too_large")
                .into());
        }
        let raw = fs::read(&p)?;
        match std::str::from_utf8(&raw) {
            Ok(s) => Ok(universal_newlines(s)),
            Err(_) => Err(unprocessable(format!("{rel} is not valid UTF-8")).into()),
        }
    }

    /// `_write(rel, data)`: size cap (skills: `max_skill`, else `max_file`), atomic, 0644.
    pub fn write_x(&self, rel: &str, data: &[u8]) -> XResult<()> {
        let cap = if rel.starts_with("skills/") {
            self.max_skill
        } else {
            self.max_file
        };
        if data.len() as u64 > cap {
            return Err(unprocessable(format!("{rel} is too large"))
                .code("file_too_large")
                .into());
        }
        let p = self.path(rel)?;
        atomic_write(&p, data, 0o644, 0o755)?;
        Ok(())
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.path(rel).is_ok_and(|p| p.exists())
    }

    fn names_in(&self, sub: &str, suffix: &str) -> XResult<Vec<String>> {
        let d = self.root.join(sub);
        if !d.is_dir() || is_symlink(&d) {
            return Ok(Vec::new());
        }
        let mut entries: Vec<(String, PathBuf)> = fs::read_dir(&d)?
            .map(|e| e.map(|e| (e.file_name().to_string_lossy().into_owned(), e.path())))
            .collect::<io::Result<_>>()?;
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out = Vec::new();
        for (fname, f) in entries {
            if is_symlink(&f) || !f.is_file() || !fname.ends_with(suffix) {
                continue;
            }
            let stem = &fname[..fname.len() - suffix.len()];
            if valid_id(stem) {
                out.push(stem.to_string());
            }
        }
        Ok(out)
    }

    // -- seeds -------------------------------------------------------------------------------------------------
    pub fn seed(&self) -> HubResult<Vec<String>> {
        self.seed_x().map_err(Exc::into_problem)
    }

    pub fn seed_x(&self) -> XResult<Vec<String>> {
        let mut created = Vec::new();
        for d in SEED_DIRS {
            let p = self.root.join(d);
            fs::create_dir_all(&p)?;
            let keep = p.join(".gitkeep");
            if d != "inbox" && !keep.exists() {
                fs::write(&keep, b"")?;
                created.push(format!("{d}/.gitkeep"));
            }
        }
        if !self.root.join("hub.yaml").exists() {
            let meta = pyd::validate(models::hub_meta(), &PyValue::Dict(Vec::new()))?;
            self.write_x("hub.yaml", yamlsafe::dump(&meta).as_bytes())?;
            created.push("hub.yaml".into());
        }
        if !self.root.join("endpoints.yaml").exists() {
            self.write_x(
                "endpoints.yaml",
                yamlsafe::dump(&endpoints_value(&default_endpoints())).as_bytes(),
            )?;
            created.push("endpoints.yaml".into());
        }
        Ok(created)
    }

    // -- items: read -------------------------------------------------------------------------------------------
    pub fn names(&self, kind: &str) -> HubResult<Vec<String>> {
        self.names_x(kind).map_err(Exc::into_problem)
    }

    pub fn names_x(&self, kind: &str) -> XResult<Vec<String>> {
        if kind == "skill" {
            let d = self.root.join("skills");
            if !d.is_dir() {
                return Ok(Vec::new());
            }
            let mut out = Vec::new();
            for e in fs::read_dir(&d)? {
                let e = e?;
                let p = e.path();
                let n = e.file_name().to_string_lossy().into_owned();
                if p.is_dir() && !is_symlink(&p) && valid_id(&n) {
                    out.push(n);
                }
            }
            out.sort();
            return Ok(out);
        }
        if let Some((sub, _)) = md_kind(kind) {
            return self.names_in(sub, ".md");
        }
        if let Some((sub, _)) = yaml_kind(kind) {
            return self.names_in(sub, ".yaml");
        }
        Err(Problem::bad_request(format!("unknown kind: {kind}")).into())
    }

    fn has(&self, kind: &str, name: &str) -> XResult<bool> {
        Ok(self.names_x(kind)?.iter().any(|n| n == name))
    }

    pub fn list_items(&self, kind: &str) -> HubResult<Vec<Item>> {
        self.names(kind)?.iter().map(|n| self.load(kind, n)).collect()
    }

    /// Never fails for bad content (the item carries `issues`); errors only for invalid names/kinds or missing items.
    pub fn load(&self, kind: &str, name: &str) -> HubResult<Item> {
        self.load_x(kind, name).map_err(Exc::into_problem)
    }

    pub fn load_x(&self, kind: &str, name: &str) -> XResult<Item> {
        if !valid_id(name) {
            return Err(Problem::bad_request("invalid name").code("invalid_id").into());
        }
        let r = if kind == "skill" {
            self.load_skill(name)
        } else if let Some((sub, model)) = md_kind(kind) {
            self.load_md(kind, name, &format!("{sub}/{name}.md"), model)
        } else if let Some((sub, model)) = yaml_kind(kind) {
            self.load_yaml(kind, name, &format!("{sub}/{name}.yaml"), model)
        } else {
            return Err(Problem::bad_request(format!("unknown kind: {kind}")).into());
        };
        match r {
            Err(e) if e.is_yaml() || e.problem_of(&["Unprocessable"]).is_some() => {
                let mut it = Item::new(kind, name);
                it.issues = vec![Issue::new("", e.message())];
                Ok(it)
            }
            other => other,
        }
    }

    fn mtime(&self, rel: &str) -> XResult<f64> {
        let p = self.path(rel)?;
        Ok(fs::metadata(p).map(|m| st_mtime(&m)).unwrap_or(0.0))
    }

    fn check_name(item: &mut Item, kind: &str, name: &str, message: &str) {
        let key = key_of(kind);
        if item.get(key).and_then(PyValue::as_str) != Some(name) {
            item.issues.push(Issue::new(key, message));
        }
    }

    fn load_md(&self, kind: &str, name: &str, rel: &str, model: &Model) -> XResult<Item> {
        if !self.exists(rel) {
            return Err(Problem::not_found(format!("no such {kind}: {name}")).into());
        }
        let (meta_raw, body) = split_frontmatter_x(&self.read_x(rel)?)?;
        let mut item = Item::new(kind, name);
        item.body = body;
        item.updated = self.mtime(rel)?;
        match pyd::validate(model, &PyValue::Dict(meta_raw)) {
            Err(e) => {
                item.issues = issues_from(&e, "");
                return Ok(item);
            }
            Ok(m) => item.meta = Some(m),
        }
        Self::check_name(&mut item, kind, name, "does not match the file name");
        Ok(item)
    }

    fn load_yaml(&self, kind: &str, name: &str, rel: &str, model: &Model) -> XResult<Item> {
        if !self.exists(rel) {
            return Err(Problem::not_found(format!("no such {kind}: {name}")).into());
        }
        let mut item = Item::new(kind, name);
        item.updated = self.mtime(rel)?;
        let doc = yaml_load_x(&self.read_x(rel)?, yamlsafe::DEFAULT_MAX_BYTES)?;
        match pyd::validate(model, &doc) {
            Err(e) => {
                item.issues = issues_from(&e, "");
                return Ok(item);
            }
            Ok(m) => item.meta = Some(m),
        }
        Self::check_name(&mut item, kind, name, "does not match the file name");
        Ok(item)
    }

    fn skill_files(&self, name: &str) -> XResult<Files> {
        let base = self.path(&format!("skills/{name}"))?;
        if !base.is_dir() {
            return Err(Problem::not_found(format!("no such skill: {name}")).into());
        }
        let mut files = Files::new();
        let mut total: u64 = 0;
        for (f, parts) in rglob(&base) {
            if is_symlink(&f) || !f.is_file() {
                continue;
            }
            let rel = parts.join("/");
            if rel == "hub.yaml" || check_relpath(&rel).is_err() {
                continue;
            }
            total += fs::metadata(&f)?.len();
            if total > self.max_skill || files.len() >= MAX_SKILL_FILES {
                return Err(unprocessable("skill is too large").code("skill_too_large").into());
            }
            files.insert(rel, fs::read(&f)?);
        }
        Ok(files)
    }

    fn load_skill(&self, name: &str) -> XResult<Item> {
        let files = self.skill_files(name)?;
        let mut updated: Option<f64> = None;
        for r in files.keys() {
            let m = self.mtime(&format!("skills/{name}/{r}"))?;
            updated = Some(updated.map_or(m, |u| if m > u { m } else { u }));
        }
        let mut item = Item::new("skill", name);
        item.updated = updated.unwrap_or(0.0);
        let raw = files.get("SKILL.md").cloned();
        item.files = files;
        let Some(raw) = raw else {
            item.issues.push(Issue::new("SKILL.md", "missing"));
            return Ok(item);
        };
        if raw.len() as u64 > self.max_file {
            item.issues.push(Issue::new("SKILL.md", "too large"));
            return Ok(item);
        }
        let split = match decode_utf8(&raw) {
            Err(msg) => Err(msg),
            Ok(text) => match split_frontmatter_x(&text) {
                Ok(x) => Ok(x),
                Err(e) if e.is_yaml() => Err(e.message()),
                Err(e) => return Err(e),
            },
        };
        let (meta_raw, body) = match split {
            Ok(x) => x,
            Err(msg) => {
                item.issues.push(Issue {
                    path: "SKILL.md".into(),
                    message: msg,
                    fix_hint: Some(FIX_HINT.into()),
                });
                return Ok(item);
            }
        };
        item.body = body;
        match pyd::validate(models::skill_meta(), &PyValue::Dict(meta_raw)) {
            Err(e) => {
                item.issues = issues_from(&e, "");
                return Ok(item);
            }
            Ok(m) => item.meta = Some(m),
        }
        Self::check_name(&mut item, "skill", name, "does not match the directory name");
        let side = format!("skills/{name}/hub.yaml");
        if self.exists(&side) {
            let parsed = load_validate(&self.read_x(&side)?, models::sidecar())?;
            match parsed {
                Ok(s) => item.sidecar = Some(s),
                Err(msg) => item
                    .issues
                    .push(Issue::new("hub.yaml", hub_core::py::text::take_chars(&msg, 200))),
            }
        } else {
            item.sidecar = Some(default_sidecar());
        }
        Ok(item)
    }

    // -- items: write ------------------------------------------------------------------------------------------
    /// `put(kind, name, payload, must_exist=None)` (payload = the request JSON as PyValue).
    pub fn put(&self, kind: &str, name: &str, payload: &PyValue, must_exist: Option<bool>) -> HubResult<Item> {
        self.put_x(kind, name, payload, must_exist).map_err(Exc::into_problem)
    }

    pub fn put_x(&self, kind: &str, name: &str, payload: &PyValue, must_exist: Option<bool>) -> XResult<Item> {
        if !valid_id(name) {
            return Err(Problem::bad_request("invalid name").code("invalid_id").into());
        }
        let exists = self.has(kind, name)?;
        if must_exist == Some(true) && !exists {
            return Err(Problem::not_found(format!("no such {kind}: {name}")).into());
        }
        if must_exist == Some(false) && exists {
            return Err(Problem::conflict(format!("{kind} already exists: {name}"))
                .code("already_exists")
                .into());
        }
        let data = dict_of(payload)?;
        match self.put_inner(kind, name, data, exists) {
            Err(Exc::Validation(e)) => Err(invalid("content failed validation", &e).into()),
            Err(e) => Err(e),
            Ok(()) => self.load_x(kind, name),
        }
    }

    fn put_inner(&self, kind: &str, name: &str, mut data: Vec<(PyValue, PyValue)>, exists: bool) -> XResult<()> {
        if kind == "skill" {
            return self.put_skill(name, data);
        }
        if let Some((sub, model)) = md_kind(kind) {
            let body = dpop(&mut data, "body").map(|v| v.py_str()).unwrap_or_default();
            Self::check_key(&mut data, key_of(kind), name)?;
            let meta = pyd::validate(model, &PyValue::Dict(data))?;
            let text = yamlsafe::join_frontmatter(&dict_items(dump_model(model, &meta, true)), &body);
            return self.write_x(&format!("{sub}/{name}.md"), text.as_bytes());
        }
        if let Some((sub, model)) = yaml_kind(kind) {
            Self::check_key(&mut data, key_of(kind), name)?;
            if kind == "mcp" {
                self.attest_on_put(name, &mut data, exists)?;
            }
            let meta = pyd::validate(model, &PyValue::Dict(data))?;
            let text = yamlsafe::dump(&dump_model(model, &meta, true));
            return self.write_x(&format!("{sub}/{name}.yaml"), text.as_bytes());
        }
        Err(Problem::bad_request(format!("unknown kind: {kind}")).into())
    }

    /// `_attest_mcp(name, data, exists)`: `clean` is bound to a digest of what was scanned. Setting it on a NEW or
    /// previously-unscanned server attests to the current fields; re-sending `clean` for an edited server keeps the OLD
    /// digest, so the edit invalidates it.
    fn attest_on_put(&self, name: &str, data: &mut Vec<(PyValue, PyValue)>, exists: bool) -> XResult<()> {
        if dget(data, "scan_status").and_then(PyValue::as_str) != Some("clean") {
            dpop(data, "scan_digest");
            return Ok(());
        }
        if dget(data, "scan_digest").is_some_and(truthy) {
            return Ok(());
        }
        let prev = if exists { Some(self.load_x("mcp", name)?) } else { None };
        let old = prev
            .as_ref()
            .and_then(|p| p.meta.as_ref())
            .filter(|m| m.get("scan_status").and_then(PyValue::as_str) == Some("clean"))
            .and_then(|m| m.get("scan_digest"))
            .filter(|d| truthy(d))
            .cloned();
        if let Some(old) = old {
            dset(data, "scan_digest", old);
        } else if let Some(a) = &self.attestor {
            let g = |k: &str| dget(data, k).cloned().unwrap_or_default();
            let d = a.digest_x(&g("command"), &g("args"), &g("url"), &g("pinned_ref"), &g("transport"))?;
            dset(data, "scan_digest", PyValue::Str(d));
        } else {
            dpop(data, "scan_digest");
        }
        Ok(())
    }

    /// `attest_mcp(name)`: the operator has scanned this server: bind `clean` to its CURRENT fields.
    pub fn attest_mcp(&self, name: &str) -> HubResult<Item> {
        self.attest_mcp_x(name).map_err(Exc::into_problem)
    }

    pub fn attest_mcp_x(&self, name: &str) -> XResult<Item> {
        let Some(a) = &self.attestor else {
            return Err(Problem::bad_request("no attestation key is configured").into());
        };
        let item = self.load_x("mcp", name)?;
        let Some(meta) = &item.meta else {
            return Err(unprocessable("mcp item is invalid; fix it before attesting").into());
        };
        let mut d = dict_items(dump_model(models::mcp_doc(), meta, true));
        dset(&mut d, "scan_status", PyValue::str("clean"));
        let g = |k: &str| dget(&d, k).cloned().unwrap_or_default();
        let digest = a.digest_x(&g("command"), &g("args"), &g("url"), &g("pinned_ref"), &g("transport"))?;
        dset(&mut d, "scan_digest", PyValue::Str(digest));
        self.put_x("mcp", name, &PyValue::Dict(d), Some(true))
    }

    /// `_check_key(data, key, name)`.
    fn check_key(data: &mut Vec<(PyValue, PyValue)>, key: &str, name: &str) -> XResult<()> {
        if let Some(v) = dget(data, key)
            && *v != PyValue::str(name)
        {
            return Err(unprocessable(format!("`{key}` in the body must match the URL ({name})")).into());
        }
        dset(data, key, PyValue::str(name));
        Ok(())
    }

    /// `(x or {}).items()`.
    fn items_or_empty(v: Option<&PyValue>) -> XResult<Vec<(PyValue, PyValue)>> {
        match v {
            None => Ok(Vec::new()),
            Some(x) if !truthy(x) => Ok(Vec::new()),
            Some(PyValue::Dict(d)) => Ok(d.clone()),
            Some(x) => Err(Exc::other(
                "AttributeError",
                format!("'{}' object has no attribute 'items'", x.type_name()),
            )),
        }
    }

    fn put_skill(&self, name: &str, mut data: Vec<(PyValue, PyValue)>) -> XResult<()> {
        let body = dpop(&mut data, "body").map(|v| v.py_str()).unwrap_or_default();
        let files_text = dpop(&mut data, "files");
        let files_b64 = dpop(&mut data, "files_b64");
        let mut sidecar_in: Vec<(String, PyValue)> = Vec::new();
        for k in ["groups", "tags", "notes", "source", "provenance"] {
            if let Some(v) = dpop(&mut data, k) {
                sidecar_in.push((k.to_string(), v));
            }
        }
        Self::check_key(&mut data, "name", name)?;
        let meta = pyd::validate(models::skill_meta(), &PyValue::Dict(data))?;
        let side_rel = format!("skills/{name}/hub.yaml");
        let mut existing_side = default_sidecar();
        if self.exists(&side_rel)
            && let Ok(s) = load_validate(&self.read_x(&side_rel)?, models::sidecar())?
        {
            existing_side = s;
        }
        let mut merged = dict_items(existing_side);
        for (k, v) in sidecar_in {
            dset(&mut merged, &k, v);
        }
        let side = pyd::validate(models::sidecar(), &PyValue::Dict(merged))?;
        let mut extra: Option<Files> = None;
        if files_text.is_some() || files_b64.is_some() {
            let mut ex = Files::new();
            for (rel, txt) in Self::items_or_empty(files_text.as_ref())? {
                let blob = txt.py_str().into_bytes();
                ex.insert(Self::extra_path(&rel.py_str())?, blob);
            }
            for (rel, b64) in Self::items_or_empty(files_b64.as_ref())? {
                let rel = rel.py_str();
                let Some(blob) = b64decode_strict(&b64.py_str()) else {
                    return Err(unprocessable(format!(
                        "invalid base64 for {}",
                        hub_core::py::text::take_chars(&rel, 60)
                    ))
                    .into());
                };
                ex.insert(Self::extra_path(&rel)?, blob);
            }
            let total: usize = ex.values().map(Vec::len).sum();
            if ex.len() > MAX_SKILL_FILES || total as u64 > self.max_skill {
                return Err(unprocessable("skill is too large").code("skill_too_large").into());
            }
            extra = Some(ex);
        }
        let md = yamlsafe::join_frontmatter(&dict_items(dump_model(models::skill_meta(), &meta, true)), &body);
        self.write_x(&format!("skills/{name}/SKILL.md"), md.as_bytes())?;
        self.write_x(
            &side_rel,
            yamlsafe::dump(&dump_model(models::sidecar(), &side, false)).as_bytes(),
        )?;
        if let Some(extra) = extra {
            for (rel, blob) in &extra {
                self.write_x(&format!("skills/{name}/{rel}"), blob)?;
            }
            let base = self.path(&format!("skills/{name}"))?;
            for (f, parts) in rglob(&base).into_iter().rev() {
                let rel = parts.join("/");
                if f.is_file()
                    && !is_symlink(&f)
                    && !extra.contains_key(&rel)
                    && !SKILL_RESERVED.contains(&rel.as_str())
                {
                    fs::remove_file(&f)?;
                } else if f.is_dir() && fs::read_dir(&f)?.next().is_none() {
                    fs::remove_dir(&f)?;
                }
            }
        }
        Ok(())
    }

    /// `_extra_path(rel)`: a safe, non-reserved relative path for an extra skill file.
    pub fn extra_path(rel: &str) -> XResult<String> {
        if let Err(e) = check_relpath(rel) {
            return Err(unprocessable(format!(
                "bad file path {}: {e}",
                hub_core::py::text::repr(hub_core::py::text::take_chars(rel, 60))
            ))
            .into());
        }
        if SKILL_RESERVED.contains(&rel) || rel.split('/').next() == Some(".git") {
            return Err(unprocessable(format!("{rel} is reserved")).into());
        }
        Ok(rel.to_string())
    }

    /// `set_sidecar(name, side)`: `side` is a Sidecar dict (written as `model_dump(mode="json")`, not re-validated).
    pub fn set_sidecar(&self, name: &str, side: &PyValue) -> HubResult<()> {
        self.set_sidecar_x(name, side).map_err(Exc::into_problem)
    }

    pub fn set_sidecar_x(&self, name: &str, side: &PyValue) -> XResult<()> {
        if !self.has("skill", name)? {
            return Err(Problem::not_found(format!("no such skill: {name}")).into());
        }
        self.write_x(
            &format!("skills/{name}/hub.yaml"),
            yamlsafe::dump(&dump_model(models::sidecar(), side, false)).as_bytes(),
        )
    }

    /// Import path: SKILL.md and extras verbatim (as the client had them), plus a sidecar.
    pub fn write_skill_files(&self, name: &str, files: &Files, sidecar: &PyValue) -> HubResult<()> {
        self.write_skill_files_x(name, files, sidecar)
            .map_err(Exc::into_problem)
    }

    pub fn write_skill_files_x(&self, name: &str, files: &Files, sidecar: &PyValue) -> XResult<()> {
        for (rel, blob) in files {
            if rel != "SKILL.md" {
                Self::extra_path(rel)?;
            }
            self.write_x(&format!("skills/{name}/{rel}"), blob)?;
        }
        self.write_x(
            &format!("skills/{name}/hub.yaml"),
            yamlsafe::dump(&dump_model(models::sidecar(), sidecar, false)).as_bytes(),
        )
    }

    /// Everything `write_skill_files` would refuse, checked BEFORE anything is changed.
    pub fn validate_skill_files(&self, files: &Files) -> HubResult<()> {
        self.validate_skill_files_x(files).map_err(Exc::into_problem)
    }

    pub fn validate_skill_files_x(&self, files: &Files) -> XResult<()> {
        if !files.contains_key("SKILL.md") {
            return Err(unprocessable("skill has no SKILL.md").code("skill_invalid").into());
        }
        if files.len() > MAX_SKILL_FILES {
            return Err(unprocessable(format!("skill has more than {MAX_SKILL_FILES} files"))
                .code("skill_too_large")
                .into());
        }
        let total: u64 = files.values().map(|v| v.len() as u64).sum();
        if total > self.max_skill || files.values().any(|v| v.len() as u64 > self.max_skill) {
            return Err(unprocessable(format!("skill is larger than {} bytes", self.max_skill))
                .code("skill_too_large")
                .into());
        }
        for rel in files.keys() {
            if rel != "SKILL.md" {
                Self::extra_path(rel)?;
            }
        }
        for rel in files.keys() {
            let parts: Vec<&str> = rel.split('/').collect();
            if (1..parts.len()).any(|i| files.contains_key(&parts[..i].join("/"))) {
                return Err(unprocessable(format!("{rel} is below a path that is also a file"))
                    .code("skill_invalid")
                    .into());
            }
        }
        Ok(())
    }

    /// Replace an existing skill: the old directory is moved aside and restored if writing the new one fails.
    pub fn swap_skill_files(&self, name: &str, files: &Files, sidecar: &PyValue) -> HubResult<()> {
        self.swap_skill_files_x(name, files, sidecar).map_err(Exc::into_problem)
    }

    pub fn swap_skill_files_x(&self, name: &str, files: &Files, sidecar: &PyValue) -> XResult<()> {
        self.validate_skill_files_x(files)?;
        let d = self.path(&format!("skills/{name}"))?;
        let aside = d.with_file_name(format!(".replace-{name}-{}", token_hex(4)));
        fs::rename(&d, &aside)?;
        if let Err(e) = self.write_skill_files_x(name, files, sidecar) {
            let _ = fs::remove_dir_all(&d);
            fs::rename(&aside, &d)?;
            return Err(e);
        }
        let _ = fs::remove_dir_all(&aside);
        Ok(())
    }

    /// Delete a skill's files WITHOUT touching collection/profile references (used when replacing on import).
    pub fn remove_skill_dir(&self, name: &str) -> HubResult<()> {
        if self.names("skill")?.iter().any(|n| n == name) {
            fs::remove_dir_all(self.path(&format!("skills/{name}"))?)?;
        }
        Ok(())
    }

    pub fn delete(&self, kind: &str, name: &str) -> HubResult<()> {
        self.delete_x(kind, name).map_err(Exc::into_problem)
    }

    pub fn delete_x(&self, kind: &str, name: &str) -> XResult<()> {
        if !self.has(kind, name)? {
            return Err(Problem::not_found(format!("no such {kind}: {name}")).into());
        }
        if kind == "skill" {
            fs::remove_dir_all(self.path(&format!("skills/{name}"))?)?;
        } else {
            let (sub, ext) = match (md_kind(kind), yaml_kind(kind)) {
                (Some((s, _)), _) => (s, ".md"),
                (None, Some((s, _))) => (s, ".yaml"),
                (None, None) => return Err(Problem::bad_request(format!("unknown kind: {kind}")).into()),
            };
            fs::remove_file(self.path(&format!("{sub}/{name}{ext}"))?)?;
        }
        self.drop_refs(kind, name)
    }

    fn drop_refs(&self, kind: &str, name: &str) -> XResult<()> {
        for mut coll in self.list_collections_x()? {
            let before = coll.members.len();
            coll.members.retain(|m| !(m.kind == kind && m.name == name));
            if coll.members.len() != before {
                let mut payload = dict_items(collection_value(&coll));
                dpop(&mut payload, "id");
                self.put_collection_x(&coll.id, &PyValue::Dict(payload))?;
            }
        }
        let pl = plural(kind).unwrap_or_default();
        for cid in self.names_of_x("profiles")? {
            let mut prof = self.get_profile_x(&cid)?;
            let mut changed = false;
            for section in [&mut prof.enable, &mut prof.disable] {
                let lst = selection_list(section, pl);
                if lst.iter().any(|x| x == name) {
                    lst.retain(|x| x != name);
                    changed = true;
                }
            }
            if changed {
                self.put_profile_x(&cid, &profile_value(&prof))?;
            }
        }
        Ok(())
    }

    pub fn duplicate_skill(&self, name: &str, new_name: &str) -> HubResult<Item> {
        self.duplicate_skill_x(name, new_name).map_err(Exc::into_problem)
    }

    pub fn duplicate_skill_x(&self, name: &str, new_name: &str) -> XResult<Item> {
        if !valid_id(new_name) {
            return Err(Problem::bad_request("invalid new name").code("invalid_id").into());
        }
        if self.has("skill", new_name)? {
            return Err(Problem::conflict("target already exists").code("already_exists").into());
        }
        let src = self.load_x("skill", name)?;
        if src.meta.is_none() {
            return Err(unprocessable("source skill is invalid").into());
        }
        let mut files = src.files.clone();
        let raw = files.get("SKILL.md").cloned().unwrap_or_default();
        let text = decode_utf8(&raw).map_err(|m| Exc::other("UnicodeDecodeError", m))?;
        let (mut meta_raw, body) = split_frontmatter_x(&text)?;
        dset(&mut meta_raw, "name", PyValue::str(new_name));
        files.insert(
            "SKILL.md".into(),
            yamlsafe::join_frontmatter(&meta_raw, &body).into_bytes(),
        );
        let mut side = dict_items(src.sidecar.unwrap_or_else(default_sidecar));
        dset(&mut side, "source", PyValue::Str(format!("duplicate:{name}")));
        self.write_skill_files_x(new_name, &files, &PyValue::Dict(side))?;
        self.load_x("skill", new_name)
    }

    // -- collections / clients / profiles ----------------------------------------------------------------------
    /// ids of `<sub>/<id>.yaml` (valid ids only, symlinks skipped), sorted
    pub fn names_of(&self, sub: &str) -> HubResult<Vec<String>> {
        self.names_of_x(sub).map_err(Exc::into_problem)
    }

    pub fn names_of_x(&self, sub: &str) -> XResult<Vec<String>> {
        self.names_in(sub, ".yaml")
    }

    /// valid collections sorted by (order, id)
    pub fn list_collections(&self) -> HubResult<Vec<CollectionDoc>> {
        self.list_collections_x().map_err(Exc::into_problem)
    }

    pub fn list_collections_x(&self) -> XResult<Vec<CollectionDoc>> {
        let mut out = Vec::new();
        for n in self.names_of_x("collections")? {
            match self.get_collection_x(&n) {
                Ok(c) => out.push(c),
                Err(e) if e.is_yaml() || e.problem_of(&["Unprocessable"]).is_some() => {}
                Err(e) => return Err(e),
            }
        }
        out.sort_by(|a, b| (a.order, &a.id).cmp(&(b.order, &b.id)));
        Ok(out)
    }

    pub fn get_collection(&self, cid: &str) -> HubResult<CollectionDoc> {
        self.get_collection_x(cid).map_err(Exc::into_problem)
    }

    pub fn get_collection_x(&self, cid: &str) -> XResult<CollectionDoc> {
        let v = self.get_doc("collections", cid, models::collection_doc(), "collection")?;
        Ok(CollectionDoc::from_validated(&v))
    }

    pub fn put_collection(&self, cid: &str, payload: &PyValue) -> HubResult<CollectionDoc> {
        self.put_collection_x(cid, payload).map_err(Exc::into_problem)
    }

    pub fn put_collection_x(&self, cid: &str, payload: &PyValue) -> XResult<CollectionDoc> {
        let v = self.put_doc("collections", cid, payload, models::collection_doc())?;
        Ok(CollectionDoc::from_validated(&v))
    }

    pub fn delete_collection(&self, cid: &str) -> HubResult<()> {
        self.delete_collection_x(cid).map_err(Exc::into_problem)
    }

    pub fn delete_collection_x(&self, cid: &str) -> XResult<()> {
        if !self.names_of_x("collections")?.iter().any(|n| n == cid) {
            return Err(Problem::not_found(format!("no such collection: {cid}")).into());
        }
        fs::remove_file(self.path(&format!("collections/{cid}.yaml"))?)?;
        for pid in self.names_of_x("profiles")? {
            let mut prof = self.get_profile_x(&pid)?;
            if prof.collections.iter().any(|c| c == cid) {
                prof.collections.retain(|c| c != cid);
                self.put_profile_x(&pid, &profile_value(&prof))?;
            }
        }
        Ok(())
    }

    pub fn get_client(&self, cid: &str) -> HubResult<ClientDoc> {
        self.get_client_x(cid).map_err(Exc::into_problem)
    }

    pub fn get_client_x(&self, cid: &str) -> XResult<ClientDoc> {
        let v = self.get_doc("clients", cid, models::client_doc(), "client")?;
        Ok(ClientDoc::from_validated(&v))
    }

    /// payload = `ClientDoc.model_dump(mode="json", exclude_none=True)`
    pub fn put_client(&self, cid: &str, payload: &PyValue) -> HubResult<ClientDoc> {
        self.put_client_x(cid, payload).map_err(Exc::into_problem)
    }

    pub fn put_client_x(&self, cid: &str, payload: &PyValue) -> XResult<ClientDoc> {
        let v = self.put_doc("clients", cid, payload, models::client_doc())?;
        Ok(ClientDoc::from_validated(&v))
    }

    pub fn delete_client(&self, cid: &str) -> HubResult<()> {
        self.delete_client_x(cid).map_err(Exc::into_problem)
    }

    pub fn delete_client_x(&self, cid: &str) -> XResult<()> {
        if !self.names_of_x("clients")?.iter().any(|n| n == cid) {
            return Err(Problem::not_found(format!("no such client: {cid}")).into());
        }
        fs::remove_file(self.path(&format!("clients/{cid}.yaml"))?)?;
        let prof = format!("profiles/{cid}.yaml");
        if self.exists(&prof) {
            fs::remove_file(self.path(&prof)?)?;
        }
        Ok(())
    }

    pub fn get_profile(&self, client: &str) -> HubResult<ProfileDoc> {
        self.get_profile_x(client).map_err(Exc::into_problem)
    }

    pub fn get_profile_x(&self, client: &str) -> XResult<ProfileDoc> {
        if !valid_id(client) {
            return Err(Problem::bad_request("invalid client id").code("invalid_id").into());
        }
        if !self.exists(&format!("profiles/{client}.yaml")) {
            return Ok(ProfileDoc::default());
        }
        let v = self.get_doc("profiles", client, models::profile_doc(), "profile")?;
        Ok(ProfileDoc::from_validated(&v))
    }

    pub fn put_profile(&self, client: &str, payload: &PyValue) -> HubResult<ProfileDoc> {
        self.put_profile_x(client, payload).map_err(Exc::into_problem)
    }

    pub fn put_profile_x(&self, client: &str, payload: &PyValue) -> XResult<ProfileDoc> {
        let v = self.put_doc("profiles", client, payload, models::profile_doc())?;
        Ok(ProfileDoc::from_validated(&v))
    }

    /// `_get_doc(sub, name, model, label)`: the validated document.
    fn get_doc(&self, sub: &str, name: &str, model: &Model, label: &str) -> XResult<PyValue> {
        if !valid_id(name) {
            return Err(Problem::bad_request("invalid id").code("invalid_id").into());
        }
        let rel = format!("{sub}/{name}.yaml");
        if !self.exists(&rel) {
            return Err(Problem::not_found(format!("no such {label}: {name}")).into());
        }
        let text = self.read_x(&rel)?;
        let doc = match yaml_load_x(&text, yamlsafe::DEFAULT_MAX_BYTES) {
            Ok(v) => v,
            Err(e) if e.is_yaml() => {
                return Err(unprocessable(format!("{label} {name} is invalid: {}", e.message())).into());
            }
            Err(e) => return Err(e),
        };
        let doc = if truthy(&doc) { doc } else { PyValue::Dict(Vec::new()) };
        let v = match pyd::validate(model, &doc) {
            Ok(v) => v,
            Err(e) => return Err(invalid(&format!("{label} {name} is invalid"), &e).into()),
        };
        if model.fields.iter().any(|f| f.name == "id") && v.get("id").and_then(PyValue::as_str) != Some(name) {
            return Err(unprocessable(format!("{label} id does not match the file name")).into());
        }
        Ok(v)
    }

    /// `_put_doc(sub, name, payload, model)`: the validated document (written with `exclude_none=True`).
    fn put_doc(&self, sub: &str, name: &str, payload: &PyValue, model: &Model) -> XResult<PyValue> {
        if !valid_id(name) {
            return Err(Problem::bad_request("invalid id").code("invalid_id").into());
        }
        let mut data = dict_of(payload)?;
        if model.fields.iter().any(|f| f.name == "id") {
            if let Some(v) = dget(&data, "id")
                && *v != PyValue::str(name)
            {
                return Err(unprocessable("`id` in the body must match the URL").into());
            }
            dset(&mut data, "id", PyValue::str(name));
        }
        let doc = pyd::validate(model, &PyValue::Dict(data))
            .map_err(|e| Exc::Problem(invalid("content failed validation", &e)))?;
        self.write_x(
            &format!("{sub}/{name}.yaml"),
            yamlsafe::dump(&dump_model(model, &doc, true)).as_bytes(),
        )?;
        Ok(doc)
    }

    // -- endpoints / whole catalog -----------------------------------------------------------------------------
    pub fn endpoints(&self) -> HubResult<IndexMap<String, String>> {
        self.endpoints_x().map_err(Exc::into_problem)
    }

    pub fn endpoints_x(&self) -> XResult<IndexMap<String, String>> {
        if !self.exists("endpoints.yaml") {
            return Ok(IndexMap::new());
        }
        let v = yaml_load_x(&self.read_x("endpoints.yaml")?, yamlsafe::DEFAULT_MAX_BYTES)?;
        let v = if truthy(&v) { v } else { PyValue::Dict(Vec::new()) };
        validate_endpoints(&v).map_err(|m| Exc::other("ValueError", m))
    }

    pub fn put_endpoints(&self, data: &PyValue) -> HubResult<IndexMap<String, String>> {
        let clean = validate_endpoints(data).map_err(unprocessable)?;
        self.write_x("endpoints.yaml", yamlsafe::dump(&endpoints_value(&clean)).as_bytes())
            .map_err(Exc::into_problem)?;
        Ok(clean)
    }

    pub fn catalog(&self) -> HubResult<Catalog> {
        self.catalog_x().map_err(Exc::into_problem)
    }

    pub fn catalog_x(&self) -> XResult<Catalog> {
        let mut cat = Catalog::default();
        for kind in hub_core::ids::KINDS {
            let pl = plural(kind).unwrap_or_default();
            for name in self.names_x(kind)? {
                let item = self.load_x(kind, &name)?;
                for i in &item.issues {
                    cat.problems
                        .push(Issue::new(format!("{pl}/{name}:{}", i.path), i.message.clone()));
                }
                cat.items.insert((kind.to_string(), name), item);
            }
        }
        for n in self.names_of_x("collections")? {
            match self.get_collection_x(&n) {
                Ok(c) => {
                    cat.collections.insert(n, c);
                }
                Err(e) => match e.problem_of(&["Unprocessable", "NotFound"]) {
                    Some(p) => cat
                        .problems
                        .push(Issue::new(format!("collections/{n}"), p.detail.clone())),
                    None => return Err(e),
                },
            }
        }
        for n in self.names_of_x("clients")? {
            match self.get_client_x(&n) {
                Ok(c) => {
                    cat.clients.insert(n, c);
                }
                Err(e) => match e.problem_of(&["Unprocessable", "NotFound"]) {
                    Some(p) => cat.problems.push(Issue::new(format!("clients/{n}"), p.detail.clone())),
                    None => return Err(e),
                },
            }
        }
        for n in self.names_of_x("profiles")? {
            match self.get_profile_x(&n) {
                Ok(p) => {
                    cat.profiles.insert(n, p);
                }
                Err(e) => match e.problem_of(&["Unprocessable"]) {
                    Some(p) => cat.problems.push(Issue::new(format!("profiles/{n}"), p.detail.clone())),
                    None => return Err(e),
                },
            }
        }
        match self.endpoints_x() {
            Ok(e) => cat.endpoints = e,
            Err(e) if e.is_value_error() || e.problem_of(&["Unprocessable"]).is_some() => {
                cat.problems.push(Issue::new("endpoints.yaml", e.message()));
            }
            Err(e) => return Err(e),
        }
        if self.exists("hub.yaml") {
            let r = load_validate(&self.read_x("hub.yaml")?, models::hub_meta())?;
            if let Err(msg) = r {
                cat.problems
                    .push(Issue::new("hub.yaml", hub_core::py::text::take_chars(&msg, 200)));
            }
        }
        Ok(cat)
    }
}

// ---- typed docs <-> Python dicts -----------------------------------------------------------------------------------

/// `CollectionDoc.model_dump(mode="json")` as a PyValue dict.
pub fn collection_value(c: &CollectionDoc) -> PyValue {
    PyValue::from_json(&serde_json::to_value(c).unwrap_or(Value::Null))
}

/// `ProfileDoc.model_dump(mode="json")` as a PyValue dict.
pub fn profile_value(p: &ProfileDoc) -> PyValue {
    PyValue::from_json(&serde_json::to_value(p).unwrap_or(Value::Null))
}

/// `ClientDoc.model_dump(mode="json", exclude_none=...)` as a PyValue dict.
pub fn client_value(c: &ClientDoc, exclude_none: bool) -> PyValue {
    PyValue::from_json(&c.dump(exclude_none))
}

fn endpoints_value(e: &IndexMap<String, String>) -> PyValue {
    PyValue::Dict(
        e.iter()
            .map(|(k, v)| (PyValue::str(k.as_str()), PyValue::str(v.as_str())))
            .collect(),
    )
}

/// `getattr(selection, plural)` as a mutable list.
pub fn selection_list<'a>(s: &'a mut models::Selection, plural: &str) -> &'a mut Vec<String> {
    match plural {
        "skills" => &mut s.skills,
        "agents" => &mut s.agents,
        "mcp" => &mut s.mcp,
        "instructions" => &mut s.instructions,
        "rules" => &mut s.rules,
        _ => &mut s.memory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_errors_read_like_cpython() {
        let cases: [(&[u8], &str); 6] = [
            (
                b"ab\xffcd",
                "'utf-8' codec can't decode byte 0xff in position 2: invalid start byte",
            ),
            (
                b"\xe2\x82",
                "'utf-8' codec can't decode bytes in position 0-1: unexpected end of data",
            ),
            (
                b"x\xe2\x41",
                "'utf-8' codec can't decode byte 0xe2 in position 1: invalid continuation byte",
            ),
            (
                b"\xe2\x82\x41",
                "'utf-8' codec can't decode bytes in position 0-1: invalid continuation byte",
            ),
            (
                b"\xe0\x80\x80",
                "'utf-8' codec can't decode byte 0xe0 in position 0: invalid continuation byte",
            ),
            (
                b"\x80",
                "'utf-8' codec can't decode byte 0x80 in position 0: invalid start byte",
            ),
        ];
        for (b, want) in cases {
            assert_eq!(decode_utf8(b).err().as_deref(), Some(want), "{b:?}");
        }
        assert_eq!(decode_utf8("é".as_bytes()).ok().as_deref(), Some("é"));
    }

    #[test]
    fn strict_base64_matches_python_validate_true() {
        // python3 -c "base64.b64decode(s, validate=True)" for each input (2026-10-28)
        let ok: [(&str, &[u8]); 6] = [
            ("", b""),
            ("AAE=", b"\x00\x01"),
            ("AA==", b"\x00"),
            ("AAEC", b"\x00\x01\x02"),
            ("AAF=", b"\x00\x01"),
            ("AB==", b"\x00"),
        ];
        for (s, want) in ok {
            assert_eq!(b64decode_strict(s).as_deref(), Some(want), "{s}");
        }
        for bad in [
            "AAE", "AAE==", "AA=", "A===", "=AAA", "AA==AA", "AB=C", "AAAAA", "AAAAA===", "AA AA", "AA\nAA", "é",
            "AAEC====", "A", "AB", "ABC", "AA=A", "AAA=AAAA", "Zg==Zg==",
        ] {
            assert_eq!(b64decode_strict(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn universal_newlines_and_truthiness() {
        assert_eq!(universal_newlines("a\r\nb\rc\n"), "a\nb\nc\n");
        assert!(!truthy(&PyValue::Float(0.0)) && truthy(&PyValue::str("x")) && !truthy(&PyValue::Dict(vec![])));
        assert_eq!(py_list(&PyValue::str("ab")).ok().map(|v| v.len()), Some(2));
        assert!(py_list(&PyValue::Int(3)).is_err());
    }

    #[test]
    fn endpoint_names_and_urls() {
        let d = |k: PyValue, v: &str| PyValue::Dict(vec![(k, PyValue::str(v))]);
        assert!(validate_endpoints(&d(PyValue::str("router"), "http://x")).is_ok());
        assert_eq!(
            validate_endpoints(&d(PyValue::str("Bad Key"), "http://x"))
                .err()
                .as_deref(),
            Some("bad endpoint name: 'Bad Key'")
        );
        assert_eq!(
            validate_endpoints(&d(PyValue::Int(5), "http://x")).err().as_deref(),
            Some("bad endpoint name: '5'")
        );
        assert_eq!(
            validate_endpoints(&d(PyValue::str("router"), "file:///etc/passwd"))
                .err()
                .as_deref(),
            Some("endpoint router must be an http(s) URL")
        );
        assert_eq!(
            validate_endpoints(&PyValue::List(vec![])).err().as_deref(),
            Some("endpoints must be a mapping")
        );
        assert!(validate_endpoints(&d(PyValue::str("_x"), "http://x")).is_err());
        assert!(validate_endpoints(&d(PyValue::str("a_1"), "https://x")).is_ok());
    }

    #[test]
    fn rglob_orders_by_path_components() -> std::io::Result<()> {
        let d = tempfile::tempdir()?;
        fs::create_dir_all(d.path().join("a/b"))?;
        fs::write(d.path().join("a.txt"), "x")?;
        fs::write(d.path().join("a/b/c"), "x")?;
        let rels: Vec<String> = rglob(d.path()).into_iter().map(|(_, p)| p.join("/")).collect();
        assert_eq!(rels, ["a", "a/b", "a/b/c", "a.txt"]);
        Ok(())
    }
}
