//! Port of services/applier.py: turns a fresh plan into files, all-or-nothing per client.
//!
//! Stage (temp files + backups at `<data>/backups/<client>/<%Y%m%dT%H%M%S-xxxx>/<root>/<path>`, 0600; nothing live is
//! touched) -> commit (compare-and-swap through safefs dir fds, atomic renames; roll back from the backups if any
//! fails) -> lock update -> apply log -> adapter.verify. The plan is recomputed against the live files right before
//! writing and must match the reviewed plan's digest, so an apply can never do something the reviewer did not see.
//! Owner: WS-B2.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hub_core::common::{atomic_write, now, sha256, token_hex};
use hub_core::py::os::realpath;
use hub_core::py::text::take_chars;
use hub_core::{HubResult, Problem};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::applylog::ApplyLog;
use super::content::Catalog;
use super::fs::RealFs;
use super::lock::{Lock, LockEntry, LockStore};
use super::planner::{ClientPlanInternal, PlannedFile, Planner};
use super::safefs::{self, RaceError, SafeFsError};
use crate::adapters::base::{Check, ClientConfig, CommandRunner, Expected};

/// `ApplyResult` (pydantic field order).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ApplyResult {
    pub client: String,
    /// applied | nothing | blocked | failed
    pub status: String,
    pub written: Vec<Value>,
    pub removed: Vec<Value>,
    pub skipped_conflicts: Vec<Value>,
    pub backup_dir: Option<String>,
    pub checks: Vec<Value>,
    pub verify_ok: Option<bool>,
    pub blocked_reasons: Vec<String>,
    pub error: String,
    pub apply_id: Option<String>,
}

impl ApplyResult {
    fn new(client: &str, status: &str) -> Self {
        Self {
            client: client.into(),
            status: status.into(),
            ..Self::default()
        }
    }
}

/// An exception raised while staging/committing: Python's `type(exc).__name__` and `str(exc)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    pub py_type: String,
    pub message: String,
}

impl OpError {
    pub fn new(py_type: &str, message: impl Into<String>) -> Self {
        Self {
            py_type: py_type.into(),
            message: message.into(),
        }
    }
}

impl From<SafeFsError> for OpError {
    fn from(e: SafeFsError) -> Self {
        let (t, m) = safefs::py_error(&e);
        Self { py_type: t, message: m }
    }
}

impl From<RaceError> for OpError {
    fn from(e: RaceError) -> Self {
        Self::new("RaceError", e.0)
    }
}

fn io_op_error(e: &std::io::Error) -> OpError {
    OpError::new(hub_core::py::os::error_name(e), hub_core::py::os::error_str(e, None))
}

/// `_Op`: one file of an apply (or rollback) in flight.
pub struct Op {
    pub pf: PlannedFile,
    pub target: PathBuf,
    /// staged temp file name inside `dirfd` (writes)
    pub tmp: Option<String>,
    pub backup: Option<PathBuf>,
    pub old_mode: Option<u32>,
    pub existed: bool,
    /// the target's parent directory, opened component-by-component with O_NOFOLLOW
    pub dirfd: Option<OwnedFd>,
    pub name: String,
    pub root_real: String,
    pub parts: Vec<String>,
    /// what the plan saw live: compare-and-swap before replacing
    pub expected: Option<Vec<u8>>,
    pub created_dirs: Vec<Vec<String>>,
}

impl Op {
    pub fn new(pf: PlannedFile, target: PathBuf, root_real: String) -> Self {
        Self {
            pf,
            target,
            tmp: None,
            backup: None,
            old_mode: None,
            existed: false,
            dirfd: None,
            name: String::new(),
            root_real,
            parts: Vec::new(),
            expected: None,
            created_dirs: Vec::new(),
        }
    }
}

/// What a test hook sees of an op (Python's tests monkeypatch `Applier._stage` / `Applier._commit`).
pub struct OpView<'a> {
    pub root: &'a str,
    pub path: &'a str,
    /// the file name (last path component)
    pub name: &'a str,
}

