//! Port of the WS-B2 rows of backend/tests/core/test_api.py (the end-to-end flow over HTTP, the repo-initialised gate,
//! the client's lock/verify record, preview plans, a real server process on a custom port). CORE's rows of the same
//! Python file live in tests/api_security.rs.

mod b2_fakes;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use b2_fakes::*;
use serde_json::json;

#[tokio::test]
async fn test_end_to_end_over_http() -> R {
    let f = hub()?;
    let app = f.api();
    let k = f.admin_key()?;
    let key = Some(k.as_str());
    let call = |m: &'static str, p: String, b: Option<serde_json::Value>| {
        let app = app.clone();
        let k = k.clone();
        async move { req(&app, m, &p, Some(&k), b).await }
    };
    let home = s(&f.home);
    assert_eq!(
        call(
            "POST",
            format!("{API}/clients"),
            Some(json!({"id": "fake1", "adapter": "fake", "display_name": "F", "roots": {"home": home}}))
        )
        .await
        .status,
        201
    );
    assert_eq!(
        call(
            "POST",
            format!("{API}/clients"),
            Some(json!({"id": "bad", "adapter": "nope", "display_name": "F"}))
        )
        .await
        .status,
        422
    );
    let sk = call(
        "PUT",
        format!("{API}/skills/alpha"),
        Some(json!({"description": "A", "body": "hi\n", "files": {"r.md": "x"}, "tags": ["t"]})),
    )
    .await;
    assert_eq!(sk.json()["file_list"][0]["path"], "SKILL.md");
    assert_eq!(
        call(
            "POST",
            format!("{API}/skills/alpha/duplicate"),
            Some(json!({"new_name": "alpha2"}))
        )
        .await
        .status,
        201
    );
    assert_eq!(
        call(
            "PUT",
            format!("{API}/instructions/style"),
            Some(json!({"title": "S", "order": 1, "body": "x\n"}))
        )
        .await
        .status,
        200
    );
    assert_eq!(
        call(
            "POST",
            format!("{API}/collections"),
            Some(json!({"id": "core", "title": "Core", "members": [{"kind": "skill", "name": "alpha"}]}))
        )
        .await
        .status,
        201
    );
    let m = call(
        "POST",
        format!("{API}/collections/core/members"),
        Some(json!({"members": [{"kind": "instruction", "name": "style"}]})),
    )
    .await;
    assert_eq!(m.json()["members"][1]["name"], "style");
    assert_eq!(
        call(
            "PUT",
            format!("{API}/profiles/fake1"),
            Some(json!({"collections": ["core"]}))
        )
        .await
        .status,
        200
    );
    let mx = req(&app, "GET", &format!("{API}/matrix"), key, None).await.json();
    assert_eq!(mx["groups"][0]["rows"][0]["cells"]["fake1"]["state"], "via_collection");
    let t = call(
        "POST",
        format!("{API}/matrix/toggle"),
        Some(json!({"client": "fake1", "kind": "skill", "name": "alpha2", "enabled": true})),
    )
    .await;
    assert_eq!(t.json()["state"], "on");
    let b = call(
        "POST",
        format!("{API}/matrix/bulk"),
        Some(json!({"clients": ["fake1"], "items": [{"kind": "skill", "name": "alpha2"}], "enabled": false})),
    )
    .await;
    assert_eq!(b.json()["ok"], json!(true));
    let b = call(
        "POST",
        format!("{API}/items/bulk"),
        Some(json!({"items": [{"kind": "skill", "name": "alpha2"}], "add_tags": ["bulk"]})),
    )
    .await;
    assert_eq!(b.json()["ok"], json!(true));
    let ch = req(&app, "GET", &format!("{API}/changes"), key, None).await.json();
    assert!(ch["count"].as_i64().is_some_and(|n| n > 5));
    assert!(
        ch["items"]
            .as_array()
            .ok_or("items")?
            .iter()
            .any(|i| i["path"] == "skills/alpha/SKILL.md")
    );
    assert_eq!(call("POST", format!("{API}/plan"), Some(json!({}))).await.status, 200); // plans the (empty) committed state
    assert_eq!(
        call(
            "POST",
            format!("{API}/changes/commit"),
            Some(json!({"message": "first"}))
        )
        .await
        .status,
        200
    );
    assert_eq!(
        call(
            "POST",
            format!("{API}/changes/commit"),
            Some(json!({"message": "again"}))
        )
        .await
        .status,
        409
    );
    let plan = call("POST", format!("{API}/plan"), Some(json!({"clients": ["fake1"]})))
        .await
        .json();
    assert!(plan["clients"][0]["summary"]["add"].as_i64().is_some_and(|n| n >= 3));
    let id = plan["id"].as_str().ok_or("plan id")?.to_string();
    assert_eq!(
        req(&app, "GET", &format!("{API}/plan/{id}"), key, None).await.json()["id"],
        json!(id)
    );
    assert_eq!(
        call("POST", format!("{API}/apply"), Some(json!({"plan_id": id})))
            .await
            .status,
        422
    ); // confirm required
    let res = call(
        "POST",
        format!("{API}/apply"),
        Some(json!({"plan_id": id, "confirm": true})),
    )
    .await
    .json();
    assert_eq!(res["results"][0]["status"], "applied");
    assert!(f.home.join("skills/alpha/SKILL.md").exists());
    let stale = call(
        "POST",
        format!("{API}/apply"),
        Some(json!({"plan_id": id, "confirm": true})),
    )
    .await;
    assert!(stale.status == 200 || stale.status == 409);
    assert_eq!(
        req(&app, "GET", &format!("{API}/clients/fake1/drift"), key, None)
            .await
            .json()["status"],
        "in_sync"
    );
    assert_eq!(
        call("POST", format!("{API}/clients/fake1/verify"), None).await.json()["ok"],
        json!(true)
    );
    assert_eq!(
        req(&app, "GET", &format!("{API}/clients/fake1/audit"), key, None)
            .await
            .status,
        200
    );
    let eff = req(&app, "GET", &format!("{API}/clients/fake1/effective"), key, None)
        .await
        .json();
    assert_eq!(eff["items"]["skills"][0]["name"], "alpha");
    assert_eq!(
        req(&app, "GET", &format!("{API}/clients"), key, None).await.json()["items"][0]["status"],
        "in_sync"
    );
    let hist = req(&app, "GET", &format!("{API}/changes/history"), key, None)
        .await
        .json()["items"]
        .clone();
    assert_eq!(hist[0]["message"], "first");
    let first = hist[0]["commit"].clone();
    // a stale plan after new content: 409 plan_stale
    call(
        "PUT",
        format!("{API}/instructions/style"),
        Some(json!({"title": "S", "order": 1, "body": "y\n"})),
    )
    .await;
    call(
        "POST",
        format!("{API}/changes/commit"),
        Some(json!({"message": "second"})),
    )
    .await;
    let r = call(
        "POST",
        format!("{API}/apply"),
        Some(json!({"plan_id": id, "confirm": true})),
    )
    .await;
    assert_eq!(r.code(), "plan_stale");
    let second = req(&app, "GET", &format!("{API}/changes/history"), key, None)
        .await
        .json()["items"][0]
        .clone();
    assert!(second["message"] == "second" && second["commit"] != first);
    assert_eq!(
        call(
            "POST",
            format!("{API}/changes/revert"),
            Some(json!({"commit": second["commit"]}))
        )
        .await
        .status,
        200
    );
    call(
        "PUT",
        format!("{API}/instructions/dirty"),
        Some(json!({"title": "S", "body": "y\n"})),
    )
    .await;
    let d = call(
        "POST",
        format!("{API}/changes/discard"),
        Some(json!({"paths": ["instructions/dirty.md"]})),
    )
    .await;
    assert_eq!(d.json()["discarded"], json!(["instructions/dirty.md"]));
    assert_eq!(
        req(&app, "DELETE", &format!("{API}/skills/alpha2"), key, None)
            .await
            .status,
        204
    );
    assert_eq!(
        req(&app, "DELETE", &format!("{API}/clients/fake1"), key, None)
            .await
            .status,
        204
    );
    Ok(())
}

