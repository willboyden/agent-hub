//! services/hub.py methods for the content repo lifecycle (init, safety, migrate, the approved snapshot), pending
//! changes (changes/commit/discard/revert/history), plan/apply/rollback, verify and drift. Owner: WS-B2.
//!
//! Content mutations hold the hub's reentrant lock (`self.lock`, Python `threading.RLock`): commit, discard, revert,
//! init, migrate, apply and rollback, exactly where Python takes `with self.lock:`.

use std::collections::BTreeSet;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use hub_core::common::{harden_tree, new_id, now, scrub};
use hub_core::models::Plan;
use hub_core::pathsafe::expand_root;
use hub_core::py::text::{is_alnum, take_chars};
use hub_core::{HubResult, Problem};
use serde_json::{Value, json};

use super::super::applylog::ApplyLog;
use super::super::content::{Catalog, ContentStore};
use super::super::contentguard;
use super::super::fs::RealFs;
use super::super::gitrepo::GitRepo;
use super::{Hub, SNAPSHOTS_KEEP};
use crate::adapters::base::Expected;

fn sha12(s: &str) -> String {
    take_chars(s, 12).to_string()
}

/// `Path(p).expanduser().absolute()` (no normalisation of `..`, as pathlib).
fn absolute(p: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let e = PathBuf::from(hub_core::py::os::expanduser(p, &home));
    if e.is_absolute() {
        e
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(e)
    }
}

/// `shutil.copytree(src, dst, symlinks=True)`: modes and times preserved (copy2/copystat), symlinks copied as links.
/// A special file (FIFO, socket, device) is refused before it is opened, as Python's `SpecialFileError` refused a
/// FIFO; `std::fs::copy` would block on a FIFO with the hub lock held (audit F5).
fn copytree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (s, d) = (e.path(), dst.join(e.file_name()));
        let md = std::fs::symlink_metadata(&s)?;
        if md.file_type().is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&s)?, &d)?;
            copy_times_nofollow(&md, &d);
        } else if md.is_dir() {
            copytree(&s, &d)?;
        } else if md.is_file() {
            // the copy itself opens without blocking and re-checks the type on the descriptor
            let (mut from, _) = crate::services::fs::open_regular(&s, false)?;
            let mut to = std::fs::OpenOptions::new().write(true).create_new(true).open(&d)?;
            std::io::copy(&mut from, &mut to)?;
            copystat(&md, &d)?;
        } else {
            return Err(std::io::Error::other(format!("`{}` is a special file", s.display())));
        }
    }
    copystat(&std::fs::symlink_metadata(src)?, dst)
}

fn copystat(md: &std::fs::Metadata, dst: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(md.mode() & 0o7777))?;
    let times = std::fs::FileTimes::new()
        .set_accessed(md.accessed()?)
        .set_modified(md.modified()?);
    std::fs::File::open(dst)?.set_times(times)
}

fn copy_times_nofollow(md: &std::fs::Metadata, dst: &Path) {
    use nix::sys::stat::{UtimensatFlags, utimensat};
    use nix::sys::time::TimeSpec;
    let _ = utimensat(
        nix::fcntl::AT_FDCWD,
        dst,
        &TimeSpec::new(md.atime(), md.atime_nsec()),
        &TimeSpec::new(md.mtime(), md.mtime_nsec()),
        UtimensatFlags::NoFollowSymlink,
    );
}

/// `shutil.rmtree(p, ignore_errors=True)`: never follows (nor removes) a symlink given as the top.
fn rmtree_quiet(p: &Path) {
    if std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
        return;
    }
    let _ = std::fs::remove_dir_all(p);
}

fn mtime(m: &std::fs::Metadata) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let t = m.mtime() as f64 + m.mtime_nsec() as f64 / 1e9;
    t
}

fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

impl Hub {
    /// `require_repo()`: content-dir safety (contentguard + client roots) and `is_repo`, else 503
    /// `content_not_initialised` "content repository is not initialised; run `hubctl init`".
    pub fn require_repo(&self) -> HubResult<()> {
        self.check_content_safety()?;
        if !self.repo.is_repo()? {
            return Err(
                Problem::unavailable("content repository is not initialised; run `hubctl init`")
                    .code("content_not_initialised"),
            );
        }
        Ok(())
    }

    /// `check_content_safety()`.
    pub fn check_content_safety(&self) -> HubResult<()> {
        let cfg = self.cfg();
        contentguard::check_location(&cfg.content_dir, &self.client_roots(), &self.app_root())?;
        self.repo.verify_safe(true)
    }

