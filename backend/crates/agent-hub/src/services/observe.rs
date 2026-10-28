//! Port of services/observe.py: the stored observation of what a client REALLY has (`<data>/observed/<client>/
//! {index.json, skills/<key>/...}`, 0700/0600, replaced atomically; an incomplete or empty observation keeps the
//! previous one), annotated against the hub's own skills. Untrusted data: never executed, never rendered as HTML.
//! Every path is checked (`check_relpath`, no control/format/line-separator characters) before it is written; compiled
//! bytecode is left out (never evaluated or imported). Security control S-OBSERVE. Owner: WS-C.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, RwLock, Weak};

use hub_core::common::{fsync_dir, scrub, sha256, token_hex};
use hub_core::ids::{slugify, valid_id};
use hub_core::pathsafe::check_relpath;
use hub_core::py::json::{Opts, dumps};
use hub_core::py::text::take_chars;
use hub_core::{HubResult, Problem};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};

use super::hub::Hub;
use super::skilleval::{
    INDEX_CAP, ReadErr, content_hash, is_bytecode, json_loads, mkdir_private, read_regular, rmtree, valid_key,
};
use crate::adapters::base::{Files, OBSERVED_ORIGINS, Observation};

pub const MAX_SKILLS: usize = 5000;
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_NOTES: usize = 50;
pub const HUB_STATUSES: [&str; 3] = ["in_hub_same", "in_hub_differs", "not_in_hub"];
pub const INDEX: &str = "index.json";

/// What the hub has, for comparison (computed by `Hub::hub_view`, WS-C).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HubView {
    /// content hash -> skill names
    pub by_hash: BTreeMap<String, Vec<String>>,
    pub hash_of: BTreeMap<String, Option<String>>,
    pub enabled: BTreeSet<String>,
}

/// `make_key(path)`: `<slug(last path segment)>-<first 8 hex of sha256(path)>`; always a valid observed-skill key.
pub fn make_key(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or(path);
    let slug = slugify(last);
    let slug = take_chars(&slug, 72);
    let slug = if slug.is_empty() { "skill" } else { slug };
    let digest = sha256(path.as_bytes());
    let key = format!("{slug}-{}", &digest[..8]);
    if valid_key(&key) {
        key
    } else {
        format!("skill-{}", &digest[..8])
    }
}

static BAD_CHAR: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]"));

/// `_bad_text(text)`: contains control/format/line-separator characters (U+202E, U+2028, DEL, ...). A Rust string is
/// always UTF-8 encodable, so Python's lone-surrogate case cannot reach here.
fn bad_text(text: &str) -> bool {
    BAD_CHAR.is_match(text)
}

/// `_display(text, limit)`: control/format characters turned into spaces, secrets scrubbed, cut to `limit`.
fn display(text: &str, limit: usize) -> String {
    let clean = BAD_CHAR.replace_all(text, " ");
    take_chars(&scrub(&clean), limit).to_string()
}

fn hub_name(name: &str) -> String {
    if valid_id(name) {
        name.to_string()
    } else {
        slugify(name)
    }
}

/// `hub_view(client)` seam.
pub type HubViewFn = Arc<dyn Fn(&str) -> HubView + Send + Sync>;

struct ObsTun {
    clock: Option<Arc<dyn Fn() -> f64 + Send + Sync>>,
    max_skills: usize,
    max_bytes: u64,
    hub_view: Option<HubViewFn>,
    /// test seam: creating a file whose path contains this text fails (Python's test monkeypatches os.open)
    fail_write: Option<String>,
}

pub struct ObserveService {
    pub root: PathBuf,
    pub hub: Weak<Hub>,
    pub eval_root: Option<PathBuf>,
    mu: Mutex<()>,
    tun: RwLock<ObsTun>,
}

fn pystr(v: &Value) -> String {
    hub_core::value::PyValue::from_json(v).py_str()
}

