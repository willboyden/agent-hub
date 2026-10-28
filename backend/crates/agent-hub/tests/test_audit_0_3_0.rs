//! Findings of the independent audit of the Rust backend before 0.3.0 (backend/PORTING.md addendum A-14), each with a
//! test that fails without its fix: F1 (no blocking open of a FIFO anywhere the hub reads a path someone else can
//! write), F3 (SB-3: the inbox `client` filter is an id, never a path), F5 (migrate-content refuses special files),
//! F8 (complete bodies within a deadline, a panicking handler is a 500 with its audit row, connection limits).

mod b2_fakes;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use agent_hub::adapters::_util as u;
use agent_hub::adapters::base::ClientConfig;
use agent_hub::api::deps::SseLimiter;
use agent_hub::api::{AppState, Req, Route, routes, security};
use agent_hub::server::{ServeLimits, serve_with};
use agent_hub::services::fs::discovery_fs;
use axum::body::Body;
use b2_fakes::*;
use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use http_body_util::BodyExt as _;
use hub_core::value::PyValue;
use indexmap::IndexMap;
use serde_json::json;

/// Run `f` on a thread; an error (not a hung suite) when it has not returned within 10 s. A reader stuck in a blocking
/// open of `fifo` is released by opening the FIFO for writing.
fn within<T: Send + 'static>(fifo: &Path, f: impl FnOnce() -> T + Send + 'static) -> Result<T, E> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(v) => Ok(v),
        Err(_) => {
            let _ = std::fs::OpenOptions::new().write(true).open(fifo);
            Err(format!("blocked on the FIFO at {}", fifo.display()).into())
        }
    }
}

fn mkfifo(p: &Path) -> R {
    nix::unistd::mkfifo(p, nix::sys::stat::Mode::from_bits_truncate(0o600))?;
    Ok(())
}

// ---- F1 ---------------------------------------------------------------------------------------------------------------

/// A FIFO planted at a delivered file: verify (which runs under the hub lock in `Hub::verify` and after every apply)
/// answers promptly with a failed check instead of blocking every later mutating request.
#[test]
fn f1_verify_with_a_fifo_at_a_delivered_path_answers_promptly() -> R {
    let f = hub()?.setup()?;
    f.run(None, &[])?;
    let target = f.home.join("agents/reviewer.md");
    std::fs::remove_file(&target)?;
    mkfifo(&target)?;
    let hub = f.hub.clone();
    let v = within(&target, move || hub.verify("fake1").map_err(|p| p.detail))??;
    assert_eq!(v["ok"], json!(false));
    // and the hub lock is free again: a plan (which takes it too) still works
    let hub = f.hub.clone();
    within(&target, move || {
        hub.plan(None, "committed").map(|_| ()).map_err(|p| p.detail)
    })??;
    drop(f); // the temp dirs live until here
    Ok(())
}

/// A FIFO in a client's skills tree: discovery skips it (and a FIFO named SKILL.md) instead of hanging the import.
#[test]
fn f1_discovery_skips_a_fifo_in_a_skills_tree() -> R {
    let t = tempfile::tempdir()?;
    let home = t.path().join("home");
    std::fs::create_dir_all(home.join("skills/s1"))?;
    std::fs::create_dir_all(home.join("skills/s2"))?;
    std::fs::write(home.join("skills/s1/SKILL.md"), "---\ndescription: one\n---\nbody\n")?;
    let fifo = home.join("skills/s1/notes.md");
    mkfifo(&fifo)?;
    mkfifo(&home.join("skills/s2/SKILL.md"))?;
    let roots = IndexMap::from([("home".to_string(), home.to_string_lossy().to_string())]);
    let cfg = ClientConfig {
        id: "c".into(),
        adapter: "x".into(),
        display_name: "c".into(),
        roots: roots.clone(),
        ..ClientConfig::default()
    };
    let (skills, notes) = within(&fifo, move || {
        let fs = discovery_fs(&roots, &PyValue::Dict(vec![]), &[]);
        let mut notes = Vec::new();
        let r = u::discover_skills(&fs, &cfg, "home", "skills", &mut notes).map_err(|e| e.py_type);
        (r, notes)
    })?;
    let skills = skills?;
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "s1");
    assert_eq!(skills[0].files.keys().collect::<Vec<_>>(), vec!["SKILL.md"]);
    assert!(notes.iter().any(|n| n.contains("'s2': no SKILL.md")), "{notes:?}");
    Ok(())
}

// ---- F3 (SB-3) -------------------------------------------------------------------------------------------------------

/// A copy of a real inbox item placed where a path-shaped `client` would reach: content/instructions/ (`../instructions`)
/// and a directory outside the content repo (`/abs/dir`). Python listed those; 0.3.0 lists nothing for a non-id client.
fn plant_items(f: &Fx) -> Result<PathBuf, E> {
    f.hub
        .inbox
        .add("fake1", "Idea", "text", "reference")
        .map_err(|p| p.detail)?;
    let dir = f.content("inbox/fake1");
    let item = std::fs::read_dir(&dir)?.next().ok_or("no inbox item")??.path();
    let outside = f.root.join("outside");
    std::fs::create_dir_all(&outside)?;
    std::fs::copy(&item, outside.join("leak-abc123.md"))?;
    std::fs::copy(&item, f.content("instructions/leak-abc123.md"))?;
    Ok(outside)
}

