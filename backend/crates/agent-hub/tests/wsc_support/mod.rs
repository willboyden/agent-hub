//! WS-C test support (the Rust side of backend/tests/core/conftest.py and fakes.py for the evaluation, observation,
//! knowledge-proxy and CLI tests): a hermetic Hub in a temp dir, a CLI runner with an injected environment, and a fake
//! SkillEvaluator/SkillSpector runner that writes canned reports. Nothing here starts a real SkillEvaluator or reaches a
//! real service port.
#![allow(
    dead_code,
    unreachable_pub,
    clippy::panic,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::type_complexity
)]

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use agent_hub::cli::{CliEnv, run_with};
use agent_hub::config::{Settings, load_settings_from};
use agent_hub::services::hub::{Hub, HubOptions};
use agent_hub::services::skilleval::{EvalResult, EvalRunner};
use hub_core::value::PyValue;
use serde_json::{Value, json};

pub type R = Result<(), Box<dyn std::error::Error>>;

pub const FAKE_BIN: &str = "/opt/fake-bin";

/// A hermetic hub: HOME, the env file, content, state and the knowledge key file all live in one temp dir.
pub struct T {
    pub tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub hub: Arc<Hub>,
}

impl T {
    pub fn data(&self) -> PathBuf {
        self.hub.cfg().data_dir
    }
    pub fn content(&self) -> PathBuf {
        self.hub.cfg().content_dir
    }
    pub fn no_env_file(&self) -> PathBuf {
        self.root.join("no-such-env-file")
    }
    /// An admin key's secret (`hub.auth.create_key(name, "admin")["secret"]`).
    pub fn key(&self, name: &str, role: &str) -> String {
        self.hub
            .auth
            .create_key(name, role, None, &[])
            .ok()
            .and_then(|v| v.get("secret").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default()
    }
}

/// The settings of the `cfg` fixture: content/state in the temp dir, no knowledge key file, inbox rate 100.
pub fn settings(root: &Path, extra_env: &[(&str, &str)]) -> Settings {
    let mut env: HashMap<String, String> = HashMap::from([
        ("HOME".to_string(), root.join("home").to_string_lossy().into_owned()),
        (
            "AGENT_HUB_ENV_FILE".to_string(),
            root.join("no-such-env-file").to_string_lossy().into_owned(),
        ),
    ]);
    for (k, v) in extra_env {
        env.insert((*k).to_string(), (*v).to_string());
    }
    let overrides = vec![
        (
            "content_dir".to_string(),
            PyValue::str(root.join("content").to_string_lossy()),
        ),
        (
            "data_dir".to_string(),
            PyValue::str(root.join("state").to_string_lossy()),
        ),
        (
            "knowledge_admin_key_file".to_string(),
            PyValue::str(root.join("no-knowledge-key").to_string_lossy()),
        ),
        ("inbox_rate_per_min".to_string(), PyValue::Int(100)),
        ("trust_loopback".to_string(), PyValue::Bool(false)),
        // a CLOSED port: nothing in these tests may reach a real knowledge service
        ("knowledge_url".to_string(), PyValue::str("http://127.0.0.1:9")),
    ];
    match load_settings_from(&env, &overrides) {
        Ok(s) => s,
        Err(e) => panic!("settings: {e}"),
    }
}

pub fn hub_with(opts: HubOptions, tweak: impl FnOnce(&mut Settings)) -> T {
    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => panic!("tempdir: {e}"),
    };
    let root = tmp.path().to_path_buf();
    let _ = std::fs::create_dir_all(root.join("home"));
    let mut cfg = settings(&root, &[]);
    tweak(&mut cfg);
    let hub = match Hub::new(cfg, opts) {
        Ok(h) => h,
        Err(p) => panic!("hub: {}", p.detail),
    };
    T { tmp, root, hub }
}

pub fn hub() -> T {
    hub_with(HubOptions::default(), |_| {})
}

/// Captured output of one `hubctl` run.
pub struct CliOut {
    pub code: i32,
    pub out: String,
    pub err: String,
}

