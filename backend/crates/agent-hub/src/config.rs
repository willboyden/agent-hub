//! config.py: `HUB_*` environment variables plus the optional per-user env file (`AGENT_HUB_ENV_FILE`, default
//! `~/.config/agent-hub/env`, python-dotenv syntax). Real environment variables win over the file; names are
//! case-insensitive (pydantic-settings `case_sensitive=False`); list fields are JSON; a bad value refuses to start with
//! pydantic's message. Owner: CORE (LOCKED).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hub_core::dotenv;
use hub_core::pyd::{self, Extra, Lit, Model, Ty, ValidationError, optional};
use hub_core::value::PyValue;

/// The repository root (Python: `Path(__file__).resolve().parents[3]`). `AGENT_HUB_APP_ROOT` wins; otherwise the
/// checkout the running binary was built in, found from the executable's own (resolved) location by
/// [`app_root_for_exe`]. DEVIATION D-APPROOT: a Rust binary has no source file next to the policy and frontend at run
/// time; no path of the build machine is compiled in (audit F2).
pub fn app_root() -> PathBuf {
    if let Some(v) = std::env::var_os("AGENT_HUB_APP_ROOT") {
        return PathBuf::from(v);
    }
    let exe = std::env::current_exe()
        .map(|p| hub_core::py::os::realpath(&p))
        .unwrap_or_default();
    app_root_for_exe(&exe)
}

/// Cargo puts the binaries in `<checkout>/backend/target/<profile>/` (`.../<profile>/deps/` for test binaries,
/// `.../<target triple>/<profile>/` when cross-building), so the checkout is the directory above the nearest
/// `backend/target` that contains the executable. A binary moved out of its build tree (or built with a
/// `CARGO_TARGET_DIR` elsewhere) needs `AGENT_HUB_APP_ROOT`; without it the app root is the binary's own directory, where
/// no policy file exists, and the hub refuses to start and names the missing file.
pub fn app_root_for_exe(exe: &Path) -> PathBuf {
    for dir in exe.ancestors().skip(1) {
        let Some(backend) = dir.parent() else { break };
        if dir.file_name().is_some_and(|n| n == "target")
            && backend.file_name().is_some_and(|n| n == "backend")
            && let Some(root) = backend.parent()
        {
            return root.to_path_buf();
        }
    }
    exe.parent().map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
}

/// `Path.home()`.
pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

/// `ENV_FILE` (from the process environment).
pub fn env_file() -> PathBuf {
    let env: HashMap<String, String> = std::env::vars().collect();
    env_file_from(&env)
}

/// `ENV_FILE` for a given environment: `AGENT_HUB_ENV_FILE`, else `$HOME/.config/agent-hub/env`.
pub fn env_file_from(environ: &HashMap<String, String>) -> PathBuf {
    match environ.get("AGENT_HUB_ENV_FILE") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home_from(environ).join(".config").join("agent-hub").join("env"),
    }
}

fn home_from(environ: &HashMap<String, String>) -> PathBuf {
    PathBuf::from(environ.get("HOME").cloned().unwrap_or_else(|| "/".into()))
}

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

/// The Settings schema in pydantic field order (defaults that depend on HOME are filled in [`load_settings`]).
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
}

/// Explicit values (like `Settings(**overrides)` / `load_settings(**overrides)`), as strings or JSON for lists.
pub type Overrides = Vec<(String, PyValue)>;