#[test]
fn f3_inbox_list_treats_client_as_an_id_not_a_path() -> R {
    let f = hub()?.setup()?;
    let outside = plant_items(&f)?;
    assert_eq!(f.hub.inbox.list(Some("fake1")).map_err(|p| p.detail)?.len(), 1);
    for c in [
        "../instructions",
        outside.to_string_lossy().as_ref(),
        "fake1/../../instructions",
        "..",
    ] {
        assert_eq!(
            f.hub.inbox.list(Some(c)).map_err(|p| p.detail)?,
            Vec::<serde_json::Value>::new(),
            "{c}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn f3_inbox_route_lists_nothing_for_a_path_shaped_client() -> R {
    let f = hub()?.setup()?;
    let outside = plant_items(&f)?;
    let viewer = f.viewer_key()?;
    let app = f.api();
    let r = req(
        &app,
        "GET",
        &format!("{API}/memory/inbox?client=fake1"),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!((r.status, r.json()["items"].as_array().map(Vec::len)), (200, Some(1)));
    for c in ["..%2Finstructions", &outside.to_string_lossy().replace('/', "%2F")] {
        let r = req(
            &app,
            "GET",
            &format!("{API}/memory/inbox?client={c}"),
            Some(&viewer),
            None,
        )
        .await;
        assert_eq!(
            (r.status, r.json()),
            (200, json!({"items": [], "next_cursor": null})),
            "{c}"
        );
    }
    Ok(())
}

// ---- F5 ---------------------------------------------------------------------------------------------------------------

/// migrate-content (`--yes`) with a FIFO in the content repo: refused promptly, nothing moved, no temp copy left.
#[test]
fn f5_migrate_content_refuses_a_fifo_instead_of_blocking() -> R {
    let f = hub()?.setup()?;
    let fifo = f.content("instructions/pipe");
    mkfifo(&fifo)?;
    let target = f.root.join("moved/content");
    let hub = f.hub.clone();
    let t = target.to_string_lossy().to_string();
    let e = within(&fifo, move || hub.migrate_content(&t, true).err())?.ok_or("migrated with a FIFO inside")?;
    assert_eq!(e.status, 500);
    assert!(f.content("instructions").is_dir() && !target.exists());
    let parent = target.parent().ok_or("parent")?;
    let left: Vec<_> = std::fs::read_dir(parent)?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".migrate-"))
        .collect();
    assert!(left.is_empty(), "temp copy left behind");
    Ok(())
}

// ---- F8 ---------------------------------------------------------------------------------------------------------------

/// The served app of `f` with an extra route that panics and a short body deadline.
fn app_with_panic_route(f: &Fx, body_deadline: Duration) -> Arc<AppState> {
    #[allow(clippy::panic)]
    fn boom(_s: Arc<AppState>, _r: Req) -> BoxFuture<'static, http::Response<Body>> {
        Box::pin(async { panic!("handler bug") })
    }
    let base = f.api(); // also writes the bootstrap key, like a served hub
    let mut table = vec![Route::new(
        &["POST"],
        "/api/v1/zz-panic",
        "/api/v1/zz-panic",
        boom,
        false,
    )];
    table.extend(routes::table());
    Arc::new(AppState {
        hub: base.hub.clone(),
        routes: table,
        frontend: None,
        knowledge_sse: SseLimiter::new(4),
        body_deadline,
    })
}

async fn send(app: &Arc<AppState>, path: &str, key: &str, body: Body) -> Result<(u16, serde_json::Value), E> {
    let r = http::Request::builder()
        .method("POST")
        .uri(path)
        .header("host", HOST)
        .header("x-agent-hub", "1")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(body)?;
    let resp = security::handle(app.clone(), SocketAddr::from(([10, 0, 0, 9], 5000)), PORT, r).await;
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await?.to_bytes();
    Ok((
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    ))
}

/// A body whose stream fails part-way is a 400, never handed to the handler as if it were the whole body.
#[tokio::test(flavor = "multi_thread")]
async fn f8_a_body_that_breaks_off_is_400_not_a_shorter_body() -> R {
    let f = hub()?.setup()?;
    let app = app_with_panic_route(&f, Duration::from_secs(5));
    let key = f.admin_key()?;
    let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
        Ok(Bytes::from_static(b"{\"rules\": []")),
        Err(std::io::Error::other("connection reset")),
    ];
    let (status, body) = send(
        &app,
        "/api/v1/policy/check",
        &key,
        Body::from_stream(futures::stream::iter(chunks)),
    )
    .await?;
    assert_eq!(
        (status, body["code"].as_str()),
        (400, Some("body_incomplete")),
        "{body}"
    );
    Ok(())
}

