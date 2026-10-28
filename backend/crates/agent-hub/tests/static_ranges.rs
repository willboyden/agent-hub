//! Range requests on the static frontend, answered like Starlette's FileResponse (PORTING.md D-RANGE, now ported):
//! the parser replays golden cases generated from Starlette itself (backend/parity/gen/gen_range_golden.py), and the
//! HTTP test checks the 206 / multipart / 400 / 416 / If-Range / 304 answers through the whole pipeline.

use std::net::SocketAddr;
use std::sync::Arc;

use agent_hub::api::static_files::{RangeOutcome, multipart_body, parse_range_header};
use agent_hub::api::{AppState, security};
use agent_hub::config::load_settings_from;
use agent_hub::server::{create_app, on_startup};
use agent_hub::services::hub::{Hub, HubOptions};
use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use hub_core::value::PyValue;
use serde_json::Value;

type R = Result<(), Box<dyn std::error::Error>>;

#[test]
fn range_header_parsing_matches_starlette() -> R {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/ranges.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let cases = doc["cases"].as_array().ok_or("no cases")?;
    assert!(cases.len() >= 100);
    for c in cases {
        let header = c["header"].as_str().ok_or("header")?;
        let size = c["size"].as_u64().ok_or("size")?;
        let want = if let Some(m) = c.get("malformed") {
            format!("malformed {}", m.as_str().unwrap_or_default())
        } else if c.get("unsatisfiable").is_some() {
            "unsatisfiable".to_string()
        } else {
            let r: Vec<(u64, u64)> = c["ranges"]
                .as_array()
                .ok_or("ranges")?
                .iter()
                .map(|p| (p[0].as_u64().unwrap_or(u64::MAX), p[1].as_u64().unwrap_or(u64::MAX)))
                .collect();
            format!("ranges {r:?}")
        };
        let got = match parse_range_header(header, size) {
            RangeOutcome::Malformed(m) => format!("malformed {m}"),
            RangeOutcome::Unsatisfiable => "unsatisfiable".to_string(),
            RangeOutcome::Ranges(r) => format!("ranges {r:?}"),
        };
        assert_eq!(got, want, "Range {header:?} on a {size}-byte file");
    }
    Ok(())
}

#[test]
fn multipart_body_matches_starlettes_layout_and_length() {
    let body = multipart_body(b"0123456789", &[(0, 2), (5, 7)], "b0undary", 10, "text/plain");
    let want = "--b0undary\r\nContent-Type: text/plain\r\nContent-Range: bytes 0-1/10\r\n\r\n01\r\n\
                --b0undary\r\nContent-Type: text/plain\r\nContent-Range: bytes 5-6/10\r\n\r\n56\r\n--b0undary--";
    assert_eq!(String::from_utf8_lossy(&body), want);
    // Starlette's generate_multipart length: per range 49 + len(boundary) + len(ctype) + len(str(size)) + digits + data
    let per = |s: usize, e: usize| 49 + 8 + 10 + 2 + s.to_string().len() + (e - 1).to_string().len() + (e - s);
    assert_eq!(body.len(), per(0, 2) + per(5, 7) + 4 + 8);
}

struct Resp {
    status: u16,
    headers: http::HeaderMap,
    body: Vec<u8>,
}

impl Resp {
    fn h(&self, k: &str) -> String {
        self.headers
            .get(k)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .unwrap_or_default()
    }
}

async fn get(app: &Arc<AppState>, path: &str, headers: &[(&str, &str)]) -> Resp {
    let mut b = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "127.0.0.1:8792");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let r = b.body(Body::empty()).unwrap_or_default();
    let resp = security::handle(app.clone(), SocketAddr::from(([127, 0, 0, 1], 5000)), 8792, r).await;
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.map(|c| c.to_bytes().to_vec()).unwrap_or_default();
    Resp {
        status: parts.status.as_u16(),
        headers: parts.headers,
        body: bytes,
    }
}

