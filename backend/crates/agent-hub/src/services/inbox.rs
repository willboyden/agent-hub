//! Port of services/inbox.py: the curated memory inbox (`<content>/inbox/<client>/<id>.md`, InboxMeta frontmatter,
//! never committed blindly), per-client rate limit and cap, promote to memory/ or reject. Agents propose memories into
//! a per-client inbox; only an admin promotes one into content/memory/. Inbox files are never compiled into any client.
//! Security control S-INBOX. Owner: WS-B1.

use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hub_core::common::{new_id, now};
use hub_core::ids::{slugify, valid_id};
use hub_core::models::inbox_meta;
use hub_core::py::text::{strip, take_chars};
use hub_core::value::PyValue;
use hub_core::{HubResult, Problem, pyd, yamlsafe};
use serde_json::{Value, json};

use super::content::{ContentStore, Exc, XResult, dict_items, dump_model, invalid, split_frontmatter_x, truthy};

pub const MAX_BODY: usize = 16 * 1024;
pub const MAX_TITLE: usize = 200;

pub struct Inbox {
    pub work: Arc<ContentStore>,
    rate: AtomicI64,
    max_per_client: AtomicI64,
    /// `_hits`: per client, the monotonic times of the adds in the last minute.
    hits: Mutex<HashMap<String, Vec<Instant>>>,
}