    /// `_client_roots()`: expanded roots of every readable client that passes `guard.root_problem` (an unreadable client
    /// cannot make the check weaker, only skip it; a refused root never receives files, so it cannot expose the repo).
    pub fn client_roots(&self) -> Vec<String> {
        let mut roots = Vec::new();
        for cid in self.work.names_of("clients").unwrap_or_default() {
            let Ok(doc) = self.work.get_client(&cid) else { continue };
            for r in doc.roots.values() {
                // Python appends root by root inside one try: roots before a bad one are kept
                let Ok(rp) = expand_root(r) else { break };
                if self.guard.root_problem(&rp).is_none() {
                    roots.push(rp.to_string_lossy().to_string());
                }
            }
        }
        roots
    }

    /// `hubctl init`: create the nested content repo, seed it, make the first commit. `{"content_dir", "seeded",
    /// "commit"}`.
    pub fn init_content(&self) -> HubResult<Value> {
        let _g = self.lock.lock();
        let cfg = self.cfg();
        contentguard::check_location(&cfg.content_dir, &self.client_roots(), &self.app_root())?;
        let fresh = !cfg.content_dir.exists();
        std::fs::create_dir_all(&cfg.content_dir)?;
        if fresh {
            std::fs::set_permissions(&cfg.content_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        self.repo.init()?;
        self.check_content_safety()?;
        let created = self.work.seed()?;
        let mut committed = Value::Null;
        if !created.is_empty() && !self.repo.status()?.is_empty() {
            committed = Value::String(self.repo.commit("init: seed content repository")?);
        }
        Ok(json!({"content_dir": cfg.content_dir.to_string_lossy(), "seeded": created, "commit": committed}))
    }

    /// `hubctl migrate-content`: move the content repo to a private location outside the source tree. The SOURCE is
    /// validated only as "a git repo owned by me with a real .git directory"; no git command ever runs against it (it
    /// is moved with plain file operations). Findings are shown first; `yes` waives them and they are stripped from the
    /// copy. Git (fsck) runs only on the sanitised target.
    pub fn migrate_content(&self, dest: &str, yes: bool) -> HubResult<Value> {
        self.migrate_content_as(dest, yes, nix::unistd::getuid().as_raw())
    }

    /// `migrate_content` with the uid the source must belong to (Python's tests monkeypatch `os.getuid`; addendum A-8).
    pub fn migrate_content_as(&self, dest: &str, yes: bool, uid: u32) -> HubResult<Value> {
        let cfg = self.cfg();
        let src = cfg.content_dir.clone();
        let target = absolute(dest);
        let git = src.join(".git");
        if src.is_symlink() || !git.is_dir() || git.is_symlink() {
            return Err(Problem::bad_request(
                "the configured content directory is not a git repository with a real .git directory",
            )
            .code("content_not_initialised"));
        }
        if std::fs::symlink_metadata(&src)?.uid() != uid || std::fs::symlink_metadata(&git)?.uid() != uid {
            return Err(Problem::bad_request(
                "the content repository is not owned by the current user; refusing to migrate it",
            ));
        }
        if target.exists() || target.is_symlink() {
            return Err(Problem::conflict(format!(
                "target {} already exists; refusing to overwrite",
                target.display()
            ))
            .code("target_exists"));
        }
        if target == src || target.starts_with(&src) || src.starts_with(&target) {
            return Err(Problem::bad_request("target must be a different, non-nested location"));
        }
        // the TARGET must be a safe place
        contentguard::check_location(&target, &self.client_roots(), &self.app_root())?;
        let findings = contentguard::hostile_findings(&src)?;
        let mut plan = serde_json::Map::new();
        plan.insert("from".into(), json!(src.to_string_lossy()));
        plan.insert("to".into(), json!(target.to_string_lossy()));
        let found: Vec<String> = findings.into_iter().filter(|f| !f.is_empty()).collect();
        plan.insert("findings".into(), json!(found));
        if !yes {
            plan.insert("dry_run".into(), json!(true));
            return Ok(Value::Object(plan));
        }
        {
            let _g = self.lock.lock();
            let parent = target.parent().map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
            if !parent.exists() {
                std::fs::create_dir_all(&parent)?;
                std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?;
            }
            let tmp = parent.join(format!(".migrate-{}", new_id("")));
            let attempt = || -> HubResult<Vec<String>> {
                copytree(&src, &tmp)?; // plain copy: modes and mtimes preserved
                let sanitized = contentguard::sanitize_git(&tmp)?;
                contentguard::tighten(&tmp)?;
                contentguard::check_ownership(&tmp)?;
                contentguard::verify_git_metadata(&tmp)?; // only now may git run, and only on the target
                let new = GitRepo::new(
                    &tmp,
                    &cfg.git_bin,
                    &cfg.git_author_name,
                    &cfg.git_author_email,
                    cfg.git_timeout_s,
                );
                new.run(&["fsck", "--strict"], true, None)?;
                if new.head()? != contentguard::plain_head(&src) {
                    return Err(Problem::conflict("copied repository does not have the same HEAD")
                        .code("migrate_verify_failed"));
                }
                std::fs::rename(&tmp, &target)?;
                Ok(sanitized)
            };
            match attempt() {
                Ok(sanitized) => {
                    plan.insert("sanitized".into(), json!(sanitized));
                }
                Err(e) => {
                    rmtree_quiet(&tmp);
                    return Err(e);
                }
            }
            std::fs::remove_dir_all(&src)?;
        }
        plan.insert("dry_run".into(), json!(false));
        let head = GitRepo::new(&target, &cfg.git_bin, "n", "e@x", 30.0).head()?;
        plan.insert("head".into(), json!(head));
        Ok(Value::Object(plan))
    }

    /// `approved()`: (content hash = HEAD tree id, read-only store over <data>/snapshots/<tree>) of the COMMITTED
    /// content: what plans render from.
    pub fn approved(&self) -> HubResult<(String, ContentStore)> {
        self.require_repo()?;
        let cfg = self.cfg();
        let tree = self.repo.head_tree()?;
        let root = cfg.data_dir.join("snapshots");
        let dest = root.join(&tree);
        if !dest.is_dir() {
            if !root.exists() {
                std::fs::create_dir_all(&root)?;
                std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
            }
            let tmp = root.join(format!(".tmp-{}", new_id("")));
            let res = self
                .repo
                .export_head(&tmp)
                .and_then(|()| std::fs::rename(&tmp, &dest).map_err(Problem::from));
            match res {
                Ok(()) => {}
                // Python catches only OSError here (our 500s); a ProblemError from git propagates as is
                Err(p) if p.status == 500 => {
                    rmtree_quiet(&tmp);
                    if !dest.is_dir() {
                        return Err(p);
                    }
                }
                Err(p) => return Err(p),
            }
            harden_tree(&dest);
            Self::prune_snapshots(&root, &tree);
        }
        Ok((
            tree,
            ContentStore::new(
                &dest,
                u64::try_from(cfg.max_content_file_bytes).unwrap_or(0),
                u64::try_from(cfg.max_skill_bytes).unwrap_or(0),
                None,
            ),
        ))
    }

    /// `_prune_snapshots(root, keep)`: the newest snapshots (by mtime) survive, `keep` always.
    fn prune_snapshots(root: &Path, keep: &str) {
        let Ok(rd) = std::fs::read_dir(root) else { return };
        let mut dirs: Vec<(f64, PathBuf)> = rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let name = e.file_name().to_string_lossy().to_string();
                if !p.is_dir() || name.starts_with('.') || name == keep {
                    return None;
                }
                Some((mtime(&std::fs::metadata(&p).ok()?), p))
            })
            .collect();
        dirs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        for (_, d) in dirs.iter().skip(SNAPSHOTS_KEEP - 1) {
            rmtree_quiet(d);
        }
    }