/// `run(argv, hub=hub)` with `HUB_API_KEY` from `environ`, the env file `env_file` and stdin not a terminal.
pub fn cli_env(t: &T, argv: &[&str], environ: &[(&str, &str)], env_file: Option<&Path>) -> CliOut {
    let mut env = CliEnv {
        environ: environ
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
        env_file: env_file.map_or_else(|| t.no_env_file(), Path::to_path_buf),
        stdin_tty: false,
        prompt: Box::new(|_| None),
    };
    let argv: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_with(&argv, Some(t.hub.clone()), &mut env, &mut out, &mut err);
    CliOut {
        code,
        out: String::from_utf8_lossy(&out).into_owned(),
        err: String::from_utf8_lossy(&err).into_owned(),
    }
}

/// test_cli_registry.py `cli(hub, *argv)`: a fresh admin key in HUB_API_KEY.
pub fn cli(t: &T, argv: &[&str]) -> CliOut {
    let key = t.key("cli-test", "admin");
    cli_env(t, argv, &[("HUB_API_KEY", &key)], None)
}

// ---- skills in the content working tree (written directly: the eval service reads files, not the store) ------------

/// test_skilleval.py `skill(hub, name, body, files)`: SKILL.md + the hub.yaml sidecar + extra files, as
/// `ContentStore.put("skill", ...)` lays them out.
pub fn skill(t: &T, name: &str, body: &str, files: &[(&str, &str)]) -> PathBuf {
    write_skill(&t.content(), name, body, files)
}

/// The same layout under any content dir.
pub fn write_skill(content: &Path, name: &str, body: &str, files: &[(&str, &str)]) -> PathBuf {
    let d = content.join("skills").join(name);
    let _ = std::fs::create_dir_all(&d);
    let md = format!("---\nname: {name}\ndescription: Alpha skill\n---\n{body}");
    let _ = std::fs::write(d.join("SKILL.md"), md);
    let _ = std::fs::write(d.join("hub.yaml"), "tags:\n- core\n");
    let files: Vec<(&str, &str)> = if files.is_empty() {
        vec![("ref/notes.md", "notes")]
    } else {
        files.to_vec()
    };
    for (rel, text) in files {
        let p = d.join(rel);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(p, text);
    }
    d
}

pub fn alpha(t: &T) -> PathBuf {
    skill(t, "alpha", "Do alpha.\n", &[])
}

// ---- reports -------------------------------------------------------------------------------------------------------

pub fn fixture(name: &str) -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/skilleval")
        .join(name);
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// fakes.py `report(name, **changes)`.
pub fn report(name: &str, changes: &[(&str, Value)]) -> Value {
    let mut d = fixture(name);
    if let Some(m) = d.as_object_mut() {
        for (k, v) in changes {
            m.insert((*k).to_string(), v.clone());
        }
    }
    d
}

/// fakes.py `Canned`.
#[derive(Clone, Debug)]
pub struct Canned {
    pub rc: i32,
    pub report: Option<Value>,
    pub markdown: Option<String>,
    pub stderr: String,
    pub timed_out: bool,
    pub extra: Vec<(String, String)>,
}

impl Default for Canned {
    fn default() -> Self {
        Self {
            rc: 1,
            report: None,
            markdown: Some("# SkillEvaluator report\n\nAll good.\n".into()),
            stderr: String::new(),
            timed_out: false,
            extra: Vec::new(),
        }
    }
}

impl Canned {
    pub fn report(rc: i32, report: Value) -> Self {
        Self {
            rc,
            report: Some(report),
            ..Self::default()
        }
    }
    /// `Canned(rc=rc, report=None, markdown=None, stderr=stderr)`.
    pub fn none(rc: i32, stderr: &str) -> Self {
        Self {
            rc,
            report: None,
            markdown: None,
            stderr: stderr.into(),
            ..Self::default()
        }
    }
}

/// A one-shot gate (`threading.Event`).
#[derive(Default, Clone)]
pub struct Event(Arc<(Mutex<bool>, Condvar)>);