fn is_symlink(p: &std::path::Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

impl Inbox {
    pub fn new(work: Arc<ContentStore>, rate_per_min: i64, max_per_client: i64) -> Self {
        Self {
            work,
            rate: AtomicI64::new(rate_per_min),
            max_per_client: AtomicI64::new(max_per_client),
            hits: Mutex::new(HashMap::new()),
        }
    }
    /// PUT /settings `inbox_rate_per_min` (Python: `self.inbox.rate = v`).
    pub fn set_rate(&self, v: i64) {
        self.rate.store(v, Ordering::SeqCst);
    }
    /// PUT /settings `inbox_max_per_client`.
    pub fn set_max_per_client(&self, v: i64) {
        self.max_per_client.store(v, Ordering::SeqCst);
    }
    pub fn rate(&self) -> i64 {
        self.rate.load(Ordering::SeqCst)
    }
    pub fn max_per_client(&self) -> i64 {
        self.max_per_client.load(Ordering::SeqCst)
    }

    /// `_throttle(client)`: at most `rate` adds per client per minute (429 `rate_limited`).
    fn throttle(&self, client: &str) -> HubResult<()> {
        let mut map = self.hits.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let cutoff = Instant::now().checked_sub(Duration::from_secs(60));
        let mut hits: Vec<Instant> = map
            .get(client)
            .map(|v| v.iter().copied().filter(|t| cutoff.is_none_or(|c| *t > c)).collect())
            .unwrap_or_default();
        if i64::try_from(hits.len()).unwrap_or(i64::MAX) >= self.rate() {
            map.insert(client.to_string(), hits);
            return Err(Problem::new(
                429,
                "rate_limited",
                "inbox rate limit reached; retry later",
            ));
        }
        hits.push(Instant::now());
        map.insert(client.to_string(), hits);
        Ok(())
    }

    /// `_ids(client)`: the item ids of one client's inbox. A client that is not an id lists nothing (Python joined it
    /// onto the inbox path, so `?client=/abs/dir` or `../..` listed `*.md` names anywhere: deviation D-INBOX-CLIENT).
    fn ids(&self, client: &str) -> XResult<Vec<String>> {
        if !valid_id(client) {
            return Ok(Vec::new());
        }
        let d = self.work.root.join("inbox").join(client);
        if !d.is_dir() || is_symlink(&d) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for e in fs::read_dir(&d)? {
            let e = e?;
            let p = e.path();
            let n = e.file_name().to_string_lossy().into_owned();
            if let Some(stem) = n.strip_suffix(".md")
                && p.is_file()
                && !is_symlink(&p)
                && valid_id(stem)
            {
                out.push(stem.to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn add(&self, client: &str, title: &str, body: &str, mtype: &str) -> HubResult<Value> {
        self.add_x(client, title, body, mtype).map_err(Exc::into_problem)
    }

    pub fn add_x(&self, client: &str, title: &str, body: &str, mtype: &str) -> XResult<Value> {
        if !self.work.names_of_x("clients")?.iter().any(|c| c == client) {
            return Err(Problem::not_found(format!("no such client: {client}")).into());
        }
        if strip(title).is_empty() || title.chars().count() > MAX_TITLE {
            return Err(Problem::bad_request(format!("title must be 1..{MAX_TITLE} characters")).into());
        }
        if strip(body).is_empty() || body.len() > MAX_BODY {
            return Err(Problem::bad_request(format!("body must be 1..{MAX_BODY} bytes")).into());
        }
        self.throttle(client)?;
        if i64::try_from(self.ids(client)?.len()).unwrap_or(i64::MAX) >= self.max_per_client() {
            return Err(Problem::new(
                429,
                "inbox_full",
                "inbox is full; an admin must promote or reject items",
            )
            .into());
        }
        let slug = slugify(title);
        let head = take_chars(&slug, 40);
        let iid = format!(
            "{}-{}",
            if head.is_empty() { "note" } else { head },
            take_chars(&new_id(""), 6)
        );
        let raw = PyValue::Dict(vec![
            (PyValue::str("id"), PyValue::str(iid.as_str())),
            (PyValue::str("type"), PyValue::str(mtype)),
            (PyValue::str("title"), PyValue::str(strip(title))),
            (PyValue::str("client"), PyValue::str(client)),
            (PyValue::str("created"), PyValue::Float(now())),
        ]);
        let meta = match pyd::validate(inbox_meta(), &raw) {
            Ok(m) => m,
            Err(e) => return Err(invalid("invalid inbox item", &e).into()),
        };
        let text = yamlsafe::join_frontmatter(&dict_items(dump_model(inbox_meta(), &meta, false)), body);
        self.work
            .write_x(&format!("inbox/{client}/{iid}.md"), text.as_bytes())?;
        Ok(json!({"id": iid, "client": client, "type": mtype, "title": meta.get("title").map(PyValue::py_str)}))
    }

    /// `_read(client, iid)`: (validated InboxMeta, body).
    fn read(&self, client: &str, iid: &str) -> XResult<(PyValue, String)> {
        let text = self.work.read_x(&format!("inbox/{client}/{iid}.md"))?;
        let (raw, body) = split_frontmatter_x(&text)?;
        match pyd::validate(inbox_meta(), &PyValue::Dict(raw)) {
            Ok(m) => Ok((m, body)),
            Err(e) => Err(invalid("inbox item is invalid", &e).into()),
        }
    }

    pub fn list(&self, client: Option<&str>) -> HubResult<Vec<Value>> {
        self.list_x(client).map_err(Exc::into_problem)
    }

    pub fn list_x(&self, client: Option<&str>) -> XResult<Vec<Value>> {
        let client = client.filter(|c| !c.is_empty());
        let mut clients: Vec<String> = match client {
            Some(c) => vec![c.to_string()],
            None => self.work.names_of_x("clients")?,
        };
        let base = self.work.root.join("inbox");
        if base.is_dir() && client.is_none() {
            for e in fs::read_dir(&base)? {
                let e = e?;
                let n = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() && valid_id(&n) {
                    clients.push(n);
                }
            }
            clients.sort();
            clients.dedup();
        }
        let mut out: Vec<(f64, String, Value)> = Vec::new();
        for c in &clients {
            for iid in self.ids(c)? {
                match self.read(c, &iid) {
                    Ok((meta, body)) => {
                        let created = match meta.get("created") {
                            Some(PyValue::Float(f)) => *f,
                            _ => 0.0,
                        };
                        out.push((
                            created,
                            iid.clone(),
                            json!({"id": iid, "client": c, "type": meta.get("type").map(PyValue::py_str),
                                   "title": meta.get("title").map(PyValue::py_str), "body": body,
                                   "created": created, "valid": true}),
                        ));
                    }
                    Err(e) if matches!(e, Exc::Problem(_)) || e.is_yaml() => {
                        out.push((0.0, iid.clone(), json!({"id": iid, "client": c, "valid": false})));
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        out.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
        Ok(out.into_iter().map(|(_, _, v)| v).collect())
    }

    /// `_find(iid)`: the client whose inbox holds the item.
    fn find(&self, iid: &str) -> XResult<String> {
        if !valid_id(iid) {
            return Err(Problem::bad_request("invalid id").code("invalid_id").into());
        }
        let base = self.work.root.join("inbox");
        if base.is_dir() {
            let mut dirs: Vec<(String, std::path::PathBuf)> = fs::read_dir(&base)?
                .filter_map(Result::ok)
                .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
                .collect();
            dirs.sort_by(|a, b| a.0.cmp(&b.0));
            for (n, d) in dirs {
                if d.is_dir() && !is_symlink(&d) && valid_id(&n) && self.ids(&n)?.iter().any(|x| x == iid) {
                    return Ok(n);
                }
            }
        }
        Err(Problem::not_found(format!("no such inbox item: {iid}")).into())
    }

    pub fn promote(&self, iid: &str, edits: Option<&PyValue>) -> HubResult<Value> {
        self.promote_x(iid, edits).map_err(Exc::into_problem)
    }

    pub fn promote_x(&self, iid: &str, edits: Option<&PyValue>) -> XResult<Value> {
        let client = self.find(iid)?;
        let (meta, body) = self.read(&client, iid)?;
        let empty = PyValue::Dict(Vec::new());
        let edits = edits.filter(|e| truthy(e)).unwrap_or(&empty);
        let mem_id = edits
            .get("id")
            .filter(|v| truthy(v))
            .map_or_else(|| iid.to_string(), PyValue::py_str);
        if !valid_id(&mem_id) {
            return Err(Problem::bad_request("invalid memory id").code("invalid_id").into());
        }
        if self.work.names_x("memory")?.contains(&mem_id) {
            return Err(Problem::conflict(format!("memory {mem_id} already exists"))
                .code("already_exists")
                .into());
        }
        let pick = |k: &str, dflt: PyValue| edits.get(k).cloned().unwrap_or(dflt);
        let payload = PyValue::Dict(vec![
            (PyValue::str("id"), PyValue::str(mem_id.as_str())),
            (
                PyValue::str("type"),
                pick("type", meta.get("type").cloned().unwrap_or_default()),
            ),
            (
                PyValue::str("title"),
                pick("title", meta.get("title").cloned().unwrap_or_default()),
            ),
            (PyValue::str("body"), pick("body", PyValue::Str(body))),
        ]);
        self.work.put_x("memory", &mem_id, &payload, Some(false))?;
        fs::remove_file(self.work.path(&format!("inbox/{client}/{iid}.md"))?)?;
        Ok(json!({"id": mem_id, "promoted_from": format!("{client}/{iid}")}))
    }

    pub fn reject(&self, iid: &str) -> HubResult<Value> {
        self.reject_x(iid).map_err(Exc::into_problem)
    }

    pub fn reject_x(&self, iid: &str) -> XResult<Value> {
        let client = self.find(iid)?;
        fs::remove_file(self.work.path(&format!("inbox/{client}/{iid}.md"))?)?;
        Ok(json!({"id": iid, "client": client, "rejected": true}))
    }
}
