//! The config editor's routes (Rust-only, PORTING.md A-18) through the real request pipeline: admin only, the floor
//! is the only current file served, drafts are checked by the real loaders and never written, and the audit row of a
//! check holds no draft text.

use std::net::SocketAddr;
use std::sync::Arc;

use agent_hub::api::{AppState, security};
use agent_hub::config::load_settings_from;
use agent_hub::server::{create_app, on_startup};
use agent_hub::services::hub::{Hub, HubOptions};
use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use hub_core::value::PyValue;
use serde_json::{Value, json};

type R = Result<(), Box<dyn std::error::Error>>;
const HOST: &str = "127.0.0.1:8792";

struct T {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    app: Arc<AppState>,
    key: String,
}

fn setup() -> Result<T, Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().to_path_buf();
    let env: std::collections::HashMap<String, String> = std::collections::HashMap::from([
        ("HOME".to_string(), root.join("home").to_string_lossy().to_string()),
        (
            "AGENT_HUB_ENV_FILE".to_string(),
            root.join("no-env").to_string_lossy().to_string(),
        ),
    ]);
    let cfg = load_settings_from(
        &env,
        &vec![("data_dir".into(), PyValue::str(root.join("data").to_string_lossy()))],
    )?;
    let hub = Hub::new(cfg, HubOptions::default()).map_err(|p| p.detail)?;
    on_startup(&hub);
    let key = std::fs::read_to_string(root.join("data/bootstrap-admin.key"))?
        .trim()
        .to_string();
    Ok(T {
        _tmp: tmp,
        root,
        app: create_app(hub),
        key,
    })
}

async fn call(t: &T, method: &str, path: &str, key: &str, extra: &[(&str, &str)], body: Option<Value>) -> (u16, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("host", HOST)
        .header("authorization", format!("Bearer {key}"));
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let r = b
        .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
        .unwrap_or_default();
    let peer: SocketAddr = SocketAddr::from(([127, 0, 0, 1], 5000));
    let resp = security::handle(t.app.clone(), peer, 8792, r).await;
    let status = resp.status().as_u16();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes().to_vec())
        .unwrap_or_default();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn admin_lists_reads_templates_and_the_floor_only() -> R {
    let t = setup()?;
    let (st, list) = call(&t, "GET", "/api/v1/config-files", &t.key, &[], None).await;
    assert_eq!(st, 200, "{list}");
    let ids: Vec<&str> = list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["id"].as_str())
        .collect();
    assert_eq!(ids, ["hub-env", "knowledge", "client-spec", "floor"]);
    assert_eq!(list["items"][3]["editable"], false);
    assert_eq!(list["items"][0]["current"], false);

    let (st, tpl) = call(
        &t,
        "GET",
        "/api/v1/config-files/client-spec/templates/cursor",
        &t.key,
        &[],
        None,
    )
    .await;
    assert_eq!(st, 200);
    assert!(tpl["text"].as_str().unwrap_or_default().contains("adapter: generic"));
    let (st, _) = call(
        &t,
        "GET",
        "/api/v1/config-files/client-spec/templates/..%2F..%2Fpolicy",
        &t.key,
        &[],
        None,
    )
    .await;
    assert_eq!(st, 404);

    let (st, floor) = call(&t, "GET", "/api/v1/config-files/floor/current", &t.key, &[], None).await;
    assert_eq!(st, 200);
    assert!(floor["text"].as_str().unwrap_or_default().contains("network_default"));
    for f in ["hub-env", "knowledge", "client-spec"] {
        let (st, p) = call(
            &t,
            "GET",
            &format!("/api/v1/config-files/{f}/current"),
            &t.key,
            &[],
            None,
        )
        .await;
        assert_eq!((st, p["code"].as_str()), (403, Some("not_served")), "{f}");
    }
    Ok(())
}