impl Event {
    pub fn set(&self) {
        let (m, c) = &*self.0;
        *m.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        c.notify_all();
    }
    pub fn is_set(&self) -> bool {
        *self.0.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn wait(&self, secs: f64) -> bool {
        let (m, c) = &*self.0;
        let g = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (g, _) = c
            .wait_timeout_while(g, Duration::from_secs_f64(secs), |set| !*set)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g
    }
}

#[derive(Clone, Debug)]
pub struct Call {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub timeout: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Seen {
    pub tier: String,
    pub files: HashMap<String, Vec<u8>>,
    pub symlinks: Vec<String>,
    pub modes: HashMap<String, u32>,
}

pub struct FakeState {
    pub tier1: Canned,
    pub tier2: Canned,
    pub llm: Option<Canned>,
    pub version_out: String,
    pub calls: Vec<Call>,
    pub seen: Vec<Seen>,
    pub cancelled: usize,
    pub write_into_cwd: bool,
    /// the report a `skillspector scan --output F` call writes to F
    pub spector: Option<Value>,
    pub spector_timed_out: bool,
    /// a test hook run after a tier wrote its reports (test_symlinks_left_in_the_output_dir_are_dropped)
    pub after_run: Option<Arc<dyn Fn(&[String]) + Send + Sync>>,
}

/// fakes.py `FakeEvalRunner`: records argv/cwd/env, snapshots the copy it was given (including any symlink), then writes
/// the canned reports into the `-o` directory. `block` holds a run open (concurrency tests).
pub struct FakeEvalRunner {
    pub st: Mutex<FakeState>,
    pub block: Option<Event>,
    pub started: Event,
}

impl FakeEvalRunner {
    pub fn new() -> Arc<Self> {
        Self::with(None, None, None, None)
    }
    pub fn with(tier1: Option<Canned>, tier2: Option<Canned>, llm: Option<Canned>, block: Option<Event>) -> Arc<Self> {
        Arc::new(Self {
            st: Mutex::new(FakeState {
                tier1: tier1.unwrap_or_else(|| Canned::report(1, fixture("tier1_incomplete.json"))),
                tier2: tier2.unwrap_or_else(|| Canned {
                    rc: 0,
                    report: Some(fixture("tier2_passed.json")),
                    markdown: None,
                    ..Canned::default()
                }),
                llm,
                version_out: "skillevaluator, version 0.5.0".into(),
                calls: Vec::new(),
                seen: Vec::new(),
                cancelled: 0,
                write_into_cwd: false,
                spector: None,
                spector_timed_out: false,
                after_run: None,
            }),
            block,
            started: Event::default(),
        })
    }
    pub fn tier1(c: Canned) -> Arc<Self> {
        Self::with(Some(c), None, None, None)
    }
    pub fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.st.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }
    /// test_skilleval.py `tool_calls(r)`: calls that evaluated something (not the `--version` probe).
    pub fn tool_calls(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| c.argv.get(1..) != Some(&["--version".to_string()][..]))
            .collect()
    }
    pub fn tier1_call(&self) -> Option<Call> {
        self.calls()
            .into_iter()
            .find(|c| c.argv.get(1).map(String::as_str) == Some("tier1"))
    }
    pub fn seen(&self) -> Vec<Seen> {
        self.state().seen.clone()
    }
    /// test_observed_eval.py `names_in_order(r)`: the copy names of the static passes, in order.
    pub fn names_in_order(&self) -> Vec<String> {
        self.tool_calls()
            .iter()
            .filter(|c| c.argv.iter().any(|a| a == "--no-llm"))
            .filter_map(|c| {
                Path::new(&c.argv[2])
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .collect()
    }
    pub fn scan_calls(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| c.argv.get(1).map(String::as_str) == Some("scan"))
            .collect()
    }
}

fn snapshot(copy: &Path) -> Seen {
    let mut seen = Seen::default();
    let mut stack = vec![copy.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let full = e.path();
            let rel = full
                .strip_prefix(copy)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let Ok(m) = std::fs::symlink_metadata(&full) else {
                continue;
            };
            if m.file_type().is_symlink() {
                seen.symlinks.push(rel);
            } else if m.is_dir() {
                stack.push(full);
            } else if m.is_file() {
                seen.files.insert(rel.clone(), std::fs::read(&full).unwrap_or_default());
                seen.modes.insert(rel, m.permissions().mode() & 0o7777);
            }
        }
    }
    seen
}

