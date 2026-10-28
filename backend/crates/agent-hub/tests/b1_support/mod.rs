//! Shared fixtures for WS-B1's ports of backend/tests/core (conftest.py + fakes.py): a hub on temp dirs with the
//! FakeAdapter registered, `add_client`, `seed_content`, and the in-process HTTP client of tests/api_security.rs.
#![allow(dead_code, unreachable_pub)]

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_hub::adapters::AdapterRegistry;
use agent_hub::adapters::base::{
    AdapterCaps, AdapterResult, AgentItem, Artifact, Check, ClientAdapter, ClientConfig, CommandRunner, Diag,
    DiscoveredContent, Expected, FileSystem, InstructionItem, McpItem, PermissionSet, RenderContext, RenderResult,
    Rule, SkillItem,
};
use agent_hub::api::{AppState, security};
use agent_hub::config::load_settings_from;
use agent_hub::server::{create_app, on_startup};
use agent_hub::services::hub::{Hub, HubOptions};
use axum::body::Body;
use http_body_util::BodyExt;
use hub_core::py::json as pyjson;
use hub_core::value::PyValue;
use hub_core::{HubResult, Problem};
use indexmap::IndexMap;
use serde_json::{Value, json};

pub type R = Result<(), Box<dyn std::error::Error>>;
pub const API: &str = "/api/v1";

pub const BEGIN: &str = "<!-- agent-hub:begin (managed block; edits inside are overwritten) -->";
pub const END: &str = "<!-- agent-hub:end -->";

/// fakes.py `FakeAdapter`: exercises every merge mode; discover reads `<home>/skills`, `agents`, `.mcp.json`,
/// `settings.json`, `CLAUDE.md` through the FileSystem port.
pub struct FakeAdapter {
    pub id: String,
    pub memory: bool,
}

impl FakeAdapter {
    pub fn new() -> Self {
        FakeAdapter {
            id: "fake".into(),
            memory: false,
        }
    }
    fn tool_map() -> IndexMap<String, Option<String>> {
        IndexMap::from([
            ("read".to_string(), Some("Read".to_string())),
            ("shell".to_string(), Some("Bash".to_string())),
            ("browser".to_string(), None),
        ])
    }
}