    /// `_approved_client(cid)`: the committed catalog, refusing a client that is not committed yet.
    pub fn approved_client(&self, cid: &str) -> HubResult<(String, Catalog)> {
        let (tree, store) = self.approved()?;
        let cat = store.catalog()?;
        if !cat.clients.contains_key(cid) {
            return Err(
                Problem::conflict("client is not in committed content yet; commit it first")
                    .code("client_not_committed"),
            );
        }
        Ok((tree, cat))
    }

    /// GET /changes.
    pub fn changes(&self) -> HubResult<Value> {
        self.require_repo()?;
        let ch = self.repo.changes(200_000)?;
        let items: Vec<Value> = ch
            .iter()
            .map(|c| json!({"path": c.path, "status": c.status, "diff": scrub(&c.diff)}))
            .collect();
        Ok(json!({"content_hash": self.repo.head_tree()?, "count": ch.len(), "items": items}))
    }

    pub fn commit(&self, message: &str, actor: &str) -> HubResult<Value> {
        let _g = self.lock.lock();
        self.require_repo()?;
        let cat = self.work.catalog()?;
        if !cat.problems.is_empty() {
            let errors: Vec<Value> = cat.problems.iter().take(50).map(to_value).collect();
            return Err(Problem::unprocessable(
                "content has validation problems; fix or discard them before committing",
            )
            .code("content_invalid")
            .with("errors", Value::Array(errors)));
        }
        let sha = self.repo.commit(message)?;
        let strays = self.repo.strays()?;
        self.audit_svc.event(
            actor,
            "content.commit",
            &json!({"commit": sha12(&sha), "strays": strays.len()}),
        )?;
        Ok(json!({"commit": sha, "content_hash": self.repo.head_tree()?, "strays": strays}))
    }

