//! FastAPI's request-body read in the hub pipeline (PORTING.md A-11): decoded with CPython's `json.loads`
//! (`hub_core::py::pyjson`) before any dependency; 422 `json_invalid` at Python's character position; 400 "There was an
//! error parsing the body" for anything else that does not decode; `null` = missing; bodies deeper than
//! `BODY_MAX_DEPTH` get that 400 too (D-JSON-DEPTH). The same answers are compared with Python in the parity corpus
//! (`core.body.*`); this file adds what the corpus cannot send (raw invalid UTF-8) and the D-JSON-DEPTH band.

use std::net::SocketAddr;
use std::sync::Arc;

use agent_hub::api::deps::{BODY_MAX_DEPTH, BODY_PARSE_ERROR};
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

struct T {
    _tmp: tempfile::TempDir,
    app: Arc<AppState>,
    admin: String,
    viewer: String,
}

fn setup() -> Result<T, Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().to_path_buf();
    let env = std::collections::HashMap::from([
        ("HOME".to_string(), root.join("home").to_string_lossy().to_string()),
        (
            "AGENT_HUB_ENV_FILE".to_string(),
            root.join("no-env").to_string_lossy().to_string(),
        ),
    ]);
    let cfg = load_settings_from(
        &env,
        &vec![
            ("data_dir".into(), PyValue::str(root.join("data").to_string_lossy())),
            (
                "content_dir".into(),
                PyValue::str(root.join("content").to_string_lossy()),
            ),
        ],
    )?;
    let hub = Hub::new(cfg, HubOptions::default()).map_err(|p| p.detail)?;
    on_startup(&hub);
    let key = |name: &str, role: &str| -> Result<String, Box<dyn std::error::Error>> {
        let k = hub.auth.create_key(name, role, None, &[]).map_err(|p| p.detail)?;
        Ok(k["secret"].as_str().unwrap_or_default().to_string())
    };
    let (admin, viewer) = (key("a", "admin")?, key("v", "viewer")?);
    Ok(T {
        _tmp: tmp,
        app: create_app(hub),
        admin,
        viewer,
    })
}

async fn post(t: &T, path: &str, key: &str, body: Vec<u8>) -> (u16, Value) {
    let r = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "127.0.0.1:8792")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap_or_default();
    let resp = security::handle(t.app.clone(), SocketAddr::from(([127, 0, 0, 1], 5000)), 8792, r).await;
    let status = resp.status().as_u16();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes().to_vec())
        .unwrap_or_default();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn nested(depth: usize) -> Vec<u8> {
    // `{"rules": [[...]]}`: the object is one level, the lists the rest
    format!("{{\"rules\": {}{}}}", "[".repeat(depth - 1), "]".repeat(depth - 1)).into_bytes()
}

#[tokio::test]
async fn bodies_decode_like_fastapi() -> R {
    let t = setup()?;
    let check = "/api/v1/policy/check";
    // json_invalid at Python's character position (CPython 3.13 points at the trailing comma; é counts as one)
    let (s, b) = post(&t, check, &t.admin, "{\"zé\": 1,}".as_bytes().to_vec()).await;
    assert_eq!(s, 422);
    assert_eq!(
        b["errors"],
        json!([{"loc": ["body", 8], "msg": "JSON decode error", "type": "json_invalid"}])
    );
    // anything else that does not decode: 400
    for bad in [b"{\"a\": \"\xff\"}".to_vec(), nested(10_000)] {
        let (s, b) = post(&t, check, &t.admin, bad).await;
        assert_eq!((s, b["detail"].as_str()), (400, Some(BODY_PARSE_ERROR)));
        assert_eq!(b["code"], "http_error");
    }
    // null is a missing body
    let (s, b) = post(&t, check, &t.admin, b"null".to_vec()).await;
    assert_eq!(s, 422);
    assert_eq!(b["errors"][0]["type"], "missing");
    // the viewer gate is in the security middleware, which runs before FastAPI reads the body: a viewer's broken
    // JSON to an admin route is a 403, as in Python (parity case core.body.viewer-bad-json)
    assert_eq!(post(&t, "/api/v1/keys", &t.viewer, b"{".to_vec()).await.0, 403);
    Ok(())
}

/// D-JSON-DEPTH: up to `BODY_MAX_DEPTH` levels reach the handler (here policy/check answers 500 for a list of lists,
/// as Python does); deeper bodies get the 400 Python gives only beyond 9991 levels. Nothing deep is ever built, so
/// the process survives any depth the body cap allows.
#[tokio::test]
async fn deep_bodies_are_capped_without_building_them() -> R {
    let t = setup()?;
    let check = "/api/v1/policy/check";
    assert_eq!(post(&t, check, &t.admin, nested(BODY_MAX_DEPTH)).await.0, 500);
    for depth in [BODY_MAX_DEPTH + 1, 5_000, 9_991, 200_000] {
        let (s, b) = post(&t, check, &t.admin, nested(depth)).await;
        assert_eq!(
            (s, b["detail"].as_str()),
            (400, Some(BODY_PARSE_ERROR)),
            "depth {depth}"
        );
    }
    Ok(())
}