impl EvalRunner for FakeEvalRunner {
    fn run(&self, argv: &[String], cwd: &Path, env: &[(String, String)], timeout: f64) -> EvalResult {
        let call = Call {
            argv: argv.to_vec(),
            cwd: cwd.to_path_buf(),
            env: env.iter().cloned().collect(),
            timeout,
        };
        self.state().calls.push(call);
        if argv.get(1..) == Some(&["--version".to_string()][..]) {
            return EvalResult {
                returncode: 0,
                stdout: format!("{}\n", self.state().version_out),
                stderr: String::new(),
                timed_out: false,
            };
        }
        if argv.get(1).map(String::as_str) == Some("scan") {
            let (spector, to) = {
                let st = self.state();
                (st.spector.clone(), st.spector_timed_out)
            };
            if let Some(rep) = spector.filter(|_| !to)
                && let Some(i) = argv.iter().position(|a| a == "--output")
            {
                let text = rep.to_string().replace("@COPY@", &argv[2]);
                let _ = std::fs::write(&argv[i + 1], text);
            }
            return EvalResult {
                returncode: if to { -9 } else { 0 },
                stdout: String::new(),
                stderr: String::new(),
                timed_out: to,
            };
        }
        self.started.set();
        if let Some(b) = &self.block {
            b.wait(10.0);
        }
        let tier = argv[1].clone();
        let copy = PathBuf::from(&argv[2]);
        let out = argv
            .iter()
            .position(|a| a == "-o")
            .map(|i| PathBuf::from(&argv[i + 1]))
            .unwrap_or_default();
        let mut seen = snapshot(&copy);
        seen.tier = tier.clone();
        let (c, write_into_cwd, hook) = {
            let mut st = self.state();
            st.seen.push(seen);
            let mut c = if tier == "tier1" {
                st.tier1.clone()
            } else {
                st.tier2.clone()
            };
            if tier == "tier1"
                && !argv.iter().any(|a| a == "--no-llm")
                && let Some(l) = &st.llm
            {
                c = l.clone();
            }
            (c, st.write_into_cwd, st.after_run.clone())
        };
        if write_into_cwd {
            let _ = std::fs::write(cwd.join("stray.txt"), "written by the evaluator");
            let _ = std::fs::write(copy.join("SKILL.md"), "tampered by the evaluator");
        }
        if c.timed_out {
            return EvalResult {
                returncode: -9,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: true,
            };
        }
        let copy_s = copy.to_string_lossy().into_owned();
        if let Some(rep) = &c.report {
            let text = rep.to_string().replace("@COPY@", &copy_s);
            let _ = std::fs::write(out.join(format!("skillevaluator-{tier}.json")), text);
        }
        if let Some(md) = &c.markdown
            && tier == "tier1"
        {
            let _ = std::fs::write(out.join("skillevaluator-tier1.md"), md.replace("@COPY@", &copy_s));
        }
        for (name, text) in &c.extra {
            let _ = std::fs::write(out.join(name), text);
        }
        if let Some(h) = hook {
            h(argv);
        }
        EvalResult {
            returncode: c.rc,
            stdout: String::new(),
            stderr: c.stderr.replace("@COPY@", &copy_s),
            timed_out: false,
        }
    }

    fn cancel(&self) {
        self.state().cancelled += 1;
    }
}

/// conftest.py `NoEvalRunner`: any call is a test bug (nothing may start SkillEvaluator unasked).
#[derive(Default)]
pub struct NoEvalRunner {
    pub calls: Mutex<Vec<Vec<String>>>,
}

impl EvalRunner for NoEvalRunner {
    fn run(&self, argv: &[String], _cwd: &Path, _env: &[(String, String)], _timeout: f64) -> EvalResult {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(argv.to_vec());
        EvalResult {
            returncode: 127,
            stdout: String::new(),
            stderr: "no SkillEvaluator in tests".into(),
            timed_out: false,
        }
    }
    fn cancel(&self) {}
}