    /// Discarding EVERYTHING (no paths) is destructive: it needs confirm=true, and dry_run lists what it would delete.
    pub fn discard(&self, paths: Option<&[String]>, actor: &str, confirm: bool, dry_run: bool) -> HubResult<Value> {
        let _g = self.lock.lock();
        self.require_repo()?;
        if dry_run {
            return Ok(json!({"dry_run": true, "would_discard": self.repo.discard(paths, true)?}));
        }
        if paths.is_none_or(<[String]>::is_empty) && !confirm {
            let would = self.repo.discard(None, true)?;
            return Err(Problem::unprocessable(
                "discarding all pending changes needs confirm: true (use dry_run: true to list them first)",
            )
            .code("confirm_required")
            .with("would_discard", json!(would)));
        }
        let done = self.repo.discard(paths, false)?;
        let first: Vec<&String> = done.iter().take(50).collect();
        self.audit_svc
            .event(actor, "content.discard", &json!({"paths": first}))?;
        Ok(json!({"discarded": done}))
    }

    pub fn revert(&self, commit: &str, actor: &str) -> HubResult<Value> {
        let _g = self.lock.lock();
        self.require_repo()?;
        let sha = self.repo.revert(commit)?;
        self.audit_svc.event(
            actor,
            "content.revert",
            &json!({"reverted": sha12(commit), "commit": sha12(&sha)}),
        )?;
        Ok(json!({"commit": sha, "content_hash": self.repo.head_tree()?}))
    }

    pub fn history(&self, limit: i64) -> HubResult<Vec<Value>> {
        self.require_repo()?;
        self.repo.history(limit)
    }

    /// POST /plan (also `hubctl plan/apply`): remembers the plan (<data>/plans/<id>.json, newest plan_retention kept).
    pub fn plan(&self, clients: Option<&[String]>, source: &str) -> HubResult<Plan> {
        let (tree, cat) = match source {
            "working" => {
                self.require_repo()?;
                ("preview".to_string(), self.work.catalog()?)
            }
            "committed" => {
                let (tree, store) = self.approved()?;
                (tree, store.catalog()?)
            }
            _ => return Err(Problem::bad_request("source must be committed or working")),
        };
        let ids: Vec<String> = match clients.filter(|c| !c.is_empty()) {
            Some(c) => c.to_vec(),
            None => {
                let mut v: Vec<String> = cat.clients.keys().cloned().collect();
                v.sort();
                v
            }
        };
        for c in &ids {
            if !cat.clients.contains_key(c) {
                return Err(Problem::not_found(format!(
                    "client {} is not in {source} content",
                    hub_core::py::repr(c)
                )));
            }
        }
        let mut cplans = Vec::new();
        for c in &ids {
            cplans.push(self.planner.plan_client(&cat, c, &[])?.plan);
        }
        let plan = Plan {
            id: new_id("plan_"),
            content_hash: tree,
            created_at: now(),
            clients: cplans,
            pending_changes: i64::try_from(self.repo.status()?.len()).unwrap_or(i64::MAX),
            preview: source == "working",
        };
        self.remember(&plan)?;
        self.telemetry.plans.inc(&[]);
        Ok(plan)
    }

