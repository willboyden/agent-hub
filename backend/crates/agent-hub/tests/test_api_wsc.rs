//! WS-C part of backend/tests/core/test_api.py: the knowledge proxy behind the hub's API (S-KPROXY). The knowledge
//! service is a wiremock server on an ephemeral loopback port (Python uses an httpx MockTransport); nothing reaches a
//! real knowledge service. A "down" service is a closed ephemeral port.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod wsc_support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};
use wsc_support::*;

const TOKEN: &str = "admin-token-value-123";

/// `httpx.Response(200, json={"ok": True, "path": req.url.path})`.
struct Echo;

impl Respond for Echo {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"ok": true, "path": req.url.path()}))
    }
}

async fn mock(r: impl Respond + 'static) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(r)
        .mount(&server)
        .await;
    server
}

fn knowledge_hub(url: &str, tweak: impl FnOnce(&mut agent_hub::config::Settings)) -> T {
    let url = url.to_string();
    hub_with(agent_hub::services::hub::HubOptions::default(), move |c| {
        c.knowledge_url = url;
        tweak(c);
    })
}

/// A loopback port nothing listens on.
fn closed_port_url() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}")
}

fn auth_of(r: &Request) -> String {
    r.headers
        .get("authorization")
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default()
}

#[tokio::test]
async fn test_knowledge_proxy_forwards_with_admin_token() {
    let server = mock(Echo).await;
    let t = knowledge_hub(&server.uri(), |c| c.knowledge_admin_key = Some(TOKEN.into()));
    let a = app(&t);
    let key = bearer(&t.key("a", "admin"));
    let viewer = bearer(&t.key("v", "viewer"));
    let r = api(
        &a,
        "GET",
        "/api/v1/knowledge/namespaces?limit=5",
        &[("authorization", viewer.as_str())],
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["path"], "/namespaces");
    let seen = server.received_requests().await.unwrap();
    assert_eq!(auth_of(&seen[0]), format!("Bearer {TOKEN}"));
    assert_eq!(seen[0].url.query(), Some("limit=5"));
    let hdrs = format!("{:?}", r.headers);
    assert!(!hdrs.contains("Bearer") && !r.text().contains("admin-token"));
    let r = api(
        &a,
        "POST",
        "/api/v1/knowledge/namespaces/docs/query",
        &[("authorization", viewer.as_str())],
        Some(json!({"text": "hi"})),
    )
    .await;
    assert_eq!(r.status, 200);
    let last = server.received_requests().await.unwrap().pop().unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&last.body).unwrap(),
        json!({"text": "hi"})
    );
    assert_eq!(
        last.headers
            .get("content-type")
            .map(|v| v.to_str().unwrap_or_default().to_string()),
        Some("application/json".into())
    );
    // a viewer may not ingest; an admin may
    assert_eq!(
        api(
            &a,
            "POST",
            "/api/v1/knowledge/namespaces/docs/ingest",
            &[("authorization", viewer.as_str())],
            Some(json!({}))
        )
        .await
        .status,
        403
    );
    assert_eq!(
        api(
            &a,
            "POST",
            "/api/v1/knowledge/namespaces/docs/ingest",
            &[("authorization", key.as_str())],
            Some(json!({"documents": []}))
        )
        .await
        .status,
        200
    );
    // the client's own Authorization header is never forwarded
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| auth_of(r) == format!("Bearer {TOKEN}"))
    );
}

#[tokio::test]
async fn test_knowledge_proxy_restrictions() {
    let server = mock(ResponseTemplate::new(200).set_body_json(json!({}))).await;
    let t = knowledge_hub(&server.uri(), |c| c.knowledge_admin_key = Some(TOKEN.into()));
    let a = app(&t);
    let h = bearer(&t.key("a", "admin"));
    let adm = [("authorization", h.as_str())];
    assert_eq!(
        api(&a, "DELETE", "/api/v1/knowledge/namespaces/docs", &adm, None)
            .await
            .status,
        200,
        "DELETE: admin only"
    );
    let viewer = bearer(&t.key("v", "viewer"));
    assert_eq!(
        api(
            &a,
            "DELETE",
            "/api/v1/knowledge/namespaces/docs",
            &[("authorization", viewer.as_str())],
            None
        )
        .await
        .status,
        403
    );
    assert_eq!(
        api(&a, "DELETE", "/api/v1/knowledge/shutdown", &adm, None).await.status,
        404,
        "same path allowlist"
    );
    assert_eq!(
        api(&a, "PUT", "/api/v1/knowledge/namespaces/docs", &adm, Some(json!({})))
            .await
            .status,
        405
    );
    for p in [
        "admin/secrets",
        "../etc",
        "namespaces/../../x",
        "namespaces/a%20b",
        "shutdown",
    ] {
        let st = api(&a, "GET", &format!("/api/v1/knowledge/{p}"), &adm, None)
            .await
            .status;
        assert!(st == 400 || st == 404, "{p}: {st}");
    }
    let cap = usize::try_from(t.hub.cfg().knowledge_body_cap_bytes).unwrap();
    let big = "x".repeat(cap + 5);
    let port = u16::try_from(t.hub.cfg().port).unwrap();
    let req = http::Request::builder()
        .method("POST")
        .uri("/api/v1/knowledge/namespaces/docs/ingest")
        .header("host", format!("127.0.0.1:{port}"))
        .header("authorization", h.as_str())
        .header("content-type", "application/json")
        .body(axum::body::Body::from(big))
        .unwrap();
    let resp =
        agent_hub::api::security::handle(a.clone(), std::net::SocketAddr::from(([10, 0, 0, 9], 5000)), port, req).await;
    assert_eq!(resp.status().as_u16(), 413);
}

