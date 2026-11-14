//! The hub's settings model (config.py `Settings`): `HUB_*` names, pydantic field order and defaults, and the loader
//! that turns an environment plus env-file bindings into [`Settings`]. Moved verbatim from `agent_hub::config` (which
//! re-exports it and keeps everything that touches the process or the disk: the app root, `$HOME`, reading the env
//! file, the bind check) so the config editor's env check also runs in the browser (crates/hub-wasm). Pure: no file,
//! no process environment; the caller passes the environment and how to find the app root. Owner: CORE (LOCKED).

use std::collections::HashMap;
use std::path::PathBuf;

use crate::dotenv;
use crate::pyd::{self, Extra, Lit, Model, Ty, ValidationError, optional};
use crate::value::PyValue;

pub const EVAL_GATES: [&str; 3] = ["off", "advisory", "require"];
pub const EVAL_TIERS: [&str; 3] = ["static", "tier1", "tier1+2"];

/// The hub's settings. `Debug` leaves out `knowledge_token` and `knowledge_admin_key` (pydantic `repr=False`).
#[derive(Clone, PartialEq)]
pub struct Settings {
    pub host: String,
    pub port: i64,
    pub content_dir: PathBuf,
    pub data_dir: PathBuf,
    pub policy_file: PathBuf,
    pub frontend_dir: Option<PathBuf>,
    pub git_bin: String,
    pub git_timeout_s: f64,
    pub git_author_name: String,
    pub git_author_email: String,
    pub allowed_hosts: Vec<String>,
    pub trust_loopback: bool,
    pub body_cap_bytes: i64,
    pub import_body_cap_bytes: i64,
    pub max_sse_connections: i64,
    pub sse_idle_timeout_s: f64,
    pub max_content_file_bytes: i64,
    pub max_skill_bytes: i64,
    pub max_live_file_bytes: i64,
    pub knowledge_url: String,
    pub knowledge_token: Option<String>,
    pub knowledge_admin_key: Option<String>,
    pub knowledge_admin_key_file: PathBuf,
    pub knowledge_timeout_s: f64,
    pub knowledge_body_cap_bytes: i64,
    pub knowledge_response_cap_bytes: i64,
    pub discover_extra_roots: Vec<String>,
    pub inbox_rate_per_min: i64,
    pub inbox_max_per_client: i64,
    pub plan_retention: i64,
    pub eval_gate: String,
    pub eval_gate_observed: String,
    pub eval_gate_min_tiers: String,
    pub enable_metrics: bool,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("content_dir", &self.content_dir)
            .field("data_dir", &self.data_dir)
            .field("policy_file", &self.policy_file)
            .field("frontend_dir", &self.frontend_dir)
            .field("git_bin", &self.git_bin)
            .field("git_timeout_s", &self.git_timeout_s)
            .field("git_author_name", &self.git_author_name)
            .field("git_author_email", &self.git_author_email)
            .field("allowed_hosts", &self.allowed_hosts)
            .field("trust_loopback", &self.trust_loopback)
            .field("body_cap_bytes", &self.body_cap_bytes)
            .field("import_body_cap_bytes", &self.import_body_cap_bytes)
            .field("max_sse_connections", &self.max_sse_connections)
            .field("sse_idle_timeout_s", &self.sse_idle_timeout_s)
            .field("max_content_file_bytes", &self.max_content_file_bytes)
            .field("max_skill_bytes", &self.max_skill_bytes)
            .field("max_live_file_bytes", &self.max_live_file_bytes)
            .field("knowledge_url", &self.knowledge_url)
            .field("knowledge_admin_key_file", &self.knowledge_admin_key_file)
            .field("knowledge_timeout_s", &self.knowledge_timeout_s)
            .field("knowledge_body_cap_bytes", &self.knowledge_body_cap_bytes)
            .field("knowledge_response_cap_bytes", &self.knowledge_response_cap_bytes)
            .field("discover_extra_roots", &self.discover_extra_roots)
            .field("inbox_rate_per_min", &self.inbox_rate_per_min)
            .field("inbox_max_per_client", &self.inbox_max_per_client)
            .field("plan_retention", &self.plan_retention)
            .field("eval_gate", &self.eval_gate)
            .field("eval_gate_observed", &self.eval_gate_observed)
            .field("eval_gate_min_tiers", &self.eval_gate_min_tiers)
            .field("enable_metrics", &self.enable_metrics)
            .finish_non_exhaustive()
    }
}