fn io_err(e: &io::Error) -> agent_hub::adapters::base::AdapterError {
    agent_hub::adapters::base::AdapterError::new(hub_core::py::os::error_name(e), e.to_string())
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
            memory: self.memory,
            tool_map: Self::tool_map(),
            model_tiers: IndexMap::new(),
            notes: "fake".into(),
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
        let mut arts = Vec::new();
        let mut diags = Vec::new();
        for s in &ctx.skills {
            let mut files: Vec<(&String, &Vec<u8>)> = s.files.iter().collect();
            files.sort();
            for (rel, blob) in files {
                let mut a = Artifact::new("home", &format!("skills/{}/{rel}", s.name), blob.clone(), "skill");
                a.source_ids = vec![s.name.clone()];
                arts.push(a);
            }
        }
        let tm = Self::tool_map();
        for a in &ctx.agents {
            for c in &a.capabilities {
                if tm.get(c).is_some_and(Option::is_none) {
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
            let body = ctx
                .instructions
                .iter()
                .map(|i| format!("## {}\n\n{}", i.title, hub_core::py::text::strip(&i.body)))
                .collect::<Vec<_>>()
                .join("\n\n");
            let mut a = Artifact::new(
                "home",
                "CLAUDE.md",
                format!("{BEGIN}\n{body}\n{END}\n").into_bytes(),
                "instruction",
            );
            a.merge = "block".into();
            a.source_ids = ctx.instructions.iter().map(|i| i.id.clone()).collect();
            arts.push(a);
        }
        let mut servers = serde_json::Map::new();
        for m in &ctx.mcp_servers {
            servers.insert(
                m.name.clone(),
                json!({"command": m.command, "args": m.args, "url": m.url}),
            );
        }
        let mut ids: Vec<String> = servers.keys().cloned().collect();
        ids.sort();
        let mut mcp = Artifact::new(
            "home",
            ".mcp.json",
            pyjson::dumps(&json!({"mcpServers": servers}), &pyjson::Opts::default()).into_bytes(),
            "mcp",
        );
        mcp.merge = "json_keys".into();
        mcp.managed_keys = vec!["mcpServers".into()];
        mcp.source_ids = ids;
        arts.push(mcp);
        let mut perms: IndexMap<String, Vec<String>> = ["deny", "ask", "allow"]
            .iter()
            .map(|d| ((*d).to_string(), Vec::new()))
            .collect();
        for r in &ctx.permissions.rules {
            perms
                .entry(r.decision.clone())
                .or_default()
                .push(format!("{}:{}", r.kind, r.matches));
        }
        let mut st = Artifact::new(
            "home",
            "settings.json",
            pyjson::dumps(&json!({"permissions": perms}), &pyjson::Opts::default()).into_bytes(),
            "rule",
        );
        st.merge = "json_keys".into();
        st.managed_keys = vec![
            "permissions.deny".into(),
            "permissions.ask".into(),
            "permissions.allow".into(),
        ];
        st.source_ids = vec!["floor".into()];
        arts.push(st);
        Ok(RenderResult {
            artifacts: arts,
            diagnostics: diags,
        })
    }
    fn discover(&self, cfg: &ClientConfig, fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> {
        let home = cfg.roots.get("home").cloned().unwrap_or_default();
        let mut dc = DiscoveredContent::default();
        let sk = format!("{home}/skills");
        if fs.is_dir(&sk) {
            for name in fs.listdir(&sk).map_err(|e| io_err(&e))? {
                let d = format!("{sk}/{name}");
                if !fs.is_dir(&d) {
                    continue;
                }
                let mut files = agent_hub::adapters::base::Files::new();
                for f in fs.listdir(&d).map_err(|e| io_err(&e))? {
                    files.insert(f.clone(), fs.read_bytes(&format!("{d}/{f}")).map_err(|e| io_err(&e))?);
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
            for f in fs.listdir(&ag).map_err(|e| io_err(&e))? {
                if let Some(stem) = f.strip_suffix(".md") {
                    let body = fs.read_bytes(&format!("{ag}/{f}")).map_err(|e| io_err(&e))?;
                    dc.agents.push(AgentItem {
                        name: stem.to_string(),
                        description: "found agent".into(),
                        capabilities: vec!["read".into()],
                        model_tier: None,
                        mode: "subagent".into(),
                        read_only: false,
                        body: String::from_utf8_lossy(&body).into_owned(),
                    });
                }
            }
        }
        if fs.exists(&format!("{home}/.mcp.json")) {
            let raw = fs.read_bytes(&format!("{home}/.mcp.json")).map_err(|e| io_err(&e))?;
            let doc: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
            if let Some(m) = doc.get("mcpServers").and_then(Value::as_object) {
                for (n, v) in m {
                    dc.mcp_servers.push(McpItem {
                        name: n.clone(),
                        transport: "stdio".into(),
                        command: Some(v.get("command").and_then(Value::as_str).unwrap_or("x").to_string()),
                        args: v
                            .get("args")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                            .unwrap_or_default(),
                        scan_status: "clean".into(), // a foreign claim of `clean` must be ignored on import
                        ..McpItem::default()
                    });
                }
            }
        }
        if fs.exists(&format!("{home}/settings.json")) {
            let raw = fs
                .read_bytes(&format!("{home}/settings.json"))
                .map_err(|e| io_err(&e))?;
            let doc: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
            let mut rules = Vec::new();
            if let Some(p) = doc.get("permissions").and_then(Value::as_object) {
                for (dec, items) in p {
                    for it in items.as_array().into_iter().flatten() {
                        let s = it.as_str().unwrap_or_default();
                        let (k, m) = s.split_once(':').unwrap_or((s, ""));
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
            let raw = fs.read_bytes(&format!("{home}/CLAUDE.md")).map_err(|e| io_err(&e))?;
            dc.instructions.push(InstructionItem {
                id: "native".into(),
                title: "Native".into(),
                order: 1,
                body: String::from_utf8_lossy(&raw).into_owned(),
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
            let p = format!("{}/{}", cfg.roots.get(&e.root).cloned().unwrap_or_default(), e.path);
            let ok = fs.exists(&p)
                && fs
                    .read_bytes(&p)
                    .is_ok_and(|b| e.merge != "own" || hub_core::common::sha256(&b) == e.sha256);
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
        let (rc, o, _) = run
            .run(&["fake-client".to_string(), "--list".to_string()], 5.0)
            .map_err(|e| io_err(&e))?;
        out.push(Check {
            name: "fake-client --list".into(),
            ok: rc == 0,
            detail: hub_core::py::text::strip(&o).to_string(),
        });
        Ok(out)
    }
}

/// fakes.py `FakeRunner`.
pub struct FakeRunner;

impl CommandRunner for FakeRunner {
    fn run(&self, _argv: &[String], _timeout: f64) -> io::Result<(i32, String, String)> {
        Ok((0, "ok".into(), String::new()))
    }
}

pub struct T {
    pub tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub hub: Arc<Hub>,
}

impl T {
    pub fn content(&self) -> PathBuf {
        self.hub.cfg().content_dir
    }
    /// conftest `home`: the fake client's config dir.
    pub fn home(&self) -> PathBuf {
        let d = self.root.join("client-home");
        let _ = std::fs::create_dir_all(&d);
        d
    }
}

pub fn registry(extra: Vec<Arc<dyn ClientAdapter>>) -> AdapterRegistry {
    let mut r = AdapterRegistry::new();
    let _ = r.register(Arc::new(FakeAdapter::new()));
    for a in extra {
        let _ = r.register(a);
    }
    r
}

fn is_not_ported(p: &Problem) -> bool {
    p.code == "not_ported" || p.detail.contains("not ported")
}

/// conftest `hub`: Settings on temp dirs (inbox_rate_per_min=100), the FakeAdapter registry, `init_content()`. Until
/// WS-B2's `init_content` is merged, a not-ported init falls back to creating and seeding the content dir.
pub fn hub_with(
    extra: Vec<Arc<dyn ClientAdapter>>,
    env_extra: &[(&str, &str)],
) -> Result<T, Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().to_path_buf();
    let mut env: HashMap<String, String> = HashMap::from([
        ("HOME".to_string(), root.join("home").to_string_lossy().into_owned()),
        (
            "AGENT_HUB_ENV_FILE".to_string(),
            root.join("no-env").to_string_lossy().into_owned(),
        ),
        ("HUB_INBOX_RATE_PER_MIN".to_string(), "100".to_string()),
        ("HUB_TRUST_LOOPBACK".to_string(), "false".to_string()),
    ]);
    for (k, v) in env_extra {
        env.insert((*k).to_string(), (*v).to_string());
    }
    let cfg = load_settings_from(
        &env,
        &vec![
            ("data_dir".into(), PyValue::str(root.join("state").to_string_lossy())),
            (
                "content_dir".into(),
                PyValue::str(root.join("content").to_string_lossy()),
            ),
            (
                "knowledge_admin_key_file".into(),
                PyValue::str(root.join("no-knowledge-key").to_string_lossy()),
            ),
        ],
    )?;
    let opts = HubOptions {
        registry: Some(registry(extra)),
        runner: Some(Arc::new(FakeRunner)),
        ..HubOptions::default()
    };
    let hub = Hub::new(cfg, opts).map_err(|p| p.detail)?;
    match hub.init_content() {
        Ok(_) => {}
        Err(p) if is_not_ported(&p) => {
            let c = hub.cfg().content_dir;
            std::fs::create_dir_all(&c)?;
            std::fs::set_permissions(&c, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
            hub.work.seed().map_err(|p| p.detail)?;
        }
        Err(p) => return Err(p.detail.into()),
    }
    Ok(T { tmp, root, hub })
}

pub fn hub() -> Result<T, Box<dyn std::error::Error>> {
    hub_with(Vec::new(), &[])
}

#[allow(clippy::needless_pass_by_value)] // called with json!(...) literals
fn payload(v: Value) -> IndexMap<String, PyValue> {
    v.as_object()
        .map(|m| m.iter().map(|(k, x)| (k.clone(), PyValue::from_json(x))).collect())
        .unwrap_or_default()
}

/// conftest `add_client`: `hub.put_client(cid, {adapter: fake, display_name, roots: {home}}, create=True)`. Until
/// WS-F's target guard is merged (its stub refuses every root), the same document is written through the store.
#[allow(clippy::needless_pass_by_value)] // called with json!(...) literals
pub fn add_client_with(h: &Hub, home: &Path, cid: &str, extra: Value) -> HubResult<()> {
    let mut doc = json!({"adapter": "fake", "display_name": "Fake 1", "roots": {"home": home.to_string_lossy()}});
    if let (Some(d), Some(x)) = (doc.as_object_mut(), extra.as_object()) {
        for (k, v) in x {
            d.insert(k.clone(), v.clone());
        }
    }
    match h.put_client(cid, &payload(doc.clone()), true) {
        Ok(_) => Ok(()),
        Err(p) if is_not_ported(&p) => {
            let mut d = doc.as_object().cloned().unwrap_or_default();
            d.insert("id".into(), json!(cid));
            h.work
                .put_client(cid, &PyValue::from_json(&Value::Object(d)))
                .map(|_| ())
        }
        Err(p) => Err(p),
    }
}

pub fn add_client(h: &Hub, home: &Path) -> HubResult<()> {
    add_client_with(h, home, "fake1", json!({}))
}

#[allow(clippy::needless_pass_by_value)] // called with json!(...) literals
pub fn pv(v: Value) -> PyValue {
    PyValue::from_json(&v)
}

/// conftest `seed_content`: one of every kind, all enabled for `client` through a collection and a direct enable.
pub fn seed_content(h: &Hub, client: &str) -> HubResult<()> {
    let w = &h.work;
    w.put(
        "skill",
        "alpha",
        &pv(
            json!({"description": "Alpha skill", "body": "Do alpha.\n", "files": {"ref/notes.md": "notes"},
                   "tags": ["core"]}),
        ),
        None,
    )?;
    w.put(
        "agent",
        "reviewer",
        &pv(json!({"description": "Reviews code", "capabilities": ["read"], "body": "Review carefully.\n"})),
        None,
    )?;
    w.put(
        "instruction",
        "style",
        &pv(json!({"title": "Style", "order": 10, "body": "Be concise.\n"})),
        None,
    )?;
    w.put(
        "rule",
        "no-web",
        &pv(json!({"title": "No web", "kind": "tool", "match": "web_fetch", "decision": "deny"})),
        None,
    )?;
    w.put(
        "memory",
        "fact1",
        &pv(json!({"type": "reference", "title": "A fact", "body": "The sky is blue.\n"})),
        None,
    )?;
    w.put_collection(
        "core",
        &pv(
            json!({"id": "core", "title": "Core", "members": [{"kind": "skill", "name": "alpha"},
                                                                {"kind": "instruction", "name": "style"}]}),
        ),
    )?;
    w.put_profile(
        client,
        &pv(json!({"collections": ["core"], "enable": {"agents": ["reviewer"], "rules": ["no-web"]}})),
    )?;
    Ok(())
}

// ---- in-process HTTP (tests/api_security.rs style) ----------------------------------------------------------------
pub struct Resp {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn code(&self) -> String {
        self.json()["code"].as_str().unwrap_or_default().to_string()
    }
}

pub struct Api {
    pub app: Arc<AppState>,
}

pub fn api(t: &T) -> Api {
    on_startup(&t.hub);
    Api {
        app: create_app(t.hub.clone()),
    }
}

impl Api {
    pub async fn req(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Option<Value>) -> Resp {
        let mut b = http::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "127.0.0.1:8792");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let r = b
            .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string().into_bytes())))
            .unwrap_or_default();
        let peer = SocketAddr::from(([10, 0, 0, 9], 5000));
        let resp = security::handle(self.app.clone(), peer, 8792, r).await;
        let (parts, body) = resp.into_parts();
        let bytes = body.collect().await.map(|c| c.to_bytes().to_vec()).unwrap_or_default();
        Resp {
            status: parts.status.as_u16(),
            headers: parts.headers,
            body: bytes,
        }
    }
}

pub fn admin_key(t: &T) -> String {
    t.hub
        .auth
        .create_key("t-admin", "admin", None, &[])
        .ok()
        .and_then(|v| v["secret"].as_str().map(str::to_string))
        .unwrap_or_default()
}
