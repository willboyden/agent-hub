//! services/hub.py: the Hub, one facade over the content repo, resolver, planner, applier and stores. The HTTP API and
//! `hubctl` are thin layers over it. Content mutations take one (reentrant) lock.
//!
//! The facade's methods are split by owner (PORTING.md module map): this file (CORE) holds construction, the lock,
//! settings and adapter views; `content_ops.rs` (WS-B1), `delivery_ops.rs` (WS-B2), `policy_ops.rs` (WS-F) and
//! `eval_ops.rs` (WS-C) add `impl Hub` blocks with the remaining methods, each with its locked signature.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::ThreadId;

use hub_core::common::harden_tree;
use hub_core::{HubResult, Problem};
use indexmap::IndexMap;
use serde_json::{Map, Value};

pub mod content_ops;
pub mod delivery_ops;
pub mod eval_ops;
pub mod policy_ops;

use super::applier::Applier;
use super::applylog::ApplyLog;
use super::attest::Attestor;
use super::audit::AuditService;
use super::auth::AuthService;
use super::content::ContentStore;
use super::floor::{Floor, load_floor};
use super::fs::{RealRunner, StreamingRunner};
use super::gitrepo::GitRepo;
use super::importer::Importer;
use super::inbox::Inbox;
use super::knowledge::KnowledgeProxy;
use super::lock::LockStore;
use super::observe::ObserveService;
use super::organize::Organizer;
use super::planner::Planner;
use super::resolver::Resolver;
use super::rollback::Rollbacker;
use super::skilleval::{EvalRunner, SkillEvalService};
use super::store::Store;
use super::targets::TargetGuard;
use crate::adapters::AdapterRegistry;
use crate::adapters::base::{ClientAdapter, CommandRunner};
use crate::config::{EVAL_GATES, EVAL_TIERS, Settings, app_root};
use crate::telemetry::Telemetry;

pub const SNAPSHOTS_KEEP: usize = 5;
pub const SELECT_CAP: usize = 2000;

/// Python's `threading.RLock` (the hub lock is re-entered on the same thread, e.g. rollback -> verify).
#[derive(Default)]
pub struct HubLock {
    owner: Mutex<Option<(ThreadId, usize)>>,
    cv: Condvar,
}

pub struct HubLockGuard<'a> {
    lock: &'a HubLock,
}

impl HubLock {
    pub fn lock(&self) -> HubLockGuard<'_> {
        let me = std::thread::current().id();
        let mut g = self.owner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            match *g {
                None => {
                    *g = Some((me, 1));
                    break;
                }
                Some((t, n)) if t == me => {
                    *g = Some((t, n + 1));
                    break;
                }
                Some(_) => g = self.cv.wait(g).unwrap_or_else(std::sync::PoisonError::into_inner),
            }
        }
        HubLockGuard { lock: self }
    }
}

impl Drop for HubLockGuard<'_> {
    fn drop(&mut self) {
        let mut g = self
            .lock
            .owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((t, n)) = *g {
            *g = if n > 1 { Some((t, n - 1)) } else { None };
        }
        drop(g);
        self.lock.cv.notify_one();
    }
}

/// Test seams (Python `Hub(cfg, registry=, runner=, knowledge_transport=, eval_runner=, observe_runner=)`).
/// The runner `observe` gets when none is injected: a big JSON document on stdout, so 120 s and 48 MiB (F4).
pub fn default_observe_runner() -> StreamingRunner {
    StreamingRunner::new(120.0, 48 * 1024 * 1024, 1024 * 1024)
}

/// Missing runners default to the real ones: `RealRunner` and [`default_observe_runner`] (addendum A-4).
#[derive(Default)]
pub struct HubOptions {
    pub registry: Option<AdapterRegistry>,
    pub eval_runner: Option<Arc<dyn EvalRunner>>,
    /// `runner=`: adapters' verify commands (and, after A-4 lands in WS-B2, the applier's)
    pub runner: Option<Arc<dyn CommandRunner>>,
    /// `observe_runner=`: `observe` (a big JSON document on stdout)
    pub observe_runner: Option<Arc<dyn CommandRunner>>,
}