#[tokio::test]
async fn test_knowledge_service_down_is_503_problem() {
    let t = knowledge_hub(&closed_port_url(), |c| c.knowledge_admin_key = Some(TOKEN.into()));
    let a = app(&t);
    let h = bearer(&t.key("a", "admin"));
    let r = api(
        &a,
        "GET",
        "/api/v1/knowledge/backends",
        &[("authorization", h.as_str())],
        None,
    )
    .await;
    assert_eq!((r.status, r.code()), (503, "knowledge_unavailable".to_string()));
    assert!(r.json()["detail"].as_str().unwrap().contains("uv run agent-knowledge"));
}

struct CapResponder;

impl Respond for CapResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if req.url.path() == "/jobs/j1" {
            return ResponseTemplate::new(404).set_body_json(json!({"detail": "nope"}));
        }
        ResponseTemplate::new(200).set_body_raw(vec![b'x'; 500], "text/html")
    }
}

#[tokio::test]
async fn test_knowledge_response_cap_and_upstream_errors() {
    let server = mock(CapResponder).await;
    let t = knowledge_hub(&server.uri(), |c| c.knowledge_response_cap_bytes = 100);
    let a = app(&t);
    let h = bearer(&t.key("a", "admin"));
    let hdr = [("authorization", h.as_str())];
    assert_eq!(
        api(&a, "GET", "/api/v1/knowledge/backends", &hdr, None).await.code(),
        "upstream_too_large"
    );
    let r = api(&a, "GET", "/api/v1/knowledge/jobs/j1", &hdr, None).await;
    assert_eq!((r.status, r.json()), (404, json!({"detail": "nope"})));
}

#[tokio::test]
async fn test_knowledge_sse_stream_is_capped() {
    let server =
        mock(ResponseTemplate::new(200).set_body_raw(b"data: 1\n\ndata: 2\n\n".to_vec(), "text/event-stream")).await;
    let t = knowledge_hub(&server.uri(), |c| c.max_sse_connections = 1);
    let a = app(&t);
    let h = bearer(&t.key("a", "admin"));
    let r = api(
        &a,
        "GET",
        "/api/v1/knowledge/jobs/j1/stream",
        &[("authorization", h.as_str())],
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.text().contains("data: 2"));
    assert!(r.header("content-type").starts_with("text/event-stream"));
    // the slot was released when the stream ended: a second stream is allowed
    assert_eq!(
        api(
            &a,
            "GET",
            "/api/v1/knowledge/jobs/j1/stream",
            &[("authorization", h.as_str())],
            None
        )
        .await
        .status,
        200
    );
}

/// Audit F7: a client that goes away while the upstream has not answered yet must not keep its stream slot.
#[tokio::test]
async fn test_knowledge_sse_slot_is_released_when_the_request_is_dropped_early() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::path("/jobs/slow/stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(b"data: 1\n\n".to_vec(), "text/event-stream")
                .set_delay(std::time::Duration::from_secs(30)),
        )
        .mount(&server)
        .await;
    Mock::given(wiremock::matchers::path("/jobs/j1/stream"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"data: 1\n\n".to_vec(), "text/event-stream"))
        .mount(&server)
        .await;
    let t = knowledge_hub(&server.uri(), |c| c.max_sse_connections = 1);
    let a = app(&t);
    let h = bearer(&t.key("a", "admin"));
    let hdrs = [("authorization", h.as_str())];
    let slow = api(&a, "GET", "/api/v1/knowledge/jobs/slow/stream", &hdrs, None);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(500), slow)
            .await
            .is_err(),
        "the slow upstream answered"
    ); // the request future is dropped here, as when the client disconnects
    let r = api(
        &a,
        "GET",
        "/api/v1/knowledge/jobs/j1/stream",
        &[("authorization", h.as_str())],
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
}

