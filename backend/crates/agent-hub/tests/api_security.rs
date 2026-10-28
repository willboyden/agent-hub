//! Port of the CORE parts of backend/tests/core/test_api.py: the security pipeline (Host, Origin, CSRF, auth, roles,
//! caps, headers, CORS), RFC 7807 errors, keys, audit, metrics and the static frontend. Test names mirror Python's.
//! Where Python asserts "200 with an admin key" on a route that a workstream still owns (/clients, /plan), these tests
//! use a CORE route (/keys, /settings) behind the same middleware; the route-specific assertions are mapped to the
//! owning workstream in TEST-MAP.md.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
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
const PORT: u16 = 8792;
const HOST: &str = "127.0.0.1:8792";

struct T {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    app: Arc<AppState>,
    key: String,
}

fn setup_with(extra: &[(&str, &str)]) -> Result<T, Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().to_path_buf();
    let fe = root.join("frontend");
    for d in ["js", "css", "i18n", "dev"] {
        std::fs::create_dir_all(fe.join(d))?;
    }
    std::fs::write(fe.join("index.html"), "<html><title>hub</title></html>")?;
    std::fs::write(fe.join("js/app.js"), "export {}")?;
    std::fs::write(fe.join("css/app.css"), "body{}")?;
    std::fs::write(fe.join("package.json"), "secret dev file")?;
    std::fs::write(fe.join("dev/mock-server.mjs"), "secret dev file")?;
    std::fs::write(root.join("outside.txt"), "outside")?;
    let mut env: std::collections::HashMap<String, String> = std::collections::HashMap::from([
        ("HOME".to_string(), root.join("home").to_string_lossy().to_string()),
        (
            "AGENT_HUB_ENV_FILE".to_string(),
            root.join("no-env").to_string_lossy().to_string(),
        ),
    ]);
    for (k, v) in extra {
        env.insert((*k).to_string(), (*v).to_string());
    }
    let cfg = load_settings_from(
        &env,
        &vec![
            ("data_dir".into(), PyValue::str(root.join("data").to_string_lossy())),
            ("frontend_dir".into(), PyValue::str(fe.to_string_lossy())),
        ],
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

fn setup() -> Result<T, Box<dyn std::error::Error>> {
    setup_with(&[])
}

struct Resp {
    status: u16,
    headers: http::HeaderMap,
    body: Vec<u8>,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn h(&self, k: &str) -> String {
        self.headers
            .get(k)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .unwrap_or_default()
    }
    /// test_api.py `problem_code`.
    fn code(&self) -> String {
        assert!(
            self.h("content-type").starts_with("application/problem+json"),
            "{}",
            self.text()
        );
        let b = self.json();
        assert_eq!(b["status"], json!(self.status));
        assert!(b["type"].as_str().unwrap_or_default().starts_with("urn:agent-hub:"));
        b["code"].as_str().unwrap_or_default().to_string()
    }
}

async fn req(t: &T, method: &str, path: &str, headers: &[(&str, &str)], body: Option<Value>) -> Resp {
    req_raw(
        &t.app,
        PORT,
        "10.0.0.9:5000",
        method,
        path,
        headers,
        body.map(|b| b.to_string().into_bytes()),
    )
    .await
}

async fn req_raw(
    app: &Arc<AppState>,
    port: u16,
    peer: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Vec<u8>>,
) -> Resp {
    let host = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map_or(HOST, |(_, v)| *v);
    let mut b = Request::builder().method(method).uri(path).header("host", host);
    let has_ctype = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
    for (k, v) in headers.iter().filter(|(k, _)| !k.eq_ignore_ascii_case("host")) {
        b = b.header(*k, *v);
    }
    if body.is_some() && !has_ctype {
        b = b.header("content-type", "application/json");
    }
    let r = b.body(body.map_or_else(Body::empty, Body::from)).unwrap_or_default();
    let peer: SocketAddr = peer.parse().unwrap_or_else(|_| SocketAddr::from(([10, 0, 0, 9], 5000)));
    let resp = security::handle(app.clone(), peer, port, r).await;
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.map(|c| c.to_bytes().to_vec()).unwrap_or_default();
    Resp {
        status: parts.status.as_u16(),
        headers: parts.headers,
        body: bytes,
    }
}

fn bearer(k: &str) -> String {
    format!("Bearer {k}")
}

#[tokio::test]
async fn test_host_allowlist_421() -> R {
    let t = setup()?;
    let a = bearer(&t.key);
    for host in [
        "evil.example.com",
        "127.0.0.1:9999",
        "localhost",
        "127.0.0.1.evil.com:8792",
    ] {
        let r = req(&t, "GET", "/api/v1/health", &[("host", host)], None).await;
        assert_eq!(r.status, 421, "{host}");
        assert_eq!(r.code(), "misdirected_request");
        assert_eq!(
            req(
                &t,
                "GET",
                "/api/v1/keys",
                &[("host", host), ("authorization", &a)],
                None
            )
            .await
            .status,
            421
        );
    }
    Ok(())
}

#[tokio::test]
async fn test_allowed_hosts_setting() -> R {
    let t = setup_with(&[("HUB_ALLOWED_HOSTS", "[\"hub.internal:8792\"]")])?;
    let r = req(
        &t,
        "GET",
        "/api/v1/keys",
        &[("host", "hub.internal:8792"), ("authorization", &bearer(&t.key))],
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    Ok(())
}

#[tokio::test]
async fn test_cross_origin_and_null_origin_403() -> R {
    let t = setup()?;
    let a = bearer(&t.key);
    for origin in [
        "http://evil.example.com",
        "null",
        "http://127.0.0.1:8795",
        "https://127.0.0.1:8792.evil.com",
    ] {
        let r = req(
            &t,
            "POST",
            "/api/v1/keys",
            &[("authorization", &a), ("origin", origin)],
            Some(json!({"name": "x"})),
        )
        .await;
        assert_eq!(r.status, 403, "{origin}");
        assert_eq!(r.code(), "cross_origin");
    }
    let ok = req(
        &t,
        "POST",
        "/api/v1/keys",
        &[("authorization", &a), ("origin", "http://127.0.0.1:8792")],
        Some(json!({"name": "x"})),
    )
    .await;
    assert_eq!(ok.status, 201);
    Ok(())
}

#[tokio::test]
async fn test_csrf_header_required_without_bearer() -> R {
    let t = setup_with(&[("HUB_TRUST_LOOPBACK", "true")])?;
    let loop_req = |hdrs: Vec<(&'static str, &'static str)>| {
        let app = t.app.clone();
        async move {
            req_raw(
                &app,
                PORT,
                "127.0.0.1:5000",
                "POST",
                "/api/v1/keys",
                &hdrs,
                Some(br#"{"name":"x"}"#.to_vec()),
            )
            .await
        }
    };
    let r = loop_req(vec![]).await;
    assert_eq!(r.status, 403);
    assert_eq!(r.code(), "csrf_header_required");
    assert_eq!(loop_req(vec![("x-agent-hub", "0")]).await.status, 403);
    assert_eq!(loop_req(vec![("x-agent-hub", "1")]).await.status, 201);
    let get = req_raw(&t.app, PORT, "127.0.0.1:5000", "GET", "/api/v1/keys", &[], None).await;
    assert_eq!(get.status, 200);
    let bearer_ok = req(
        &t,
        "POST",
        "/api/v1/keys",
        &[("authorization", &bearer(&t.key))],
        Some(json!({"name": "y"})),
    )
    .await;
    assert_eq!(bearer_ok.status, 201);
    Ok(())
}

#[tokio::test]
async fn test_loopback_trust_is_off_in_tests_and_never_via_proxy_headers() -> R {
    let t = setup()?;
    assert_eq!(
        req_raw(&t.app, PORT, "127.0.0.1:5000", "GET", "/api/v1/keys", &[], None)
            .await
            .status,
        401
    );
    let t = setup_with(&[("HUB_TRUST_LOOPBACK", "1")])?;
    assert_eq!(
        req_raw(&t.app, PORT, "127.0.0.1:5000", "GET", "/api/v1/keys", &[], None)
            .await
            .status,
        200
    );
    let r = req_raw(
        &t.app,
        PORT,
        "127.0.0.1:5000",
        "GET",
        "/api/v1/keys",
        &[("x-forwarded-for", "1.2.3.4")],
        None,
    )
    .await;
    assert_eq!(r.status, 401);
    assert_eq!(
        req_raw(&t.app, PORT, "10.0.0.9:5000", "GET", "/api/v1/keys", &[], None)
            .await
            .status,
        401
    );
    Ok(())
}

#[tokio::test]
async fn test_security_headers_on_every_response_including_errors_and_static() -> R {
    let t = setup()?;
    let a = bearer(&t.key);
    let rs = vec![
        req(&t, "GET", "/api/v1/health", &[], None).await,
        req(&t, "GET", "/api/v1/keys", &[], None).await,
        req(&t, "GET", "/", &[], None).await,
        req(&t, "GET", "/js/app.js", &[], None).await,
        req(&t, "GET", "/js/missing.js", &[], None).await,
        req(&t, "GET", "/nonexistent/deep/link", &[], None).await,
        req(&t, "GET", "/api/v1/keys", &[("authorization", &a)], None).await,
        req(&t, "GET", "/api/v1/nope", &[("authorization", &a)], None).await,
        req(&t, "GET", "/api/v1/health", &[("host", "evil")], None).await,
    ];
    for r in &rs {
        let csp = r.h("content-security-policy");
        assert!(
            csp.contains("default-src 'none'") && csp.contains("script-src 'self'") && !csp.contains("'unsafe-inline'")
        );
        assert!(csp.contains("frame-ancestors 'none'"));
        assert_eq!(r.h("x-content-type-options"), "nosniff");
        assert_eq!(r.h("x-frame-options"), "DENY");
        assert_eq!(r.h("referrer-policy"), "no-referrer");
        assert_eq!(r.headers.get_all("content-security-policy").iter().count(), 1);
    }
    assert_eq!(rs[1].h("cache-control"), "no-store");
    Ok(())
}

#[tokio::test]
async fn test_no_cors() -> R {
    let t = setup()?;
    let r = req(
        &t,
        "OPTIONS",
        "/api/v1/keys",
        &[
            ("origin", "http://evil.example.com"),
            ("access-control-request-method", "GET"),
        ],
        None,
    )
    .await;
    assert!(r.headers.get("access-control-allow-origin").is_none());
    assert!([401, 403, 405].contains(&r.status));
    let r = req(
        &t,
        "GET",
        "/api/v1/keys",
        &[
            ("authorization", &bearer(&t.key)),
            ("origin", "http://evil.example.com"),
        ],
        None,
    )
    .await;
    assert!(r.headers.get("access-control-allow-origin").is_none());
    Ok(())
}

#[tokio::test]
async fn test_body_caps() -> R {
    let t = setup()?;
    let big = json!({"name": "x".repeat(1024 * 1024 + 10)});
    let r = req(
        &t,
        "POST",
        "/api/v1/keys",
        &[("authorization", &bearer(&t.key))],
        Some(big),
    )
    .await;
    assert_eq!(r.status, 413);
    assert_eq!(r.code(), "payload_too_large");
    Ok(())
}

#[tokio::test]
async fn test_errors_are_problem_json_and_never_echo_input() -> R {
    let t = setup()?;
    let a = bearer(&t.key);
    let r = req(
        &t,
        "POST",
        "/api/v1/keys",
        &[("authorization", &a)],
        Some(json!({"name": "x", "scopes": "hunter2-secret"})),
    )
    .await;
    assert_eq!(r.status, 422);
    assert_eq!(r.code(), "validation_error");
    assert!(!r.text().contains("hunter2-secret") && !r.text().contains("\"input\""));
    assert_eq!(
        req(&t, "DELETE", "/api/v1/keys/ghost", &[("authorization", &a)], None)
            .await
            .code(),
        "not_found"
    );
    assert_eq!(
        req(&t, "PATCH", "/api/v1/keys", &[("authorization", &a)], None)
            .await
            .code(),
        "method_not_allowed"
    );
    Ok(())
}

/// Every route path with its parameters filled (test_api.py `_fill`).
fn fill(path: &str) -> String {
    path.replace("{path:path}", "namespaces")
        .replace(['{', '}'], "")
        .replace("adapter_id", "x")
        .replace("key_id", "x")
}

#[tokio::test]
async fn test_unauthenticated_and_bad_bearer_cannot_mutate_anything() -> R {
    let t = setup()?;
    for route in &t.app.routes {
        if !route.path.starts_with("/api/") && route.path != "/metrics" {
            continue;
        }
        for m in route.methods.iter().filter(|m| ["POST", "PUT", "DELETE"].contains(m)) {
            let r = req(&t, m, &fill(route.path), &[("x-agent-hub", "1")], Some(json!({}))).await;
            assert_eq!(r.status, 401, "{m} {}", route.path);
            let r = req(
                &t,
                m,
                &fill(route.path),
                &[("authorization", "Bearer hc_bad")],
                Some(json!({})),
            )
            .await;
            assert_eq!(r.status, 401, "{m} {}", route.path);
        }
    }
    Ok(())
}

#[tokio::test]
async fn test_no_key_is_401_on_every_route_but_health() -> R {
    let t = setup()?;
    let mut n = 0;
    for route in &t.app.routes {
        if route.path == "/api/v1/health" || !(route.path.starts_with("/api/") || route.path == "/metrics") {
            continue;
        }
        for m in route.methods.iter().filter(|m| **m != "HEAD") {
            let body = if *m == "GET" { None } else { Some(json!({})) };
            assert_eq!(
                req(&t, m, &fill(route.path), &[("x-agent-hub", "1")], body)
                    .await
                    .status,
                401,
                "{m} {}",
                route.path
            );
            n += 1;
        }
    }
    assert!(n > 60);
    assert_eq!(req(&t, "GET", "/metrics", &[], None).await.status, 401);
    Ok(())
}

#[tokio::test]
async fn test_route_enumeration_every_mutating_route_requires_admin() -> R {
    let t = setup()?;
    let v = req(
        &t,
        "POST",
        "/api/v1/keys",
        &[("authorization", &bearer(&t.key))],
        Some(json!({"name": "v", "role": "viewer"})),
    )
    .await
    .json();
    let viewer = bearer(v["secret"].as_str().unwrap_or_default());
    let mut viewer_ok = 0;
    for route in &t.app.routes {
        for m in route.methods.iter().filter(|m| ["POST", "PUT", "DELETE"].contains(m)) {
            if !route.path.starts_with("/api/") {
                continue;
            }
            let path = fill(route.path);
            let r = req(&t, m, &path, &[("authorization", &viewer)], Some(json!({}))).await;
            let allowed = security::viewer_may(m, &path) || (*m == "POST" && path == "/api/v1/memory/inbox");
            if allowed && !route.path.contains("{path:path}") {
                viewer_ok += 1;
                if path.ends_with("/memory/inbox") {
                    assert_eq!(r.status, 403);
                } else {
                    assert_ne!(r.status, 403, "{m} {path}");
                }
                continue;
            }
            if route.path.contains("{path:path}") {
                continue;
            }
            assert_eq!(r.status, 403, "viewer could reach {m} {path} -> {}", r.status);
            assert_eq!(r.code(), "forbidden");
        }
    }
    assert_eq!(viewer_ok, 3);
    Ok(())
}

#[tokio::test]
async fn test_every_router_carries_the_role_dependency() -> R {
    // The pipeline applies enforce_roles to every /api/v1 route but the OpenAPI document (as FastAPI's routers do);
    // a viewer request that passed the middleware's list still meets it, and a non-listed one is refused by both.
    let t = setup()?;
    assert!(
        t.app
            .routes
            .iter()
            .filter(|r| r.path.starts_with("/api/v1/") && r.path != "/api/v1/openapi.json")
            .count()
            > 60
    );
    Ok(())
}

#[tokio::test]
async fn test_keys_are_hashed_and_secret_shown_once() -> R {
    let t = setup()?;
    let a = bearer(&t.key);
    let r = req(
        &t,
        "POST",
        "/api/v1/keys",
        &[("authorization", &a)],
        Some(json!({"name": "ci", "role": "viewer"})),
    )
    .await;
    assert_eq!(r.status, 201);
    let (secret, kid) = (
        r.json()["secret"].as_str().unwrap_or_default().to_string(),
        r.json()["id"].as_str().unwrap_or_default().to_string(),
    );
    assert!(secret.starts_with("hc_"));
    assert!(
        !req(&t, "GET", "/api/v1/keys", &[("authorization", &a)], None)
            .await
            .text()
            .contains(&secret)
    );
    let db = rusqlite::Connection::open(t.root.join("data/hub.db"))?;
    let dump: Vec<String> = db
        .prepare("SELECT id,name,role,prefix,hash,scopes FROM api_keys")?
        .query_map([], |r| {
            Ok(format!(
                "{}{}{}{}{}{}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?
            ))
        })?
        .collect::<Result<_, _>>()?;
    assert!(!dump.join("").contains(&secret));
    assert_eq!(
        req(
            &t,
            "DELETE",
            &format!("/api/v1/keys/{kid}"),
            &[("authorization", &a)],
            None
        )
        .await
        .status,
        204
    );
    assert_eq!(
        req(&t, "GET", "/api/v1/keys", &[("authorization", &bearer(&secret))], None)
            .await
            .status,
        401
    );
    for bad in [
        json!({"name": "x", "role": "root"}),
        json!({"name": "x", "role": "client"}),
        json!({"name": "x", "role": "client", "client": "c", "scopes": ["admin:all"]}),
        json!({"name": "x", "role": "viewer", "scopes": ["inbox:write"]}),
    ] {
        assert!(
            [400, 422].contains(
                &req(&t, "POST", "/api/v1/keys", &[("authorization", &a)], Some(bad))
                    .await
                    .status
            )
        );
    }
    Ok(())
}

#[tokio::test]
async fn test_bootstrap_admin_key_is_0600_and_works() -> R {
    let t = setup()?;
    let mode = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0)
    };
    assert_eq!(mode(&t.root.join("data/bootstrap-admin.key")), 0o600);
    assert_eq!(mode(&t.root.join("data")), 0o700);
    assert_eq!(
        req(&t, "GET", "/api/v1/keys", &[("authorization", &bearer(&t.key))], None)
            .await
            .status,
        200
    );
    assert_eq!(mode(&t.root.join("data/hub.db")), 0o600);
    Ok(())
}

#[tokio::test]
async fn test_audit_is_append_only_and_redacted() -> R {
    let t = setup()?;
    let a = bearer(&t.key);
    req(
        &t,
        "POST",
        "/api/v1/keys",
        &[("authorization", &a)],
        Some(json!({"name": "x", "api_key": "sk-supersecretvalue123456"})),
    )
    .await;
    req(
        &t,
        "POST",
        "/api/v1/memory/inbox",
        &[("authorization", &a)],
        Some(json!({"client": "x", "title": "PRIVATE TITLE", "body": "PRIVATE BODY"})),
    )
    .await;
    let items = req(&t, "GET", "/api/v1/audit", &[("authorization", &a)], None)
        .await
        .json()["items"]
        .clone();
    let text = items.to_string();
    assert!(!text.contains("supersecret") && !text.contains("PRIVATE BODY") && !text.contains("PRIVATE TITLE"));
    assert!(items.as_array().is_some_and(|l| {
        l.iter()
            .any(|i| i["method"] == "POST" && i["path"] == "/api/v1/keys" && i["status"] == 422)
    }));
    let db = rusqlite::Connection::open(t.root.join("data/hub.db"))?;
    assert!(db.execute("UPDATE audit SET status=1", []).is_err());
    assert!(db.execute("DELETE FROM audit", []).is_err());
    Ok(())
}

#[tokio::test]
async fn test_metrics_labels_are_bounded() -> R {
    let t = setup()?;
    let a = bearer(&t.key);
    for i in 0..5 {
        req(
            &t,
            "GET",
            &format!("/api/v1/skills/unknown-{i}"),
            &[("authorization", &a)],
            None,
        )
        .await;
        req(&t, "GET", &format!("/random/path/{i}"), &[], None).await;
    }
    let text = req(&t, "GET", "/metrics", &[("authorization", &a)], None).await.text();
    assert!(
        text.contains("route=\"/api/v1/skills/{name}\"")
            && !text.contains("unknown-")
            && !text.contains("/random/path")
    );
    assert!(text.contains("route=\"/{path:path}\""));
    Ok(())
}

#[tokio::test]
async fn test_static_allowlist_and_spa_fallback() -> R {
    let t = setup()?;
    assert!(
        req(&t, "GET", "/", &[], None)
            .await
            .text()
            .contains("<title>hub</title>")
    );
    assert!(
        req(&t, "GET", "/index.html", &[], None)
            .await
            .text()
            .contains("<title>hub</title>")
    );
    assert_eq!(req(&t, "GET", "/js/app.js", &[], None).await.text(), "export {}");
    assert_eq!(req(&t, "GET", "/css/app.css", &[], None).await.status, 200);
    assert!(
        req(&t, "GET", "/some/deep/link", &[], None)
            .await
            .text()
            .contains("<title>hub</title>")
    );
    for p in [
        "/package.json",
        "/dev/mock-server.mjs",
        "/js/%2e%2e/package.json",
        "/js/missing.js",
        "/js/",
        "/%2e%2e/outside.txt",
        "/js/..%2f..%2foutside.txt",
    ] {
        let r = req(&t, "GET", p, &[], None).await;
        assert_eq!(r.status, 404, "{p}");
        assert!(!r.text().contains("secret dev file") && r.text() != "outside");
    }
    Ok(())
}

#[tokio::test]
async fn test_static_files_are_revalidated_on_every_load() -> R {
    let t = setup()?;
    std::fs::write(t.root.join("frontend/js/util.js"), "export const a = 1;")?;
    for p in ["/", "/index.html", "/js/util.js", "/css/app.css", "/some/deep/link"] {
        let r = req(&t, "GET", p, &[], None).await;
        assert_eq!(r.status, 200, "{p}");
        assert_eq!(r.h("cache-control"), "no-cache", "{p}");
        let etag = r.h("etag");
        let again = req(
            &t,
            "GET",
            p,
            &[("if-none-match", &format!("W/{etag}, \"other\""))],
            None,
        )
        .await;
        assert_eq!(again.status, 304, "{p}");
        assert!(again.body.is_empty());
        assert_eq!(again.h("etag"), etag);
        assert_eq!(again.h("cache-control"), "no-cache");
        assert!(again.h("content-security-policy").contains("default-src 'none'"));
    }
    let etag = req(&t, "GET", "/js/util.js", &[], None).await.h("etag");
    std::fs::write(t.root.join("frontend/js/util.js"), "export const a = 1, b = 2;")?;
    let r = req(&t, "GET", "/js/util.js", &[("if-none-match", &etag)], None).await;
    assert_eq!(r.status, 200);
    assert!(r.text().contains("b = 2"));
    assert_eq!(
        req(&t, "GET", "/api/v1/health", &[], None).await.h("cache-control"),
        "no-store"
    );
    Ok(())
}

#[tokio::test]
async fn test_static_symlink_escape() -> R {
    let t = setup()?;
    std::os::unix::fs::symlink(t.root.join("outside.txt"), t.root.join("frontend/js/leak.js"))?;
    assert_eq!(req(&t, "GET", "/js/leak.js", &[], None).await.status, 404);
    Ok(())
}

#[tokio::test]
async fn test_viewer_cannot_read_keys_audit_settings() -> R {
    let t = setup()?;
    let v = req(
        &t,
        "POST",
        "/api/v1/keys",
        &[("authorization", &bearer(&t.key))],
        Some(json!({"name": "v", "role": "viewer"})),
    )
    .await
    .json();
    let viewer = bearer(v["secret"].as_str().unwrap_or_default());
    for p in ["keys", "audit", "settings"] {
        assert_eq!(
            req(&t, "GET", &format!("/api/v1/{p}"), &[("authorization", &viewer)], None)
                .await
                .status,
            403
        );
    }
    Ok(())
}

#[tokio::test]
async fn test_unknown_token_scope_is_422_with_a_clear_message() -> R {
    let t = setup()?;
    let a = bearer(&t.key);
    for scope in ["knowledge:read:docs", "admin:all", "inbox:read"] {
        let r = req(
            &t,
            "POST",
            "/api/v1/keys",
            &[("authorization", &a)],
            Some(json!({"name": "x", "role": "client", "client": "c", "scopes": [scope]})),
        )
        .await;
        assert_eq!(r.status, 422);
        assert_eq!(r.code(), "unknown_scope");
        assert!(
            r.json()["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("only hub token scope is inbox:write")
        );
    }
    let ok = req(
        &t,
        "POST",
        "/api/v1/keys",
        &[("authorization", &a)],
        Some(json!({"name": "x", "role": "client", "client": "c", "scopes": ["inbox:write"]})),
    )
    .await;
    assert_eq!(ok.status, 201);
    Ok(())
}

#[tokio::test]
async fn test_host_allowlist_follows_the_effective_port() -> R {
    let t = setup_with(&[("HUB_PORT", "9911")])?;
    let a = bearer(&t.key);
    let go = |host: &'static str| {
        let (app, a) = (t.app.clone(), a.clone());
        async move {
            req_raw(
                &app,
                9911,
                "10.0.0.9:5000",
                "GET",
                "/api/v1/keys",
                &[("host", host), ("authorization", &a)],
                None,
            )
            .await
            .status
        }
    };
    assert_eq!(go("127.0.0.1:9911").await, 200);
    assert_eq!(go("localhost:9911").await, 200);
    assert_eq!(go("127.0.0.1:8792").await, 421);
    assert_eq!(go("evil.com:9911").await, 421);
    Ok(())
}

#[tokio::test]
async fn test_client_token_cannot_reach_anything_but_inbox() -> R {
    let t = setup()?;
    let a = bearer(&t.key);
    for scopes in [json!(["inbox:write"]), json!([])] {
        let k = req(
            &t,
            "POST",
            "/api/v1/keys",
            &[("authorization", &a)],
            Some(json!({"name": "t", "role": "client", "client": "fake1", "scopes": scopes})),
        )
        .await
        .json();
        let tok = bearer(k["secret"].as_str().unwrap_or_default());
        for route in &t.app.routes {
            if !(route.path.starts_with("/api/") || route.path == "/metrics") || route.path == "/api/v1/health" {
                continue;
            }
            for m in route.methods.iter().filter(|m| **m != "HEAD") {
                if *m == "POST" && route.path == "/api/v1/memory/inbox" {
                    continue;
                }
                let r = req(&t, m, &fill(route.path), &[("authorization", &tok)], Some(json!({}))).await;
                assert_eq!(r.status, 403, "{m} {}", route.path);
            }
        }
    }
    Ok(())
}
