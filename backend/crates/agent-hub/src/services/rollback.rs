//! Port of services/rollback.py: undo an apply: restore the pre-apply backups and delete the files the apply created,
//! but ONLY where the live file still equals what that apply wrote (so later hand edits are never clobbered), unless
//! named in `force`. Uses the applier's dirfd/O_NOFOLLOW writes and compare-and-swap, all-or-nothing; dry runs show
//! the diff. Owner: WS-B2.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use hub_core::common::{now, sha256, token_hex};
use hub_core::pathsafe::check_relpath;
use hub_core::py::os::realpath;
use hub_core::{HubResult, Problem};
use serde_json::{Map, Value, json};

use super::applier::{Applier, Op, utc_stamp};
use super::applylog::{ApplyLog, truthy};
use super::fs::{LiveFiles, MAX_READ, path_io_error};
use super::lock::{Lock, LockEntry, LockStore};
use super::merge::{Desired, Merged};
use super::planner::{PlannedFile, unified};
use crate::adapters::base::ClientConfig;

pub struct Rollbacker {
    pub applier: Arc<Applier>,
    pub log: Arc<ApplyLog>,
    pub locks: Arc<LockStore>,
}

/// (record file, live now, bytes to restore)
type Planned = (Value, Option<Vec<u8>>, Option<Vec<u8>>);

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn with(base: &Map<String, Value>, extra: &[(&str, Value)]) -> Value {
    let mut m = base.clone();
    for (k, v) in extra {
        m.insert((*k).to_string(), v.clone());
    }
    Value::Object(m)
}

impl Rollbacker {
    pub fn new(applier: Arc<Applier>, log: Arc<ApplyLog>, locks: Arc<LockStore>) -> Self {
        Self { applier, log, locks }
    }

    /// `_backup_bytes(rec_file)`: the pre-apply bytes, only from inside the backups dir.
    fn backup_bytes(&self, f: &Value) -> HubResult<Vec<u8>> {
        let rel = match f.get("backup") {
            Some(Value::String(r)) if !r.is_empty() => r.clone(),
            Some(v) if truthy(Some(v)) => return Err(Problem::internal()), // Python: `base / <non-str>` TypeError
            _ => {
                return Err(Problem::conflict("this apply has no backup for a file it changed").code("backup_missing"));
            }
        };
        let base = realpath(&self.applier.backups_dir);
        let p = realpath(&base.join(&rel));
        if p == base || !p.starts_with(&base) || !p.is_file() {
            return Err(Problem::conflict(format!("backup for {} is missing", s(&f["path"]))).code("backup_missing"));
        }
        Ok(super::fs::read_regular(&p, false, u64::MAX)?)
    }