pub fn j(v: &Value, path: &str) -> Value {
    let mut cur = v.clone();
    for part in path.split('.') {
        cur = match part.parse::<usize>() {
            Ok(i) => cur.get(i).cloned().unwrap_or(Value::Null),
            Err(_) => cur.get(part).cloned().unwrap_or(Value::Null),
        };
    }
    cur
}

pub fn empty() -> Value {
    json!({})
}

/// `hub.put_client(cid, doc, create=True)`.
pub fn put_client(t: &T, cid: &str, doc: &Value) -> hub_core::HubResult<Value> {
    let m: indexmap::IndexMap<String, PyValue> = doc
        .as_object()
        .map(|o| o.iter().map(|(k, v)| (k.clone(), PyValue::from_json(v))).collect())
        .unwrap_or_default();
    t.hub.put_client(cid, &m, true)
}

/// `hub.work.put(kind, name, payload)`.
pub fn put(t: &T, kind: &str, name: &str, payload: &Value) -> hub_core::HubResult<()> {
    t.hub
        .work
        .put(kind, name, &PyValue::from_json(payload), None)
        .map(|_| ())
}

/// `hub.work.put_profile(client, payload)`.
pub fn put_profile(t: &T, client: &str, payload: &Value) -> hub_core::HubResult<()> {
    t.hub.work.put_profile(client, &PyValue::from_json(payload)).map(|_| ())
}

// ---- a standalone evaluation + observation service pair (no Hub: Python's SkillEvalService(root, content_dir, ...)) --

pub struct S {
    pub tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub svc: agent_hub::services::skilleval::SkillEvalService,
    pub obs: agent_hub::services::observe::ObserveService,
}

impl S {
    pub fn content(&self) -> PathBuf {
        self.root.join("content")
    }
    /// `hub.cfg.data_dir`
    pub fn data(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn eval_root(&self) -> PathBuf {
        self.root.join("state").join("skilleval")
    }
    pub fn skill(&self, name: &str) -> PathBuf {
        write_skill(&self.content(), name, "Do alpha.\n", &[])
    }
    pub fn skill_with(&self, name: &str, body: &str, files: &[(&str, &str)]) -> PathBuf {
        write_skill(&self.content(), name, body, files)
    }
}

pub fn standalone() -> S {
    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => panic!("tempdir: {e}"),
    };
    let root = tmp.path().to_path_buf();
    let _ = std::fs::create_dir_all(root.join("content").join("skills"));
    let _ = std::fs::create_dir_all(root.join("state"));
    let svc = agent_hub::services::skilleval::SkillEvalService::new(
        &root.join("state").join("skilleval"),
        &root.join("content"),
        std::sync::Weak::new(),
        Some(Arc::new(NoEvalRunner::default())),
        200,
        4 * 1024 * 1024,
        &root.join("state").join("observed"),
    );
    // tests never find, let alone run, a real SkillEvaluator
    svc.set_which(Some(Arc::new(|_: &str, _: &str| None)));
    let obs = agent_hub::services::observe::ObserveService::new(
        &root.join("state").join("observed"),
        std::sync::Weak::new(),
        Some(&root.join("state").join("skilleval")),
    );
    S { tmp, root, svc, obs }
}

pub const LAUNCHERS: &[&str] = &["skillevaluator-local", "skillevaluator"];

/// test_skilleval.py `use(hub, runner, launchers=, environ=, home=)`: point the service at a fake SkillEvaluator.
pub fn use_(
    svc: &agent_hub::services::skilleval::SkillEvalService,
    r: Arc<FakeEvalRunner>,
    launchers: &[&str],
    environ: Option<&[(&str, &str)]>,
    home: Option<&Path>,
) -> Arc<FakeEvalRunner> {
    svc.set_runner(r.clone());
    let names: Vec<String> = launchers.iter().map(|s| (*s).to_string()).collect();
    svc.set_which(Some(Arc::new(move |name: &str, _path: &str| {
        names.iter().any(|n| n == name).then(|| format!("{FAKE_BIN}/{name}"))
    })));
    let env: Vec<(String, String)> = environ
        .unwrap_or(&[("PATH", "/usr/bin:/bin"), ("HOME", "/home/operator")])
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    svc.set_environ(Some(env));
    if let Some(h) = home {
        svc.set_home(Some(h.to_path_buf()));
    }
    r
}