pub type OpHook = Box<dyn Fn(&OpView<'_>) -> Result<(), OpError> + Send + Sync>;

/// Test seams: called before each `_stage` / `_commit`; an `Err` acts as an exception raised there.
#[derive(Default)]
pub struct ApplyHooks {
    pub before_stage: Option<OpHook>,
    pub before_commit: Option<OpHook>,
}

pub struct Applier {
    pub planner: Arc<Planner>,
    pub locks: Arc<LockStore>,
    pub backups_dir: PathBuf,
    pub runner: Arc<dyn CommandRunner>,
    pub log: Arc<ApplyLog>,
    hooks: Mutex<Option<Arc<ApplyHooks>>>,
}

/// `time.strftime(fmt, time.gmtime())` for the two formats the applier uses.
pub fn utc_stamp(with_z: bool) -> String {
    let t = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}{}",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        if with_z { "Z" } else { "" }
    )
}

impl Applier {
    pub fn new(
        planner: Arc<Planner>,
        locks: Arc<LockStore>,
        backups_dir: &Path,
        runner: Arc<dyn CommandRunner>,
        log: Arc<ApplyLog>,
    ) -> Self {
        Self {
            planner,
            locks,
            backups_dir: backups_dir.to_path_buf(),
            runner,
            log,
            hooks: Mutex::new(None),
        }
    }

    /// Install (or with `None` remove) the test hooks.
    pub fn set_test_hooks(&self, hooks: Option<ApplyHooks>) {
        *self.hooks.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = hooks.map(Arc::new);
    }

