//! One explicit test per knowledge-service security control (PORTING.md §14 S-K-*), beyond what the ports of the
//! Python tests already pin: Host allowlist variants, path-segment refusals, Origin/CSRF on every non-safe method,
//! body caps, headers on every kind of response, decode-before-auth / auth-before-validation, path-ingest roots, glob
//! and file filters, SQL-safe ids in Lance predicates, rate limiting, the non-loopback refusal, and secrets that are
//! never echoed.

#![cfg(test)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use agent_knowledge::app::{self, App, Options};
use agent_knowledge::config::Config;
use agent_knowledge::embed::HashEmbedder;
use agent_knowledge::env;
use agent_knowledge::paths::{check_glob, iter_files, resolve_base};
use agent_knowledge::ratelimit::RateLimiter;
use agent_knowledge::security::{self, netloc};
use agent_knowledge::stores::memory::MemoryStore;
use axum::body::Body as AxBody;
use serde_json::Value;

struct H {
    app: Arc<App>,
    admin: String,
    _tmp: tempfile::TempDir,
}

fn hub(adjust: impl FnOnce(&mut Config)) -> H {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut cfg = Config::with_data_dir(&tmp.path().join("data"));
    cfg.router_url = "http://127.0.0.1:9".into();
    adjust(&mut cfg);
    let data = cfg.data_dir.clone();
    let app = app::create_app_with(
        cfg,
        Options {
            stores: Some(vec![("lancedb".into(), Arc::new(MemoryStore::new()))]),
            embedder: Some(Arc::new(HashEmbedder::new(4))),
            env: Some(env::from_pairs(&[])),
        },
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let admin = std::fs::read_to_string(data.join("admin.key"))
        .unwrap_or_default()
        .trim()
        .to_string();
    H { app, admin, _tmp: tmp }
}

async fn req(h: &H, m: &str, p: &str, headers: &[(&str, &str)], body: &str) -> (u16, http::HeaderMap, Value) {
    let mut b = http::Request::builder().method(m).uri(p);
    if !headers.iter().any(|(k, _)| *k == "host") {
        b = b.header("host", "127.0.0.1:8795");
    }
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    if !body.is_empty() {
        b = b.header("content-length", body.len().to_string());
    }
    let resp = app::handle(
        h.app.clone(),
        b.body(AxBody::from(body.to_string())).unwrap_or_else(|e| panic!("{e}")),
    )
    .await;
    let st = resp.status().as_u16();
    let hd = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    (st, hd, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn assert_security_headers(h: &http::HeaderMap, what: &str) {
    for (k, v) in security::SECURITY_HEADERS {
        assert_eq!(h.get(k).and_then(|x| x.to_str().ok()), Some(v), "{what}: {k}");
    }
    assert!(h.get("access-control-allow-origin").is_none(), "{what}: CORS");
}

#[tokio::test]
async fn host_allowlist_421_with_configured_extras() {
    let h = hub(|c| {
        c.allowed_hosts = vec!["Knowledge.Internal:8795".into()];
        c.host = "10.0.0.5".into();
    });
    for host in [
        "127.0.0.1:8795",
        "LOCALHOST:8795",
        "[::1]:8795",
        "10.0.0.5:8795",
        "knowledge.internal:8795",
    ] {
        assert_eq!(req(&h, "GET", "/health", &[("host", host)], "").await.0, 200, "{host}");
    }
    for host in ["127.0.0.1", "127.0.0.1:9999", "evil.example", "localhost.:8795", ""] {
        let (st, hd, b) = req(&h, "GET", "/health", &[("host", host)], "").await;
        assert_eq!((st, b["code"].as_str()), (421, Some("misdirected_request")), "{host:?}");
        assert_security_headers(&hd, "421");
    }
}

#[tokio::test]
async fn invalid_path_segments_400_before_anything_else() {
    let h = hub(|_| {});
    for p in [
        "//x",
        "/a//b",
        "/namespaces/%2e%2e/x",
        "/namespaces/%2E/x",
        "/a/.",
        "/x%5cy",
        "/x%00y",
    ] {
        let (st, _, b) = req(&h, "GET", p, &[], "").await;
        assert_eq!((st, b["code"].as_str()), (400, Some("invalid_path")), "{p}");
    }
    // a trailing empty segment is allowed (Starlette then redirects)
    assert_eq!(req(&h, "GET", "/namespaces/", &[], "").await.0, 307);
}

#[tokio::test]
async fn origin_and_csrf_on_every_non_safe_method() {
    let h = hub(|_| {});
    for m in ["POST", "PUT", "PATCH", "DELETE", "PROPFIND"] {
        let (st, _, b) = req(&h, m, "/tokens/x", &[], "").await;
        assert_eq!((st, b["code"].as_str()), (403, Some("csrf_header_required")), "{m}");
        let (st, _, b) = req(
            &h,
            m,
            "/tokens/x",
            &[("x-agent-knowledge", "1"), ("origin", "http://127.0.0.1:1")],
            "",
        )
        .await;
        assert_eq!((st, b["code"].as_str()), (403, Some("cross_origin")), "{m}");
    }
    // the same origin passes the precheck (then authentication answers)
    let (st, _, _) = req(
        &h,
        "DELETE",
        "/tokens/x",
        &[("x-agent-knowledge", "1"), ("origin", "http://127.0.0.1:8795")],
        "",
    )
    .await;
    assert_eq!(st, 401);
    // GET never needs the header and never checks Origin
    let (st, _, _) = req(&h, "GET", "/health", &[("origin", "http://evil.example")], "").await;
    assert_eq!(st, 200);
    // a malformed bracketed origin is an exception in the middleware: Python's bare 500
    assert!(netloc("http://[evil").is_err());
    let (st, hd, b) = req(
        &h,
        "POST",
        "/namespaces",
        &[("x-agent-knowledge", "1"), ("origin", "http://[evil")],
        "",
    )
    .await;
    assert_eq!((st, b["code"].as_str()), (500, Some("internal_error")));
    assert_security_headers(&hd, "500");
}

#[tokio::test]
async fn body_caps_apply_to_non_safe_methods_only() {
    let h = hub(|c| c.body_cap_bytes = 10);
    let auth = format!("Bearer {}", h.admin);
    let (st, _, b) = req(
        &h,
        "POST",
        "/namespaces",
        &[("authorization", &auth), ("content-type", "application/json")],
        &"x".repeat(11),
    )
    .await;
    assert_eq!((st, b["code"].as_str()), (413, Some("payload_too_large")));
    // exactly at the cap is fine (then the body is decoded: not JSON -> 422)
    let (st, _, _) = req(
        &h,
        "POST",
        "/namespaces",
        &[("authorization", &auth), ("content-type", "application/json")],
        &"x".repeat(10),
    )
    .await;
    assert_eq!(st, 422);
    // GET bodies are neither capped nor read
    let (st, _, _) = req(&h, "GET", "/health", &[], &"x".repeat(100)).await;
    assert_eq!(st, 200);
}

#[tokio::test]
async fn security_headers_on_every_kind_of_response() {
    let h = hub(|_| {});
    let auth = format!("Bearer {}", h.admin);
    type Case<'a> = (&'a str, &'a str, Vec<(&'a str, &'a str)>, &'a str, u16);
    let cases: Vec<Case<'_>> = vec![
        ("GET", "/health", vec![], "", 200),
        ("GET", "/nope", vec![], "", 404),
        ("PATCH", "/namespaces", vec![("x-agent-knowledge", "1")], "", 405),
        ("GET", "/namespaces/", vec![], "", 307),
        ("GET", "/namespaces", vec![], "", 401),
        (
            "DELETE",
            "/tokens/nope",
            vec![("authorization", auth.as_str())],
            "",
            404,
        ),
        (
            "POST",
            "/namespaces",
            vec![("authorization", auth.as_str()), ("content-type", "application/json")],
            "{\"name\":\"a\",\"backend\":\"lancedb\",\"dim\":4}",
            201,
        ),
        (
            "DELETE",
            "/namespaces/a",
            vec![("authorization", auth.as_str())],
            "",
            204,
        ),
        ("GET", "/metrics", vec![("authorization", auth.as_str())], "", 200),
    ];
    for (m, p, hdrs, body, want) in cases {
        let (st, hd, _) = req(&h, m, p, &hdrs, body).await;
        assert_eq!(st, want, "{m} {p}");
        assert_security_headers(&hd, &format!("{m} {p}"));
    }
}

#[tokio::test]
async fn json_is_decoded_before_auth_and_validated_after_it() {
    let h = hub(|_| {});
    let j = [("x-agent-knowledge", "1"), ("content-type", "application/json")];
    // FastAPI reads and decodes the body before any dependency: bad JSON is a 422 even without a token
    let (st, _, b) = req(&h, "POST", "/namespaces", &j, "{bad").await;
    assert_eq!((st, b["detail"].as_str()), (422, Some("body.1: JSON decode error")));
    let deep = format!("{}{}", "[".repeat(9992), "]".repeat(9992));
    let (st, _, b) = req(&h, "POST", "/namespaces", &j, &deep).await;
    assert_eq!((st, b["code"].as_str()), (400, Some("http_error")));
    // but a decodable body that fails validation never reaches validation without a valid token
    let (st, _, _) = req(&h, "POST", "/namespaces", &j, "{\"zzz\": 1}").await;
    assert_eq!(st, 401);
}

#[tokio::test]
async fn admin_rate_limit_and_denial_metrics() {
    let h = hub(|c| c.default_rate_per_min = 2);
    let auth = format!("Bearer {}", h.admin);
    let mut st = Vec::new();
    for _ in 0..3 {
        st.push(req(&h, "GET", "/namespaces", &[("authorization", &auth)], "").await.0);
    }
    assert_eq!(st, vec![200, 200, 429]);
    let (_, hd, b) = req(&h, "GET", "/namespaces", &[("authorization", &auth)], "").await;
    assert_eq!(b["code"].as_str(), Some("rate_limited"));
    assert_eq!(hd.get("retry-after").and_then(|v| v.to_str().ok()), Some("30"));
    assert_eq!(h.app.ctx.tel.denied.value(&["rate_limited"]), 2.0);
    let rl = RateLimiter::default();
    assert!(rl.allow("k", 1) && !rl.allow("k", 1));
}

#[test]
fn path_ingest_roots_and_traversal() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let root = tmp.path().join("root");
    let outside = tmp.path().join("outside");
    for d in [&root, &outside] {
        std::fs::create_dir(d).unwrap_or_else(|e| panic!("{e}"));
    }
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap_or_else(|e| panic!("{e}"));
    let roots = vec![root.clone()];
    let code = |p: &str, r: &[std::path::PathBuf]| resolve_base(p, r).err().map(|e| (e.status, e.code));
    assert_eq!(
        code(&root.to_string_lossy(), &[]),
        Some((403, "path_not_allowed".into()))
    );
    assert_eq!(code("relative", &roots), Some((403, "path_not_allowed".into())));
    assert_eq!(code("/a\0b", &roots), Some((403, "path_not_allowed".into())));
    assert_eq!(
        code(&format!("/{}", "a".repeat(1024)), &roots),
        Some((403, "path_not_allowed".into()))
    );
    assert_eq!(
        code(&root.join("missing").to_string_lossy(), &roots),
        Some((422, "path_not_found".into()))
    );
    assert_eq!(
        code(&root.join("escape").to_string_lossy(), &roots),
        Some((403, "path_not_allowed".into()))
    );
    assert_eq!(
        code(&format!("{}/../outside", root.display()), &roots),
        Some((403, "path_not_allowed".into()))
    );
    assert!(resolve_base(&root.to_string_lossy(), &roots).is_ok());
}