impl ObserveService {
    pub fn new(root: &Path, hub: Weak<Hub>, eval_root: Option<&Path>) -> Self {
        Self {
            root: root.to_path_buf(),
            hub,
            eval_root: eval_root.map(Path::to_path_buf),
            mu: Mutex::new(()),
            tun: RwLock::new(ObsTun {
                clock: None,
                max_skills: MAX_SKILLS,
                max_bytes: MAX_BYTES,
                hub_view: None,
                fail_write: None,
            }),
        }
    }

    // -- seams (Python constructor keywords and test monkeypatches) -------------------------------------------------------
    fn tun_mut(&self) -> std::sync::RwLockWriteGuard<'_, ObsTun> {
        self.tun.write().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn tun(&self) -> std::sync::RwLockReadGuard<'_, ObsTun> {
        self.tun.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// `ObserveService(..., max_skills=, max_bytes=)`.
    #[must_use]
    pub fn with_limits(self, max_skills: usize, max_bytes: u64) -> Self {
        {
            let mut t = self.tun_mut();
            t.max_skills = max_skills;
            t.max_bytes = max_bytes;
        }
        self
    }
    pub fn set_clock(&self, c: Option<Arc<dyn Fn() -> f64 + Send + Sync>>) {
        self.tun_mut().clock = c;
    }
    /// `hub_view` callback; None = the hub's `Hub::hub_view` (an empty view without a hub).
    pub fn set_hub_view(&self, f: Option<HubViewFn>) {
        self.tun_mut().hub_view = f;
    }
    #[doc(hidden)]
    pub fn set_write_failpoint(&self, needle: Option<String>) {
        self.tun_mut().fail_write = needle;
    }

    fn clock(&self) -> f64 {
        let c = self.tun().clock.clone();
        c.map_or_else(hub_core::common::now, |c| c())
    }

    fn hub_view(&self, client: &str) -> HubView {
        let f = self.tun().hub_view.clone();
        if let Some(f) = f {
            return f(client);
        }
        self.hub
            .upgrade()
            .and_then(|h| h.hub_view(client).ok())
            .unwrap_or_default()
    }

