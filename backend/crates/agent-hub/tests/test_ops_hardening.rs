//! Port of the CORE rows of backend/tests/core/test_ops_hardening.py (L2-L4): a public bind is refused by both entry
//! points, the hub's state directories are private (and tightened on every start), and viewers cannot read the
//! knowledge paths that list credentials. `test_check_bind` itself lives in src/config.rs; the git rows belong to WS-B2.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_hub::adapters::AdapterRegistry;
use agent_hub::adapters::base::{
    AdapterCaps, AdapterResult, Artifact, Check, ClientAdapter, ClientConfig, CommandRunner, DiscoveredContent,
    Expected, FileSystem, RenderContext, RenderResult,
};
use agent_hub::api::{AppState, security};
use agent_hub::config::{Settings, load_settings_from};
use agent_hub::server::{create_app, on_startup};
use agent_hub::services::hub::{Hub, HubOptions};
use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use hub_core::value::PyValue;
use indexmap::IndexMap;
use serde_json::json;

type R = Result<(), Box<dyn std::error::Error>>;

/// conftest.py `cfg`: content and state under a temp dir, the app's real policy floor, no knowledge key.
fn cfg(tmp: &Path, extra: &[(&str, PyValue)]) -> Result<Settings, Box<dyn std::error::Error>> {
    let env = std::collections::HashMap::from([
        ("HOME".to_string(), tmp.join("home").to_string_lossy().to_string()),
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
        ("inbox_rate_per_min".into(), PyValue::str("100")),
    ];
    o.extend(extra.iter().map(|(k, v)| ((*k).to_string(), v.clone())));
    Ok(load_settings_from(&env, &o)?)
}

/// test_ops_hardening.py `modes`: the mode of the root and of every directory under it (symlinks not followed).
fn modes(root: &Path) -> std::collections::BTreeSet<u32> {
    let mut out = std::collections::BTreeSet::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(st) = std::fs::symlink_metadata(&d) else {
            continue;
        };
        if !st.is_dir() {
            continue;
        }
        out.insert(st.permissions().mode() & 0o777);
        if let Ok(rd) = std::fs::read_dir(&d) {
            stack.extend(rd.flatten().map(|e| e.path()));
        }
    }
    out
}

// ---- L2: loopback-only bind ----------------------------------------------------------------------------------------

fn refuses_public_bind(bin: &str, args: &[&str]) -> R {
    let d = tempfile::tempdir()?;
    let free = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port(); // never a live port, even on a bug
    let out = std::process::Command::new(bin)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", d.path())
        .env("HUB_HOST", "0.0.0.0")
        .env("HUB_PORT", free.to_string())
        .env("HUB_CONTENT_DIR", d.path().join("c"))
        .env("HUB_DATA_DIR", d.path().join("s"))
        .env("AGENT_HUB_ENV_FILE", d.path().join("none"))
        .output()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("HUB_ALLOW_NONLOOPBACK"), "{text}");
    Ok(())
}

/// `[entry1]`: `python -c "from agent_hub.main import run; run()"` = the `agent-hub` binary.
#[test]
fn test_serve_and_main_refuse_a_public_bind() -> R {
    refuses_public_bind(env!("CARGO_BIN_EXE_agent-hub"), &[])
}

/// `[entry0]`: `python -m agent_hub.cli serve` = `hubctl serve`.
#[test]
fn test_serve_and_main_refuse_a_public_bind_hubctl_serve() -> R {
    refuses_public_bind(env!("CARGO_BIN_EXE_hubctl"), &["serve"])
}

// ---- L3: private state directories ---------------------------------------------------------------------------------

/// fakes.py `FakeAdapter`, reduced to what this test renders: skills and agents under the `home` root.
struct Fake;

