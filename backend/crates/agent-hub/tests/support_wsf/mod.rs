//! Shared helpers of the WS-F test files (policy & safety): ports of backend/tests/core/conftest.py (`cfg`, `hub`,
//! `add_client`) and fakes.py (`FakeAdapter`, `FakeRunner`), the real policy floor, and `reexec`, which re-runs one test
//! in a child process with a fixed environment (Rust tests cannot change their own environment: `set_var` is unsafe and
//! the workspace forbids unsafe code). Not every test file uses every helper.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_hub::adapters::AdapterRegistry;
use agent_hub::adapters::base::{
    AdapterCaps, AdapterResult, Artifact, Check, ClientAdapter, ClientConfig, CommandRunner, Diag, DiscoveredContent,
    Expected, FileSystem, InstructionItem, McpItem, PermissionSet, RenderContext, RenderResult, Rule, SkillItem,
};
use agent_hub::config::{Settings, app_root, load_settings_from};
use agent_hub::services::floor::{Floor, load_floor};
use agent_hub::services::hub::{Hub, HubOptions};
use hub_core::value::PyValue;
use indexmap::IndexMap;
use serde_json::{Value, json};

pub(crate) mod doctor_fakes;

pub(crate) type E = Box<dyn std::error::Error>;
pub(crate) type R = Result<(), E>;

/// A `Problem` as a boxed error (its detail).
#[allow(clippy::needless_pass_by_value)] // used as `map_err(p)`
pub(crate) fn p(e: hub_core::Problem) -> E {
    format!("{} {}: {}", e.status, e.code, e.detail).into()
}

pub(crate) fn rule(kind: &str, m: &str, decision: &str) -> Rule {
    Rule {
        kind: kind.into(),
        matches: m.into(),
        decision: decision.into(),
        reason: None,
    }
}

/// `Floor(load_floor(APP_ROOT / "policy" / "floor.yaml"))`.
pub(crate) fn real_floor() -> Result<Floor, E> {
    Ok(Floor::new(
        load_floor(&app_root().join("policy").join("floor.yaml")).map_err(p)?,
    ))
}

#[allow(clippy::needless_pass_by_value)] // called with json!(...) literals
pub(crate) fn pv(v: Value) -> PyValue {
    PyValue::from_json(&v)
}

/// A JSON object as the `dict[str, Any]` hub methods take.
pub(crate) fn dict(v: &Value) -> IndexMap<String, PyValue> {
    v.as_object()
        .map(|m| m.iter().map(|(k, x)| (k.clone(), PyValue::from_json(x))).collect())
        .unwrap_or_default()
}

/// The current user's home (what Python's `Path.home()` and the floor's `~` mean in the test process).
pub(crate) fn home() -> PathBuf {
    agent_hub::config::home()
}

// ---- conftest.py -------------------------------------------------------------------------------------------------------

/// conftest.py `cfg`: content and state under a temp dir, the app's real policy floor, no knowledge key.
pub(crate) fn cfg(tmp: &Path, extra: &[(&str, PyValue)]) -> Result<Settings, E> {
    let env = std::collections::HashMap::from([
        ("HOME".to_string(), home().to_string_lossy().to_string()),
        (
            "AGENT_HUB_ENV_FILE".to_string(),
            tmp.join("no-such-env-file").to_string_lossy().to_string(),
        ),
    ]);
    let mut o: Vec<(String, PyValue)> = vec![
        (
            "content_dir".into(),
            PyValue::str(tmp.join("content").to_string_lossy()),
        ),
        ("data_dir".into(), PyValue::str(tmp.join("state").to_string_lossy())),
        (
            "knowledge_admin_key_file".into(),
            PyValue::str(tmp.join("no-knowledge-key").to_string_lossy()),
        ),
        ("inbox_rate_per_min".into(), PyValue::Int(100)),
    ];
    o.extend(extra.iter().map(|(k, v)| ((*k).to_string(), v.clone())));
    Ok(load_settings_from(&env, &o)?)
}

/// conftest.py `hub`: a Hub on `cfg` with the fake adapter (plus `more`), its content repo initialised.
/// Python registers extra adapters on the live hub; the Rust registry is fixed at construction, so they come here.
pub(crate) fn hub_with(
    tmp: &Path,
    extra: &[(&str, PyValue)],
    more: Vec<Arc<dyn ClientAdapter>>,
) -> Result<Arc<Hub>, E> {
    let mut reg = AdapterRegistry::new();
    reg.register(Arc::new(FakeAdapter::default()))?;
    for a in more {
        reg.register(a)?;
    }
    let hub = Hub::new(
        cfg(tmp, extra)?,
        HubOptions {
            registry: Some(reg),
            runner: Some(Arc::new(FakeRunner)),
            ..HubOptions::default()
        },
    )
    .map_err(p)?;
    hub.init_content().map_err(p)?;
    Ok(hub)
}

pub(crate) fn hub(tmp: &Path) -> Result<Arc<Hub>, E> {
    hub_with(tmp, &[], Vec::new())
}