/// `use(hub)` with the defaults.
pub fn use_default(svc: &agent_hub::services::skilleval::SkillEvalService) -> Arc<FakeEvalRunner> {
    use_(svc, FakeEvalRunner::new(), LAUNCHERS, None, None)
}

// ---- the API (security pipeline + routes) without a socket --------------------------------------------------------------

pub struct ApiResp {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
}

impl ApiResp {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn code(&self) -> String {
        self.json()["code"].as_str().unwrap_or_default().to_string()
    }
    pub fn header(&self, k: &str) -> String {
        self.headers
            .get(k)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .unwrap_or_default()
    }
}

pub fn app(t: &T) -> Arc<agent_hub::api::AppState> {
    agent_hub::server::create_app(t.hub.clone())
}

/// One request through the hub's own pipeline, from a non-loopback peer, Host 127.0.0.1:<cfg port>.
pub async fn api(
    app: &Arc<agent_hub::api::AppState>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> ApiResp {
    use http_body_util::BodyExt;
    let port = u16::try_from(app.hub.cfg().port).unwrap_or(8792);
    let host = format!("127.0.0.1:{port}");
    let mut b = http::Request::builder().method(method).uri(path).header("host", &host);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    if body.is_some() && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
        b = b.header("content-type", "application/json");
    }
    let r = b
        .body(body.map_or_else(axum::body::Body::empty, |v| axum::body::Body::from(v.to_string())))
        .unwrap_or_default();
    let peer: std::net::SocketAddr = std::net::SocketAddr::from(([10, 0, 0, 9], 5000));
    let resp = agent_hub::api::security::handle(app.clone(), peer, port, r).await;
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.map(|c| c.to_bytes().to_vec()).unwrap_or_default();
    ApiResp {
        status: parts.status.as_u16(),
        headers: parts.headers,
        body: bytes,
    }
}

pub fn bearer(k: &str) -> String {
    format!("Bearer {k}")
}

// ---- observed skills (test_observed_eval.py helpers) ----------------------------------------------------------------

use agent_hub::adapters::base::{
    AdapterCaps, AdapterResult, Check, ClientAdapter, ClientConfig, CommandRunner, DiscoveredContent, Expected,
    FileSystem, Observation, ObservedSkill, RenderContext, RenderResult, SkillItem,
};

/// `md(name, body, desc)`.
pub fn md(name: &str, body: &str) -> Vec<u8> {
    format!("---\nname: {name}\ndescription: A skill\n---\n{body}").into_bytes()
}

/// `oskill(path, name, origin, body, extra)`.
pub fn oskill(path: &str, name: Option<&str>, origin: &str, body: &str, extra: &[(&str, &[u8])]) -> ObservedSkill {
    let n = name.map_or_else(|| path.rsplit('/').next().unwrap_or(path).to_string(), str::to_string);
    let mut files = indexmap::IndexMap::new();
    files.insert("SKILL.md".to_string(), md(&n, body));
    for (k, v) in extra {
        files.insert((*k).to_string(), v.to_vec());
    }
    ObservedSkill {
        path: path.into(),
        name: n,
        description: "A skill".into(),
        origin: origin.into(),
        files,
    }
}

/// `oskill(path)` with the defaults (origin agent, body "Do it.\n").
pub fn os1(path: &str) -> ObservedSkill {
    oskill(path, None, "agent", "Do it.\n", &[])
}

/// The ObsAdapter's observation (`source "fake container"`, one note).
pub fn observation(skills: Vec<ObservedSkill>, complete: bool) -> Observation {
    Observation {
        skills,
        source: "fake container".into(),
        notes: vec!["note one".into()],
        complete,
    }
}