#[tokio::test]
async fn viewer_keys_are_refused() -> R {
    let t = setup()?;
    let (st, v) = call(
        &t,
        "POST",
        "/api/v1/keys",
        &t.key,
        &[],
        Some(json!({"name": "v", "role": "viewer"})),
    )
    .await;
    assert_eq!(st, 201, "{v}");
    let viewer = v["secret"].as_str().unwrap_or_default().to_string();
    // every Rust-only route, with every method the table gives it: a route added to RUST_ONLY is covered here at once
    let table = agent_hub::api::routes::table();
    let mut tried = 0;
    for path in agent_hub::api::routes::RUST_ONLY {
        let Some(route) = table.iter().find(|r| r.path == path) else {
            panic!("{path} is in RUST_ONLY but not in the route table")
        };
        let concrete = path.replace("{file}", "floor").replace("{name}", "default");
        for m in route.methods {
            for file in ["floor", "hub-env", "knowledge", "client-spec"] {
                let url = concrete.replace("/floor/", &format!("/{file}/"));
                let body = (*m == "POST").then(|| json!({"text": "port: 1\n"}));
                let (st, _) = call(&t, m, &url, &viewer, &[], body).await;
                assert_eq!(st, 403, "{m} {url}");
                tried += 1;
            }
        }
    }
    assert!(tried >= 4 * agent_hub::api::routes::RUST_ONLY.len(), "{tried}");
    Ok(())
}

#[tokio::test]
async fn a_check_never_writes_and_the_audit_row_holds_no_draft() -> R {
    let t = setup()?;
    let secret = "zz-knowledge-admin-value-that-must-not-leak";
    let draft = format!("HUB_PORT=9100\nHUB_KNOWLEDGE_ADMIN_KEY={secret}\n");
    let (st, r) = call(
        &t,
        "POST",
        "/api/v1/config-files/hub-env/validate",
        &t.key,
        &[],
        Some(json!({"text": draft})),
    )
    .await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["ok"], true);
    assert!(!r.to_string().contains(secret));
    let (_, audit) = call(&t, "GET", "/api/v1/audit?limit=5", &t.key, &[], None).await;
    let row = audit["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| {
            e["target"].as_str().unwrap_or_default().contains("config-files") || e.to_string().contains("config-files")
        })
        .cloned()
        .unwrap_or(Value::Null);
    assert!(!row.is_null(), "{audit}");
    assert!(!audit.to_string().contains(secret), "{audit}");
    assert!(!audit.to_string().contains("HUB_PORT"), "{audit}");
    // nothing was written: no env file, no content
    assert!(!t.root.join("no-env").exists());
    assert!(!t.root.join("home/.config").exists());

    // a client file: the content loader's model and the generic dry run, still nothing on disk
    let cursor = std::fs::read_to_string(agent_hub::config::app_root().join("examples/clients/cursor.yaml"))?;
    let (st, r) = call(
        &t,
        "POST",
        "/api/v1/config-files/client-spec/validate",
        &t.key,
        &[],
        Some(json!({"text": cursor})),
    )
    .await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["summary"]["id"], "cursor");
    assert!(
        r["summary"]["artifacts"].as_array().is_some_and(|a| !a.is_empty()),
        "{r}"
    );
    let (st, r) = call(
        &t,
        "POST",
        "/api/v1/config-files/client-spec/validate",
        &t.key,
        &[],
        Some(json!({"text": "id: Bad Id\nadapter: nope\n"})),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(r["ok"], false, "{r}");
    assert!(!t.root.join("data/content/clients/cursor.yaml").exists());

    // the floor is locked; a cross-origin POST never reaches the handler
    let (st, p) = call(
        &t,
        "POST",
        "/api/v1/config-files/floor/validate",
        &t.key,
        &[],
        Some(json!({"text": "version: 1\n"})),
    )
    .await;
    assert_eq!((st, p["code"].as_str()), (403, Some("floor_locked")));
    let (st, _) = call(
        &t,
        "POST",
        "/api/v1/config-files/knowledge/validate",
        &t.key,
        &[("origin", "http://evil.example")],
        Some(json!({"text": "port: 1\n"})),
    )
    .await;
    assert_eq!(st, 403);
    let (st, p) = call(
        &t,
        "POST",
        "/api/v1/config-files/knowledge/validate",
        &t.key,
        &[],
        Some(json!({"nope": 1})),
    )
    .await;
    assert_eq!((st, p["code"].as_str()), (422, Some("text_required")));
    Ok(())
}
