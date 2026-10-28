//! Port of services/lock.py: the lock manifest (what the hub last delivered to each client) at
//! `<data>/locks/<client>.json`, written `model_dump_json(indent=2) + "\n"` (0600, dir 0700, atomic). A legacy lock
//! (import-only, no plan) is read as an adoption. Owner: WS-B2. PHASE-1: types locked, store stubbed.

use std::path::{Path, PathBuf};

use hub_core::HubResult;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

fn own() -> String {
    "own".into()
}
fn mode_644() -> u32 {
    0o644
}
fn one() -> i64 {
    1
}

/// `LockEntry` (pydantic field order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockEntry {
    pub root: String,
    pub path: String,
    pub kind: String,
    /// whole file as delivered
    pub sha256: String,
    /// the hub-owned slice (== sha256 for `own`)
    pub slice_sha256: String,
    #[serde(default = "own")]
    pub merge: String,
    #[serde(default)]
    pub managed_keys: Vec<String>,
    /// the hub created this file (only then may it ever delete it)
    #[serde(default)]
    pub created: bool,
    #[serde(default)]
    pub source_ids: Vec<String>,
    #[serde(default = "mode_644")]
    pub mode: u32,
}

/// `Lock` (pydantic field order).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lock {
    #[serde(default = "one")]
    pub version: i64,
    pub client: String,
    #[serde(default)]
    pub applied_at: f64,
    #[serde(default)]
    pub content_hash: String,
    /// key "root:path"
    #[serde(default)]
    pub files: IndexMap<String, LockEntry>,
    /// when import adopted the live files (NOT an apply)
    #[serde(default)]
    pub adopted_at: f64,
    #[serde(default)]
    pub plan_id: String,
    #[serde(default)]
    pub verified_at: Option<f64>,
    #[serde(default)]
    pub verified_ok: Option<bool>,
}

impl Lock {
    pub fn new(client: &str) -> Self {
        Self {
            version: 1,
            client: client.into(),
            applied_at: 0.0,
            content_hash: String::new(),
            files: IndexMap::new(),
            adopted_at: 0.0,
            plan_id: String::new(),
            verified_at: None,
            verified_ok: None,
        }
    }
}

pub struct LockStore {
    pub dir: PathBuf,
}

impl LockStore {
    pub fn new(dir: &Path) -> Self {
        Self { dir: dir.to_path_buf() }
    }

    /// `_path(client)`: ValueError("bad client id") for an invalid id.
    fn path(&self, client: &str) -> Option<PathBuf> {
        hub_core::ids::valid_id(client).then(|| self.dir.join(format!("{client}.json")))
    }

    /// None when absent or unreadable/invalid. A legacy lock written by an import (no plan was ever applied) reads its
    /// `applied_at` as an adoption.
    pub fn load(&self, client: &str) -> Option<Lock> {
        let text = std::fs::read(self.path(client)?).ok()?;
        let text = String::from_utf8(text).ok()?;
        let mut lock: Lock = serde_json::from_str(&text).ok()?;
        if lock.content_hash.is_empty() && lock.plan_id.is_empty() && lock.applied_at != 0.0 && lock.adopted_at == 0.0 {
            lock.adopted_at = lock.applied_at;
            lock.applied_at = 0.0;
        }
        Some(lock)
    }

    /// `touch` stamps `applied_at = now()` first. Written `model_dump_json(indent=2) + "\n"`, 0600, dir 0700, atomic.
    pub fn save(&self, lock: &mut Lock, touch: bool) -> HubResult<()> {
        if touch {
            lock.applied_at = hub_core::common::now();
        }
        let Some(path) = self.path(&lock.client) else {
            return Err(hub_core::Problem::internal()); // Python: ValueError("bad client id"), unhandled
        };
        let v = serde_json::to_value(&*lock).map_err(|_| hub_core::Problem::internal())?;
        let text = hub_core::py::json::pydantic(&v, Some(2)) + "\n";
        hub_core::common::atomic_write(&path, text.as_bytes(), 0o600, 0o700)?;
        Ok(())
    }

    pub fn delete(&self, client: &str) {
        if let Some(p) = self.path(client) {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    type R = Result<(), Box<dyn std::error::Error>>;

    /// Bytes as pydantic's `Lock.model_dump_json(indent=2) + "\n"` (generated with the Python model, see
    /// tests/golden/gen_delivery.py `lock`).
    #[test]
    fn save_matches_pydantic_bytes_and_modes() -> R {
        let d = tempfile::tempdir()?;
        let store = LockStore::new(&d.path().join("locks"));
        let mut lock = Lock::new("fake1");
        lock.applied_at = 1761600000.25;
        lock.content_hash = "abc".into();
        lock.plan_id = "plan_0123456789ab".into();
        lock.verified_ok = Some(true);
        lock.verified_at = Some(1761600001.5);
        lock.files.insert(
            "home:CLAUDE.md".into(),
            LockEntry {
                root: "home".into(),
                path: "CLAUDE.md".into(),
                kind: "instruction".into(),
                sha256: "s1".into(),
                slice_sha256: "s2".into(),
                merge: "block".into(),
                managed_keys: vec![],
                created: true,
                source_ids: vec!["style".into()],
                mode: 0o644,
            },
        );
        store.save(&mut lock, false)?;
        let p = d.path().join("locks/fake1.json");
        let want = include_str!("../../tests/golden/lock_fake1.json");
        assert_eq!(std::fs::read_to_string(&p)?, want);
        assert_eq!(std::fs::metadata(&p)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(d.path().join("locks"))?.permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(store.load("fake1"), Some(lock.clone()));
        store.save(&mut lock, true)?;
        assert!(store.load("fake1").is_some_and(|l| l.applied_at > 1761600000.25));
        Ok(())
    }

    #[test]
    fn legacy_and_invalid_locks() -> R {
        let d = tempfile::tempdir()?;
        let store = LockStore::new(d.path());
        std::fs::write(
            d.path().join("fake1.json"),
            r#"{"version": 1, "client": "fake1", "applied_at": 1700000000.0, "content_hash": "", "files": {}}"#,
        )?;
        let lk = store.load("fake1").ok_or("not loaded")?;
        assert_eq!((lk.adopted_at, lk.applied_at), (1700000000.0, 0.0));
        std::fs::write(d.path().join("bad.json"), "{not json")?;
        assert!(store.load("bad").is_none());
        assert!(store.load("../x").is_none() && store.load("missing").is_none());
        std::fs::write(d.path().join("noclient.json"), "{}")?;
        assert!(store.load("noclient").is_none());
        store.delete("fake1");
        assert!(store.load("fake1").is_none());
        Ok(())
    }
}
