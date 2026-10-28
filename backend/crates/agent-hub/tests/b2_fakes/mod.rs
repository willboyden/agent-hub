//! WS-B2's shared fixtures for the Hub-level test ports (backend/tests/core/conftest.py + fakes.py): settings in a temp
//! dir, a Hub with the FakeAdapter (every merge mode) and a FakeRunner, `add_client`, `seed_content`, and an in-process
//! HTTP client over the real security pipeline. Included with `mod b2_fakes;` by WS-B2's test files.
#![allow(dead_code, unreachable_pub, clippy::needless_pass_by_value)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_hub::adapters::AdapterRegistry;
use agent_hub::adapters::base::{
    AdapterCaps, AdapterResult, AgentItem, Artifact, Check, ClientAdapter, ClientConfig, CommandRunner, Diag,
    DiscoveredContent, Expected, FileSystem, InstructionItem, McpItem, PermissionSet, RenderContext, RenderResult,
    Rule, SkillItem,
};
use agent_hub::api::{AppState, security};
use agent_hub::config::{Settings, load_settings_from};
use agent_hub::server::{create_app, on_startup};
use agent_hub::services::hub::{Hub, HubOptions};
use agent_hub::services::merge;
use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use hub_core::HubResult;
use hub_core::models::Plan;
use hub_core::value::PyValue;
use indexmap::IndexMap;
use serde_json::{Value, json};

pub type E = Box<dyn std::error::Error>;
pub type R = Result<(), E>;

pub const BEGIN: &str = "<!-- agent-hub:begin (managed block; edits inside are overwritten) -->";
pub const END: &str = "<!-- agent-hub:end -->";
pub const PORT: u16 = 8792;
pub const HOST: &str = "127.0.0.1:8792";
pub const API: &str = "/api/v1";

/// A Python dict/list literal as the PyValue the content store takes.
pub fn py(v: Value) -> PyValue {
    PyValue::from_json(&v)
}

/// A JSON object as `dict[str, Any]` (Hub.put_client's payload).
pub fn obj(v: Value) -> IndexMap<String, PyValue> {
    v.as_object()
        .map(|m| m.iter().map(|(k, x)| (k.clone(), PyValue::from_json(x))).collect())
        .unwrap_or_default()
}