#[test]
fn glob_charset_and_file_filters() {
    for bad in [
        "",
        "/etc/*",
        "../x",
        "a/../b",
        "a b",
        "*;rm",
        "[ab]",
        "x\\y",
        &"a".repeat(201),
    ] {
        assert!(check_glob(bad).is_err(), "{bad:?}");
    }
    for ok in ["**/*", "*.md", "docs/**/?.txt", "a-b_c.d"] {
        assert!(check_glob(ok).is_ok(), "{ok}");
    }
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let root = tmp.path();
    let mk = |p: &str, data: &[u8]| {
        let f = root.join(p);
        if let Some(d) = f.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        std::fs::write(f, data).unwrap_or_else(|e| panic!("{e}"));
    };
    let ssh_dir = format!(".{}", "ssh"); // spelled out at run time only
    mk(&format!("{ssh_dir}/id.txt"), b"x");
    mk(".git/config.md", b"x");
    mk("secrets/a.md", b"x");
    mk(".env.local.md", b"x");
    mk("ok/A.MD", b"upper-case suffix is fine");
    mk("big.md", &vec![b'x'; 2 * 1024 * 1024 + 1]);
    mk("tool.exe", b"x");
    mk("keep.txt", b"x");
    let files = iter_files(root, root, "**/*", 100).unwrap_or_default();
    let rel: Vec<String> = files
        .iter()
        .map(|f| f.strip_prefix(root).unwrap_or(f).to_string_lossy().into_owned())
        .collect();
    assert_eq!(rel, vec!["keep.txt", "ok/A.MD"]);
    let e = iter_files(root, root, "**/*", 1).err().map(|e| (e.status, e.code));
    assert_eq!(e, Some((413, "too_many_files".into())));
}