pub struct Hub {
    /// Settings; PUT /settings mutates a few fields at run time (Python `setattr(self.cfg, k, v)`).
    pub cfg: RwLock<Settings>,
    pub registry: AdapterRegistry,
    pub floor: Arc<Floor>,
    pub store: Arc<Store>,
    pub audit_svc: AuditService,
    pub auth: AuthService,
    pub telemetry: Telemetry,
    pub repo: GitRepo,
    pub attestor: Arc<Attestor>,
    pub work: Arc<ContentStore>,
    pub locks: Arc<LockStore>,
    pub guard: Arc<TargetGuard>,
    pub resolver: Arc<Resolver>,
    pub planner: Arc<Planner>,
    pub runner: Arc<dyn CommandRunner>,
    pub observe_runner: Arc<dyn CommandRunner>,
    pub applies: Arc<ApplyLog>,
    pub applier: Arc<Applier>,
    pub rollbacker: Rollbacker,
    pub organizer: Arc<Organizer>,
    pub importer: Importer,
    pub inbox: Inbox,
    pub knowledge: KnowledgeProxy,
    pub lock: HubLock,
    /// plan id -> plan (in-memory cache of <data>/plans/*.json)
    pub plans: Mutex<IndexMap<String, hub_core::models::Plan>>,
    pub skilleval: SkillEvalService,
    pub observe_svc: ObserveService,
}

#[allow(clippy::needless_pass_by_value)] // used as `map_err(startup_io)`
fn startup_io(e: std::io::Error) -> Problem {
    Problem::unavailable(format!("cannot prepare the data directory: {e}")).code("data_dir_unusable")
}