/// test_observed_eval.py `ObsAdapter`: a fake client whose `observe` returns whatever the test put in it.
pub struct ObsAdapter {
    pub st: Mutex<(Vec<ObservedSkill>, bool)>,
}

impl ObsAdapter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            st: Mutex::new((Vec::new(), true)),
        })
    }
    pub fn set(&self, skills: Vec<ObservedSkill>, complete: bool) {
        *self.st.lock().unwrap() = (skills, complete);
    }
}

impl ClientAdapter for ObsAdapter {
    fn id(&self) -> &str {
        "fakeobs"
    }
    fn display_name(&self) -> &str {
        "Fake observed client"
    }
    fn docs(&self) -> &str {
        ""
    }
    fn caps(&self) -> AdapterCaps {
        AdapterCaps {
            skills: true,
            ..AdapterCaps::default()
        }
    }
    fn default_config(&self) -> ClientConfig {
        ClientConfig {
            id: "fakeobs".into(),
            adapter: "fakeobs".into(),
            ..ClientConfig::default()
        }
    }
    fn render(&self, _ctx: &RenderContext) -> AdapterResult<RenderResult> {
        Ok(RenderResult::default())
    }
    fn discover(&self, _cfg: &ClientConfig, _fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> {
        Ok(DiscoveredContent::default())
    }
    fn verify(
        &self,
        _: &ClientConfig,
        _: &[Expected],
        _: &dyn FileSystem,
        _: &dyn CommandRunner,
    ) -> AdapterResult<Vec<Check>> {
        Ok(Vec::new())
    }
    fn observe(
        &self,
        _cfg: &ClientConfig,
        _fs: &dyn FileSystem,
        _run: &dyn CommandRunner,
    ) -> AdapterResult<Observation> {
        let (skills, complete) = self.st.lock().unwrap().clone();
        Ok(observation(skills, complete))
    }
}

/// fakes.py `FakeAdapter.discover` (skills only): `<home>/skills/<name>/<files>`; observe = the default wrapper.
pub struct DiscAdapter;

impl ClientAdapter for DiscAdapter {
    fn id(&self) -> &str {
        "fake"
    }
    fn display_name(&self) -> &str {
        "Fake client"
    }
    fn docs(&self) -> &str {
        ""
    }
    fn caps(&self) -> AdapterCaps {
        AdapterCaps {
            skills: true,
            ..AdapterCaps::default()
        }
    }
    fn default_config(&self) -> ClientConfig {
        ClientConfig {
            id: "fake".into(),
            adapter: "fake".into(),
            ..ClientConfig::default()
        }
    }
    fn render(&self, _ctx: &RenderContext) -> AdapterResult<RenderResult> {
        Ok(RenderResult::default())
    }
    fn discover(&self, cfg: &ClientConfig, fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> {
        let mut dc = DiscoveredContent::default();
        let home = cfg.roots.get("home").cloned().unwrap_or_default();
        let sk = format!("{home}/skills");
        if fs.is_dir(&sk) {
            for name in fs.listdir(&sk).unwrap_or_default() {
                let d = format!("{sk}/{name}");
                if !fs.is_dir(&d) {
                    continue;
                }
                let mut files = indexmap::IndexMap::new();
                for f in fs.listdir(&d).unwrap_or_default() {
                    if let Ok(b) = fs.read_bytes(&format!("{d}/{f}")) {
                        files.insert(f, b);
                    }
                }
                if files.contains_key("SKILL.md") {
                    dc.skills.push(SkillItem {
                        name,
                        description: "found".into(),
                        files,
                        ..SkillItem::default()
                    });
                }
            }
        }
        Ok(dc)
    }
    fn verify(
        &self,
        _: &ClientConfig,
        _: &[Expected],
        _: &dyn FileSystem,
        _: &dyn CommandRunner,
    ) -> AdapterResult<Vec<Check>> {
        Ok(Vec::new())
    }
}

/// A hub whose registry has the ObsAdapter and the discover-only fake, plus a fake evaluator.
pub fn obs_hub(r: Arc<FakeEvalRunner>) -> (T, Arc<ObsAdapter>) {
    let a = ObsAdapter::new();
    let mut reg = agent_hub::adapters::AdapterRegistry::new();
    let _ = reg.register(a.clone());
    let _ = reg.register(Arc::new(DiscAdapter));
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            registry: Some(reg),
            eval_runner: Some(r.clone()),
            ..agent_hub::services::hub::HubOptions::default()
        },
        |_| {},
    );
    use_(&t.hub.skilleval, r, LAUNCHERS, None, None);
    (t, a)
}