#[cfg(feature = "lance")]
#[tokio::test]
async fn lance_predicates_only_ever_hold_regex_checked_ids() {
    use agent_knowledge::filters::Node;
    use agent_knowledge::stores::VectorStore;
    use agent_knowledge::stores::lance::{LanceOptions, LanceStore};
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let s = LanceStore::new(tmp.path(), LanceOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    assert!(s.ensure("ns", 2).await.is_ok());
    for nasty in ["x' OR '1'='1", "a\")--", "a b", "a;DROP"] {
        let e = s
            .delete("ns", &Node::eq_str("doc_id", nasty))
            .await
            .err()
            .map(|e| (e.status, e.code));
        assert_eq!(e, Some((422, "invalid_id".into())), "{nasty}");
    }
}

#[test]
fn non_loopback_bind_is_refused_without_the_flag() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let cfg = tmp.path().join("k.yaml");
    let _ = std::fs::write(
        &cfg,
        format!(
            "host: 0.0.0.0\nport: 1\ndata_dir: {}\nrouter_url: http://127.0.0.1:9\n",
            tmp.path().join("d").display()
        ),
    );
    let args = vec!["--config".to_string(), cfg.to_string_lossy().into_owned()];
    assert_eq!(agent_knowledge::main_entry(&args), 1);
    assert!(!tmp.path().join("d").exists()); // refused before anything was created
    let _ = std::fs::write(&cfg, "port: 70000\nrouter_url: http://127.0.0.1:9\n");
    assert_eq!(agent_knowledge::main_entry(&args), 1);
    assert_eq!(agent_knowledge::main_entry(&["--bogus".to_string()]), 2);
}