    /// `_remember(plan)`: cache + `<data>/plans/<id>.json` (pydantic `model_dump_json()`, 0600), only the newest
    /// `plan_retention` plan files kept.
    fn remember(&self, plan: &Plan) -> HubResult<()> {
        let mut cache = self.plans.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(plan.id.clone(), plan.clone());
        let d = self.plans_dir();
        let text = hub_core::py::json::pydantic(&to_value(plan), None);
        hub_core::common::atomic_write(&d.join(format!("{}.json", plan.id)), text.as_bytes(), 0o600, 0o700)?;
        let mut files: Vec<(f64, PathBuf)> = std::fs::read_dir(&d)?
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if !(name.starts_with("plan_") && name.ends_with(".json")) {
                    return None;
                }
                Some((mtime(&std::fs::metadata(e.path()).ok()?), e.path()))
            })
            .collect();
        files.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let keep = usize::try_from(self.cfg().plan_retention.max(0)).unwrap_or(0);
        for (_, old) in files.iter().skip(keep) {
            if std::fs::remove_file(old).is_ok()
                && let Some(stem) = old.file_stem()
            {
                cache.shift_remove(stem.to_string_lossy().as_ref());
            }
        }
        Ok(())
    }

    pub fn get_plan(&self, plan_id: &str) -> HubResult<Plan> {
        if let Some(p) = self
            .plans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(plan_id)
        {
            return Ok(p.clone());
        }
        if !plan_id.strip_prefix("plan_").is_some_and(is_alnum) {
            return Err(Problem::not_found("no such plan"));
        }
        std::fs::read(self.plans_dir().join(format!("{plan_id}.json")))
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|t| serde_json::from_str::<Plan>(&t).ok())
            .ok_or_else(|| Problem::not_found("no such plan"))
    }

    pub fn apply(&self, plan_id: &str, confirm: bool, adopt_paths: &[String], actor: &str) -> HubResult<Value> {
        if !confirm {
            return Err(Problem::unprocessable("apply needs confirm: true").code("confirm_required"));
        }
        let _g = self.lock.lock();
        let plan = self.get_plan(plan_id)?;
        if plan.preview {
            return Err(
                Problem::conflict("this plan is a working-tree preview; commit the content and plan again")
                    .code("plan_is_preview"),
            );
        }
        let (tree, store) = self.approved()?;
        if tree != plan.content_hash {
            return Err(Problem::conflict("content changed since this plan was made; plan again").code("plan_stale"));
        }
        let cat = store.catalog()?;
        // frozenset(adopt_paths): membership only
        let adopt: Vec<String> = adopt_paths
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for cp in &plan.clients {
            // pre-pass: refuse before writing anything
            if !cat.clients.contains_key(&cp.client) {
                return Err(Problem::conflict(format!("client {} is gone", cp.client)).code("plan_stale"));
            }
            if self.planner.plan_client(&cat, &cp.client, &[])?.plan.digest != cp.digest {
                return Err(
                    Problem::conflict("live files changed since this plan was made; plan again").code("plan_stale"),
                );
            }
        }
        let mut results = Vec::new();
        for cp in &plan.clients {
            let r = self
                .applier
                .apply_client(&cat, &tree, &cp.client, Some(&cp.digest), &adopt, &plan.id)?;
            if let Some(mut lk) = self.locks.load(&cp.client)
                && (r.status == "applied" || r.status == "nothing")
            {
                lk.plan_id.clone_from(&plan.id);
                if let Some(ok) = r.verify_ok {
                    lk.verified_at = Some(now());
                    lk.verified_ok = Some(ok);
                }
                self.locks.save(&mut lk, false)?;
            }
            self.telemetry.applies.inc(&[&r.status]);
            let paths = |v: &[Value]| -> Vec<Value> { v.iter().take(50).map(|w| w["path"].clone()).collect() };
            self.audit_svc.event(
                actor,
                "apply",
                &json!({"plan": plan.id, "client": cp.client, "status": r.status,
                        "written": paths(&r.written), "removed": paths(&r.removed),
                        "conflicts": r.skipped_conflicts.len(), "verify_ok": r.verify_ok}),
            )?;
            results.push(to_value(&r));
        }
        Ok(json!({"plan_id": plan.id, "content_hash": tree, "results": results}))
    }

    pub fn list_applies(&self, cid: &str) -> HubResult<Value> {
        if !self.work.names_of("clients")?.iter().any(|c| c == cid) {
            return Err(Problem::not_found(format!("no such client: {cid}")));
        }
        let items: Vec<Value> = self.applies.list(cid)?.iter().map(ApplyLog::summary).collect();
        Ok(json!({"items": items, "next_cursor": null}))
    }

    /// Undo an apply (default: the latest). Files edited since are skipped and reported unless named in force_paths.
    pub fn rollback(
        &self,
        cid: &str,
        apply_id: Option<&str>,
        confirm: bool,
        dry_run: bool,
        force_paths: &[String],
        actor: &str,
    ) -> HubResult<Value> {
        if !dry_run && !confirm {
            return Err(
                Problem::unprocessable("rollback needs confirm: true (use dry_run: true to preview the diff)")
                    .code("confirm_required"),
            );
        }
        let _g = self.lock.lock();
        let (_, cat) = self.approved_client(cid)?;
        let res = self.resolver.resolve(&cat, cid)?;
        let bad_root = res.diagnostics.iter().any(|d| d.code == "bad_root");
        let Some(cfg) = res.cfg.as_ref().filter(|_| !bad_root) else {
            return Err(Problem::conflict("the client's roots are not usable; cannot roll back").code("bad_root"));
        };
        let force: BTreeSet<String> = force_paths.iter().cloned().collect();
        let mut out = self.rollbacker.run(cfg, cid, apply_id, dry_run, &force)?;
        if !dry_run {
            let files: Vec<Value> = out["changed"]
                .as_array()
                .map(|a| a.iter().take(50).map(|c| c["path"].clone()).collect())
                .unwrap_or_default();
            let n_conf = out["conflicts"].as_array().map_or(0, Vec::len);
            self.audit_svc.event(
                actor,
                "rollback",
                &json!({"client": cid, "apply": out.get("apply_id").cloned().unwrap_or(Value::Null),
                        "status": out["status"], "files": files, "conflicts": n_conf}),
            )?;
            if matches!(out["status"].as_str(), Some("rolled_back" | "partially_rolled_back")) {
                let v = match self.verify(cid) {
                    Ok(v) => v,
                    // Python catches Conflict and Unavailable only
                    Err(p) if p.status == 409 || p.status == 503 => json!({"ok": null, "detail": p.detail}),
                    Err(p) => return Err(p),
                };
                if let Some(m) = out.as_object_mut() {
                    m.insert("verify".into(), v);
                }
            }
        }
        Ok(out)
    }

    pub fn drift(&self, cid: &str) -> HubResult<Value> {
        let (_, cat) = self.approved_client(cid)?;
        let mut d = self.planner.drift(&self.planner.plan_client(&cat, cid, &[])?)?;
        if let Some(m) = d.as_object_mut() {
            m.insert("observed".into(), self.observe_svc.summary(cid).unwrap_or(Value::Null));
        }
        Ok(d)
    }

    pub fn verify(&self, cid: &str) -> HubResult<Value> {
        let (_, cat) = self.approved_client(cid)?;
        let res = self.resolver.resolve(&cat, cid)?;
        let Some(mut lock) = self.locks.load(cid) else {
            return Err(Problem::conflict("nothing has been applied to this client yet").code("never_applied"));
        };
        let (Some(adapter), Some(cfg)) = (&res.adapter, &res.cfg) else {
            return Err(Problem::unavailable("adapter not installed").code("adapter_missing"));
        };
        let expected: Vec<Expected> = lock
            .files
            .values()
            .map(|e| Expected {
                root: e.root.clone(),
                path: e.path.clone(),
                sha256: if e.merge == "own" {
                    e.sha256.clone()
                } else {
                    e.slice_sha256.clone()
                },
                kind: e.kind.clone(),
                source_ids: e.source_ids.clone(),
                merge: e.merge.clone(),
                managed_keys: e.managed_keys.clone(),
            })
            .collect();
        let roots: Vec<PathBuf> = cfg.roots.values().map(PathBuf::from).collect();
        let checks = adapter
            .verify(cfg, &expected, &RealFs::new(&roots, false), self.runner.as_ref())
            .map_err(|e| {
                Problem::unavailable(format!("adapter verify failed ({})", e.py_type)).code("verify_failed")
            })?;
        let ok = checks.iter().all(|c| c.ok);
        lock.verified_at = Some(now());
        lock.verified_ok = Some(ok);
        self.locks.save(&mut lock, false)?;
        let checks: Vec<Value> = checks.iter().map(to_value).collect();
        Ok(json!({"client": cid, "ok": ok, "checks": checks}))
    }

    /// The plans dir.
    pub fn plans_dir(&self) -> PathBuf {
        self.cfg().data_dir.join("plans")
    }
}