/// The byte cap on the stream itself (Python: `sent > knowledge_response_cap_bytes` ends it).
#[tokio::test]
async fn test_knowledge_sse_stream_stops_at_the_byte_cap() {
    let body: Vec<u8> = (0..50).flat_map(|i| format!("data: {i}\n\n").into_bytes()).collect();
    let server = mock(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")).await;
    let t = knowledge_hub(&server.uri(), |c| c.knowledge_response_cap_bytes = 10);
    let a = app(&t);
    let h = bearer(&t.key("a", "admin"));
    let r = api(
        &a,
        "GET",
        "/api/v1/knowledge/jobs/j1/stream",
        &[("authorization", h.as_str())],
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.body.len() <= 10, "{}", r.text());
}

struct Refuses;

impl Respond for Refuses {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        ResponseTemplate::new(if req.url.path() == "/namespaces" { 401 } else { 403 })
            .set_body_json(json!({"detail": "nope"}))
    }
}

#[tokio::test]
async fn test_upstream_401_403_are_never_the_hubs_own() {
    let server = mock(Refuses).await;
    let t = knowledge_hub(&server.uri(), |c| c.knowledge_admin_key = Some(TOKEN.into()));
    let a = app(&t);
    let h = bearer(&t.key("a", "admin"));
    let hdr = [("authorization", h.as_str())];
    for p in ["namespaces", "tokens"] {
        let r = api(&a, "GET", &format!("/api/v1/knowledge/{p}"), &hdr, None).await;
        assert_eq!(
            (r.status, r.code()),
            (502, "knowledge_upstream_unauthorized".to_string()),
            "{p}"
        );
        let d = r.json()["detail"].as_str().unwrap().to_string();
        assert!(
            d.contains("HUB_KNOWLEDGE_ADMIN_KEY")
                && d.contains("KNOWLEDGE_ADMIN_KEY_FILE")
                && d.contains("agent-knowledge/admin.key")
        );
        assert!(!r.text().contains(TOKEN));
    }
    assert_eq!(
        api(&a, "DELETE", "/api/v1/knowledge/namespaces/x", &hdr, None)
            .await
            .status,
        502
    );
}

fn key_file(dir: &Path, mode: u32, text: &str) -> std::path::PathBuf {
    let f = dir.join("admin.key");
    std::fs::write(&f, text).unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(mode)).unwrap();
    f
}

#[tokio::test]
async fn test_admin_key_is_read_lazily_from_the_service_key_file() {
    let server = mock(ResponseTemplate::new(200).set_body_json(json!({"ok": true}))).await;
    let dir = tempfile::tempdir().unwrap();
    let kf = dir.path().join("admin.key");
    let kf2 = kf.clone();
    let t = knowledge_hub(&server.uri(), move |c| c.knowledge_admin_key_file = kf2);
    let a = app(&t);
    let h = bearer(&t.key("a", "admin"));
    let hdr = [("authorization", h.as_str())];
    assert_eq!(
        api(&a, "GET", "/api/v1/knowledge/namespaces", &hdr, None).await.status,
        200
    );
    let last = |reqs: Vec<Request>| auth_of(reqs.last().unwrap());
    assert_eq!(
        last(server.received_requests().await.unwrap()),
        "",
        "no file yet: no key sent"
    );
    key_file(dir.path(), 0o600, "file-key-abc\n");
    let r = api(&a, "GET", "/api/v1/knowledge/namespaces", &hdr, None).await;
    assert_eq!(r.status, 200, "picked up without restart");
    assert_eq!(last(server.received_requests().await.unwrap()), "Bearer file-key-abc");
    assert!(!r.text().contains("file-key-abc"));
    // the setting/env wins (Python sets cfg.knowledge_admin_key on the live config; a hub configured with it here)
    let kf3 = kf.clone();
    let t2 = knowledge_hub(&server.uri(), move |c| {
        c.knowledge_admin_key_file = kf3;
        c.knowledge_admin_key = Some("env-key".into());
    });
    let a2 = app(&t2);
    let h2 = bearer(&t2.key("a", "admin"));
    api(
        &a2,
        "GET",
        "/api/v1/knowledge/namespaces",
        &[("authorization", h2.as_str())],
        None,
    )
    .await;
    assert_eq!(last(server.received_requests().await.unwrap()), "Bearer env-key");
    assert!(
        !api(&a, "GET", "/api/v1/settings", &hdr, None)
            .await
            .text()
            .contains("file-key-abc")
    );
    assert!(
        !api(&a, "GET", "/api/v1/audit", &hdr, None)
            .await
            .text()
            .contains("file-key-abc")
    );
}