#[tokio::test]
async fn secrets_and_private_text_are_never_echoed() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let h = hub(|c| c.data_dir = tmp.path().join("d"));
    let auth = format!("Bearer {}", h.admin);
    let j = [("authorization", auth.as_str()), ("content-type", "application/json")];
    // the token secret is shown once and only its hash is stored; the listing has neither
    let _ = req(
        &h,
        "POST",
        "/namespaces",
        &j,
        r#"{"name":"n","backend":"lancedb","dim":4}"#,
    )
    .await;
    let (_, _, tok) = req(&h, "POST", "/tokens", &j, r#"{"client":"c","scopes":[{"ns":"n"}]}"#).await;
    let secret = tok["token"].as_str().unwrap_or_default().to_string();
    assert!(secret.starts_with("akn_"));
    let (_, _, list) = req(&h, "GET", "/tokens", &[("authorization", &auth)], "").await;
    assert!(!list.to_string().contains(&secret) && !list.to_string().contains("hash"));
    // validation details carry locations and messages, never the input
    let (_, _, b) = req(
        &h,
        "POST",
        "/namespaces",
        &j,
        r#"{"name": ["PRIVATE-1"], "backend": "PRIVATE-2", "dim": "PRIVATE-3"}"#,
    )
    .await;
    assert!(!b.to_string().contains("PRIVATE"));
    // the audit log is metadata only and private
    let _ = req(
        &h,
        "POST",
        "/namespaces/n/ingest",
        &j,
        r#"{"documents":[{"text":"PRIVATE-DOC"}]}"#,
    )
    .await;
    let audit = tmp.path().join("d/audit.jsonl");
    let log = std::fs::read_to_string(&audit).unwrap_or_default();
    assert!(!log.contains("PRIVATE") && !log.contains(&h.admin));
    assert_eq!(
        std::fs::metadata(&audit).map(|m| m.permissions().mode() & 0o777).ok(),
        Some(0o600)
    );
    assert!(Path::new(&tmp.path().join("d/admin.key")).exists());
}