    /// `run(cfg, client, apply_id, dry_run, force)`: `{"status", "apply_id", "changed", "conflicts", "message"?, ...}`.
    pub fn run(
        &self,
        cfg: &ClientConfig,
        client: &str,
        apply_id: Option<&str>,
        dry_run: bool,
        force: &BTreeSet<String>,
    ) -> HubResult<Value> {
        let rec = match apply_id.filter(|a| !a.is_empty()) {
            Some(id) => Some(self.log.get(client, id)?),
            None => match self.log.latest_active(client)? {
                Some(r) => Some(r),
                None => self.log.list(client)?.into_iter().next(),
            },
        };
        let Some(rec) = rec else {
            return Err(Problem::not_found("this client has no recorded applies to roll back").code("no_applies"));
        };
        let rec_id = s(&rec["apply_id"]).to_string();
        if truthy(rec.get("rolled_back_at")) {
            return Ok(json!({
                "status": "already_rolled_back", "apply_id": rec["apply_id"], "changed": [], "conflicts": [],
                "message": format!("apply {rec_id} was already rolled back; nothing to do"),
            }));
        }
        let latest = self.log.latest_active(client)?;
        let live = LiveFiles::new(&cfg.roots, MAX_READ);
        let mut items: Vec<Value> = Vec::new();
        let mut conflicts: Vec<Value> = Vec::new();
        // (record file, live now, bytes to restore)
        let mut planned: Vec<Planned> = Vec::new();
        let mut done_keys: BTreeSet<String> = rec
            .get("done")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let files = rec
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(Problem::internal)?;
        for f in files {
            let (root, path) = (s(&f["root"]), s(&f["path"]));
            let key = format!("{root}:{path}");
            if done_keys.contains(&key) {
                continue;
            }
            let cur = match check_relpath(path)
                .map_err(|e| path_io_error(&e))
                .and_then(|_| live.read(root, path))
            {
                Ok(c) => c,
                Err(e) => {
                    conflicts.push(json!({"root": root, "path": path, "reason": format!("live file unreadable: {e}")}));
                    continue;
                }
            };
            let cur_sha = cur.as_deref().map(sha256);
            let post_sha = f.get("post_sha").and_then(Value::as_str);
            if cur_sha.as_deref() != post_sha && !force.contains(&key) && !force.contains(&format!("{client}:{key}")) {
                let reason = if cur.is_none() {
                    "file is gone"
                } else {
                    "file was edited since this apply wrote it"
                };
                conflicts.push(json!({"root": root, "path": path, "reason": reason}));
                continue;
            }
            if f.get("action").and_then(Value::as_str) == Some("add") {
                let Some(cur) = cur else { continue }; // already gone: nothing to undo
                items.push(json!({"root": root, "path": path, "action": "delete",
                                  "diff": unified(Some(&cur), None, path)}));
                planned.push((f.clone(), Some(cur), None));
            } else {
                let restore = self.backup_bytes(f)?;
                if cur.as_deref() == Some(restore.as_slice()) {
                    continue;
                }
                items.push(json!({"root": root, "path": path, "action": "restore",
                                  "diff": unified(cur.as_deref(), Some(&restore), path)}));
                planned.push((f.clone(), cur, Some(restore)));
            }
        }
        if !conflicts.is_empty()
            && let Some(l) = &latest
            && l.get("apply_id") != rec.get("apply_id")
        {
            return Err(Problem::conflict(
                "this is not the latest apply for the client and some of its files changed since; roll back the newer \
                 applies first",
            )
            .code("rollback_not_latest")
            .with("conflicts", Value::Array(conflicts)));
        }
        let mut base = Map::new();
        base.insert("apply_id".into(), rec["apply_id"].clone());
        base.insert("changed".into(), Value::Array(items));
        base.insert("conflicts".into(), Value::Array(conflicts.clone()));
        if dry_run {
            return Ok(with(&base, &[("status", json!("dry_run"))]));
        }
        if planned.is_empty() {
            if conflicts.is_empty() {
                // every file is already back
                self.log.update(client, &rec_id, &json!({"rolled_back_at": now()}))?;
            }
            let (status, message) = if conflicts.is_empty() {
                ("nothing_to_do", "already in the pre-apply state")
            } else {
                ("conflicts_only", "no file could be rolled back")
            };
            return Ok(with(&base, &[("status", json!(status)), ("message", json!(message))]));
        }
        let mut ops: Vec<Op> = Vec::new();
        for (f, live_now, restore_to) in &planned {
            let (root, path) = (s(&f["root"]), s(&f["path"]));
            let mode = f
                .get("mode_before")
                .and_then(Value::as_u64)
                .and_then(|m| u32::try_from(m).ok())
                .filter(|m| *m != 0)
                .unwrap_or(0o644);
            let d = Desired {
                root: root.into(),
                path: path.into(),
                kind: f.get("kind").and_then(Value::as_str).unwrap_or("skill").into(),
                merge: "own".into(),
                source_ids: vec![],
                mode,
                managed_keys: vec![],
                content: restore_to.clone(),
                managed: true,
            };
            let mut pf = PlannedFile::new(d, if restore_to.is_none() { "remove" } else { "change" });
            pf.merged = Merged {
                new: restore_to.clone(),
                ..Merged::default()
            };
            pf.live = live_now.clone();
            pf.delete_file = restore_to.is_none();
            let target = live.target(root, path).map_err(|_| Problem::internal())?;
            let root_real = realpath(Path::new(cfg.roots.get(root).ok_or_else(Problem::internal)?))
                .to_string_lossy()
                .to_string();
            ops.push(Op::new(pf, target, root_real));
        }
        let bdir =
            self.applier
                .backups_dir
                .join(client)
                .join(format!("{}-rollback-{}", utc_stamp(false), token_hex(2)));
        if let Some(err) = self.applier.run_ops(&mut ops, &bdir)? {
            return Err(
                Problem::conflict(format!("rollback aborted, nothing was changed: {err}")).code("rollback_failed"),
            );
        }
        Applier::prune_dirs(&ops, cfg);
        let mut lock = self.locks.load(client).unwrap_or_else(|| Lock::new(client));
        for (f, _, _) in &planned {
            let key = format!("{}:{}", s(&f["root"]), s(&f["path"]));
            match f.get("lock_before") {
                Some(before) if truthy(Some(before)) => {
                    let entry: LockEntry = serde_json::from_value(before.clone()).map_err(|_| Problem::internal())?;
                    lock.files.insert(key, entry);
                }
                _ => {
                    lock.files.shift_remove(&key);
                }
            }
        }
        self.locks.save(&mut lock, false)?;
        for (f, _, _) in &planned {
            done_keys.insert(format!("{}:{}", s(&f["root"]), s(&f["path"])));
        }
        let complete = conflicts.is_empty();
        let bdir_s = bdir.to_string_lossy().to_string();
        self.log.update(
            client,
            &rec_id,
            &json!({"done": done_keys.iter().collect::<Vec<_>>(), "rollback_backup_dir": bdir_s,
                    "rolled_back_at": if complete { json!(now()) } else { Value::Null }}),
        )?;
        let status = if complete {
            "rolled_back"
        } else {
            "partially_rolled_back"
        };
        Ok(with(&base, &[("status", json!(status)), ("backup_dir", json!(bdir_s))]))
    }
}