#[tokio::test]
async fn test_unsafe_key_files_are_refused() {
    for kind in ["loose", "symlink", "huge"] {
        let server = mock(ResponseTemplate::new(200).set_body_json(json!({"ok": true}))).await;
        let dir = tempfile::tempdir().unwrap();
        let kf = match kind {
            "loose" => key_file(dir.path(), 0o644, "file-key-abc\n"),
            "symlink" => {
                std::fs::create_dir(dir.path().join("sub")).unwrap();
                let real = key_file(&dir.path().join("sub"), 0o600, "real-key");
                let link = dir.path().join("link.key");
                std::os::unix::fs::symlink(real, &link).unwrap();
                link
            }
            _ => key_file(dir.path(), 0o600, &"x".repeat(5000)),
        };
        let t = knowledge_hub(&server.uri(), move |c| c.knowledge_admin_key_file = kf);
        let a = app(&t);
        let h = bearer(&t.key("a", "admin"));
        let r = api(
            &a,
            "GET",
            "/api/v1/knowledge/namespaces",
            &[("authorization", h.as_str())],
            None,
        )
        .await;
        assert_eq!(
            (r.status, r.code()),
            (502, "knowledge_key_unusable".to_string()),
            "{kind}"
        );
        assert!(server.received_requests().await.unwrap().is_empty(), "{kind}");
        assert!(!r.text().contains("real-key") && !r.text().contains("xxxx"), "{kind}");
    }
}

struct Health;

impl Respond for Health {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if req.url.path() == "/backends" {
            return if auth_of(req) == "Bearer file-key-abc" {
                ResponseTemplate::new(200).set_body_json(json!({"qdrant": {"ok": true}}))
            } else {
                ResponseTemplate::new(401).set_body_json(json!({}))
            };
        }
        ResponseTemplate::new(200).set_body_json(json!({"status": "ok"}))
    }
}

#[tokio::test]
async fn test_knowledge_health_summary() {
    let server = mock(Health).await;
    let dir = tempfile::tempdir().unwrap();
    let kf = dir.path().join("admin.key");
    let t = knowledge_hub(&server.uri(), move |c| c.knowledge_admin_key_file = kf);
    let a = app(&t);
    let h = bearer(&t.key("v", "viewer"));
    let hdr = [("authorization", h.as_str())];
    let s = api(&a, "GET", "/api/v1/knowledge/health", &hdr, None).await.json();
    assert_eq!(
        (s["reachable"].clone(), s["authenticated"].clone()),
        (json!(true), json!(false))
    );
    assert!(s["detail"].as_str().unwrap().contains("HUB_KNOWLEDGE_ADMIN_KEY"));
    key_file(dir.path(), 0o600, "file-key-abc\n");
    let s = api(&a, "GET", "/api/v1/knowledge/health", &hdr, None).await.json();
    assert_eq!(s["reachable"], true);
    assert_eq!(s["authenticated"], true);
    assert_eq!(s["backends"], json!({"qdrant": {"ok": true}}));
    assert_eq!(s["key_source"], "file");
    assert!(!s.to_string().contains("file-key-abc"));
    // the service goes down (a closed port: wiremock keeps serving pooled connections after a drop)
    let kf2 = dir.path().join("admin.key");
    let t2 = knowledge_hub(&closed_port_url(), move |c| c.knowledge_admin_key_file = kf2);
    let a2 = app(&t2);
    let h2 = bearer(&t2.key("v", "viewer"));
    let s = api(
        &a2,
        "GET",
        "/api/v1/knowledge/health",
        &[("authorization", h2.as_str())],
        None,
    )
    .await
    .json();
    assert_eq!(s["reachable"], false);
    assert!(s["detail"].as_str().unwrap().contains("uv run agent-knowledge"));
}

/// Audit F8: an upstream that keeps trickling bytes (each read well inside the per-operation timeout) cannot hold a
/// proxied request open beyond the total deadline (4 x HUB_KNOWLEDGE_TIMEOUT_S).
#[tokio::test(flavor = "multi_thread")]
async fn test_knowledge_call_has_a_total_deadline() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); // ephemeral, loopback only
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in listener.incoming().take(4) {
            let Ok(mut s) = s else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let head = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100000\r\n\r\n";
                if s.write_all(head).is_err() {
                    return;
                }
                for _ in 0..1000 {
                    if s.write_all(b" ").is_err() {
                        break; // the hub gave up and closed the connection
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            });
        }
    });
    let t = knowledge_hub(&format!("http://127.0.0.1:{port}"), |c| c.knowledge_timeout_s = 0.3);
    let a = app(&t);
    let h = bearer(&t.key("a", "admin"));
    let t0 = std::time::Instant::now();
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        api(
            &a,
            "GET",
            "/api/v1/knowledge/namespaces",
            &[("authorization", h.as_str())],
            None,
        ),
    )
    .await
    .expect("the proxied call outlived its total deadline");
    assert_eq!(r.status, 503, "{}", r.text());
    assert!(t0.elapsed() < std::time::Duration::from_secs(6), "{:?}", t0.elapsed());
}