#[tokio::test]
async fn event_streams_are_capped() {
    let h = hub(|c| c.sse_max_streams = 0);
    let auth = format!("Bearer {}", h.admin);
    let j = [("authorization", auth.as_str()), ("content-type", "application/json")];
    let _ = req(
        &h,
        "POST",
        "/namespaces",
        &j,
        r#"{"name":"n","backend":"lancedb","dim":4}"#,
    )
    .await;
    let (st, _, job) = req(
        &h,
        "POST",
        "/namespaces/n/ingest",
        &j,
        r#"{"documents":[{"text":"a"}]}"#,
    )
    .await;
    assert_eq!(st, 202);
    let jid = job["job"]["id"].as_str().unwrap_or_default().to_string();
    let (st, hd, b) = req(
        &h,
        "GET",
        &format!("/jobs/{jid}/stream"),
        &[("authorization", &auth)],
        "",
    )
    .await;
    assert_eq!((st, b["code"].as_str()), (429, Some("too_many_streams")));
    assert_eq!(hd.get("retry-after").and_then(|v| v.to_str().ok()), Some("30"));
}

/// Over a real socket: a client that uploads more than the cap still reads its 413 (the server keeps reading and
/// discarding the body like uvicorn instead of resetting the connection under the client's writes).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_cap_upload_still_gets_its_413() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let h = hub(|c| c.port = i64::from(port));
    let server = tokio::spawn(app::serve(h.app.clone(), listener));
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let body = vec![b'x'; 6 * 1024 * 1024];
    let head = format!(
        "POST /namespaces HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Agent-Knowledge: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap_or_else(|e| panic!("{e}"));
    s.write_all(&body)
        .await
        .unwrap_or_else(|e| panic!("write under the 413 failed: {e}"));
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out);
    assert!(text.starts_with("HTTP/1.1 413"), "{text}");
    assert!(text.contains("payload_too_large"));
    server.abort();
}

/// Since 0.3.0, as on the hub: a body that breaks off is a 400 (it was handed on as a shorter body) and a body that
/// stops arriving is a 408 after the deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_that_breaks_off_is_400_and_a_stalled_one_408() {
    use futures::StreamExt as _;
    let h = hub(|_| {});
    let post = |body: AxBody| {
        http::Request::builder()
            .method("POST")
            .uri("/namespaces")
            .header("host", "127.0.0.1:8795")
            .header("x-agent-knowledge", "1")
            .header("authorization", format!("Bearer {}", h.admin))
            .header("content-type", "application/json")
            .body(body)
            .unwrap_or_else(|e| panic!("{e}"))
    };
    let parse = |b: &[u8]| serde_json::from_slice::<Value>(b).unwrap_or(Value::Null);
    let broken: Vec<Result<bytes::Bytes, std::io::Error>> = vec![
        Ok(bytes::Bytes::from_static(b"{\"name\"")),
        Err(std::io::Error::other("connection reset")),
    ];
    let r = app::handle(h.app.clone(), post(AxBody::from_stream(futures::stream::iter(broken)))).await;
    let status = r.status().as_u16();
    let body = http_body_util::BodyExt::collect(r.into_body())
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    assert_eq!(
        (status, parse(&body)["code"].clone()),
        (400, Value::from("body_incomplete"))
    );

    h.app.set_body_deadline(std::time::Duration::from_millis(300));
    let first: Vec<Result<bytes::Bytes, std::io::Error>> = vec![Ok(bytes::Bytes::from_static(b"{\"name\""))];
    let stalled = futures::stream::iter(first).chain(futures::stream::pending());
    let t0 = std::time::Instant::now();
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        app::handle(h.app.clone(), post(AxBody::from_stream(stalled))),
    )
    .await
    .unwrap_or_else(|_| panic!("no answer to a stalled body"));
    let status = r.status().as_u16();
    let body = http_body_util::BodyExt::collect(r.into_body())
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    assert_eq!(
        (status, parse(&body)["code"].clone()),
        (408, Value::from("body_timeout"))
    );
    assert!(t0.elapsed() < std::time::Duration::from_secs(5));
}