impl ClientAdapter for Fake {
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
            agents: true,
            instructions: true,
            mcp: true,
            permissions: true,
            egress: true,
            notes: "fake".into(),
            ..AdapterCaps::default()
        }
    }
    fn default_config(&self) -> ClientConfig {
        ClientConfig {
            id: "fake".into(),
            adapter: "fake".into(),
            display_name: "Fake".into(),
            description: String::new(),
            roots: IndexMap::from([("home".to_string(), "~/.fake".to_string())]),
            params: PyValue::Dict(Vec::new()),
            spec: None,
            strict: false,
            manage: IndexMap::new(),
            icon: None,
            color: None,
        }
    }
    fn render(&self, ctx: &RenderContext) -> AdapterResult<RenderResult> {
        let mut arts = Vec::new();
        for s in &ctx.skills {
            let mut files: Vec<_> = s.files.iter().collect();
            files.sort_by(|a, b| a.0.cmp(b.0));
            for (rel, blob) in files {
                let mut a = Artifact::new("home", &format!("skills/{}/{rel}", s.name), blob.clone(), "skill");
                a.source_ids = vec![s.name.clone()];
                arts.push(a);
            }
        }
        for a in &ctx.agents {
            let mut art = Artifact::new(
                "home",
                &format!("agents/{}.md", a.name),
                format!("# {}\n{}", a.name, a.body).into_bytes(),
                "agent",
            );
            art.source_ids = vec![a.name.clone()];
            arts.push(art);
        }
        Ok(RenderResult {
            artifacts: arts,
            diagnostics: Vec::new(),
        })
    }
    fn discover(&self, _cfg: &ClientConfig, _fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> {
        Ok(DiscoveredContent::default())
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

#[allow(clippy::needless_pass_by_value)] // called with json!(...) literals
fn pv(v: serde_json::Value) -> PyValue {
    PyValue::from_json(&v)
}

#[test]
fn test_state_directories_are_private() -> R {
    let d = tempfile::tempdir()?;
    let home = d.path().join("client-home");
    std::fs::create_dir(&home)?;
    let mut reg = AdapterRegistry::new();
    reg.register(Arc::new(Fake))?;
    let hub = Hub::new(
        cfg(d.path(), &[])?,
        HubOptions {
            registry: Some(reg),
            ..HubOptions::default()
        },
    )
    .map_err(|p| p.detail)?;
    hub.init_content().map_err(|p| p.detail)?;
    // conftest.add_client + seed_content
    let client: IndexMap<String, PyValue> = IndexMap::from([
        ("adapter".to_string(), PyValue::str("fake")),
        ("display_name".to_string(), PyValue::str("Fake 1")),
        ("roots".to_string(), pv(json!({"home": home.to_string_lossy()}))),
    ]);
    hub.put_client("fake1", &client, true).map_err(|p| p.detail)?;
    let w = &hub.work;
    let put = |kind: &str, name: &str, v: serde_json::Value| w.put(kind, name, &pv(v), None).map(|_| ());
    put("skill", "alpha", json!({"description": "Alpha skill", "body": "Do alpha.\n", "files": {"ref/notes.md": "notes"}, "tags": ["core"]}))
        .map_err(|p| p.detail)?;
    put(
        "agent",
        "reviewer",
        json!({"description": "Reviews code", "capabilities": ["read"], "body": "Review carefully.\n"}),
    )
    .map_err(|p| p.detail)?;
    put(
        "instruction",
        "style",
        json!({"title": "Style", "order": 10, "body": "Be concise.\n"}),
    )
    .map_err(|p| p.detail)?;
    put(
        "rule",
        "no-web",
        json!({"title": "No web", "kind": "tool", "match": "web_fetch", "decision": "deny"}),
    )
    .map_err(|p| p.detail)?;
    put(
        "memory",
        "fact1",
        json!({"type": "reference", "title": "A fact", "body": "The sky is blue.\n"}),
    )
    .map_err(|p| p.detail)?;
    w.put_collection(
        "core",
        &pv(json!({"id": "core", "title": "Core", "members": [{"kind": "skill", "name": "alpha"}, {"kind": "instruction", "name": "style"}]})),
    )
    .map_err(|p| p.detail)?;
    w.put_profile(
        "fake1",
        &pv(json!({"collections": ["core"], "enable": {"agents": ["reviewer"], "rules": ["no-web"]}})),
    )
    .map_err(|p| p.detail)?;
    hub.commit("c", "t").map_err(|p| p.detail)?;
    std::fs::create_dir(home.join("agents"))?;
    std::fs::write(home.join("agents/reviewer.md"), "old")?;
    let plan = hub.plan(None, "committed").map_err(|p| p.detail)?;
    // creates plans, snapshots, locks, backups
    hub.apply(&plan.id, true, &["home:agents/reviewer.md".to_string()], "t")
        .map_err(|p| p.detail)?;
    let data = hub.cfg().data_dir;
    for sub in ["plans", "snapshots", "locks", "backups"] {
        assert!(data.join(sub).is_dir(), "{sub}");
    }
    assert_eq!(modes(&data), std::collections::BTreeSet::from([0o700]));
    // client dirs keep normal modes
    assert_eq!(
        std::fs::metadata(home.join("skills"))?.permissions().mode() & 0o777,
        0o755
    );
    hub.close();
    Ok(())
}

#[test]
fn test_existing_loose_state_dirs_are_tightened_on_startup() -> R {
    let d = tempfile::tempdir()?;
    let c = cfg(d.path(), &[])?;
    for sub in ["plans", "backups/c1/2026", "snapshots/abc/sub"] {
        std::fs::create_dir_all(c.data_dir.join(sub))?;
        std::fs::set_permissions(c.data_dir.join(sub), std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::set_permissions(&c.data_dir, std::fs::Permissions::from_mode(0o755))?;
    let mut reg = AdapterRegistry::new();
    reg.register(Arc::new(Fake))?;
    let data = c.data_dir.clone();
    let hub = Hub::new(
        c,
        HubOptions {
            registry: Some(reg),
            ..HubOptions::default()
        },
    )
    .map_err(|p| p.detail)?;
    hub.close();
    assert_eq!(modes(&data), std::collections::BTreeSet::from([0o700]));
    Ok(())
}

// ---- L4: admin-only knowledge reads --------------------------------------------------------------------------------

async fn get(app: &Arc<AppState>, path: &str, key: &str) -> u16 {
    let r = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "127.0.0.1:8792")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap_or_default();
    let resp = security::handle(app.clone(), SocketAddr::from(([127, 0, 0, 1], 5000)), 8792, r).await;
    let status = resp.status().as_u16();
    let _ = resp.into_body().collect().await;
    status
}

/// The hub on a fake knowledge service that answers 200 `{}` to everything; returns (app, viewer key, admin key).
async fn knowledge_app(
    tmp: &Path,
) -> Result<(Arc<AppState>, String, String, wiremock::MockServer), Box<dyn std::error::Error>> {
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&upstream)
        .await;
    let c = cfg(tmp, &[("knowledge_url", PyValue::str(upstream.uri()))])?;
    let hub = Hub::new(c, HubOptions::default()).map_err(|p| p.detail)?;
    on_startup(&hub);
    let key = |name: &str, role: &str| -> Result<String, Box<dyn std::error::Error>> {
        let k = hub.auth.create_key(name, role, None, &[]).map_err(|p| p.detail)?;
        Ok(k["secret"].as_str().unwrap_or_default().to_string())
    };
    let (v, a) = (key("v", "viewer")?, key("a", "admin")?);
    Ok((create_app(hub), v, a, upstream))
}

/// The router-level half of the test below, runnable before the proxy is ported: a viewer gets 403 on every path
/// that lists credentials or backend internals, before anything is forwarded.
#[tokio::test]
async fn viewers_get_403_on_credential_listing_knowledge_paths() -> R {
    let d = tempfile::tempdir()?;
    let (app, v, _a, upstream) = knowledge_app(d.path()).await?;
    for p in ["tokens", "indexes", "backends", "tokens/x"] {
        assert_eq!(get(&app, &format!("/api/v1/knowledge/{p}"), &v).await, 403, "{p}");
    }
    assert!(upstream.received_requests().await.unwrap_or_default().is_empty());
    Ok(())
}

#[tokio::test]
async fn test_viewers_cannot_read_credential_listing_knowledge_paths() -> R {
    let d = tempfile::tempdir()?;
    let (app, v, a, _upstream) = knowledge_app(d.path()).await?;
    for p in ["tokens", "indexes", "backends", "tokens/x"] {
        assert_eq!(get(&app, &format!("/api/v1/knowledge/{p}"), &v).await, 403, "{p}");
        assert_eq!(get(&app, &format!("/api/v1/knowledge/{p}"), &a).await, 200, "{p}");
    }
    assert_eq!(get(&app, "/api/v1/knowledge/namespaces", &v).await, 200);
    assert_eq!(get(&app, "/api/v1/knowledge/jobs", &v).await, 200);
    Ok(())
}