    // -- writing ---------------------------------------------------------------------------------------------------------
    /// Replace <root>/<client>/ with this observation; returns the index document. An OSError while writing becomes
    /// Hub.observe's 503 `observe_store_failed` (the previous observation is unchanged).
    pub fn store(&self, client: &str, obs: &Observation) -> HubResult<Value> {
        if !valid_id(client) {
            return Err(Problem::bad_request("invalid client id").code("invalid_id"));
        }
        let mut notes: Vec<String> = obs.notes.iter().take(MAX_NOTES).map(|n| display(n, 300)).collect();
        let mut complete = obs.complete;
        if obs.skills.is_empty() && !complete {
            let tail = if notes.is_empty() {
                String::new()
            } else {
                format!(": {}", notes.iter().take(3).cloned().collect::<Vec<_>>().join("; "))
            };
            return Err(Problem::unavailable(format!(
                "the client could not be observed completely and nothing was found; the previous observation is kept{tail}"
            ))
            .code("observe_failed"));
        }
        let (max_skills, max_bytes) = {
            let t = self.tun();
            (t.max_skills, t.max_bytes)
        };
        let mut entries: Vec<Value> = Vec::new();
        let mut payload: IndexMap<String, Files> = IndexMap::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut total: u64 = 0;
        let mut bytecode = 0usize;
        for s in &obs.skills {
            if entries.len() >= max_skills {
                complete = false;
                notes.push(format!("stopped at {max_skills} skills; the rest were not stored"));
                break;
            }
            let path = match (bad_text(&s.path), check_relpath(&s.path)) {
                (false, Ok(p)) => p.to_string(),
                _ => {
                    notes.push("skipped a skill whose path is unsafe".into());
                    continue;
                }
            };
            let mut files: Files = IndexMap::new();
            for (rel, data) in &s.files {
                if is_bytecode(rel) {
                    bytecode += 1;
                    continue;
                }
                match (bad_text(rel), check_relpath(rel)) {
                    (false, Ok(r)) => {
                        files.insert(r.to_string(), data.clone());
                    }
                    _ => notes.push(format!("skipped an unsafe file path inside {path}")),
                }
            }
            let clash: Vec<String> = files
                .keys()
                .filter(|r| files.keys().any(|o| o.starts_with(&format!("{r}/"))))
                .cloned()
                .collect();
            for rel in clash {
                files.shift_remove(&rel); // a path cannot be both a file and a directory
                notes.push(format!("skipped {path}/{rel}: it is also used as a directory"));
            }
            if !files.contains_key("SKILL.md") {
                notes.push(format!("skipped {path}: no SKILL.md"));
                continue;
            }
            let size: u64 = files.values().map(|b| b.len() as u64).sum();
            if total + size > max_bytes {
                complete = false;
                notes.push(format!(
                    "skipped {path}: the {} MiB limit for one observation is reached",
                    max_bytes / (1024 * 1024)
                ));
                continue;
            }
            let key = make_key(&path);
            if seen.contains(&key) {
                notes.push(format!("skipped a second skill at the same path {path}"));
                continue;
            }
            seen.insert(key.clone());
            total += size;
            let origin = if OBSERVED_ORIGINS.contains(&s.origin.as_str()) {
                s.origin.clone()
            } else {
                "client".into()
            };
            entries.push(json!({"key": key, "path": path, "name": display(&s.name, 200),
                                "description": display(&s.description, 500), "origin": origin,
                                "content_hash": content_hash(&files), "files": files.len(), "bytes": size}));
            payload.insert(key, files);
        }
        if bytecode > 0 {
            notes.push(format!(
                "left out {bytecode} compiled Python bytecode file(s) (__pycache__, .pyc): never evaluated or imported"
            ));
        }
        notes.truncate(MAX_NOTES + 5);
        let index = json!({"client": client, "observed_at": self.clock(), "source": display(&obs.source, 300),
                           "complete": complete, "notes": notes, "skills": entries});
        let _g = self.mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.replace(client, &index, &payload).map_err(|e| {
            Problem::unavailable(format!(
                "could not store the observation ({}); the previous one is unchanged",
                hub_core::py::os::error_name(&e)
            ))
            .code("observe_store_failed")
        })?;
        Ok(index)
    }

