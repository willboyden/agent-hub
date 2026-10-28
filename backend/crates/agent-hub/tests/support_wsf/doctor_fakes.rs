//! test_doctor.py's `FakeFS`, `ctx` and `by_id`, shared by test_doctor.rs and test_doctor_coverage.rs.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agent_hub::config::{Settings, app_root, load_settings_from};
use agent_hub::services::doctor::{CHECKS, Check, DoctorContext, DoctorFs, DoctorStat, run_doctor_with};
use hub_core::value::PyValue;
use serde_json::{Value, json};

use super::E;

const STATE: [&str; 4] = [
    "~/.local/share/agent-hub",
    "~/.config/agent-hub",
    "~/.local/share/agent-knowledge",
    "~/.config/agent-knowledge",
];

pub(crate) fn good_settings() -> Value {
    let mut deny: Vec<String> = STATE
        .iter()
        .flat_map(|p| ["Read", "Write", "Edit"].map(|t| format!("{t}({p}/**)")))
        .collect();
    deny.push("Write(**/hub/policy/**)".into());
    deny.push("Edit(**/hub/policy/**)".into());
    let files: Vec<Value> = STATE.iter().map(|p| json!({"path": p, "mode": "deny"})).collect();
    json!({"sandbox": {"credentials": {"files": files}, "network": {"allowedDomains": []}}, "permissions": {"deny": deny}})
}

/// test_doctor.py `FakeFS`: files (path -> text), directories (path -> (mode, uid)), file modes.
#[derive(Default, Clone)]
pub(crate) struct FakeFs {
    pub(crate) files: HashMap<String, String>,
    pub(crate) modes: HashMap<String, (u32, u32)>,
    pub(crate) file_modes: HashMap<String, u32>,
}

impl FakeFs {
    pub(crate) fn files<P: AsRef<Path>>(files: &[(P, &str)]) -> Self {
        Self {
            files: files
                .iter()
                .map(|(k, v)| (k.as_ref().to_string_lossy().to_string(), (*v).to_string()))
                .collect(),
            ..Self::default()
        }
    }
}

impl DoctorFs for FakeFs {
    fn read_text(&self, path: &Path) -> Option<String> {
        self.files.get(&*path.to_string_lossy()).cloned()
    }
    fn stat(&self, path: &Path) -> Option<DoctorStat> {
        let k = path.to_string_lossy().to_string();
        if let Some((m, u)) = self.modes.get(&k) {
            return Some(DoctorStat {
                mode: 0o040_000 | m,
                uid: *u,
                size: 0,
            });
        }
        self.files.get(&k).map(|t| DoctorStat {
            mode: 0o100_000 | self.file_modes.get(&k).copied().unwrap_or(0o600),
            uid: nix::unistd::getuid().as_raw(),
            size: t.len() as u64,
        })
    }
    fn glob(&self, directory: &Path, pattern: &str) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self
            .files
            .keys()
            .map(PathBuf::from)
            .filter(|k| {
                k.parent() == Some(directory)
                    && k.file_name()
                        .is_some_and(|n| hub_core::py::fnmatchcase(&n.to_string_lossy(), pattern))
            })
            .collect();
        out.sort();
        out
    }
}

/// `permission_gaps`: client id -> missing floor denies.
pub(crate) type GapsFn = Box<dyn Fn(&str) -> Option<Vec<String>> + Send + Sync>;

/// test_doctor.py `ctx(tmp_path, fs, **kw)`: everything a DoctorContext borrows, owned.
pub(crate) struct Ctx {
    pub(crate) cfg: Settings,
    pub(crate) fs: FakeFs,
    pub(crate) env: HashMap<String, String>,
    pub(crate) home: PathBuf,
    pub(crate) clients: Vec<Value>,
    pub(crate) knowledge: Option<Value>,
    pub(crate) gaps: GapsFn,
    pub(crate) floor_read: Vec<String>,
    pub(crate) floor_write: Vec<String>,
    pub(crate) skill_evaluator: Option<Value>,
}

pub(crate) fn ctx(tmp: &Path, fs: FakeFs) -> Result<Ctx, E> {
    let env = HashMap::from([
        ("HOME".to_string(), tmp.join("home").to_string_lossy().to_string()),
        (
            "AGENT_HUB_ENV_FILE".to_string(),
            tmp.join("no-env").to_string_lossy().to_string(),
        ),
    ]);
    let st = tmp.join("state");
    let cfg = load_settings_from(
        &env,
        &vec![
            ("data_dir".into(), PyValue::str(st.to_string_lossy())),
            ("content_dir".into(), PyValue::str(st.join("content").to_string_lossy())),
            ("trust_loopback".into(), PyValue::Bool(false)),
            (
                "knowledge_admin_key_file".into(),
                PyValue::str(st.join("kadmin.key").to_string_lossy()),
            ),
        ],
    )?;
    Ok(Ctx {
        cfg,
        fs,
        env: HashMap::new(),
        home: tmp.join("home"),
        clients: Vec::new(),
        knowledge: None,
        gaps: Box::new(|_| None),
        floor_read: Vec::new(),
        floor_write: Vec::new(),
        skill_evaluator: None,
    })
}

impl Ctx {
    pub(crate) fn run_with(&self, checks: &[Check]) -> Value {
        let c = DoctorContext {
            cfg: &self.cfg,
            fs: &self.fs,
            env: &self.env,
            home: self.home.clone(),
            app_root: app_root(),
            uid: nix::unistd::getuid().as_raw(),
            clients: self.clients.clone(),
            knowledge: self.knowledge.clone(),
            permission_gaps: &*self.gaps,
            floor_read: self.floor_read.clone(),
            floor_write: self.floor_write.clone(),
            skill_evaluator: self.skill_evaluator.clone(),
        };
        run_doctor_with(&c, checks)
    }
    pub(crate) fn run(&self) -> Value {
        self.run_with(&CHECKS)
    }
}

/// `by_id(rep, fid)`.
pub(crate) fn by_id(rep: &Value, fid: &str) -> Vec<Value> {
    rep["findings"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|f| f["id"].as_str().is_some_and(|i| i == fid || i.starts_with(fid)))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}