/// `load_settings(**overrides)` with the given environment (normally `std::env::vars()`).
pub fn load_settings_from(environ: &HashMap<String, String>, overrides: &Overrides) -> Result<Settings, SettingsError> {
    let model = settings_model();
    let field_names: Vec<&str> = model.fields.iter().map(|f| f.name).collect();
    let mut raw: Vec<(String, PyValue, &'static str)> = Vec::new();

    // env file (lowest of the sources), then real env (wins); keys are case-insensitive
    let ef = env_file_from(environ);
    let mut file_vals: Vec<(String, Option<String>)> = Vec::new();
    if ef.is_file()
        && let Ok(text) = std::fs::read_to_string(&ef)
    {
        file_vals = dotenv::values(&text, environ);
    }
    let lower_env: HashMap<String, &String> = environ.iter().map(|(k, v)| (k.to_lowercase(), v)).collect();
    let file_map: HashMap<String, Option<String>> = file_vals.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect();
    for name in &field_names {
        let keys: Vec<String> = if *name == "knowledge_admin_key_file" {
            vec!["knowledge_admin_key_file".into(), "hub_knowledge_admin_key_file".into()]
        } else {
            vec![format!("hub_{name}")]
        };
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

/// `load_settings(**overrides)` from the process environment.
pub fn load_settings(overrides: &Overrides) -> Result<Settings, SettingsError> {
    let env: HashMap<String, String> = std::env::vars().collect();
    load_settings_from(&env, overrides)
}

/// Exactly `localhost`, or an IP address that is loopback (127.0.0.0/8, ::1). Used by [`check_bind`] and the doctor.
pub fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// `check_bind(host)`: refuse a non-loopback bind unless `HUB_ALLOW_NONLOOPBACK=1`. Loopback is exactly `localhost` or
/// an IP address that is loopback (127.0.0.0/8, ::1). Python accepted any host starting with `127.`, so a NAME such as
/// `127.example.org` passed as loopback; it is now refused like any other name (audit F10, deviation D-BIND).
pub fn check_bind(host: &str) -> Result<(), String> {
    if is_loopback_host(host) {
        return Ok(());
    }
    if std::env::var("HUB_ALLOW_NONLOOPBACK").as_deref() != Ok("1") {
        return Err(format!(
            "refusing to bind {}: the hub is loopback-only. Set HUB_ALLOW_NONLOOPBACK=1 to override (and put an authenticating reverse proxy in front)",
            hub_core::py::repr(host)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Audit F2: the app root comes from where the binary is, never from a path compiled into it.
    #[test]
    fn app_root_is_found_from_the_executable() {
        for (exe, root) in [
            ("/srv/hub/backend/target/release/agent-hub", "/srv/hub"),
            ("/srv/hub/backend/target/debug/hubctl", "/srv/hub"),
            ("/srv/hub/backend/target/debug/deps/test_apply-0123abcd", "/srv/hub"),
            (
                "/srv/hub/backend/target/x86_64-unknown-linux-gnu/release/agent-hub",
                "/srv/hub",
            ),
            (
                "/a/backend/target/b/backend/target/release/agent-hub",
                "/a/backend/target/b",
            ), // the nearest one
            ("/usr/local/bin/agent-hub", "/usr/local/bin"), // moved out of its build tree
            ("/srv/target/release/agent-hub", "/srv/target/release"), // no backend/target
        ] {
            assert_eq!(app_root_for_exe(Path::new(exe)), PathBuf::from(root), "{exe}");
        }
        // this test binary itself runs from backend/target/<profile>/deps of the checkout
        assert!(app_root().join("policy/floor.yaml").is_file());
        assert!(app_root().join("backend/Cargo.toml").is_file());
    }

    #[test]
    fn test_check_bind() {
        assert!(check_bind("127.0.0.1").is_ok());
        assert!(check_bind("127.1.2.3").is_ok());
        assert!(check_bind("::1").is_ok());
        assert!(check_bind("localhost").is_ok());
        assert!(check_bind("127.255.255.254").is_ok());
        // audit F10: names that merely start with "127." are not loopback (HUB_ALLOW_NONLOOPBACK is unset in tests)
        for name in [
            "127.evil.example",
            "127.0.0.1.nip.io",
            "127.",
            "0127.0.0.1",
            "localhost.example",
            "::ffff:8.8.8.8",
        ] {
            assert!(check_bind(name).is_err(), "{name}");
        }
        assert_eq!(
            check_bind("0.0.0.0").err().unwrap_or_default(),
            "refusing to bind '0.0.0.0': the hub is loopback-only. Set HUB_ALLOW_NONLOOPBACK=1 to override (and put an authenticating reverse proxy in front)"
        );
    }

    #[test]
    fn test_env_file_is_read_but_real_env_wins() -> Result<(), Box<dyn std::error::Error>> {
        let d = tempfile::tempdir()?;
        let f = d.path().join("env");
        std::fs::write(&f, "HUB_PORT=9001\nHUB_GIT_AUTHOR_NAME=FromFile\n")?;
        let file = f.to_string_lossy().to_string();
        let mut e = env(&[("HOME", "/h"), ("AGENT_HUB_ENV_FILE", &file)]);
        assert_eq!(load_settings_from(&e, &vec![])?.port, 9001);
        e.insert("HUB_PORT".into(), "9002".into());
        let s = load_settings_from(&e, &vec![])?;
        assert_eq!((s.port, s.git_author_name.as_str()), (9002, "FromFile"));
        Ok(())
    }

    #[test]
    fn test_autouse_fixture_ignores_the_users_env_file() -> Result<(), SettingsError> {
        // the Rust tests never read the developer's env file: every load names a missing one explicitly
        let e = env(&[("HOME", "/h"), ("AGENT_HUB_ENV_FILE", "/nonexistent/agent-hub-env")]);
        assert!(!env_file_from(&e).exists());
        assert_eq!(load_settings_from(&e, &vec![])?.port, 8792);
        let s = load_settings_from(
            &e,
            &vec![
                ("knowledge_token".into(), PyValue::str("hunter2")),
                ("knowledge_admin_key".into(), PyValue::str("hunter3")),
            ],
        )?;
        assert_eq!(s.knowledge_token.as_deref(), Some("hunter2"));
        let shown = format!("{s:?}");
        assert!(!shown.contains("token") && !shown.contains("hunter") && !shown.contains("knowledge_admin_key:"));
        Ok(())
    }

    #[test]
    fn env_values_parse_like_pydantic_settings() -> Result<(), SettingsError> {
        let e = env(&[
            ("HOME", "/h"),
            ("hub_port", "9000"),
            ("HUB_TRUST_LOOPBACK", "yes"),
            ("HUB_ALLOWED_HOSTS", "[\"a:1\"]"),
            ("KNOWLEDGE_ADMIN_KEY_FILE", "/k"),
            ("AGENT_HUB_ENV_FILE", "/nonexistent"),
        ]);
        let s = load_settings_from(&e, &vec![("data_dir".into(), PyValue::str("/d"))])?;
        assert_eq!(s.port, 9000);
        assert!(s.trust_loopback);
        assert_eq!(s.allowed_hosts, vec!["a:1".to_string()]);
        assert_eq!(s.knowledge_admin_key_file, PathBuf::from("/k"));
        assert_eq!(s.content_dir, PathBuf::from("/d/content"));
        let bad = load_settings_from(&env(&[("HUB_PORT", "x")]), &vec![]);
        let msg = bad.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            msg.starts_with("1 validation error for Settings\nport\n  Input should be a valid integer"),
            "{msg}"
        );
        Ok(())
    }
}