/// A body that stops arriving is a 408 after the deadline instead of holding the request open.
#[tokio::test(flavor = "multi_thread")]
async fn f8_a_body_that_stalls_is_408_after_the_deadline() -> R {
    let f = hub()?.setup()?;
    let app = app_with_panic_route(&f, Duration::from_millis(300));
    let key = f.admin_key()?;
    let first: Vec<Result<Bytes, std::io::Error>> = vec![Ok(Bytes::from_static(b"{\"rules\""))];
    let stalled = futures::stream::iter(first).chain(futures::stream::pending());
    let t0 = std::time::Instant::now();
    let r = tokio::time::timeout(
        Duration::from_secs(10),
        send(&app, "/api/v1/policy/check", &key, Body::from_stream(stalled)),
    )
    .await
    .map_err(|_| "no answer to a stalled body")??;
    assert_eq!((r.0, r.1["code"].as_str()), (408, Some("body_timeout")), "{}", r.1);
    assert!(t0.elapsed() < Duration::from_secs(5));
    Ok(())
}

/// A handler that panics answers 500 (a problem document, the security headers) and the call is still audited.
#[tokio::test(flavor = "multi_thread")]
async fn f8_a_panicking_handler_is_a_500_and_is_audited() -> R {
    let f = hub()?.setup()?;
    let app = app_with_panic_route(&f, Duration::from_secs(5));
    let key = f.admin_key()?;
    let (status, body) = send(&app, "/api/v1/zz-panic", &key, Body::empty()).await?;
    assert_eq!((status, body["code"].as_str()), (500, Some("internal_error")), "{body}");
    let page = f.hub.audit_svc.list_page(50, None).map_err(|p| p.detail)?;
    let rows = page["items"].as_array().ok_or("no audit items")?;
    assert!(
        rows.iter()
            .any(|r| r["path"] == "/api/v1/zz-panic" && r["status"] == 500),
        "no audit row: {rows:?}"
    );
    Ok(())
}

async fn serve_on_ephemeral(
    f: &Fx,
    limits: ServeLimits,
) -> Result<
    (
        u16,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ),
    E,
> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?; // never a live port
    let port = listener.local_addr()?.port();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let app = f.api();
    let h = tokio::spawn(serve_with(app, listener, limits, async move {
        let _ = rx.await;
    }));
    Ok((port, tx, h))
}

async fn read_until_eof(s: &mut tokio::net::TcpStream, within: Duration) -> Result<Vec<u8>, E> {
    use tokio::io::AsyncReadExt as _;
    let mut out = Vec::new();
    tokio::time::timeout(within, s.read_to_end(&mut out))
        .await
        .map_err(|_| "the server kept the connection open")??;
    Ok(out)
}

fn health_request(port: u16) -> Vec<u8> {
    format!("GET /api/v1/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").into_bytes()
}

/// A connection that sends nothing, or stays idle after a response (keep-alive), is closed after the header timeout;
/// shutdown returns.
#[tokio::test(flavor = "multi_thread")]
async fn f8_idle_and_slow_header_connections_are_closed() -> R {
    use tokio::io::AsyncWriteExt as _;
    let f = hub()?.setup()?;
    let limits = ServeLimits {
        header_read_timeout: Duration::from_millis(400),
        ..ServeLimits::default()
    };
    let (port, stop, served) = serve_on_ephemeral(&f, limits).await?;
    let mut silent = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    assert!(read_until_eof(&mut silent, Duration::from_secs(5)).await?.is_empty());
    let mut partial = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    partial
        .write_all(b"GET /api/v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\n")
        .await?; // never finishes its headers
    let _ = read_until_eof(&mut partial, Duration::from_secs(5)).await?;
    let mut kept = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    kept.write_all(&health_request(port)).await?;
    let got = read_until_eof(&mut kept, Duration::from_secs(5)).await?; // answered, then closed when idle
    assert!(
        String::from_utf8_lossy(&got).starts_with("HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&got)
    );
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(15), served).await???;
    Ok(())
}

/// At most `max_connections` are served at once: the next one waits until a slot is free.
#[tokio::test(flavor = "multi_thread")]
async fn f8_connections_are_capped() -> R {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let f = hub()?.setup()?;
    let limits = ServeLimits {
        header_read_timeout: Duration::from_secs(20),
        max_connections: 1,
        shutdown_grace: Duration::from_secs(1),
    };
    let (port, stop, served) = serve_on_ephemeral(&f, limits).await?;
    let holder = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?; // takes the only slot, sends nothing
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut waiting = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?; // queued in the backlog
    waiting.write_all(&health_request(port)).await?;
    let mut buf = [0u8; 64];
    assert!(
        tokio::time::timeout(Duration::from_millis(600), waiting.read(&mut buf))
            .await
            .is_err(),
        "a second connection was served while the cap was reached"
    );
    drop(holder); // frees the slot
    let n = tokio::time::timeout(Duration::from_secs(5), waiting.read(&mut buf))
        .await
        .map_err(|_| "never served after the slot was freed")??;
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(15), served).await???;
    Ok(())
}