pub fn s(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

// ---- fakes.py ---------------------------------------------------------------------------------------------------------

/// adapters/_util.py `slice_digest` (the FakeAdapter verifies merged files on the hub-owned slice).
pub fn slice_digest(raw: &[u8], merge_mode: &str, keys: &[String]) -> Option<String> {
    if merge_mode == "own" {
        return Some(hub_core::common::sha256(raw));
    }
    let text = String::from_utf8_lossy(raw).into_owned();
    if merge_mode == "block" {
        let (a, b) = merge::find_region(&text, "md")
            .ok()
            .flatten()
            .or_else(|| merge::find_region(&text, "hash").ok().flatten())?;
        let mut r = text[a..b].to_string();
        if !r.ends_with('\n') {
            r.push('\n');
        }
        return Some(hub_core::common::sha256(r.as_bytes()));
    }
    let doc = merge::load_doc(raw, if merge_mode == "json_keys" { "json" } else { "yaml" }).ok()?;
    let mut ks: Vec<&String> = keys.iter().collect();
    ks.sort();
    let found: Vec<(PyValue, PyValue)> = ks
        .into_iter()
        .filter_map(|k| {
            merge::get_dotted(&doc, k)
                .filter(|v| !v.is_none())
                .map(|v| (PyValue::str(k.as_str()), v.clone()))
        })
        .collect();
    if found.is_empty() {
        None
    } else {
        merge::canon(&PyValue::Dict(found)).ok()
    }
}

fn dumps(v: &Value) -> Vec<u8> {
    hub_core::py::json::dumps(v, &hub_core::py::json::Opts::default()).into_bytes()
}

pub type RenderFn = Arc<dyn Fn(&RenderContext) -> AdapterResult<RenderResult> + Send + Sync>;

/// fakes.py `FakeAdapter` (id `fake`), or a subclass of it with another id and its own `render` (MixedAdapter,
/// EvilAdapter, ModeAdapter, Dup, Boom of test_apply.py).
#[derive(Clone)]
pub struct FakeAdapter {
    pub id: String,
    pub render_fn: Option<RenderFn>,
}

impl FakeAdapter {
    pub fn new() -> Self {
        Self {
            id: "fake".into(),
            render_fn: None,
        }
    }
    pub fn subclass(
        id: &str,
        f: impl Fn(&RenderContext) -> AdapterResult<RenderResult> + Send + Sync + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            render_fn: Some(Arc::new(f)),
        }
    }
    fn tool_map() -> IndexMap<String, Option<String>> {
        IndexMap::from([
            ("read".to_string(), Some("Read".to_string())),
            ("shell".to_string(), Some("Bash".to_string())),
            ("browser".to_string(), None),
        ])
    }
    /// The FakeAdapter's own render.
    pub fn base_render(ctx: &RenderContext) -> RenderResult {
        let mut arts = Vec::new();
        let mut diags = Vec::new();
        for sk in &ctx.skills {
            let mut files: Vec<(&String, &Vec<u8>)> = sk.files.iter().collect();
            files.sort();
            for (rel, blob) in files {
                let mut a = Artifact::new("home", &format!("skills/{}/{rel}", sk.name), blob.clone(), "skill");
                a.source_ids = vec![sk.name.clone()];
                arts.push(a);
            }
        }
        for a in &ctx.agents {
            for c in &a.capabilities {
                if matches!(Self::tool_map().get(c), Some(None)) {
                    diags.push(Diag {
                        severity: "error".into(),
                        code: "unsupported_capability".into(),
                        message: format!("{c} unsupported"),
                        item: Some(a.name.clone()),
                    });
                }
            }
            let mut art = Artifact::new(
                "home",
                &format!("agents/{}.md", a.name),
                format!("# {}\n{}", a.name, a.body).into_bytes(),
                "agent",
            );
            art.source_ids = vec![a.name.clone()];
            arts.push(art);
        }
        if !ctx.instructions.is_empty() {
            let body: Vec<String> = ctx
                .instructions
                .iter()
                .map(|i| format!("## {}\n\n{}", i.title, hub_core::py::text::strip(&i.body)))
                .collect();
            let mut art = Artifact::new(
                "home",
                "CLAUDE.md",
                format!("{BEGIN}\n{}\n{END}\n", body.join("\n\n")).into_bytes(),
                "instruction",
            );
            art.merge = "block".into();
            art.source_ids = ctx.instructions.iter().map(|i| i.id.clone()).collect();
            arts.push(art);
        }
        let mut servers = serde_json::Map::new();
        for m in &ctx.mcp_servers {
            servers.insert(
                m.name.clone(),
                json!({"command": m.command, "args": m.args, "url": m.url}),
            );
        }
        let mut names: Vec<String> = servers.keys().cloned().collect();
        names.sort();
        let mut mcp = Artifact::new("home", ".mcp.json", dumps(&json!({"mcpServers": servers})), "mcp");
        mcp.merge = "json_keys".into();
        mcp.managed_keys = vec!["mcpServers".into()];
        mcp.source_ids = names;
        arts.push(mcp);
        let mut perms = serde_json::Map::from_iter([
            ("deny".to_string(), json!([])),
            ("ask".to_string(), json!([])),
            ("allow".to_string(), json!([])),
        ]);
        for r in &ctx.permissions.rules {
            if let Some(Value::Array(a)) = perms.get_mut(&r.decision) {
                a.push(json!(format!("{}:{}", r.kind, r.matches)));
            }
        }
        let mut st = Artifact::new("home", "settings.json", dumps(&json!({"permissions": perms})), "rule");
        st.merge = "json_keys".into();
        st.managed_keys = vec![
            "permissions.deny".into(),
            "permissions.ask".into(),
            "permissions.allow".into(),
        ];
        st.source_ids = vec!["floor".into()];
        arts.push(st);
        RenderResult {
            artifacts: arts,
            diagnostics: diags,
        }
    }
}