    fn hooks(&self) -> Option<Arc<ApplyHooks>> {
        self.hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// `apply_client(catalog, content_hash, client_id, expected_digest, adopt, plan_id)`.
    pub fn apply_client(
        &self,
        catalog: &Catalog,
        content_hash: &str,
        client_id: &str,
        expected_digest: Option<&str>,
        adopt: &[String],
        plan_id: &str,
    ) -> HubResult<ApplyResult> {
        let plan = |a: &[String]| self.planner.plan_client(catalog, client_id, a);
        self.apply_with(&plan, content_hash, client_id, expected_digest, adopt, plan_id)
    }

    /// `apply_client` with the planning step injected (`plan(adopt)`), so the applier can be driven with a hand-built
    /// resolution.
    pub fn apply_with(
        &self,
        plan: &dyn Fn(&[String]) -> HubResult<ClientPlanInternal>,
        content_hash: &str,
        client_id: &str,
        expected_digest: Option<&str>,
        adopt: &[String],
        plan_id: &str,
    ) -> HubResult<ApplyResult> {
        let mut ip = plan(&[])?; // the digest is always over the un-adopted plan
        if let Some(want) = expected_digest
            && ip.plan.digest != want
        {
            return Err(
                Problem::conflict("live files or content changed since the plan was made; plan again")
                    .code("plan_stale"),
            );
        }
        if !adopt.is_empty() {
            ip = plan(adopt)?;
        }
        if ip.plan.blocked {
            return Ok(ApplyResult {
                blocked_reasons: ip.plan.blocked_reasons,
                ..ApplyResult::new(client_id, "blocked")
            });
        }
        let Some(cfg) = ip.resolved.cfg.clone() else {
            return Err(Problem::internal());
        };
        let live = super::fs::LiveFiles::new(&cfg.roots, super::fs::MAX_READ);
        let mut ops: Vec<Op> = Vec::new();
        let mut skipped: Vec<Value> = Vec::new();
        for pf in &ip.files {
            let d = &pf.desired;
            let adoptable_conflict =
                pf.action == "conflict" && pf.adopted && pf.adoptable && pf.merged.unparseable.is_empty();
            if !(matches!(pf.action.as_str(), "add" | "change" | "remove") || adoptable_conflict) {
                if pf.action == "conflict" {
                    skipped.push(json!({"root": d.root, "path": d.path, "reason": pf.reason}));
                }
                continue;
            }
            let mut pf = pf.clone();
            if pf.action == "conflict" && d.content.is_none() {
                // adopted removal
                pf.delete_file =
                    d.merge == "own" || (pf.merged.empty_after && pf.entry.as_ref().is_some_and(|e| e.created));
                if !pf.delete_file && pf.merged.new.is_none() {
                    continue;
                }
            }
            // Python: an unsafe path here raises PathError (a ValueError): an unexpected error
            let target = live.target(&d.root, &d.path).map_err(|_| Problem::internal())?;
            let root_real = realpath(Path::new(cfg.roots.get(&d.root).map_or("", String::as_str)))
                .to_string_lossy()
                .to_string();
            ops.push(Op::new(pf, target, root_real));
        }
        let ts = format!("{}-{}", utc_stamp(false), token_hex(2));
        let bdir = self.backups_dir.join(client_id).join(ts);
        let mut result = ApplyResult {
            skipped_conflicts: skipped.clone(),
            ..ApplyResult::new(client_id, "nothing")
        };
        if let Some(err) = self.run_ops(&mut ops, &bdir)? {
            return Ok(ApplyResult {
                skipped_conflicts: skipped,
                error: format!("{err}; nothing was changed"),
                ..ApplyResult::new(client_id, "failed")
            });
        }
        let prev: Vec<(String, Value)> = ops
            .iter()
            .map(|op| {
                let key = op.pf.key();
                let before = ip
                    .lock
                    .as_ref()
                    .and_then(|l| l.files.get(&key))
                    .map_or(Value::Null, |e| serde_json::to_value(e).unwrap_or(Value::Null));
                (key, before)
            })
            .collect();
        for op in &ops {
            if op.backup.is_some() {
                result.backup_dir = Some(bdir.to_string_lossy().to_string());
            }
            let entry = json!({"root": op.pf.desired.root, "path": op.pf.desired.path});
            if op.pf.delete_file {
                result.removed.push(entry);
            } else {
                result.written.push(entry);
            }
        }
        Self::prune_dirs(&ops, &cfg);
        self.update_lock(&ip, content_hash, &ops, &skipped)?;
        result.status = if ops.is_empty() { "nothing" } else { "applied" }.into();
        if !ops.is_empty() {
            result.apply_id = Some(self.record(client_id, plan_id, content_hash, &ops, &prev)?);
        }
        self.verify(&ip, &ops, &cfg, &mut result);
        Ok(result)
    }

    /// Stage every op (temp files + backups), then commit them; on any failure roll everything back. Returns an error
    /// message (starting with `conflict:` for a race) or None. Shared by apply and rollback. `Err` only where Python's
    /// own rollback raised (a non-OSError while undoing).
    pub fn run_ops(&self, ops: &mut [Op], bdir: &Path) -> HubResult<Option<String>> {
        let hooks = self.hooks();
        let mut done = 0usize;
        let mut failure: Option<OpError> = None;
        for op in ops.iter_mut() {
            if let Err(e) = Self::hook(hooks.as_deref(), op, true).and_then(|()| Self::stage(op, bdir)) {
                failure = Some(e);
                break;
            }
        }
        if failure.is_none() {
            for op in ops.iter_mut() {
                if let Err(e) = Self::hook(hooks.as_deref(), op, false).and_then(|()| Self::commit(op)) {
                    failure = Some(e);
                    break;
                }
                done += 1;
            }
        }
        let Some(exc) = failure else {
            Self::close(ops);
            return Ok(None);
        };
        Self::rollback(ops, done)?;
        Self::close(ops);
        let kind = if exc.py_type == "RaceError" {
            "conflict".to_string()
        } else {
            exc.py_type
        };
        Ok(Some(format!("{kind}: {}", take_chars(&exc.message, 200))))
    }

    fn hook(hooks: Option<&ApplyHooks>, op: &Op, stage: bool) -> Result<(), OpError> {
        let Some(h) = hooks else { return Ok(()) };
        let f = if stage { &h.before_stage } else { &h.before_commit };
        let Some(f) = f else { return Ok(()) };
        let d = &op.pf.desired;
        let name = d.path.rsplit('/').next().unwrap_or_default();
        f(&OpView {
            root: &d.root,
            path: &d.path,
            name,
        })
    }

    /// `_record(...)`: the apply log entry; returns the apply id.
    fn record(
        &self,
        client: &str,
        plan_id: &str,
        content_hash: &str,
        ops: &[Op],
        prev: &[(String, Value)],
    ) -> HubResult<String> {
        let suffix = if plan_id.is_empty() {
            format!("x{}", token_hex(3))
        } else {
            plan_id.to_string()
        };
        let apply_id = format!("{}-{suffix}", utc_stamp(true));
        let mut files = Vec::new();
        for op in ops {
            let d = &op.pf.desired;
            let action = if op.pf.delete_file {
                "remove"
            } else if op.existed {
                "change"
            } else {
                "add"
            };
            let backup = op.backup.as_ref().map_or(Value::Null, |b| {
                Value::String(
                    b.strip_prefix(&self.backups_dir)
                        .unwrap_or(b)
                        .to_string_lossy()
                        .to_string(),
                )
            });
            let key = op.pf.key();
            let lock_before = prev
                .iter()
                .find(|(k, _)| *k == key)
                .map_or(Value::Null, |(_, v)| v.clone());
            files.push(json!({
                "root": d.root, "path": d.path, "kind": d.kind, "action": action,
                "existed_before": op.existed,
                "backup": backup,
                "pre_sha": op.expected.as_deref().map_or(Value::Null, |e| Value::String(sha256(e))),
                "post_sha": if op.pf.delete_file { Value::Null }
                            else { Value::String(sha256(op.pf.merged.new.as_deref().unwrap_or_default())) },
                "mode_before": op.old_mode.map_or(Value::Null, Value::from),
                "lock_before": lock_before,
            }));
        }
        self.log.save(&json!({
            "apply_id": apply_id, "client": client, "time": now(), "plan_id": plan_id,
            "content_hash": content_hash, "files": files,
        }))?;
        Ok(apply_id)
    }

    // -- stage / commit / rollback ----------------------------------------------------------------------------------

    fn stage(op: &mut Op, bdir: &Path) -> Result<(), OpError> {
        let mut parts: Vec<String> = op.pf.desired.path.split('/').map(str::to_string).collect();
        op.name = parts.pop().unwrap_or_default();
        op.parts = parts;
        let fd = safefs::open_dir(&op.root_real, &op.parts, !op.pf.delete_file, &mut op.created_dirs)?;
        let (live, mode) = safefs::read_at(&fd, &op.name)?;
        op.dirfd = Some(fd);
        if live != op.pf.live {
            // changed between planning and staging
            return Err(RaceError(format!("{} changed since it was planned", op.pf.desired.path)).into());
        }
        op.existed = live.is_some();
        op.old_mode = mode;
        if let Some(bytes) = &live {
            let backup = bdir.join(&op.pf.desired.root).join(&op.pf.desired.path);
            atomic_write(&backup, bytes, 0o600, 0o700).map_err(|e| io_op_error(&e))?;
            op.backup = Some(backup);
        }
        op.expected = live;
        if op.pf.delete_file {
            return Ok(());
        }
        let Some(data) = op.pf.merged.new.as_deref() else {
            return Err(OpError::new(
                "ApplyError",
                format!("nothing to write for {}", op.pf.desired.path),
            ));
        };
        let m = if op.existed && op.pf.desired.merge != "own" {
            op.old_mode
        } else {
            Some(op.pf.desired.mode)
        };
        let Some(fd) = op.dirfd.as_ref() else {
            return Err(OpError::new("OSError", "no directory"));
        };
        op.tmp = Some(safefs::write_tmp(fd, data, m.unwrap_or(0o644))?);
        Ok(())
    }

    fn commit(op: &mut Op) -> Result<(), OpError> {
        let Some(fd) = op.dirfd.as_ref() else {
            return Err(OpError::new("OSError", "no directory"));
        };
        // compare-and-swap: re-read the live target through the same dirfd right before touching it
        let (cur, _) = safefs::read_at(fd, &op.name)?;
        if cur != op.expected {
            return Err(RaceError(format!(
                "{} changed since it was planned (someone edited it)",
                op.pf.desired.path
            ))
            .into());
        }
        if op.pf.delete_file {
            safefs::unlink_at(fd, &op.name)?;
        } else {
            let tmp = op.tmp.clone().unwrap_or_default();
            safefs::replace(fd, &tmp, &op.name)?;
            op.tmp = None;
        }
        Ok(())
    }

    /// Undo staged temp files, restore the committed ops (newest first) from their backups, remove created dirs.
    /// OSErrors are suppressed as in Python; a RaceError while reopening a created dir's parent escapes (500).
    fn rollback(ops: &mut [Op], done: usize) -> HubResult<()> {
        for op in ops.iter_mut() {
            if let (Some(tmp), Some(fd)) = (&op.tmp, &op.dirfd) {
                safefs::unlink_quiet(fd, tmp);
            }
        }
        for op in ops[..done].iter().rev() {
            let Some(fd) = op.dirfd.as_ref() else { continue };
            if let Some(b) = &op.backup {
                let restored = super::fs::read_regular(b, false, u64::MAX).ok().and_then(|data| {
                    let tmp = safefs::write_tmp(fd, &data, op.old_mode.filter(|m| *m != 0).unwrap_or(0o644));
                    match tmp {
                        Ok(t) => safefs::replace(fd, &t, &op.name).err(),
                        Err(e) => Some(e),
                    }
                });
                if let Some(SafeFsError::Race(_)) = restored {
                    return Err(Problem::internal());
                }
            } else if !op.pf.delete_file {
                safefs::unlink_quiet(fd, &op.name);
            }
        }
        for op in ops.iter().rev() {
            for comps in op.created_dirs.iter().rev() {
                let (last, parent) = comps.split_last().map_or(("", &[][..]), |(l, p)| (l.as_str(), p));
                match safefs::open_dir(&op.root_real, parent, false, &mut Vec::new()) {
                    Ok(pfd) => {
                        let _ = safefs::rmdir_at(&pfd, last);
                    }
                    Err(SafeFsError::Race(_)) => return Err(Problem::internal()),
                    Err(SafeFsError::Io(_)) => {}
                }
            }
        }
        Ok(())
    }

    fn close(ops: &mut [Op]) {
        for op in ops.iter_mut() {
            op.dirfd = None;
        }
    }

    /// `_prune_dirs`: after deleting hub-created files, remove the directories they leave empty (never the root).
    pub fn prune_dirs(ops: &[Op], cfg: &ClientConfig) {
        for op in ops {
            if !op.pf.delete_file {
                continue;
            }
            let Some(root) = cfg.roots.get(&op.pf.desired.root) else {
                continue;
            };
            let root = realpath(Path::new(root));
            let mut p = op.target.parent().map(Path::to_path_buf);
            while let Some(d) = p {
                if d == root || !d.starts_with(&root) {
                    break;
                }
                if std::fs::remove_dir(&d).is_err() {
                    break;
                }
                p = d.parent().map(Path::to_path_buf);
            }
        }
    }

    // -- lock / verify ----------------------------------------------------------------------------------------------

    fn update_lock(&self, ip: &ClientPlanInternal, content_hash: &str, ops: &[Op], skipped: &[Value]) -> HubResult<()> {
        let mut lock = ip.lock.clone().unwrap_or_else(|| Lock::new(&ip.plan.client));
        let mut files = lock.files.clone();
        let written: Vec<String> = ops.iter().map(|o| o.pf.key()).collect();
        let skipped_keys: Vec<String> = skipped
            .iter()
            .map(|s| {
                format!(
                    "{}:{}",
                    s["root"].as_str().unwrap_or_default(),
                    s["path"].as_str().unwrap_or_default()
                )
            })
            .collect();
        for pf in &ip.files {
            let d = &pf.desired;
            let key = pf.key();
            if pf.released {
                // concern no longer managed: forget the entry, never touch the file
                files.shift_remove(&key);
                continue;
            }
            if skipped_keys.contains(&key)
                || (matches!(pf.action.as_str(), "advisory" | "conflict") && !written.contains(&key))
            {
                continue;
            }
            if d.content.is_none() {
                files.shift_remove(&key);
                continue;
            }
            if !d.managed {
                continue;
            }
            let prev = files.get(&key);
            let fin = if pf.action == "unchanged" {
                pf.live.as_deref()
            } else {
                pf.merged.new.as_deref()
            };
            let Some(fin) = fin else { continue };
            let created = match prev {
                Some(p) => p.created,
                None => pf.live.is_none() && pf.action == "add",
            };
            let entry = LockEntry {
                root: d.root.clone(),
                path: d.path.clone(),
                kind: d.kind.clone(),
                sha256: sha256(fin),
                slice_sha256: pf.merged.slice_new.clone().unwrap_or_else(|| sha256(fin)),
                merge: d.merge.clone(),
                managed_keys: d.managed_keys.clone(),
                created,
                source_ids: d.source_ids.clone(),
                mode: d.mode,
            };
            files.insert(key, entry);
        }
        lock.files = files;
        lock.content_hash = content_hash.into();
        self.locks.save(&mut lock, true)
    }

    fn verify(&self, ip: &ClientPlanInternal, ops: &[Op], cfg: &ClientConfig, result: &mut ApplyResult) {
        let Some(adapter) = &ip.resolved.adapter else {
            return;
        };
        let written: Vec<String> = ops.iter().map(|o| o.pf.key()).collect();
        let mut expected = Vec::new();
        for pf in &ip.files {
            let d = &pf.desired;
            if d.content.is_none()
                || !d.managed
                || pf.action == "advisory"
                || (pf.action == "conflict" && !written.contains(&pf.key()))
            {
                continue;
            }
            // what SHOULD be on disk; key/block merges are verified on the hub-owned SLICE
            let intended = if pf.action == "unchanged" {
                pf.live.as_deref()
            } else {
                pf.merged.new.as_deref()
            };
            let digest = match intended {
                Some(i) if d.merge == "own" => Some(sha256(i)),
                _ => pf.merged.slice_new.clone(),
            };
            if let (Some(_), Some(digest)) = (intended, digest) {
                expected.push(Expected {
                    root: d.root.clone(),
                    path: d.path.clone(),
                    sha256: digest,
                    kind: d.kind.clone(),
                    source_ids: d.source_ids.clone(),
                    merge: d.merge.clone(),
                    managed_keys: d.managed_keys.clone(),
                });
            }
        }
        let roots: Vec<PathBuf> = cfg.roots.values().map(PathBuf::from).collect();
        let fs = RealFs::new(&roots, false);
        let checks = adapter
            .verify(cfg, &expected, &fs, self.runner.as_ref())
            .unwrap_or_else(|e| {
                vec![Check {
                    name: "adapter.verify".into(),
                    ok: false,
                    detail: format!("raised {}", e.py_type),
                }]
            });
        result.verify_ok = Some(checks.iter().all(|c| c.ok));
        result.checks = checks
            .iter()
            .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
            .collect();
    }
}

#[cfg(test)]
#[path = "applier_tests.rs"]
mod tests;