#[tokio::test]
async fn test_content_requires_initialised_repo() -> R {
    let f = hub_with(Vec::new(), false)?; // note: no init_content()
    let app = f.api();
    let k = f.admin_key()?;
    assert_eq!(
        req(&app, "GET", &format!("{API}/health"), None, None).await.json()["content_initialised"],
        json!(false)
    );
    let r = req(
        &app,
        "PUT",
        &format!("{API}/instructions/x"),
        Some(&k),
        Some(json!({"title": "t", "body": "b"})),
    )
    .await;
    assert_eq!((r.status, r.code()), (503, "content_not_initialised".to_string()));
    assert_eq!(
        req(&app, "GET", &format!("{API}/skills"), Some(&k), None).await.status,
        200
    );
    Ok(())
}

#[tokio::test]
async fn test_client_lock_info_and_verify_record() -> R {
    let f = hub()?;
    let app = f.api();
    let f = f.setup()?;
    let k = f.admin_key()?;
    let key = Some(k.as_str());
    let c = req(&app, "GET", &format!("{API}/clients/fake1"), key, None)
        .await
        .json();
    assert!(c["last_applied_at"].is_null() && c["last_verified_ok"].is_null());
    let plan = req(&app, "POST", &format!("{API}/plan"), key, Some(json!({})))
        .await
        .json();
    req(
        &app,
        "POST",
        &format!("{API}/apply"),
        key,
        Some(json!({"plan_id": plan["id"], "confirm": true})),
    )
    .await;
    let c = req(&app, "GET", &format!("{API}/clients/fake1"), key, None)
        .await
        .json();
    assert_eq!(c["last_applied_plan"], plan["id"]);
    assert!(c["last_applied_at"].is_f64() && c["last_verified_ok"] == json!(true));
    std::fs::write(f.home.join("agents/reviewer.md"), "tampered")?;
    req(&app, "POST", &format!("{API}/clients/fake1/verify"), key, None).await;
    let c = req(&app, "GET", &format!("{API}/clients"), key, None).await.json()["items"][0].clone();
    assert!(c["last_verified_ok"] == json!(false) && c["last_applied_plan"] == plan["id"]);
    Ok(())
}