impl ClientAdapter for FakeAdapter {
    fn id(&self) -> &str {
        &self.id
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
            agents: true,
            instructions: true,
            mcp: true,
            permissions: true,
            egress: true,
            memory: false,
            tool_map: Self::tool_map(),
            notes: "fake".into(),
            ..AdapterCaps::default()
        }
    }
    fn default_config(&self) -> ClientConfig {
        ClientConfig {
            id: "fake".into(),
            adapter: "fake".into(),
            display_name: "Fake".into(),
            roots: IndexMap::from([("home".to_string(), "~/.fake".to_string())]),
            ..ClientConfig::default()
        }
    }
    fn render(&self, ctx: &RenderContext) -> AdapterResult<RenderResult> {
        match &self.render_fn {
            Some(f) => f(ctx),
            None => Ok(Self::base_render(ctx)),
        }
    }
    fn discover(&self, cfg: &ClientConfig, fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> {
        let err = |e: std::io::Error| agent_hub::adapters::base::AdapterError::new("OSError", e.to_string());
        let home = cfg.roots.get("home").cloned().unwrap_or_default();
        let mut dc = DiscoveredContent::default();
        let sk = format!("{home}/skills");
        if fs.is_dir(&sk) {
            for name in fs.listdir(&sk).map_err(err)? {
                let d = format!("{sk}/{name}");
                if !fs.is_dir(&d) {
                    continue;
                }
                let mut files = IndexMap::new();
                for f in fs.listdir(&d).map_err(err)? {
                    files.insert(f.clone(), fs.read_bytes(&format!("{d}/{f}")).map_err(err)?);
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
        let ag = format!("{home}/agents");
        if fs.is_dir(&ag) {
            for f in fs.listdir(&ag).map_err(err)? {
                if let Some(stem) = f.strip_suffix(".md") {
                    dc.agents.push(AgentItem {
                        name: stem.into(),
                        description: "found agent".into(),
                        capabilities: vec!["read".into()],
                        body: String::from_utf8_lossy(&fs.read_bytes(&format!("{ag}/{f}")).map_err(err)?).into_owned(),
                        ..AgentItem::default()
                    });
                }
            }
        }
        let parse = |b: Vec<u8>| serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null);
        if fs.exists(&format!("{home}/.mcp.json")) {
            let doc = parse(fs.read_bytes(&format!("{home}/.mcp.json")).map_err(err)?);
            if let Some(m) = doc.get("mcpServers").and_then(Value::as_object) {
                for (n, v) in m {
                    dc.mcp_servers.push(McpItem {
                        name: n.clone(),
                        transport: "stdio".into(),
                        command: Some(v.get("command").and_then(Value::as_str).unwrap_or("x").into()),
                        args: v
                            .get("args")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                            .unwrap_or_default(),
                        scan_status: "clean".into(), // a foreign claim of `clean` must be ignored on import
                        ..McpItem::default()
                    });
                }
            }
        }
        if fs.exists(&format!("{home}/settings.json")) {
            let doc = parse(fs.read_bytes(&format!("{home}/settings.json")).map_err(err)?);
            let mut rules = Vec::new();
            if let Some(p) = doc.get("permissions").and_then(Value::as_object) {
                for (dec, items) in p {
                    for it in items.as_array().into_iter().flatten().filter_map(Value::as_str) {
                        let (k, m) = it.split_once(':').unwrap_or((it, ""));
                        rules.push(Rule {
                            kind: k.into(),
                            matches: m.into(),
                            decision: dec.clone(),
                            reason: None,
                        });
                    }
                }
            }
            dc.permissions = Some(PermissionSet {
                rules,
                ..PermissionSet::default()
            });
        }
        dc.suggested_params = vec![
            ("instructions_mode".into(), PyValue::str("own")),
            ("keep".into(), PyValue::str("adapter")),
        ];
        if fs.exists(&format!("{home}/CLAUDE.md")) {
            dc.instructions.push(InstructionItem {
                id: "native".into(),
                title: "Native".into(),
                order: 1,
                body: String::from_utf8_lossy(&fs.read_bytes(&format!("{home}/CLAUDE.md")).map_err(err)?).into_owned(),
            });
        }
        Ok(dc)
    }
    fn verify(
        &self,
        cfg: &ClientConfig,
        expected: &[Expected],
        fs: &dyn FileSystem,
        run: &dyn CommandRunner,
    ) -> AdapterResult<Vec<Check>> {
        let mut out = Vec::new();
        for e in expected {
            let p = format!("{}/{}", cfg.roots.get(&e.root).map_or("", String::as_str), e.path);
            let ok = fs.exists(&p)
                && fs.read_bytes(&p).ok().is_some_and(|b| {
                    if e.merge == "own" {
                        hub_core::common::sha256(&b) == e.sha256
                    } else {
                        slice_digest(&b, &e.merge, &e.managed_keys).as_deref() == Some(e.sha256.as_str())
                    }
                });
            out.push(Check {
                name: format!("{}:{}", e.root, e.path),
                ok,
                detail: if ok {
                    String::new()
                } else {
                    "missing or hash differs".into()
                },
            });
        }
        let (rc, o, _) =
            run.run(&["fake-client".into(), "--list".into()], 5.0)
                .unwrap_or((127, String::new(), String::new()));
        out.push(Check {
            name: "fake-client --list".into(),
            ok: rc == 0,
            detail: o.trim().to_string(),
        });
        Ok(out)
    }
}

/// fakes.py `FakeRunner`.
#[derive(Default)]
pub struct FakeRunner {
    pub rc: Mutex<i32>,
    pub calls: Mutex<Vec<Vec<String>>>,
}

impl FakeRunner {
    pub fn set_rc(&self, rc: i32) {
        *self.rc.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = rc;
    }
    pub fn n_calls(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, argv: &[String], _timeout: f64) -> std::io::Result<(i32, String, String)> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(argv.to_vec());
        Ok((
            *self.rc.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
            "ok".into(),
            String::new(),
        ))
    }
}

// ---- conftest.py --------------------------------------------------------------------------------------------------------

/// conftest.py `cfg`: content and state under a temp dir, the app's real policy floor, no knowledge key.
pub fn settings(tmp: &Path) -> Result<Settings, E> {
    let env = std::collections::HashMap::from([
        ("HOME".to_string(), s(&tmp.join("home"))),
        ("AGENT_HUB_ENV_FILE".to_string(), s(&tmp.join("no-such-env-file"))),
    ]);
    let fe = tmp.join("frontend");
    std::fs::create_dir_all(fe.join("js"))?;
    std::fs::write(fe.join("index.html"), "<!doctype html><title>hub</title>")?;
    let o: Vec<(String, PyValue)> = vec![
        ("content_dir".into(), PyValue::str(s(&tmp.join("content")))),
        ("data_dir".into(), PyValue::str(s(&tmp.join("state")))),
        ("frontend_dir".into(), PyValue::str(s(&fe))),
        (
            "knowledge_admin_key_file".into(),
            PyValue::str(s(&tmp.join("no-knowledge-key"))),
        ),
        ("inbox_rate_per_min".into(), PyValue::str("100")),
    ];
    Ok(load_settings_from(&env, &o)?)
}

/// The conftest fixtures `cfg` + `registry` + `runner` + `hub` (+ `home`).
pub struct Fx {
    pub tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
    pub hub: Arc<Hub>,
    pub runner: Arc<FakeRunner>,
}

/// `Hub(cfg, registry=..., runner=FakeRunner())` with `extra` adapters registered next to the FakeAdapter
/// (Python registers them on the live registry; here the registry is built up front).
pub fn hub_with(extra: Vec<FakeAdapter>, init: bool) -> Result<Fx, E> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().to_path_buf();
    let home = root.join("client-home");
    std::fs::create_dir(&home)?;
    let mut reg = AdapterRegistry::new();
    reg.register(Arc::new(FakeAdapter::new()))?;
    for a in extra {
        reg.register(Arc::new(a))?;
    }
    let runner = Arc::new(FakeRunner::default());
    let hub = Hub::new(
        settings(&root)?,
        HubOptions {
            registry: Some(reg),
            runner: Some(runner.clone()),
            ..HubOptions::default()
        },
    )
    .map_err(|p| p.detail)?;
    if init {
        hub.init_content().map_err(|p| p.detail)?;
    }
    Ok(Fx {
        tmp,
        root,
        home,
        hub,
        runner,
    })
}