#[tokio::test]
async fn range_requests_are_answered_like_starlette() -> R {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    let fe = root.join("frontend");
    std::fs::create_dir_all(fe.join("js"))?;
    std::fs::write(fe.join("index.html"), "<html></html>")?;
    std::fs::write(fe.join("js/app.js"), "0123456789")?;
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
            ("frontend_dir".into(), PyValue::str(fe.to_string_lossy())),
        ],
    )?;
    let hub = Hub::new(cfg, HubOptions::default()).map_err(|p| p.detail)?;
    on_startup(&hub);
    let app = create_app(hub);

    let full = get(&app, "/js/app.js", &[]).await;
    assert_eq!((full.status, full.body.as_slice()), (200, b"0123456789".as_slice()));
    let etag = full.h("etag");
    let lm = full.h("last-modified");

    let one = get(&app, "/js/app.js", &[("range", "bytes=2-4")]).await;
    assert_eq!((one.status, one.body.as_slice()), (206, b"234".as_slice()));
    assert_eq!(one.h("content-range"), "bytes 2-4/10");
    assert_eq!(one.h("content-length"), "3");
    assert_eq!(one.h("content-type"), "text/javascript; charset=utf-8");
    assert_eq!(
        (one.h("etag"), one.h("cache-control")),
        (etag.clone(), "no-cache".to_string())
    );
    assert_eq!(one.h("accept-ranges"), "bytes");

    let many = get(&app, "/js/app.js", &[("range", "bytes=5-6,0-1")]).await;
    assert_eq!(many.status, 206);
    let ctype = many.h("content-type");
    let boundary = ctype
        .strip_prefix("multipart/byteranges; boundary=")
        .ok_or("multipart content type")?
        .to_string();
    assert!(boundary.len() == 26 && boundary.chars().all(|c| c.is_ascii_hexdigit()));
    let want = multipart_body(
        b"0123456789",
        &[(0, 2), (5, 7)],
        &boundary,
        10,
        "text/javascript; charset=utf-8",
    );
    assert_eq!(many.body, want);
    assert_eq!(many.h("content-length"), want.len().to_string());
    assert_eq!(many.h("content-range"), "");

    let past = get(&app, "/js/app.js", &[("range", "bytes=20-")]).await;
    assert_eq!((past.status, past.body.len()), (416, 0));
    assert_eq!(past.h("content-range"), "bytes */10");
    assert_eq!(past.h("content-type"), "text/plain; charset=utf-8");

    let bad = get(&app, "/js/app.js", &[("range", "bytes=x")]).await;
    assert_eq!(bad.status, 400);
    assert_eq!(
        String::from_utf8_lossy(&bad.body),
        "Range header: range must be requested"
    );
    let units = get(&app, "/js/app.js", &[("range", "items=0-1")]).await;
    assert_eq!(
        (units.status, String::from_utf8_lossy(&units.body).into_owned()),
        (400, "Only support bytes range".into())
    );

    let stale = get(&app, "/js/app.js", &[("range", "bytes=2-4"), ("if-range", "\"stale\"")]).await;
    assert_eq!((stale.status, stale.body.len()), (200, 10));
    for fresh in [etag.as_str(), lm.as_str()] {
        let r = get(&app, "/js/app.js", &[("range", "bytes=2-4"), ("if-range", fresh)]).await;
        assert_eq!(r.status, 206, "If-Range {fresh}");
    }
    let cached = get(
        &app,
        "/js/app.js",
        &[("range", "bytes=2-4"), ("if-none-match", etag.as_str())],
    )
    .await;
    assert_eq!(cached.status, 304);
    let too_many = vec!["0-1"; 101].join(",");
    let r = get(&app, "/js/app.js", &[("range", &format!("bytes={too_many}"))]).await;
    assert_eq!((r.status, r.body.len()), (200, 10));
    Ok(())
}