/// conftest.py `home`: the fake client's config dir (a temp dir, never a real client location).
pub(crate) fn client_home(tmp: &Path) -> Result<PathBuf, E> {
    let d = tmp.join("client-home");
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// conftest.py `add_client(hub, home, cid="fake1", **extra)`.
pub(crate) fn add_client(hub: &Hub, home: &Path, cid: &str, extra: &Value) -> R {
    let mut doc = json!({"adapter": "fake", "display_name": "Fake 1", "roots": {"home": home.to_string_lossy()}});
    if let (Some(d), Some(x)) = (doc.as_object_mut(), extra.as_object()) {
        for (k, v) in x {
            d.insert(k.clone(), v.clone());
        }
    }
    hub.put_client(cid, &dict(&doc), true).map_err(p)?;
    Ok(())
}

pub(crate) fn put(hub: &Hub, kind: &str, name: &str, v: Value) -> R {
    hub.work.put(kind, name, &pv(v), None).map_err(p)?;
    Ok(())
}

pub(crate) fn put_profile(hub: &Hub, client: &str, v: Value) -> R {
    hub.work.put_profile(client, &pv(v)).map_err(p)?;
    Ok(())
}

pub(crate) fn commit(hub: &Hub) -> R {
    hub.commit("c", "t").map_err(p)?;
    Ok(())
}

pub(crate) fn plan(hub: &Hub, clients: Option<&[&str]>) -> Result<hub_core::models::Plan, E> {
    let owned: Option<Vec<String>> = clients.map(|c| c.iter().map(|s| (*s).to_string()).collect());
    hub.plan(owned.as_deref(), "committed").map_err(p)
}

// ---- fakes.py ----------------------------------------------------------------------------------------------------------

/// `FakeRunner`: every command succeeds with "ok".
pub(crate) struct FakeRunner;

impl CommandRunner for FakeRunner {
    fn run(&self, _argv: &[String], _timeout: f64) -> std::io::Result<(i32, String, String)> {
        Ok((0, "ok".into(), String::new()))
    }
}

pub(crate) const BEGIN: &str = "<!-- agent-hub:begin (managed block; edits inside are overwritten) -->";
pub(crate) const END: &str = "<!-- agent-hub:end -->";

/// What a scripted adapter renders instead of FakeAdapter's own render.
pub(crate) type RenderFn = Arc<dyn Fn(&RenderContext) -> Vec<Artifact> + Send + Sync>;

/// fakes.py `FakeAdapter` (every merge mode), optionally with another id and a scripted render (the test files'
/// `Leaky`, `Sneaky`, `LeakyToml` subclasses).
#[derive(Clone, Default)]
pub(crate) struct FakeAdapter {
    pub(crate) id: Option<String>,
    pub(crate) memory: bool,
    pub(crate) render_with: Option<RenderFn>,
}

impl FakeAdapter {
    pub(crate) fn scripted(
        id: &str,
        f: impl Fn(&RenderContext) -> Vec<Artifact> + Send + Sync + 'static,
    ) -> Arc<dyn ClientAdapter> {
        Arc::new(Self {
            id: Some(id.into()),
            memory: false,
            render_with: Some(Arc::new(f)),
        })
    }

    fn tool_map() -> IndexMap<String, Option<String>> {
        IndexMap::from([
            ("read".to_string(), Some("Read".to_string())),
            ("shell".to_string(), Some("Bash".to_string())),
            ("browser".to_string(), None),
        ])
    }
}

/// `Artifact(root, path, content, kind, merge, managed_keys, source_ids)`.
pub(crate) fn artifact(root: &str, path: &str, content: &[u8], kind: &str, merge: &str, keys: &[&str]) -> Artifact {
    let mut a = Artifact::new(root, path, content.to_vec(), kind);
    a.merge = merge.into();
    a.managed_keys = keys.iter().map(|k| (*k).to_string()).collect();
    a
}

impl ClientAdapter for FakeAdapter {
    fn id(&self) -> &str {
        self.id.as_deref().unwrap_or("fake")
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
        if let Some(f) = &self.render_with {
            return Ok(RenderResult {
                artifacts: f(ctx),
                diagnostics: Vec::new(),
            });
        }
        let mut arts = Vec::new();
        let mut diags = Vec::new();
        for s in &ctx.skills {
            let mut files: Vec<_> = s.files.iter().collect();
            files.sort_by(|a, b| a.0.cmp(b.0));
            for (rel, blob) in files {
                let mut a = Artifact::new("home", &format!("skills/{}/{rel}", s.name), blob.clone(), "skill");
                a.source_ids = vec![s.name.clone()];
                arts.push(a);
            }
        }
        let tm = Self::tool_map();
        for a in &ctx.agents {
            for c in &a.capabilities {
                if matches!(tm.get(c), Some(None)) {
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
            let mut a = artifact(
                "home",
                "CLAUDE.md",
                format!("{BEGIN}\n{}\n{END}\n", body.join("\n\n")).as_bytes(),
                "instruction",
                "block",
                &[],
            );
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
        let mcp = hub_core::py::json::dumps(&json!({"mcpServers": servers}), &hub_core::py::json::Opts::default());
        let mut a = artifact("home", ".mcp.json", mcp.as_bytes(), "mcp", "json_keys", &["mcpServers"]);
        a.source_ids = ids;
        arts.push(a);
        let mut perms: IndexMap<&str, Vec<String>> =
            IndexMap::from([("deny", vec![]), ("ask", vec![]), ("allow", vec![])]);
        for r in &ctx.permissions.rules {
            if let Some(v) = perms.get_mut(r.decision.as_str()) {
                v.push(format!("{}:{}", r.kind, r.matches));
            }
        }
        let settings = hub_core::py::json::dumps(&json!({"permissions": perms}), &hub_core::py::json::Opts::default());
        let mut a = artifact(
            "home",
            "settings.json",
            settings.as_bytes(),
            "rule",
            "json_keys",
            &["permissions.deny", "permissions.ask", "permissions.allow"],
        );
        a.source_ids = vec!["floor".into()];
        arts.push(a);
        Ok(RenderResult {
            artifacts: arts,
            diagnostics: diags,
        })
    }
    fn discover(&self, cfg: &ClientConfig, fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> {
        let io = |e: std::io::Error| agent_hub::adapters::base::AdapterError::new("OSError", e.to_string());
        let home = cfg.roots.get("home").cloned().unwrap_or_default();
        let mut dc = DiscoveredContent::default();
        let sk = format!("{home}/skills");
        if fs.is_dir(&sk) {
            for name in fs.listdir(&sk).map_err(io)? {
                let d = format!("{sk}/{name}");
                if !fs.is_dir(&d) {
                    continue;
                }
                let mut files = agent_hub::adapters::base::Files::new();
                for f in fs.listdir(&d).map_err(io)? {
                    files.insert(f.clone(), fs.read_bytes(&format!("{d}/{f}")).map_err(io)?);
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
        let bad = |e: serde_json::Error| agent_hub::adapters::base::AdapterError::new("JSONDecodeError", e.to_string());
        if fs.exists(&format!("{home}/.mcp.json")) {
            let doc: Value =
                serde_json::from_slice(&fs.read_bytes(&format!("{home}/.mcp.json")).map_err(io)?).map_err(bad)?;
            if let Some(m) = doc.get("mcpServers").and_then(Value::as_object) {
                for (n, v) in m {
                    dc.mcp_servers.push(McpItem {
                        name: n.clone(),
                        transport: "stdio".into(),
                        command: Some(v.get("command").and_then(Value::as_str).unwrap_or("x").to_string()),
                        args: v
                            .get("args")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                            .unwrap_or_default(),
                        scan_status: "clean".into(), // a foreign claim of `clean` must be ignored on import
                        ..McpItem::default()
                    });
                }
            }
        }
        if fs.exists(&format!("{home}/settings.json")) {
            let doc: Value =
                serde_json::from_slice(&fs.read_bytes(&format!("{home}/settings.json")).map_err(io)?).map_err(bad)?;
            let mut rules = Vec::new();
            if let Some(m) = doc.get("permissions").and_then(Value::as_object) {
                for (dec, items) in m {
                    for it in items.as_array().into_iter().flatten().filter_map(Value::as_str) {
                        let (k, m) = it.split_once(':').unwrap_or((it, ""));
                        rules.push(rule(k, m, dec));
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
                body: String::from_utf8_lossy(&fs.read_bytes(&format!("{home}/CLAUDE.md")).map_err(io)?).into_owned(),
            });
        }
        Ok(dc)
    }
    fn verify(
        &self,
        _cfg: &ClientConfig,
        _expected: &[Expected],
        _fs: &dyn FileSystem,
        _run: &dyn CommandRunner,
    ) -> AdapterResult<Vec<Check>> {
        Ok(Vec::new())
    }
}

// ---- re-running one test in a child process ------------------------------------------------------------------------

/// A HOME below /home (and outside /tmp) that does not exist. Tests whose expectations assume such a home (the floor's
/// `/tmp/**` is not near `~`, `/home` contains `~`) re-run themselves with it, so they also pass where HOME is /root,
/// under /tmp or anywhere else (audit F9).
pub(crate) const FIXED_HOME: &str = "/home/agent-hub-test";

/// `Ok(true)` when this process IS the child for `name` (run the body); otherwise spawn this test binary once more,
/// running only `name` (ignored or not) with `envs` set and common provider API-key variables removed, assert that it ran and
/// passed, and return `Ok(false)` (the caller returns).
pub(crate) fn reexec(name: &str, envs: &[(&str, &str)]) -> Result<bool, E> {
    if std::env::var("WSF_CHILD_TEST").as_deref() == Ok(name) {
        return Ok(true);
    }
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args([
        name,
        "--exact",
        "--include-ignored",
        "--test-threads",
        "1",
        "--nocapture",
    ])
    .env("WSF_CHILD_TEST", name);
    for k in [
        "LITELLM_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "HF_TOKEN",
        "HUB_API_KEY",
    ] {
        cmd.env_remove(k);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output()?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() || !text.contains("1 passed") {
        return Err(format!("child run of {name} failed:\n{text}").into());
    }
    Ok(false)
}