pub fn hub() -> Result<Fx, E> {
    hub_with(Vec::new(), true)
}

impl Fx {
    /// conftest `add_client(hub, home, cid, **extra)`.
    pub fn add_client(&self, home: &Path, cid: &str, extra: Value) -> HubResult<Value> {
        let mut doc = json!({"adapter": "fake", "display_name": "Fake 1", "roots": {"home": s(home)}});
        if let (Some(m), Some(e)) = (doc.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                m.insert(k.clone(), v.clone());
            }
        }
        self.hub.put_client(cid, &obj(doc), true)
    }

    /// conftest `seed_content(hub, client)`: one of every kind, all enabled for `client` through a collection and a
    /// direct enable.
    pub fn seed_content(&self, client: &str) -> HubResult<()> {
        let w = &self.hub.work;
        w.put(
            "skill",
            "alpha",
            &py(
                json!({"description": "Alpha skill", "body": "Do alpha.\n", "files": {"ref/notes.md": "notes"},
                        "tags": ["core"]}),
            ),
            None,
        )?;
        w.put(
            "agent",
            "reviewer",
            &py(json!({"description": "Reviews code", "capabilities": ["read"], "body": "Review carefully.\n"})),
            None,
        )?;
        w.put(
            "instruction",
            "style",
            &py(json!({"title": "Style", "order": 10, "body": "Be concise.\n"})),
            None,
        )?;
        w.put(
            "rule",
            "no-web",
            &py(json!({"title": "No web", "kind": "tool", "match": "web_fetch", "decision": "deny"})),
            None,
        )?;
        w.put(
            "memory",
            "fact1",
            &py(json!({"type": "reference", "title": "A fact", "body": "The sky is blue.\n"})),
            None,
        )?;
        w.put_collection(
            "core",
            &py(
                json!({"id": "core", "title": "Core", "members": [{"kind": "skill", "name": "alpha"},
                                                                    {"kind": "instruction", "name": "style"}]}),
            ),
        )?;
        w.put_profile(
            client,
            &py(json!({"collections": ["core"], "enable": {"agents": ["reviewer"], "rules": ["no-web"]}})),
        )?;
        Ok(())
    }

    pub fn put(&self, kind: &str, name: &str, v: Value) -> HubResult<()> {
        self.hub.work.put(kind, name, &py(v), None).map(|_| ())
    }

    pub fn commit(&self) -> HubResult<Value> {
        self.hub.commit("c", "t")
    }

    /// test_apply.py `run(hub, clients, adopt)`.
    pub fn run(&self, clients: Option<&[&str]>, adopt: &[&str]) -> HubResult<(Plan, Value)> {
        let cl: Option<Vec<String>> = clients.map(|c| c.iter().map(|x| (*x).to_string()).collect());
        let plan = self.hub.plan(cl.as_deref(), "committed")?;
        let adopt: Vec<String> = adopt.iter().map(|x| (*x).to_string()).collect();
        let res = self.hub.apply(&plan.id, true, &adopt, "t")?;
        Ok((plan, res))
    }

    /// The `setup` fixture: fake1 + seeded content, committed.
    pub fn setup(self) -> Result<Self, E> {
        self.add_client(&self.home, "fake1", json!({})).map_err(|p| p.detail)?;
        self.seed_content("fake1").map_err(|p| p.detail)?;
        self.commit().map_err(|p| p.detail)?;
        Ok(self)
    }

    /// The `full` fixture: mcp and permissions managed too, so the merge modes are exercised.
    pub fn full(self) -> Result<Self, E> {
        self.add_client(
            &self.home,
            "fake1",
            json!({"manage": {"mcp": true, "permissions": true}}),
        )
        .map_err(|p| p.detail)?;
        self.seed_content("fake1").map_err(|p| p.detail)?;
        self.commit().map_err(|p| p.detail)?;
        Ok(self)
    }

    pub fn data(&self, rel: &str) -> PathBuf {
        self.root.join("state").join(rel)
    }
    pub fn content(&self, rel: &str) -> PathBuf {
        self.root.join("content").join(rel)
    }

    pub fn admin_key(&self) -> Result<String, E> {
        Ok(self
            .hub
            .auth
            .create_key("t-admin", "admin", None, &[])
            .map_err(|p| p.detail)?["secret"]
            .as_str()
            .ok_or("no secret")?
            .to_string())
    }
    pub fn viewer_key(&self) -> Result<String, E> {
        Ok(self
            .hub
            .auth
            .create_key("t-viewer", "viewer", None, &[])
            .map_err(|p| p.detail)?["secret"]
            .as_str()
            .ok_or("no secret")?
            .to_string())
    }

    /// The conftest `api` fixture: the app over the real security pipeline.
    pub fn api(&self) -> Arc<AppState> {
        on_startup(&self.hub);
        create_app(self.hub.clone())
    }
}