    fn create_file(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        if let Some(needle) = self.tun().fail_write.clone()
            && path.to_string_lossy().contains(&needle)
        {
            return Err(io::Error::from_raw_os_error(nix::libc::ENOSPC));
        }
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path)?;
        f.write_all(data)?;
        f.flush()?;
        f.sync_all()
    }

    fn replace(&self, client: &str, index: &Value, payload: &IndexMap<String, Files>) -> io::Result<()> {
        mkdir_private(&self.root)?;
        // leftovers of a crashed observation (we hold the lock)
        let pattern = format!(".*-{client}-*");
        if let Ok(rd) = fs::read_dir(&self.root) {
            for e in rd.flatten() {
                let Some(name) = e.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let p = e.path();
                if hub_core::py::fnmatch::fnmatchcase(&name, &pattern)
                    && (name.starts_with(".tmp-") || name.starts_with(".old-"))
                    && p.is_dir()
                    && !fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink())
                {
                    rmtree(&p);
                }
            }
        }
        let token = token_hex(4);
        let tmp = self.root.join(format!(".tmp-{client}-{token}"));
        let old = self.root.join(format!(".old-{client}-{token}"));
        let dest = self.root.join(client);
        let res = (|| -> io::Result<()> {
            fs::DirBuilder::new().mode(0o700).create(&tmp)?;
            for (key, files) in payload {
                for (rel, data) in files {
                    let target = tmp.join("skills").join(key).join(rel);
                    if let Some(parent) = target.parent() {
                        mkdir_private(parent)?;
                    }
                    self.create_file(&target, data)?;
                }
            }
            match fs::DirBuilder::new().mode(0o700).create(tmp.join("skills")) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            let text = dumps(
                index,
                &Opts {
                    indent: Some(1),
                    sort_keys: true,
                    ..Opts::default()
                },
            );
            self.create_file(&tmp.join(INDEX), text.as_bytes())?;
            let had_old = fs::symlink_metadata(&dest).is_ok();
            if had_old {
                fs::rename(&dest, &old)?;
            }
            if let Err(e) = fs::rename(&tmp, &dest) {
                if had_old {
                    let _ = fs::rename(&old, &dest);
                }
                return Err(e);
            }
            fsync_dir(&self.root);
            if had_old {
                rmtree(&old);
            }
            Ok(())
        })();
        if res.is_err() {
            rmtree(&tmp);
        }
        res
    }

    // -- reading ---------------------------------------------------------------------------------------------------------
    pub fn index(&self, client: &str) -> Option<Value> {
        if !valid_id(client) {
            return None;
        }
        let got = read_regular(&self.root.join(client).join(INDEX), INDEX_CAP).ok()??;
        let doc = json_loads(&got.0)?;
        (doc.is_object() && doc.get("skills").is_some_and(Value::is_array)).then_some(doc)
    }

    pub fn entry(&self, client: &str, key: &str) -> Option<Value> {
        if !valid_key(key) {
            return None;
        }
        let idx = self.index(client)?;
        idx.get("skills")
            .and_then(Value::as_array)?
            .iter()
            .find(|s| s.is_object() && s.get("key").and_then(Value::as_str) == Some(key))
            .cloned()
    }

    /// The stored files of one observed skill (regular files only, never through a symlink; no compiled bytecode, the
    /// same rule as evaluation, so an import delivers exactly what was evaluated).
    pub fn files(&self, client: &str, key: &str) -> HubResult<Files> {
        if self.entry(client, key).is_none() {
            return Err(Problem::not_found(format!("no such observed skill: {client}:{key}")));
        }
        let base = self.root.join(client).join("skills").join(key);
        let max_bytes = self.tun().max_bytes;
        let mut out: Files = IndexMap::new();
        let mut total: u64 = 0;
        let mut stack = vec![base.clone()];
        while let Some(d) = stack.pop() {
            let (dirs, files) = list_names(&d);
            for n in files {
                let full = d.join(&n);
                let Some(rel) = full.strip_prefix(&base).ok().and_then(Path::to_str).map(str::to_string) else {
                    continue;
                };
                if is_bytecode(&rel) || check_relpath(&rel).is_err() {
                    continue;
                }
                match read_regular(&full, max_bytes.saturating_sub(total)) {
                    Ok(Some((data, _))) => {
                        total += data.len() as u64;
                        out.insert(rel, data);
                    }
                    Ok(None) | Err(ReadErr::TooBig | ReadErr::Os(_)) => {}
                }
            }
            for n in dirs.into_iter().rev() {
                let p = d.join(&n);
                if !fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
                    stack.push(p);
                }
            }
        }
        if !out.contains_key("SKILL.md") {
            return Err(Problem::not_found(format!(
                "observed skill {client}:{key} has no SKILL.md"
            )));
        }
        Ok(out)
    }

    /// The stored index with, per skill, the comparison with the hub computed NOW: `hub_status`, `hub_name`,
    /// `enabled_for_client`.
    pub fn annotated(&self, client: &str) -> Option<Value> {
        let idx = self.index(client)?;
        let view = self.hub_view(client);
        let mut items = Vec::new();
        for s in idx
            .get("skills")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
        {
            let Some(m) = s.as_object() else { continue };
            let name = m.get("name").filter(|v| truthy(v)).map(pystr).unwrap_or_default();
            let hn = hub_name(&name);
            let ch = m.get("content_hash").map_or_else(|| "None".to_string(), pystr);
            let names = view.by_hash.get(&ch).cloned().unwrap_or_default();
            let (status, hub_name): (&str, Option<String>) = if !names.is_empty() {
                let pick = if names.contains(&hn) {
                    hn.clone()
                } else {
                    names[0].clone()
                };
                ("in_hub_same", Some(pick))
            } else if !hn.is_empty() && view.hash_of.contains_key(&hn) {
                ("in_hub_differs", Some(hn.clone()))
            } else {
                ("not_in_hub", None)
            };
            let enabled = hub_name
                .as_ref()
                .is_some_and(|n| !n.is_empty() && view.enabled.contains(n));
            let mut item = m.clone();
            item.insert("hub_status".into(), Value::String(status.into()));
            item.insert("hub_name".into(), hub_name.map_or(Value::Null, Value::String));
            item.insert("enabled_for_client".into(), Value::Bool(enabled));
            items.push(Value::Object(item));
        }
        let mut out: Map<String, Value> = idx
            .as_object()
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| *k != "skills")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        out.insert("items".into(), Value::Array(items));
        Some(Value::Object(out))
    }

    /// `{"by_origin": {...}, "by_hub_status": {...}}`
    pub fn count(items: &[Value]) -> Value {
        let mut by_origin: IndexMap<String, i64> = IndexMap::new();
        for i in items {
            let o = i.get("origin").map_or_else(|| "None".to_string(), pystr);
            *by_origin.entry(o).or_insert(0) += 1;
        }
        let mut by_status = Map::new();
        for k in HUB_STATUSES {
            let n = items
                .iter()
                .filter(|i| i.get("hub_status").and_then(Value::as_str) == Some(k))
                .count();
            by_status.insert(k.into(), json!(n));
        }
        let by_origin: Map<String, Value> = by_origin.into_iter().map(|(k, v)| (k, json!(v))).collect();
        json!({"by_origin": by_origin, "by_hub_status": by_status})
    }

    /// Short summary for drift and the client views (None before the first observation).
    pub fn summary(&self, client: &str) -> Option<Value> {
        let doc = self.annotated(client)?;
        let items = doc.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        let c = Self::count(&items);
        let bs = &c["by_hub_status"];
        Some(json!({
            "observed_at": doc.get("observed_at").cloned().unwrap_or(Value::Null),
            "total": items.len(),
            "complete": truthy(doc.get("complete").unwrap_or(&Value::Null)),
            "not_in_hub": bs["not_in_hub"], "in_hub_differs": bs["in_hub_differs"], "in_hub_same": bs["in_hub_same"],
            "by_origin": c["by_origin"],
        }))
    }

    /// Drop a client's observation and its stored evaluation runs (the client was deleted).
    pub fn forget(&self, client: &str) {
        if !valid_id(client) {
            return;
        }
        let _g = self.mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut dirs = vec![self.root.join(client)];
        if let Some(e) = &self.eval_root {
            dirs.push(e.join("@observed").join(client));
        }
        for d in dirs {
            rmtree(&d);
        }
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// One directory's (dirnames, filenames), sorted; a symlink to a directory counts as a directory (os.walk).
fn list_names(d: &Path) -> (Vec<String>, Vec<String>) {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    if let Ok(rd) = fs::read_dir(d) {
        for e in rd.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if fs::metadata(e.path()).is_ok_and(|m| m.is_dir()) {
                dirs.push(name);
            } else {
                files.push(name);
            }
        }
    }
    dirs.sort();
    files.sort();
    (dirs, files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_display() {
        let k = make_key("devops/My Skill!");
        assert!(valid_key(&k) && k.starts_with("my-skill-"));
        assert_eq!(k, make_key("devops/My Skill!"));
        assert_ne!(k, make_key("other/My Skill!"));
        assert!(make_key("!!!").starts_with("skill-"));
        assert!(valid_key(&make_key(&"x".repeat(400))));
        assert_eq!(display("a\u{202e}b\nc", 10), "a b c");
        assert!(bad_text("x\u{2028}") && bad_text("\u{7f}") && !bad_text("ok ü"));
    }
}