async fn read_until_eof(s: &mut tokio::net::TcpStream, within: std::time::Duration) -> Vec<u8> {
    use tokio::io::AsyncReadExt as _;
    let mut out = Vec::new();
    tokio::time::timeout(within, s.read_to_end(&mut out))
        .await
        .unwrap_or_else(|_| panic!("the server kept the connection open"))
        .unwrap_or_else(|e| panic!("{e}"));
    out
}

/// The same connection limits as the hub: silent, half-sent and idle keep-alive connections are closed after the
/// header timeout, and shutdown returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_and_slow_header_connections_are_closed_like_the_hub() {
    use tokio::io::AsyncWriteExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0") // ephemeral, loopback only
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let h = hub(|c| c.port = i64::from(port));
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let limits = app::ServeLimits {
        header_read_timeout: std::time::Duration::from_millis(400),
        ..app::ServeLimits::default()
    };
    let served = tokio::spawn(app::serve_with(h.app.clone(), listener, limits, async move {
        let _ = stop_rx.await;
    }));
    let connect = || tokio::net::TcpStream::connect(("127.0.0.1", port));
    let health = format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    // silent, then half-sent headers: both closed after the header timeout
    let mut silent = connect().await.unwrap_or_else(|e| panic!("{e}"));
    assert!(
        read_until_eof(&mut silent, std::time::Duration::from_secs(5))
            .await
            .is_empty()
    );
    let mut partial = connect().await.unwrap_or_else(|e| panic!("{e}"));
    partial
        .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n")
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let _ = read_until_eof(&mut partial, std::time::Duration::from_secs(5)).await;
    // answered, then closed when idle
    let mut kept = connect().await.unwrap_or_else(|e| panic!("{e}"));
    kept.write_all(health.as_bytes())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let got = read_until_eof(&mut kept, std::time::Duration::from_secs(5)).await;
    assert!(
        String::from_utf8_lossy(&got).starts_with("HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&got)
    );
    let _ = stop_tx.send(());
    let done = tokio::time::timeout(std::time::Duration::from_secs(15), served)
        .await
        .unwrap_or_else(|_| panic!("shutdown did not return"));
    assert!(matches!(done, Ok(Ok(()))));
}

/// At most `max_connections` are served at once; the next one waits until a slot is free.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connections_are_capped_like_the_hub() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0") // ephemeral, loopback only
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let h = hub(|c| c.port = i64::from(port));
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let limits = app::ServeLimits {
        header_read_timeout: std::time::Duration::from_secs(20),
        max_connections: 1,
        shutdown_grace: std::time::Duration::from_secs(1),
    };
    let served = tokio::spawn(app::serve_with(h.app.clone(), listener, limits, async move {
        let _ = stop_rx.await;
    }));
    let connect = || tokio::net::TcpStream::connect(("127.0.0.1", port));
    let holder = connect().await.unwrap_or_else(|e| panic!("{e}")); // takes the only slot, sends nothing
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let mut waiting = connect().await.unwrap_or_else(|e| panic!("{e}"));
    let health = format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    waiting
        .write_all(health.as_bytes())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let mut buf = [0u8; 64];
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(600), waiting.read(&mut buf))
            .await
            .is_err(),
        "a second connection was served while the cap was reached"
    );
    drop(holder); // frees the slot
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), waiting.read(&mut buf))
        .await
        .unwrap_or_else(|_| panic!("never served after the slot was freed"))
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
    let _ = stop_tx.send(());
    let done = tokio::time::timeout(std::time::Duration::from_secs(15), served)
        .await
        .unwrap_or_else(|_| panic!("shutdown did not return"));
    assert!(matches!(done, Ok(Ok(()))));
}