#[tokio::test]
async fn test_preview_plan_cannot_be_applied() -> R {
    let f = hub()?;
    let app = f.api();
    let f = f.setup()?;
    let k = f.admin_key()?;
    let key = Some(k.as_str());
    f.put(
        "instruction",
        "style",
        json!({"title": "Style", "order": 10, "body": "PREVIEW ONLY\n"}),
    )?;
    let prev = req(
        &app,
        "POST",
        &format!("{API}/plan"),
        key,
        Some(json!({"source": "working"})),
    )
    .await
    .json();
    assert!(prev["preview"] == json!(true) && prev["content_hash"] == "preview");
    assert_ne!(f.hub.repo.head_tree()?, "preview");
    let diffs = |p: &serde_json::Value| -> String {
        p["clients"][0]["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x["diff"].as_str())
            .collect()
    };
    assert!(diffs(&prev).contains("PREVIEW ONLY"));
    let r = req(
        &app,
        "POST",
        &format!("{API}/apply"),
        key,
        Some(json!({"plan_id": prev["id"], "confirm": true})),
    )
    .await;
    assert_eq!((r.status, r.code()), (409, "plan_is_preview".to_string()));
    assert!(!f.home.join("CLAUDE.md").exists());
    let committed = req(&app, "POST", &format!("{API}/plan"), key, Some(json!({})))
        .await
        .json();
    assert!(committed["preview"] == json!(false) && !diffs(&committed).contains("PREVIEW ONLY"));
    assert_eq!(
        req(
            &app,
            "POST",
            &format!("{API}/plan"),
            key,
            Some(json!({"source": "nope"}))
        )
        .await
        .status,
        400
    );
    let id = prev["id"].as_str().unwrap_or_default();
    assert_eq!(
        req(&app, "GET", &format!("{API}/plan/{id}"), key, None).await.json()["preview"],
        json!(true)
    );
    Ok(())
}

fn free_port() -> Result<u16, E> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port()) // never a live port
}

/// GET /api/v1/health from a real server process (plain HTTP/1.1 over a socket; no client crate needed).
fn health(port: u16) -> Option<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let r = format!("GET /api/v1/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    s.write_all(r.as_bytes()).ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    Some(out)
}

fn serve_and_probe(bin: &str, extra: &[String], port: u16) -> R {
    let d = tempfile::tempdir()?;
    let mut child = std::process::Command::new(bin)
        .args(extra)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", s(&d.path().join("home")))
        .env("HUB_CONTENT_DIR", s(&d.path().join("c")))
        .env("HUB_DATA_DIR", s(&d.path().join("s")))
        .env("AGENT_HUB_ENV_FILE", s(&d.path().join("none")))
        .env("HUB_PORT", port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let mut body = None;
    for _ in 0..60 {
        if let Some(b) = health(port) {
            body = Some(b);
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = child.kill();
    let _ = child.wait();
    let body = body.ok_or("the server never answered")?;
    assert!(
        body.starts_with("HTTP/1.1 200") && body.contains("\"status\":\"ok\""),
        "{body}"
    );
    Ok(())
}

/// Python starts `python -m agent_hub.cli serve` with HUB_PORT, then `serve --port N`; the Rust CLI is `hubctl serve`.
#[test]
fn test_real_subprocess_serves_on_a_custom_port() -> R {
    let port = free_port()?;
    for extra in [Vec::new(), vec!["--port".to_string(), port.to_string()]] {
        let mut args = vec!["serve".to_string()];
        args.extend(extra);
        serve_and_probe(env!("CARGO_BIN_EXE_hubctl"), &args, port)?;
    }
    Ok(())
}

/// The same probe against the `agent-hub` server binary (CORE's entry point; HUB_PORT): runs now, and proves the
/// health route answers through gitrepo on a fresh, uninitialised content dir.
#[test]
fn real_agent_hub_binary_serves_health_on_a_custom_port() -> R {
    serve_and_probe(env!("CARGO_BIN_EXE_agent-hub"), &[], free_port()?)
}