/// test_observed_eval.py `results(**validators)`: tier1 `results`; `_` in a keyword means a space unless all upper.
pub fn results(validators: &[(&str, &str, &[(&str, &str)])]) -> Value {
    let out: Vec<Value> = validators
        .iter()
        .map(|(name, status, findings)| {
            let n = if name.chars().all(|c| !c.is_lowercase()) { (*name).to_string() } else { name.replace('_', " ") };
            let f: Vec<Value> = findings
                .iter()
                .map(|(sev, msg)| json!({"category": "X", "severity": sev, "check": null, "message": msg}))
                .collect();
            json!({"validator": n, "passed": *status == "passed", "status": status, "incomplete_scans": [], "findings": f})
        })
        .collect();
    Value::Array(out)
}

/// test_observed_eval.py `canned(**validators)`: every expected security validator passed plus a publication finding.
pub fn canned(over: &[(&str, &str, &[(&str, &str)])]) -> Canned {
    let mut base: Vec<(&str, &str, &[(&str, &str)])> = vec![
        ("Security_Scan", "passed", &[]),
        ("PII_Scan", "passed", &[]),
        ("Code_Risk_Analysis", "passed", &[]),
        ("Secrets_Detection", "passed", &[]),
        ("Code_Integrity_&_Hygiene", "passed", &[]),
        ("Unicode_Smuggling_Detection", "passed", &[]),
        (
            "Schema_&_Repository_Governance",
            "failed",
            &[("high", "Author not specified in metadata")],
        ),
    ];
    for o in over {
        if let Some(slot) = base.iter_mut().find(|b| b.0 == o.0) {
            *slot = *o;
        } else {
            base.push(*o);
        }
    }
    Canned::report(
        1,
        report(
            "tier1_incomplete.json",
            &[("overall_status", json!("failed")), ("results", results(&base))],
        ),
    )
}

/// conftest.py `seed_content(hub, client)`: one of every kind, all enabled for `client` through a collection and a
/// direct enable.
pub fn seed_content(t: &T, client: &str) -> hub_core::HubResult<()> {
    put(
        t,
        "skill",
        "alpha",
        &json!({"description": "Alpha skill", "body": "Do alpha.\n", "files": {"ref/notes.md": "notes"}, "tags": ["core"]}),
    )?;
    put(
        t,
        "agent",
        "reviewer",
        &json!({"description": "Reviews code", "capabilities": ["read"], "body": "Review carefully.\n"}),
    )?;
    put(
        t,
        "instruction",
        "style",
        &json!({"title": "Style", "order": 10, "body": "Be concise.\n"}),
    )?;
    put(
        t,
        "rule",
        "no-web",
        &json!({"title": "No web", "kind": "tool", "match": "web_fetch", "decision": "deny"}),
    )?;
    put(
        t,
        "memory",
        "fact1",
        &json!({"type": "reference", "title": "A fact", "body": "The sky is blue.\n"}),
    )?;
    let coll = json!({"id": "core", "title": "Core", "members": [{"kind": "skill", "name": "alpha"}, {"kind": "instruction", "name": "style"}]});
    t.hub.work.put_collection("core", &PyValue::from_json(&coll))?;
    put_profile(
        t,
        client,
        &json!({"collections": ["core"], "enable": {"agents": ["reviewer"], "rules": ["no-web"]}}),
    )
}

/// conftest.py `home`: the fake client's config dir, `<tmp>/client-home` (never the temp root itself, which holds the
/// hub's state dir and is therefore refused as a client root by TargetGuard).
pub fn client_home(root: &std::path::Path) -> std::path::PathBuf {
    let home = root.join("client-home");
    let _ = std::fs::create_dir_all(&home);
    home
}