/// test_apply.py `result(res, client)`.
pub fn result(res: &Value, client: &str) -> Value {
    res["results"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["client"] == client).cloned())
        .unwrap_or(Value::Null)
}

/// test_apply.py `acts(plan, client)`.
pub fn acts(plan: &Plan, client: &str) -> IndexMap<String, String> {
    plan.clients
        .iter()
        .find(|c| c.client == client)
        .map(|cp| {
            cp.files
                .iter()
                .map(|f| (format!("{}:{}", f.root, f.path), f.action.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// `{relpath: bytes}` of every regular file under `p` (symlinks not followed).
pub fn snapshot(p: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = e.path();
            let Ok(md) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if md.is_dir() {
                stack.push(path);
            } else if md.is_file() {
                let rel = s(path.strip_prefix(p).unwrap_or(&path));
                out.insert(rel, std::fs::read(&path).unwrap_or_default());
            }
        }
    }
    out
}

/// Every path under `p` (files and dirs).
pub fn all_paths(p: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = e.path();
            if path.is_dir() && !path.is_symlink() {
                stack.push(path.clone());
            }
            out.push(path);
        }
    }
    out
}

pub fn tmp_files(p: &Path) -> usize {
    all_paths(p)
        .iter()
        .filter(|x| {
            x.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".hub-tmp-"))
        })
        .count()
}

pub fn mode_of(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0)
}

pub fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

// ---- in-process HTTP (TestClient) -------------------------------------------------------------------------------------

pub struct Resp {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    /// test_api.py `problem_code`.
    pub fn code(&self) -> String {
        self.json()["code"].as_str().unwrap_or_default().to_string()
    }
}

/// One request through `security::handle` (Host 127.0.0.1:8792, the CSRF header, a bearer key when given).
pub async fn req(app: &Arc<AppState>, method: &str, path: &str, key: Option<&str>, body: Option<Value>) -> Resp {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("host", HOST)
        .header("x-agent-hub", "1");
    if let Some(k) = key {
        b = b.header("authorization", format!("Bearer {k}"));
    }
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let r = b
        .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string().into_bytes())))
        .unwrap_or_default();
    let peer = SocketAddr::from(([10, 0, 0, 9], 5000));
    let resp = security::handle(app.clone(), peer, PORT, r).await;
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.map(|c| c.to_bytes().to_vec()).unwrap_or_default();
    Resp {
        status: parts.status.as_u16(),
        headers: parts.headers,
        body: bytes,
    }
}