impl Hub {
    /// `Hub(cfg)`: loads the floor (fail closed), creates the private state dirs (0700, tightened on every start),
    /// opens the DB and wires every service.
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(cfg: Settings, opts: HubOptions) -> HubResult<Arc<Hub>> {
        let registry = opts.registry.unwrap_or_else(crate::adapters::load_registry);
        let mut policy = load_floor(&cfg.policy_file)?;
        // the hub's own state is off-limits to every managed client, wherever data_dir points
        let g = format!("{}/**", cfg.data_dir.display());
        for lst in [&mut policy.deny_path_read, &mut policy.deny_path_write] {
            if !lst.contains(&g) {
                lst.push(g.clone());
            }
        }
        std::fs::create_dir_all(&cfg.data_dir).map_err(startup_io)?;
        std::fs::set_permissions(&cfg.data_dir, std::fs::Permissions::from_mode(0o700)).map_err(startup_io)?;
        for sub in ["plans", "backups", "snapshots", "locks", "applies"] {
            let d = cfg.data_dir.join(sub);
            if !d.exists() {
                std::fs::create_dir(&d).map_err(startup_io)?;
                std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).map_err(startup_io)?;
            }
        }
        harden_tree(&cfg.data_dir);
        let store = Arc::new(
            Store::open(&cfg.db_path()).map_err(|e| Problem::unavailable(format!("cannot open the database: {e}")))?,
        );
        let app = app_root();
        let floor = Arc::new(Floor::new(policy.clone()));
        let attestor = Arc::new(Attestor::new(&cfg.data_dir.join("attest.key")));
        let work = Arc::new(ContentStore::new(
            &cfg.content_dir,
            u64::try_from(cfg.max_content_file_bytes).unwrap_or(0),
            u64::try_from(cfg.max_skill_bytes).unwrap_or(0),
            Some(attestor.clone()),
        ));
        let locks = Arc::new(LockStore::new(&cfg.data_dir.join("locks")));
        let guard = Arc::new(TargetGuard::new(&policy, &cfg.data_dir, &cfg.content_dir, &app, None));
        let resolver = Arc::new(Resolver::new(
            floor.clone(),
            registry.clone(),
            Some(guard.clone()),
            Some(attestor.clone()),
        ));
        let planner = Arc::new(Planner::new(
            resolver.clone(),
            registry.clone(),
            locks.clone(),
            u64::try_from(cfg.max_live_file_bytes).unwrap_or(0),
        ));
        let runner: Arc<dyn CommandRunner> = opts.runner.unwrap_or_else(|| Arc::new(RealRunner::default()));
        let observe_runner: Arc<dyn CommandRunner> = opts
            .observe_runner
            .unwrap_or_else(|| Arc::new(default_observe_runner()));
        let applies = Arc::new(ApplyLog::new(&cfg.data_dir.join("applies")));
        let applier = Arc::new(Applier::new(
            planner.clone(),
            locks.clone(),
            &cfg.data_dir.join("backups"),
            runner.clone(),
            applies.clone(),
        ));
        let rollbacker = Rollbacker::new(applier.clone(), applies.clone(), locks.clone());
        let organizer = Arc::new(Organizer::new(work.clone(), resolver.clone(), registry.clone()));
        let importer = Importer::new(
            work.clone(),
            registry.clone(),
            floor.clone(),
            organizer.clone(),
            &cfg.discover_extra_roots,
        );
        let inbox = Inbox::new(work.clone(), cfg.inbox_rate_per_min, cfg.inbox_max_per_client);
        let knowledge = KnowledgeProxy::new(&cfg);
        let repo = GitRepo::new(
            &cfg.content_dir,
            &cfg.git_bin,
            &cfg.git_author_name,
            &cfg.git_author_email,
            cfg.git_timeout_s,
        );
        let data_dir = cfg.data_dir.clone();
        let content_dir = cfg.content_dir.clone();
        let max_skill = u64::try_from(cfg.max_skill_bytes).unwrap_or(0);
        Ok(Arc::new_cyclic(|weak| Hub {
            registry,
            floor,
            audit_svc: AuditService::new(store.clone()),
            auth: AuthService::new(store.clone()),
            store,
            telemetry: Telemetry::new(),
            repo,
            attestor,
            work,
            locks,
            guard,
            resolver,
            planner,
            runner,
            observe_runner,
            applies,
            applier,
            rollbacker,
            organizer,
            importer,
            inbox,
            knowledge,
            lock: HubLock::default(),
            plans: Mutex::new(IndexMap::new()),
            skilleval: SkillEvalService::new(
                &data_dir.join("skilleval"),
                &content_dir,
                weak.clone(),
                opts.eval_runner,
                super::content::MAX_SKILL_FILES,
                max_skill,
                &data_dir.join("observed"),
            ),
            observe_svc: ObserveService::new(
                &data_dir.join("observed"),
                weak.clone(),
                Some(&data_dir.join("skilleval")),
            ),
            cfg: RwLock::new(cfg),
        }))
    }

    /// A snapshot of the current settings.
    pub fn cfg(&self) -> Settings {
        self.cfg
            .read()
            .map(|c| c.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// `with self.mutating():` serialise content mutations and refuse them when the content repo does not exist yet.
    pub fn mutating(&self) -> HubResult<HubLockGuard<'_>> {
        let g = self.lock.lock();
        self.require_repo()?;
        Ok(g)
    }

    /// `close()`.
    pub fn close(&self) {
        self.skilleval.shutdown();
    }

    // -- settings (CORE) -------------------------------------------------------------------------------------------
    pub const EDITABLE: [(&'static str, i64, i64); 3] = [
        ("plan_retention", 1, 200),
        ("inbox_max_per_client", 1, 5000),
        ("inbox_rate_per_min", 1, 600),
    ];
    pub const ENUMS: [(&'static str, &'static [&'static str]); 3] = [
        ("eval_gate", &EVAL_GATES),
        ("eval_gate_observed", &EVAL_GATES),
        ("eval_gate_min_tiers", &EVAL_TIERS),
    ];

    fn cfg_int(c: &Settings, key: &str) -> i64 {
        match key {
            "plan_retention" => c.plan_retention,
            "inbox_max_per_client" => c.inbox_max_per_client,
            "inbox_rate_per_min" => c.inbox_rate_per_min,
            _ => 0,
        }
    }

    fn cfg_str(c: &Settings, key: &str) -> String {
        match key {
            "eval_gate" => c.eval_gate.clone(),
            "eval_gate_observed" => c.eval_gate_observed.clone(),
            "eval_gate_min_tiers" => c.eval_gate_min_tiers.clone(),
            _ => String::new(),
        }
    }

    fn stored(&self, key: &str) -> Option<Value> {
        let row = self
            .store
            .one(
                "SELECT value FROM settings WHERE key=?",
                &[rusqlite::types::Value::Text(key.into())],
            )
            .ok()??;
        row.get("value")
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str(s).ok())
    }

    /// `_setting(key)`: the stored int, else the configured one.
    pub fn setting(&self, key: &str) -> i64 {
        match self.stored(key) {
            Some(v) => v
                .as_i64()
                .or_else(|| {
                    // Python int(float): truncation toward zero is the intended semantics
                    #[allow(clippy::cast_possible_truncation)]
                    v.as_f64().map(|f| f.trunc() as i64)
                })
                .unwrap_or(0),
            None => Self::cfg_int(&self.cfg(), key),
        }
    }

    /// `_setting_str(key)`: the stored enum value when still allowed, else the configured default.
    pub fn setting_str(&self, key: &str) -> String {
        let allowed = Self::ENUMS
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| *v)
            .unwrap_or(&[]);
        if let Some(Value::String(s)) = self.stored(key)
            && allowed.contains(&s.as_str())
        {
            return s;
        }
        Self::cfg_str(&self.cfg(), key)
    }

    /// GET /settings.
    pub fn settings(&self) -> Value {
        let c = self.cfg();
        let mut eff = Map::new();
        for (k, _, _) in Self::EDITABLE {
            eff.insert(k.into(), Value::from(self.setting(k)));
        }
        for (k, _) in Self::ENUMS {
            eff.insert(k.into(), Value::String(self.setting_str(k)));
        }
        let enums: Map<String, Value> = Self::ENUMS
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_string(),
                    Value::Array(v.iter().map(|s| Value::String((*s).into())).collect()),
                )
            })
            .collect();
        serde_json::json!({
            "editable": eff,
            "enums": enums,
            "info": {
                "content_dir": c.content_dir.to_string_lossy(),
                "data_dir": c.data_dir.to_string_lossy(),
                "policy_file": c.policy_file.to_string_lossy(),
                "host": c.host,
                "port": c.port,
                "knowledge": self.knowledge.status(),
                "adapters": self.registry.ids(),
                "adapter_load_errors": self.registry.load_errors,
                "trust_loopback": c.trust_loopback,
            }
        })
    }

    /// PUT /settings: validate everything first (a bad value changes nothing), then store and apply.
    pub fn update_settings(&self, patch: &Map<String, Value>) -> HubResult<Value> {
        let mut unknown: Vec<&String> = patch
            .keys()
            .filter(|k| !Self::EDITABLE.iter().any(|(e, _, _)| e == k) && !Self::ENUMS.iter().any(|(e, _)| e == k))
            .collect();
        if !unknown.is_empty() {
            unknown.sort();
            let list = unknown
                .iter()
                .map(|s| hub_core::py::repr(s))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Problem::unprocessable(format!(
                "unknown or read-only settings: [{list}]"
            )));
        }
        let int_of = |v: &Value| {
            if v.is_boolean() {
                None
            } else {
                v.as_i64().filter(|_| v.is_i64() || v.is_u64())
            }
        };
        for (k, v) in patch {
            if let Some((_, allowed)) = Self::ENUMS.iter().find(|(e, _)| e == k)
                && !v.as_str().is_some_and(|s| allowed.contains(&s))
            {
                return Err(Problem::unprocessable(format!(
                    "{k} must be one of {}",
                    allowed.join(", ")
                )));
            }
            if let Some((_, lo, hi)) = Self::EDITABLE.iter().find(|(e, _, _)| e == k)
                && !int_of(v).is_some_and(|i| *lo <= i && i <= *hi)
            {
                return Err(Problem::unprocessable(format!("{k} must be an integer in {lo}..{hi}")));
            }
        }
        for (k, v) in patch {
            let text = hub_core::py::json::dumps(v, &hub_core::py::json::Opts::default());
            self.store
                .execute(
                    "INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    &[
                        rusqlite::types::Value::Text(k.clone()),
                        rusqlite::types::Value::Text(text),
                    ],
                )
                .map_err(|_| Problem::internal())?;
            let mut c = self.cfg.write().unwrap_or_else(std::sync::PoisonError::into_inner);
            match k.as_str() {
                "eval_gate" => c.eval_gate = v.as_str().unwrap_or_default().into(),
                "eval_gate_observed" => c.eval_gate_observed = v.as_str().unwrap_or_default().into(),
                "eval_gate_min_tiers" => c.eval_gate_min_tiers = v.as_str().unwrap_or_default().into(),
                "plan_retention" => c.plan_retention = int_of(v).unwrap_or(c.plan_retention),
                "inbox_max_per_client" => {
                    c.inbox_max_per_client = int_of(v).unwrap_or(c.inbox_max_per_client);
                    self.inbox.set_max_per_client(c.inbox_max_per_client);
                }
                "inbox_rate_per_min" => {
                    c.inbox_rate_per_min = int_of(v).unwrap_or(c.inbox_rate_per_min);
                    self.inbox.set_rate(c.inbox_rate_per_min);
                }
                _ => {}
            }
        }
        Ok(self.settings())
    }

    // -- adapters (CORE) -------------------------------------------------------------------------------------------
    /// GET /adapters items.
    pub fn adapters_view(&self) -> Vec<Value> {
        self.registry
            .all()
            .iter()
            .map(|a| Self::adapter_view(a.as_ref()))
            .collect()
    }

    pub fn adapter_view(a: &dyn ClientAdapter) -> Value {
        let docs: String = a.docs().trim().chars().take(4000).collect();
        serde_json::json!({
            "id": a.id(),
            "display_name": a.display_name(),
            "caps": serde_json::to_value(a.caps()).unwrap_or(Value::Null),
            "default_config": serde_json::to_value(a.default_config()).unwrap_or(Value::Null),
            "docs": docs,
        })
    }

    /// `sha(data)`.
    pub fn sha(&self, data: &[u8]) -> String {
        hub_core::common::sha256(data)
    }

    /// The app root (policy, frontend, deploy scripts).
    pub fn app_root(&self) -> PathBuf {
        app_root()
    }
}