impl Settings {
    /// `db_path`.
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("hub.db")
    }
}

fn gate_lits() -> &'static [Lit] {
    &[Lit::S("off"), Lit::S("advisory"), Lit::S("require")]
}

fn s(v: &str) -> PyValue {
    PyValue::str(v)
}

/// The Settings schema in pydantic field order (defaults that depend on HOME are filled in [`settings_from_sources`]).
fn settings_model() -> Model {
    let path = || Ty::str(); // pydantic Path accepts str input; outputs are stringified paths
    Model {
        name: "Settings",
        fields: vec![
            optional("host", Ty::str(), || s("127.0.0.1")),
            optional("port", Ty::int(), || PyValue::Int(8792)),
            optional("content_dir", path(), || PyValue::None),
            optional("data_dir", path(), || PyValue::None),
            optional("policy_file", path(), || PyValue::None),
            optional("frontend_dir", Ty::opt(path()), || PyValue::None),
            optional("git_bin", Ty::str(), || s("git")),
            optional("git_timeout_s", Ty::Float, || PyValue::Float(30.0)),
            optional("git_author_name", Ty::str(), || s("Agent Hub")),
            optional("git_author_email", Ty::str(), || s("agent-hub@localhost")),
            optional("allowed_hosts", Ty::list(Ty::str()), || PyValue::List(vec![])),
            optional("trust_loopback", Ty::Bool, || PyValue::Bool(false)),
            optional("body_cap_bytes", Ty::int(), || PyValue::Int(1024 * 1024)),
            optional("import_body_cap_bytes", Ty::int(), || PyValue::Int(4 * 1024 * 1024)),
            optional("max_sse_connections", Ty::int(), || PyValue::Int(8)),
            optional("sse_idle_timeout_s", Ty::Float, || PyValue::Float(300.0)),
            optional("max_content_file_bytes", Ty::int(), || PyValue::Int(256 * 1024)),
            optional("max_skill_bytes", Ty::int(), || PyValue::Int(4 * 1024 * 1024)),
            optional("max_live_file_bytes", Ty::int(), || PyValue::Int(8 * 1024 * 1024)),
            optional("knowledge_url", Ty::str(), || s("http://127.0.0.1:8795")),
            optional("knowledge_token", Ty::opt(Ty::str()), || PyValue::None),
            optional("knowledge_admin_key", Ty::opt(Ty::str()), || PyValue::None),
            optional("knowledge_admin_key_file", path(), || PyValue::None),
            optional("knowledge_timeout_s", Ty::Float, || PyValue::Float(30.0)),
            optional("knowledge_body_cap_bytes", Ty::int(), || PyValue::Int(1024 * 1024)),
            optional("knowledge_response_cap_bytes", Ty::int(), || {
                PyValue::Int(8 * 1024 * 1024)
            }),
            optional("discover_extra_roots", Ty::list(Ty::str()), || PyValue::List(vec![])),
            optional("inbox_rate_per_min", Ty::int(), || PyValue::Int(20)),
            optional("inbox_max_per_client", Ty::int(), || PyValue::Int(200)),
            optional("plan_retention", Ty::int(), || PyValue::Int(20)),
            optional("eval_gate", Ty::Lit(gate_lits()), || s("advisory")),
            optional("eval_gate_observed", Ty::Lit(gate_lits()), || s("require")),
            optional(
                "eval_gate_min_tiers",
                Ty::Lit(&[Lit::S("static"), Lit::S("tier1"), Lit::S("tier1+2")]),
                || s("static"),
            ),
            optional("enable_metrics", Ty::Bool, || PyValue::Bool(true)),
        ],
        extra: Extra::Ignore,
        check: None,
        transform: None,
    }
}

const LIST_FIELDS: [&str; 2] = ["allowed_hosts", "discover_extra_roots"];

/// Startup failure: pydantic's ValidationError text, or pydantic-settings' JSON parse error for a list field.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("{0}")]
    Invalid(#[from] ValidationError),
    #[error("error parsing value for field \"{0}\" from source \"{1}\"")]
    Parse(String, &'static str),
    /// A draft's `${...}` expansion grew past [`MAX_DRAFT_EXPANSION`] (the config editor's check only).
    #[error("{0}")]
    Expansion(#[from] dotenv::ExpansionTooLarge),
}

/// The total size the resolved values of a checked draft may reach through `${...}` (four times the editor's 64 KiB
/// draft cap). The operator's own file at start is read without a limit, like python-dotenv.
pub const MAX_DRAFT_EXPANSION: usize = 256 * 1024;

/// Explicit values (like `Settings(**overrides)` / `load_settings(**overrides)`), as strings or JSON for lists.
pub type Overrides = Vec<(String, PyValue)>;

/// `$HOME` of a given environment (`/` without one).
pub fn home_from(environ: &HashMap<String, String>) -> PathBuf {
    PathBuf::from(environ.get("HOME").cloned().unwrap_or_else(|| "/".into()))
}

/// The settings a draft env file gives on its own, read exactly as the server reads the real one but with an empty
/// environment: nothing of the server's own environment is merged in or interpolated (`${VAR}` stays unset), so a
/// check of a draft can never reveal it. Used by the config editor's dry run; nothing is read from or written to disk.
/// A `${...}` expansion whose values grow past [`MAX_DRAFT_EXPANSION`] bytes is refused. `app_root` gives the default
/// `policy_file` (`<app root>/policy/floor.yaml`).
pub fn settings_from_env_text(text: &str, app_root: &dyn Fn() -> PathBuf) -> Result<Settings, SettingsError> {
    let environ: HashMap<String, String> = HashMap::new();
    let vals = dotenv::values_bounded(text, &environ, MAX_DRAFT_EXPANSION)?;
    settings_from_sources(&environ, vals, &vec![], app_root)
}

/// The env var names the server reads for a settings field (`HUB_<FIELD>`; `knowledge_admin_key_file` also
/// `KNOWLEDGE_ADMIN_KEY_FILE`), lower-case like the lookup. Every name the hub's settings read, in field order.
pub fn settings_env_names() -> Vec<(&'static str, Vec<String>)> {
    settings_model()
        .fields
        .iter()
        .map(|f| (f.name, env_keys(f.name)))
        .collect()
}

fn env_keys(name: &str) -> Vec<String> {
    if name == "knowledge_admin_key_file" {
        vec!["knowledge_admin_key_file".into(), "hub_knowledge_admin_key_file".into()]
    } else {
        vec![format!("hub_{name}")]
    }
}

/// The settings from the real environment (wins), the env file's bindings and explicit overrides (win over both).
/// `app_root` gives the default `policy_file`.
pub fn settings_from_sources(
    environ: &HashMap<String, String>,
    file_vals: Vec<(String, Option<String>)>,
    overrides: &Overrides,
    app_root: &dyn Fn() -> PathBuf,
) -> Result<Settings, SettingsError> {
    let model = settings_model();
    let field_names: Vec<&str> = model.fields.iter().map(|f| f.name).collect();
    let mut raw: Vec<(String, PyValue, &'static str)> = Vec::new();
    let lower_env: HashMap<String, &String> = environ.iter().map(|(k, v)| (k.to_lowercase(), v)).collect();
    let file_map: HashMap<String, Option<String>> = file_vals.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect();
    for name in &field_names {
        let keys = env_keys(name);
        let found_env = keys
            .iter()
            .find_map(|k| lower_env.get(k).map(|v| ((*v).clone(), "EnvSettingsSource")));
        let found_file = || {
            keys.iter().find_map(|k| {
                file_map
                    .get(k)
                    .and_then(|v| v.clone())
                    .map(|v| (v, "DotEnvSettingsSource"))
            })
        };
        if let Some((val, src)) = found_env.or_else(found_file) {
            let pv = if LIST_FIELDS.contains(name) {
                match serde_json::from_str::<serde_json::Value>(&val) {
                    Ok(j) => PyValue::from_json(&j),
                    Err(_) => return Err(SettingsError::Parse((*name).to_string(), src)),
                }
            } else {
                PyValue::Str(val)
            };
            raw.push(((*name).to_string(), pv, src));
        }
    }
    for (k, v) in overrides {
        raw.retain(|(n, _, _)| n != k);
        raw.push((k.clone(), v.clone(), "init"));
    }
    let given: Vec<String> = raw.iter().map(|(k, _, _)| k.clone()).collect();
    let input = PyValue::Dict(raw.into_iter().map(|(k, v, _)| (PyValue::Str(k), v)).collect());
    let v = pyd::validate(&model, &input)?;
    let get = |k: &str| v.get(k).cloned().unwrap_or(PyValue::None);
    let gs = |k: &str| get(k).as_str().map(str::to_string).unwrap_or_default();
    let gi = |k: &str| if let PyValue::Int(i) = get(k) { i } else { 0 };
    let gf = |k: &str| if let PyValue::Float(f) = get(k) { f } else { 0.0 };
    let gb = |k: &str| matches!(get(k), PyValue::Bool(true));
    let gl = |k: &str| {
        get(k)
            .as_list()
            .map(|l| l.iter().map(PyValue::py_str).collect())
            .unwrap_or_default()
    };
    let go = |k: &str| get(k).as_str().map(str::to_string);
    let home = home_from(environ);
    let data_dir = go("data_dir").map_or_else(|| home.join(".local").join("share").join("agent-hub"), PathBuf::from);
    let content_dir = if given.iter().any(|g| g == "content_dir") {
        PathBuf::from(gs("content_dir"))
    } else {
        data_dir.join("content")
    };
    Ok(Settings {
        host: gs("host"),
        port: gi("port"),
        content_dir,
        data_dir,
        policy_file: go("policy_file").map_or_else(|| app_root().join("policy").join("floor.yaml"), PathBuf::from),
        frontend_dir: go("frontend_dir").map(PathBuf::from),
        git_bin: gs("git_bin"),
        git_timeout_s: gf("git_timeout_s"),
        git_author_name: gs("git_author_name"),
        git_author_email: gs("git_author_email"),
        allowed_hosts: gl("allowed_hosts"),
        trust_loopback: gb("trust_loopback"),
        body_cap_bytes: gi("body_cap_bytes"),
        import_body_cap_bytes: gi("import_body_cap_bytes"),
        max_sse_connections: gi("max_sse_connections"),
        sse_idle_timeout_s: gf("sse_idle_timeout_s"),
        max_content_file_bytes: gi("max_content_file_bytes"),
        max_skill_bytes: gi("max_skill_bytes"),
        max_live_file_bytes: gi("max_live_file_bytes"),
        knowledge_url: gs("knowledge_url"),
        knowledge_token: go("knowledge_token"),
        knowledge_admin_key: go("knowledge_admin_key"),
        knowledge_admin_key_file: go("knowledge_admin_key_file").map_or_else(
            || {
                home.join(".local")
                    .join("share")
                    .join("agent-knowledge")
                    .join("admin.key")
            },
            PathBuf::from,
        ),
        knowledge_timeout_s: gf("knowledge_timeout_s"),
        knowledge_body_cap_bytes: gi("knowledge_body_cap_bytes"),
        knowledge_response_cap_bytes: gi("knowledge_response_cap_bytes"),
        discover_extra_roots: gl("discover_extra_roots"),
        inbox_rate_per_min: gi("inbox_rate_per_min"),
        inbox_max_per_client: gi("inbox_max_per_client"),
        plan_retention: gi("plan_retention"),
        eval_gate: gs("eval_gate"),
        eval_gate_observed: gs("eval_gate_observed"),
        eval_gate_min_tiers: gs("eval_gate_min_tiers"),
        enable_metrics: gb("enable_metrics"),
    })
}

/// Exactly `localhost`, or an IP address that is loopback (127.0.0.0/8, ::1). Used by the bind check, the doctor and
/// the config editor's env check.
pub fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}
