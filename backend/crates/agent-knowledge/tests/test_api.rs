//! Port of knowledge/tests/test_api.py (+ its conftest): the HTTP surface through the full in-process pipeline
//! (`app::handle`: precheck, routing, body decoding, dependencies, handlers, headers, metrics, audit) with in-memory
//! stores and the hash embedder, like the Python fixture. Host header `127.0.0.1:8795` = the configured port.

#![cfg(test)]

use std::convert::Infallible;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_knowledge::app::{self, App, Options, ROUTES};
use agent_knowledge::config::Config;
use agent_knowledge::embed::HashEmbedder;
use agent_knowledge::env;
use agent_knowledge::filters::Node;
use agent_knowledge::stores::memory::MemoryStore;
use agent_knowledge::stores::{Hit, Record, StoreResult, StoreStats, Stores, VectorStore};
use axum::body::Body as AxBody;
use bytes::Bytes;
use futures::future::BoxFuture;
use hub_core::PyValue;
use serde_json::{Value, json};

const HOST: &str = "127.0.0.1:8795";

struct R {
    status: u16,
    headers: http::HeaderMap,
    body: Bytes,
}

impl R {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn code(&self) -> String {
        self.json()["code"].as_str().unwrap_or_default().to_string()
    }
    fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
    }
}

struct Hub {
    app: Arc<App>,
    admin: String,
    data: PathBuf,
    root: PathBuf,
    tmp: tempfile::TempDir,
}

fn memory_stores() -> Stores {
    vec![
        ("lancedb".into(), Arc::new(MemoryStore::new())),
        ("qdrant".into(), Arc::new(MemoryStore::new())),
    ]
}

impl Hub {
    fn new() -> Self {
        Self::with(|_| {}, memory_stores())
    }

    fn with(adjust: impl FnOnce(&mut Config), stores: Stores) -> Self {
        let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap_or_else(|e| panic!("{e}"));
        let mut cfg = Config::with_data_dir(&tmp.path().join("data"));
        cfg.ingest_roots = vec![root.clone()];
        cfg.index_roots = vec![root.clone()];
        cfg.router_url = "http://127.0.0.1:9".into(); // never used: the hash embedder is injected
        adjust(&mut cfg);
        let data = cfg.data_dir.clone();
        let opts = Options {
            stores: Some(stores),
            embedder: Some(Arc::new(HashEmbedder::new(32))),
            env: Some(env::from_pairs(&[])),
        };
        let app = app::create_app_with(cfg, opts).unwrap_or_else(|e| panic!("{e}"));
        let admin = std::fs::read_to_string(data.join("admin.key"))
            .unwrap_or_default()
            .trim()
            .to_string();
        Self {
            app,
            admin,
            data,
            root,
            tmp,
        }
    }

    async fn raw(&self, method: &str, path: &str, headers: &[(&str, &str)], body: AxBody) -> R {
        let mut b = http::Request::builder().method(method).uri(path);
        let mut has_host = false;
        for (k, v) in headers {
            has_host |= k.eq_ignore_ascii_case("host");
            b = b.header(*k, *v);
        }
        if !has_host {
            b = b.header("host", HOST);
        }
        let req = b.body(body).unwrap_or_else(|e| panic!("{e}"));
        let resp = app::handle(self.app.clone(), req).await;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        R { status, headers, body }
    }

    /// `hub.call(method, path, tok, json=...)`: bearer (admin unless given), a JSON body when given.
    async fn call(&self, method: &str, path: &str, tok: Option<&str>, body: Option<Value>) -> R {
        let auth = format!("Bearer {}", tok.unwrap_or(&self.admin));
        match body {
            Some(v) => {
                let bytes = serde_json::to_vec(&v).unwrap_or_default();
                let len = bytes.len().to_string();
                let h = [
                    ("authorization", auth.as_str()),
                    ("content-type", "application/json"),
                    ("content-length", len.as_str()),
                ];
                self.raw(method, path, &h, AxBody::from(bytes)).await
            }
            None => {
                self.raw(method, path, &[("authorization", auth.as_str())], AxBody::empty())
                    .await
            }
        }
    }

    async fn mk_ns(&self, name: &str, backend: &str, dim: i64) {
        let r = self
            .call(
                "POST",
                "/namespaces",
                None,
                Some(json!({"name": name, "backend": backend, "dim": dim})),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.text());
    }

    async fn mk_token(&self, scopes: Value, extra: Value) -> String {
        let mut body = json!({"client": "tester", "scopes": scopes});
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                b.insert(k.clone(), v.clone());
            }
        }
        let r = self.call("POST", "/tokens", None, Some(body)).await;
        assert_eq!(r.status, 201, "{}", r.text());
        r.json()["token"].as_str().unwrap_or_default().to_string()
    }

    async fn wait_job(&self, jid: &str, tok: Option<&str>) -> Value {
        for _ in 0..200 {
            let j = self.call("GET", &format!("/jobs/{jid}"), tok, None).await.json();
            if j["state"] == "done" || j["state"] == "failed" {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("job did not finish");
    }

    async fn ingest(&self, ns: &str, docs: Value, tok: Option<&str>) -> Value {
        let r = self
            .call(
                "POST",
                &format!("/namespaces/{ns}/ingest"),
                tok,
                Some(json!({"documents": docs})),
            )
            .await;
        assert_eq!(r.status, 202, "{}", r.text());
        let jid = r.json()["job"]["id"].as_str().unwrap_or_default().to_string();
        self.wait_job(&jid, tok).await
    }
}

fn docs() -> Value {
    json!([
        {"id": "d1", "text": "the quick brown fox jumps over the lazy dog", "metadata": {"lang": "en", "n": 1}},
        {"id": "d2", "text": "vector databases store embeddings for retrieval", "metadata": {"lang": "en", "n": 2}},
        {"id": "d3", "text": "le renard brun rapide saute", "metadata": {"lang": "fr", "n": 3}}
    ])
}

/// The route table minus /health with Python's placeholder substitution.
fn routes_and_urls() -> Vec<(&'static str, &'static str, String)> {
    ROUTES
        .iter()
        .filter(|(_, p, _, _)| *p != "/health")
        .flat_map(|(methods, p, _, _)| {
            let url = p
                .replace("{ns}", "docs")
                .replace("{jid}", "x")
                .replace("{tid}", "x")
                .replace("{name}", "x")
                .replace("{doc_id:path}", "x");
            methods.iter().map(move |m| (*m, *p, url.clone()))
        })
        .collect()
}

#[tokio::test]
async fn test_health_open_everything_else_needs_auth() {
    let hub = Hub::new();
    assert_eq!(hub.raw("GET", "/health", &[], AxBody::empty()).await.status, 200);
    let routes = routes_and_urls();
    assert_eq!(routes.len(), 19);
    for (m, p, url) in &routes {
        let h = [
            ("authorization", "Bearer nope"),
            ("content-type", "application/json"),
            ("content-length", "2"),
        ];
        let r = hub.raw(m, url, &h, AxBody::from("{}")).await;
        assert_eq!(r.status, 401, "{m} {p}");
        let h = [
            ("x-agent-knowledge", "1"),
            ("content-type", "application/json"),
            ("content-length", "2"),
        ];
        assert_eq!(hub.raw(m, url, &h, AxBody::from("{}")).await.status, 401, "{m} {p}");
    }
}

#[tokio::test]
async fn test_scopes_default_read_only_and_enforced() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    hub.mk_ns("secret", "lancedb", 32).await;
    hub.ingest("docs", docs(), None).await;
    let ro = hub.mk_token(json!([{"ns": "docs"}]), json!({})).await; // mode omitted -> read
    let listing = hub.call("GET", "/tokens", None, None).await.json();
    assert_eq!(listing["items"][0]["scopes"], json!([{"ns": "docs", "mode": "read"}]));
    let q = |t: &str| json!({"text": t});
    assert_eq!(
        hub.call("POST", "/namespaces/docs/query", Some(&ro), Some(q("fox")))
            .await
            .status,
        200
    );
    // read token cannot write, delete, or touch other namespaces
    let r = hub
        .call(
            "POST",
            "/namespaces/docs/ingest",
            Some(&ro),
            Some(json!({"documents": [{"text": "x"}]})),
        )
        .await;
    assert_eq!((r.status, r.code().as_str()), (403, "forbidden"));
    assert_eq!(
        hub.call("DELETE", "/namespaces/docs/documents/d1", Some(&ro), None)
            .await
            .status,
        403
    );
    // indistinguishable from a namespace that does not exist
    assert_eq!(
        hub.call("POST", "/namespaces/secret/query", Some(&ro), Some(q("fox")))
            .await
            .status,
        404
    );
    assert_eq!(
        hub.call("POST", "/namespaces/nonexistent/query", Some(&ro), Some(q("fox")))
            .await
            .status,
        404
    );
    assert_eq!(hub.call("GET", "/namespaces/secret", Some(&ro), None).await.status, 404);
    let names = hub.call("GET", "/namespaces", Some(&ro), None).await.json();
    assert_eq!(
        names["items"]
            .as_array()
            .map(|a| a.iter().map(|n| n["name"].clone()).collect::<Vec<_>>()),
        Some(vec![json!("docs")])
    );
    // admin-only surfaces
    for (m, p, body) in [
        ("GET", "/tokens", json!({})),
        (
            "POST",
            "/namespaces",
            json!({"name": "zz", "backend": "lancedb", "dim": 4}),
        ),
        ("GET", "/backends", json!({})),
        ("GET", "/metrics", json!({})),
        ("DELETE", "/namespaces/docs", json!({})),
        (
            "POST",
            "/indexes",
            json!({"name": "zz", "kind": "graphify", "path": "/tmp"}),
        ),
        ("PUT", "/namespaces/docs", json!({})),
    ] {
        assert_eq!(hub.call(m, p, Some(&ro), Some(body)).await.status, 403, "{m} {p}");
    }
    // write token may ingest inline but never `path`
    let rw = hub.mk_token(json!([{"ns": "docs", "mode": "write"}]), json!({})).await;
    assert_eq!(
        hub.ingest("docs", json!([{"id": "d9", "text": "hello there"}]), Some(&rw))
            .await["state"],
        "done"
    );
    let r = hub
        .call(
            "POST",
            "/namespaces/docs/ingest",
            Some(&rw),
            Some(json!({"path": "/tmp", "glob": "*.md"})),
        )
        .await;
    assert_eq!(r.status, 403);
    let r = hub
        .call("DELETE", "/namespaces/docs/documents/d9", Some(&rw), None)
        .await;
    assert_eq!(r.json()["deleted_chunks"], 1);
}

#[tokio::test]
async fn test_jobs_visible_only_with_write_scope() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    let job = hub.ingest("docs", docs(), None).await;
    let ro = hub.mk_token(json!([{"ns": "docs"}]), json!({})).await;
    let jid = job["id"].as_str().unwrap_or_default();
    assert_eq!(
        hub.call("GET", &format!("/jobs/{jid}"), Some(&ro), None).await.status,
        404
    );
    assert_eq!(
        hub.call("GET", "/jobs", Some(&ro), None).await.json()["items"],
        json!([])
    );
    assert_eq!(
        hub.call("GET", "/jobs", None, None).await.json()["items"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
}

#[tokio::test]
async fn test_query_filters_min_score_and_k() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    hub.ingest("docs", docs(), None).await;
    let p = "/namespaces/docs/query";
    let r = hub
        .call(
            "POST",
            p,
            None,
            Some(json!({"text": "quick fox", "k": 3, "filter": {"lang": "fr"}})),
        )
        .await
        .json();
    let ids: Vec<Value> = r["hits"]
        .as_array()
        .map(|a| a.iter().map(|h| h["metadata"]["doc_id"].clone()).collect())
        .unwrap_or_default();
    assert_eq!(ids, vec![json!("d3")]);
    let r = hub
        .call("POST", p, None, Some(json!({"text": "quick brown fox", "k": 1})))
        .await
        .json();
    assert_eq!(r["hits"][0]["id"], "d1#0");
    let r = hub
        .call(
            "POST",
            p,
            None,
            Some(json!({"text": "fox", "min_score": 0.999999, "filter": {"n": {"$gt": 100}}})),
        )
        .await;
    assert_eq!(r.json()["hits"], json!([]));
    for bad in [
        json!({"text": "x", "filter": {"a; drop": 1}}),
        json!({"text": "x", "k": 0}),
        json!({"text": "x", "k": 10_000}),
        json!({"text": "x", "vector": vec![0.0; 32]}),
        json!({}),
        json!({"text": "x", "unknown": 1}),
    ] {
        assert_eq!(hub.call("POST", p, None, Some(bad.clone())).await.status, 422, "{bad}");
    }
}

#[tokio::test]
async fn test_dim_and_model_mismatch_refused() {
    let hub = Hub::new();
    hub.mk_ns("wrongdim", "lancedb", 8).await; // the hash embedder emits 32-d
    let job = hub.ingest("wrongdim", docs(), None).await;
    assert_eq!(
        (job["state"].clone(), job["error_code"].clone()),
        (json!("failed"), json!("dim_mismatch"))
    );
    let r = hub
        .call("POST", "/namespaces/wrongdim/query", None, Some(json!({"text": "fox"})))
        .await;
    assert_eq!((r.status, r.code().as_str()), (409, "dim_mismatch"));
    let r = hub
        .call(
            "POST",
            "/namespaces/wrongdim/query",
            None,
            Some(json!({"vector": vec![0.1; 32]})),
        )
        .await;
    assert_eq!(r.status, 409);
    hub.mk_ns("ok", "lancedb", 32).await;
    let r = hub
        .call(
            "POST",
            "/namespaces/ok/query",
            None,
            Some(json!({"text": "fox", "embedding_model": "other-model"})),
        )
        .await;
    assert_eq!((r.status, r.code().as_str()), (409, "model_mismatch"));
    let r = hub.call("PUT", "/namespaces/ok", None, Some(json!({"dim": 64}))).await;
    assert_eq!((r.status, r.code().as_str()), (409, "immutable_field"));
    let r = hub
        .call("PUT", "/namespaces/ok", None, Some(json!({"description": "new"})))
        .await;
    assert_eq!(r.json()["description"], "new");
}

#[tokio::test]
async fn test_quota_and_rate_limit() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    hub.ingest("docs", docs(), None).await;
    let t = hub.mk_token(json!([{"ns": "docs"}]), json!({"daily_queries": 2})).await;
    let mut codes = Vec::new();
    for _ in 0..3 {
        codes.push(
            hub.call("POST", "/namespaces/docs/query", Some(&t), Some(json!({"text": "fox"})))
                .await
                .status,
        );
    }
    assert_eq!(codes, vec![200, 200, 429]);
    let w = hub
        .mk_token(json!([{"ns": "docs", "mode": "write"}]), json!({"daily_writes": 2}))
        .await;
    let body = json!({"documents": [{"text": "a"}, {"text": "b"}, {"text": "c"}]});
    let r = hub.call("POST", "/namespaces/docs/ingest", Some(&w), Some(body)).await;
    assert_eq!((r.status, r.code().as_str()), (429, "quota_exceeded"));
    assert_eq!(r.header("retry-after").as_deref(), Some("30"));
    let r2 = hub.call("GET", "/tokens", None, None).await.json();
    assert_eq!(r2["items"][0]["usage_today"]["queries"], 2);
    let lim = hub.mk_token(json!([{"ns": "docs"}]), json!({"rate_per_min": 3})).await;
    let mut codes = Vec::new();
    for _ in 0..5 {
        codes.push(hub.call("GET", "/namespaces", Some(&lim), None).await.status);
    }
    assert_eq!(codes, vec![200, 200, 200, 429, 429]);
}

#[tokio::test]
async fn test_namespace_capacity_cap() {
    let hub = Hub::with(|c| c.max_chunks_per_namespace = 2, memory_stores());
    hub.mk_ns("docs", "lancedb", 32).await;
    let body = json!({"documents": [{"text": "a"}, {"text": "b"}, {"text": "c"}]});
    let r = hub.call("POST", "/namespaces/docs/ingest", None, Some(body)).await;
    assert_eq!((r.status, r.code().as_str()), (409, "namespace_full"));
}

#[tokio::test]
async fn test_job_lifecycle_and_sse() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    let r = hub
        .call(
            "POST",
            "/namespaces/docs/ingest",
            None,
            Some(json!({"documents": docs()})),
        )
        .await;
    let job = r.json()["job"].clone();
    assert!(["queued", "running", "done"].contains(&job["state"].as_str().unwrap_or_default()));
    let jid = job["id"].as_str().unwrap_or_default().to_string();
    let s = hub.call("GET", &format!("/jobs/{jid}/stream"), None, None).await;
    assert_eq!(
        s.header("content-type").as_deref(),
        Some("text/event-stream; charset=utf-8")
    );
    let body = s.text();
    assert!(
        body.contains("event: progress") && body.contains("event: end") && body.contains(r#""state": "done""#),
        "{body}"
    );
    let done = hub.wait_job(&jid, None).await;
    assert_eq!(
        (
            done["docs_total"].clone(),
            done["docs_done"].clone(),
            done["chunks"].clone()
        ),
        (json!(3), json!(3), json!(3))
    );
    assert_eq!(done["error"], Value::Null);
    assert_eq!(hub.call("GET", "/namespaces/docs", None, None).await.json()["count"], 3);
    assert_eq!(hub.call("GET", "/jobs/nope", None, None).await.status, 404);
    // the stream slot was given back
    assert_eq!(hub.app.ctx.streams.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_reingest_replaces_chunks_and_delete() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    let long = (0..400).map(|i| format!("word{i}")).collect::<Vec<_>>().join(" ");
    hub.ingest("docs", json!([{"id": "big", "text": long}]), None).await;
    let n1 = hub.call("GET", "/namespaces/docs", None, None).await.json()["count"]
        .as_i64()
        .unwrap_or(0);
    assert!(n1 > 1);
    hub.ingest("docs", json!([{"id": "big", "text": "tiny now"}]), None)
        .await;
    assert_eq!(hub.call("GET", "/namespaces/docs", None, None).await.json()["count"], 1);
    assert_eq!(
        hub.call("DELETE", "/namespaces/docs/documents/big", None, None)
            .await
            .json()["deleted_chunks"],
        1
    );
    assert_eq!(
        hub.call("DELETE", "/namespaces/docs/documents/bad%20id!", None, None)
            .await
            .status,
        422
    );
}

#[tokio::test]
async fn test_ingest_validation() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    for body in [
        json!({}),
        json!({"documents": [{"text": "a"}], "path": "/x"}),
        json!({"documents": []}),
        json!({"documents": [{"text": "a", "metadata": {"doc_id": "spoof"}}]}),
        json!({"documents": [{"text": "a", "metadata": {"bad key": 1}}]}),
        json!({"documents": [{"text": "a", "metadata": {"k": [1]}}]}),
        json!({"documents": [{"id": "../x", "text": "a"}]}),
        json!({"documents": [{"id": "a", "text": "x"}, {"id": "a", "text": "y"}]}),
        json!({"documents": [{"text": "a"}], "chunk_size": 10}),
    ] {
        assert_eq!(
            hub.call("POST", "/namespaces/docs/ingest", None, Some(body.clone()))
                .await
                .status,
            422,
            "{body}"
        );
    }
}

fn write(p: &Path, text: &[u8]) {
    std::fs::write(p, text).unwrap_or_else(|e| panic!("{e}"));
}

#[tokio::test]
async fn test_path_ingest_allowlist_traversal_and_symlinks() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    let root = hub.root.clone();
    std::fs::create_dir(root.join("sub")).unwrap_or_else(|e| panic!("{e}"));
    write(&root.join("sub/a.md"), b"alpha document about foxes");
    write(&root.join("b.txt"), b"beta document");
    write(&root.join(".env"), b"TOKEN=hunter2");
    write(&root.join("notes.env.txt"), b"fine");
    write(&root.join("img.bin"), b"\x00\x01");
    let outside = hub.tmp.path().join("outside");
    std::fs::create_dir(&outside).unwrap_or_else(|e| panic!("{e}"));
    write(&outside.join("leak.md"), b"OUTSIDE SECRET");
    std::os::unix::fs::symlink(outside.join("leak.md"), root.join("link.md")).unwrap_or_else(|e| panic!("{e}"));
    std::os::unix::fs::symlink(&outside, root.join("linkdir")).unwrap_or_else(|e| panic!("{e}"));
    let go = |path: String, glob: &str| json!({"path": path, "glob": glob});
    let p = "/namespaces/docs/ingest";
    // escapes are refused up front
    for bad in [
        outside.to_string_lossy().into_owned(),
        format!("{}/../outside", root.display()),
        "/etc".into(),
        "relative/path".into(),
        root.join("linkdir").to_string_lossy().into_owned(),
        "/".into(),
    ] {
        let st = hub.call("POST", p, None, Some(go(bad.clone(), "**/*"))).await.status;
        assert!(st == 403 || st == 422, "{bad} -> {st}");
    }
    for bad_glob in ["../*", "/etc/*", "**/../../*", "a b", "*;rm"] {
        let st = hub
            .call("POST", p, None, Some(go(root.to_string_lossy().into_owned(), bad_glob)))
            .await
            .status;
        assert_eq!(st, 422, "{bad_glob}");
    }
    assert_eq!(
        hub.call(
            "POST",
            p,
            None,
            Some(go(root.join("missing").to_string_lossy().into_owned(), "**/*"))
        )
        .await
        .status,
        422
    );
    let r = hub
        .call("POST", p, None, Some(go(root.to_string_lossy().into_owned(), "**/*")))
        .await;
    let jid = r.json()["job"]["id"].as_str().unwrap_or_default().to_string();
    assert_eq!(hub.wait_job(&jid, None).await["state"], "done");
    let q = json!({"text": "document secret hunter2 outside", "k": 20});
    let hits = hub.call("POST", "/namespaces/docs/query", None, Some(q)).await.json()["hits"].clone();
    let hits = hits.as_array().cloned().unwrap_or_default();
    let mut sources: Vec<String> = hits
        .iter()
        .filter_map(|h| h["metadata"]["source"].as_str().map(str::to_string))
        .collect();
    sources.sort();
    sources.dedup();
    assert_eq!(sources, vec!["b.txt", "notes.env.txt", "sub/a.md"]); // no .env, no symlink escapes, no binary
    assert!(!hits.iter().any(|h| {
        let t = h["text"].as_str().unwrap_or_default();
        t.contains("OUTSIDE") || t.contains("hunter2")
    }));
}

#[tokio::test]
async fn test_path_ingest_file_cap() {
    let hub = Hub::with(|c| c.max_files_per_job = 2, memory_stores());
    hub.mk_ns("docs", "lancedb", 32).await;
    for i in 0..4 {
        write(&hub.root.join(format!("f{i}.md")), format!("doc {i}").as_bytes());
    }
    let body = json!({"path": hub.root.to_string_lossy(), "glob": "*.md"});
    let r = hub.call("POST", "/namespaces/docs/ingest", None, Some(body)).await;
    let jid = r.json()["job"]["id"].as_str().unwrap_or_default().to_string();
    let job = hub.wait_job(&jid, None).await;
    assert_eq!(
        (job["state"].clone(), job["error_code"].clone()),
        (json!("failed"), json!("too_many_files"))
    );
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0)
}

#[tokio::test]
async fn test_tokens_hashed_at_rest_and_revocable() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    let tok = hub.mk_token(json!([{"ns": "docs"}]), json!({})).await;
    let dbfile = hub.data.join("knowledge.db");
    let raw = std::fs::read(&dbfile).unwrap_or_default();
    assert!(!raw.windows(tok.len()).any(|w| w == tok.as_bytes()));
    let hashes = hub.app.ctx.db.token_hashes().unwrap_or_default();
    assert!(hashes[0] != tok && hashes[0].len() == 64);
    assert_eq!(mode(&hub.data.join("admin.key")), 0o600);
    assert_eq!(mode(&dbfile), 0o600);
    assert_eq!(mode(&hub.data), 0o700);
    let listing = hub.call("GET", "/tokens", None, None).await.text();
    assert!(!listing.contains(&tok) && !listing.contains("hash"));
    let tid = hub.call("GET", "/tokens", None, None).await.json()["items"][0]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        hub.call("DELETE", &format!("/tokens/{tid}"), None, None).await.status,
        204
    );
    assert_eq!(hub.call("GET", "/namespaces", Some(&tok), None).await.status, 401);
}

#[tokio::test]
async fn test_dropping_namespace_revokes_scopes() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    let tok = hub.mk_token(json!([{"ns": "docs", "mode": "write"}]), json!({})).await;
    assert_eq!(hub.call("DELETE", "/namespaces/docs", None, None).await.status, 204);
    hub.mk_ns("docs", "lancedb", 32).await; // same name, new namespace: the old token must not inherit access
    let r = hub
        .call("POST", "/namespaces/docs/query", Some(&tok), Some(json!({"text": "x"})))
        .await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn test_token_creation_validation() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    for body in [
        json!({"client": "bad client!", "scopes": []}),
        json!({"client": "ok", "scopes": [{"ns": "nope"}]}),
        json!({"client": "ok", "scopes": [{"ns": "docs", "mode": "admin"}]}),
        json!({"client": "ok", "scopes": [{"ns": "*"}]}),
    ] {
        let st = hub.call("POST", "/tokens", None, Some(body.clone())).await.status;
        assert!(st == 404 || st == 422, "{body} -> {st}");
    }
}

#[tokio::test]
async fn test_indexes_registry() {
    let hub = Hub::new();
    std::fs::create_dir(hub.root.join("graphify-out")).unwrap_or_else(|e| panic!("{e}"));
    let body = json!({"name": "demo", "kind": "graphify", "path": hub.root.join("graphify-out").to_string_lossy(), "project": "demo"});
    let r = hub.call("POST", "/indexes", None, Some(body)).await;
    assert_eq!((r.status, r.json()["exists"].clone()), (201, json!(true)));
    let outside = json!({"name": "x", "kind": "serena", "path": hub.tmp.path().to_string_lossy()});
    assert_eq!(hub.call("POST", "/indexes", None, Some(outside)).await.status, 403);
    let bad = json!({"name": "y", "kind": "other", "path": hub.root.to_string_lossy()});
    assert_eq!(hub.call("POST", "/indexes", None, Some(bad)).await.status, 422);
    let items = hub.call("GET", "/indexes", None, None).await.json();
    assert!(items["items"][0].get("path").is_some());
    assert!(items["items"][0]["updated"].is_f64());
    assert_eq!(hub.call("DELETE", "/indexes/demo", None, None).await.status, 204);
}

#[tokio::test]
async fn test_backends_and_metrics() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    let b = hub.call("GET", "/backends", None, None).await.json();
    let mut names: Vec<String> = b["backends"]
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    assert_eq!(names, vec!["lancedb", "qdrant"]);
    assert_eq!(b["embedder"]["api_key_env"], "KNOWLEDGE_EMBED_API_KEY");
    assert_eq!(b["warnings"], json!([]));
    assert_eq!(b["namespaces"][0]["dim"], 32);
    assert!(b["data_dir_free_bytes"].as_i64().is_some_and(|f| f > 0));
    assert!(!b.to_string().contains("sk-"));
    hub.call("POST", "/namespaces/docs/query", None, Some(json!({"text": "x"})))
        .await;
    let m = hub.call("GET", "/metrics", None, None).await;
    assert_eq!(
        m.header("content-type").as_deref(),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    let m = m.text();
    assert!(m.contains("agent_knowledge_queries_total") && m.contains(r#"ns="docs""#));
}

#[tokio::test]
async fn test_hardening_host_origin_csrf_headers_bodycap() {
    let hub = Hub::new();
    let r = hub
        .raw("GET", "/health", &[("host", "evil.example")], AxBody::empty())
        .await;
    assert_eq!(r.status, 421);
    assert!(
        r.header("content-type")
            .unwrap_or_default()
            .starts_with("application/problem+json")
    );
    assert_eq!(
        hub.raw(
            "POST",
            "/namespaces",
            &[("origin", "http://evil.example")],
            AxBody::empty()
        )
        .await
        .status,
        403
    );
    assert_eq!(
        hub.raw("POST", "/namespaces", &[("origin", "null")], AxBody::empty())
            .await
            .status,
        403
    );
    let h = [("content-type", "application/json"), ("content-length", "2")];
    let r = hub.raw("POST", "/namespaces", &h, AxBody::from("{}")).await; // no bearer, no CSRF header
    assert_eq!((r.status, r.code().as_str()), (403, "csrf_header_required"));
    let r = hub.raw("GET", "/health", &[], AxBody::empty()).await;
    assert!(
        r.header("content-security-policy")
            .unwrap_or_default()
            .contains("default-src 'none'")
    );
    assert_eq!(r.header("x-content-type-options").as_deref(), Some("nosniff"));
    assert_eq!(r.header("cache-control").as_deref(), Some("no-store"));
    assert!(r.header("access-control-allow-origin").is_none());
    let o = [
        ("origin", "http://127.0.0.1:8795"),
        ("access-control-request-method", "POST"),
    ];
    let r = hub.raw("OPTIONS", "/namespaces", &o, AxBody::empty()).await;
    assert!(r.header("access-control-allow-origin").is_none());
    let mut big = b"{\"documents\":[{\"text\":\"".to_vec();
    big.extend(std::iter::repeat_n(b'a', 5 * 1024 * 1024));
    big.extend_from_slice(b"\"}]}");
    let len = big.len().to_string();
    let auth = format!("Bearer {}", hub.admin);
    let h = [
        ("authorization", auth.as_str()),
        ("content-type", "application/json"),
        ("content-length", len.as_str()),
    ];
    let r = hub.raw("POST", "/namespaces/docs/ingest", &h, AxBody::from(big)).await;
    assert_eq!((r.status, r.code().as_str()), (413, "payload_too_large"));
    // a chunked (no content-length) body over the cap is also stopped
    let chunks = futures::stream::iter((0..6).map(|_| Ok::<_, Infallible>(Bytes::from(vec![b'a'; 1024 * 1024]))));
    let h = [("authorization", auth.as_str()), ("content-type", "application/json")];
    let r = hub
        .raw("POST", "/namespaces/docs/ingest", &h, AxBody::from_stream(chunks))
        .await;
    assert_eq!(r.status, 413);
    assert_eq!(hub.call("GET", "/nope", None, None).await.code(), "not_found");
}

#[tokio::test]
async fn test_validation_errors_do_not_echo_input() {
    let hub = Hub::new();
    let r = hub
        .call(
            "POST",
            "/namespaces",
            None,
            Some(json!({"name": "PRIVATE-TEXT-abc", "backend": "nope", "dim": "x"})),
        )
        .await;
    assert_eq!(r.status, 422);
    assert!(!r.text().contains("PRIVATE-TEXT"));
}

#[tokio::test]
async fn test_audit_log_is_metadata_only() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    hub.call(
        "POST",
        "/namespaces/docs/ingest",
        None,
        Some(json!({"documents": [{"text": "TOP-SECRET-BODY"}]})),
    )
    .await;
    let log = std::fs::read_to_string(hub.data.join("audit.jsonl")).unwrap_or_default();
    assert!(
        !log.contains("TOP-SECRET-BODY") && log.contains(r#""who": "admin""#),
        "{log}"
    );
    assert_eq!(mode(&hub.data.join("audit.jsonl")), 0o600);
}

#[tokio::test]
async fn test_backend_unavailable_and_unknown_backend() {
    let only_lance: Stores = vec![("lancedb".into(), Arc::new(MemoryStore::new()))];
    let hub = Hub::with(|_| {}, only_lance);
    let r = hub
        .call(
            "POST",
            "/namespaces",
            None,
            Some(json!({"name": "q", "backend": "qdrant", "dim": 4})),
        )
        .await;
    assert_eq!((r.status, r.code().as_str()), (422, "backend_unavailable"));
}

/// All 4 parametrisations of `test_malformed_auth`.
#[tokio::test]
async fn test_malformed_auth() {
    let hub = Hub::new();
    for token in ["", "Bearer", "Basic abc", "bearer  "] {
        let r = hub
            .raw("GET", "/namespaces", &[("authorization", token)], AxBody::empty())
            .await;
        assert_eq!(r.status, 401, "{token:?}");
    }
}

#[tokio::test]
async fn test_authz_precedes_body_validation_on_every_route() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    hub.mk_ns("other", "lancedb", 32).await;
    let ro = hub.mk_token(json!([{"ns": "docs"}]), json!({})).await;
    let other = hub.mk_token(json!([{"ns": "other", "mode": "write"}]), json!({})).await;
    let junk = json!({"documents": 5, "text": 5, "k": "x", "name": 1, "zzz": 1});
    let jbytes = serde_json::to_vec(&junk).unwrap_or_default();
    let jlen = jbytes.len().to_string();
    let mut seen = 0;
    for (m, path, url) in routes_and_urls() {
        seen += 1;
        let h = [
            ("x-agent-knowledge", "1"),
            ("content-type", "application/json"),
            ("content-length", jlen.as_str()),
        ];
        let r0 = hub.raw(m, &url, &h, AxBody::from(jbytes.clone())).await;
        assert_eq!(r0.status, 401, "{m} {path}"); // (a) no token
        if path.starts_with("/namespaces/{ns}") {
            let st = hub.call(m, &url, Some(&other), Some(junk.clone())).await.status; // (c) other namespace
            assert!(st == 403 || st == 404, "{m} {path} {st}");
        }
        if (m, path) == ("POST", "/namespaces/{ns}/ingest")
            || (m, path) == ("DELETE", "/namespaces/{ns}/documents/{doc_id:path}")
        {
            assert_eq!(
                hub.call(m, &url, Some(&ro), Some(junk.clone())).await.status,
                403,
                "{m} {path}"
            ); // (b)
        }
        if ["/tokens", "/backends", "/metrics", "/indexes"].contains(&path) {
            assert_eq!(
                hub.call(m, &url, Some(&ro), Some(junk.clone())).await.status,
                403,
                "{m} {path}"
            );
        }
    }
    assert_eq!(seen, 19);
}

#[tokio::test]
async fn test_backends_warns_on_master_key_env_name() {
    for name in ["LITELLM_API_KEY", "LITELLM_MASTER_KEY", "MY_MASTER_KEY"] {
        let hub = Hub::with(|c| c.router_api_key_env = name.into(), memory_stores());
        let w = hub.call("GET", "/backends", None, None).await.json()["warnings"].clone();
        let w = w.as_array().cloned().unwrap_or_default();
        assert_eq!(w.len(), 1, "{name}");
        assert!(w[0].as_str().unwrap_or_default().contains("MASTER key"), "{name}");
    }
}

#[tokio::test]
async fn test_indexes_require_admin() {
    let hub = Hub::new();
    let ro = hub.mk_token(json!([]), json!({})).await;
    assert_eq!(hub.call("GET", "/indexes", Some(&ro), None).await.status, 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_capacity_is_atomic_across_concurrent_ingests() {
    let hub = Hub::with(|c| c.max_chunks_per_namespace = 3, memory_stores());
    hub.mk_ns("docs", "lancedb", 32).await;
    let two = json!({"documents": [{"id": "a", "text": "one"}, {"id": "b", "text": "two"}]});
    let two2 = json!({"documents": [{"id": "c", "text": "three"}, {"id": "d", "text": "four"}]});
    let (r1, r2) = tokio::join!(
        hub.call("POST", "/namespaces/docs/ingest", None, Some(two)),
        hub.call("POST", "/namespaces/docs/ingest", None, Some(two2))
    );
    let mut st = vec![r1.status, r2.status];
    st.sort_unstable();
    assert_eq!(st, vec![202, 409]);
    for r in [&r1, &r2] {
        if r.status == 202 {
            hub.wait_job(r.json()["job"]["id"].as_str().unwrap_or_default(), None)
                .await;
        }
    }
    assert_eq!(hub.call("GET", "/namespaces/docs", None, None).await.json()["count"], 2);
    assert!(hub.app.ctx.svc.reservations().is_empty()); // reservations released
}

#[tokio::test]
async fn test_path_ingest_enforces_namespace_cap() {
    let hub = Hub::with(|c| c.max_chunks_per_namespace = 2, memory_stores());
    hub.mk_ns("docs", "lancedb", 32).await;
    for i in 0..4 {
        write(&hub.root.join(format!("f{i}.md")), format!("document {i}").as_bytes());
    }
    let body = json!({"path": hub.root.to_string_lossy(), "glob": "*.md"});
    let r = hub.call("POST", "/namespaces/docs/ingest", None, Some(body)).await;
    let job = hub
        .wait_job(r.json()["job"]["id"].as_str().unwrap_or_default(), None)
        .await;
    assert_eq!(
        (job["state"].clone(), job["error_code"].clone()),
        (json!("failed"), json!("namespace_full"))
    );
    assert_eq!(hub.call("GET", "/namespaces/docs", None, None).await.json()["count"], 2);
    assert!(hub.app.ctx.svc.reservations().is_empty());
}

/// The in-memory store with 30 ms of latency on every write (Python's `Slow(MemoryStore)`).
struct Slow(MemoryStore);

impl VectorStore for Slow {
    fn name(&self) -> &'static str {
        "memory"
    }
    fn ensure<'a>(&'a self, ns: &'a str, dim: i64) -> StoreResult<'a, ()> {
        self.0.ensure(ns, dim)
    }
    fn upsert<'a>(&'a self, ns: &'a str, records: Vec<Record>) -> StoreResult<'a, usize> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            self.0.upsert(ns, records).await
        })
    }
    fn query<'a>(
        &'a self,
        ns: &'a str,
        vector: &'a [f64],
        k: i64,
        flt: Option<&'a Node>,
        min_score: Option<f64>,
    ) -> StoreResult<'a, Vec<Hit>> {
        self.0.query(ns, vector, k, flt, min_score)
    }
    fn delete<'a>(&'a self, ns: &'a str, flt: &'a Node) -> StoreResult<'a, i64> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            self.0.delete(ns, flt).await
        })
    }
    fn stats<'a>(&'a self, ns: &'a str) -> StoreResult<'a, StoreStats> {
        self.0.stats(ns)
    }
    fn drop_ns<'a>(&'a self, ns: &'a str) -> StoreResult<'a, ()> {
        self.0.drop_ns(ns)
    }
    fn health(&self) -> BoxFuture<'_, PyValue> {
        self.0.health()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_same_namespace_jobs_do_not_interleave() {
    let stores: Stores = vec![
        ("lancedb".into(), Arc::new(Slow(MemoryStore::new()))),
        ("qdrant".into(), Arc::new(MemoryStore::new())),
    ];
    let hub = Hub::with(|_| {}, stores);
    hub.mk_ns("docs", "lancedb", 32).await;
    let long = (0..300).map(|i| format!("alpha{i}")).collect::<Vec<_>>().join(" "); // several chunks
    let a = hub
        .call(
            "POST",
            "/namespaces/docs/ingest",
            None,
            Some(json!({"documents": [{"id": "x", "text": long}], "chunk_size": 200, "chunk_overlap": 20})),
        )
        .await;
    let b = hub
        .call(
            "POST",
            "/namespaces/docs/ingest",
            None,
            Some(json!({"documents": [{"id": "x", "text": "beta only"}]})),
        )
        .await;
    hub.wait_job(a.json()["job"]["id"].as_str().unwrap_or_default(), None)
        .await;
    hub.wait_job(b.json()["job"]["id"].as_str().unwrap_or_default(), None)
        .await;
    let hits = hub
        .call(
            "POST",
            "/namespaces/docs/query",
            None,
            Some(json!({"text": "alpha beta", "k": 50})),
        )
        .await
        .json()["hits"]
        .clone();
    let hits = hits.as_array().cloned().unwrap_or_default();
    // last writer wins whole: either only B's single chunk, never a mix of A's tail chunks under B's head
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["text"], "beta only");
}

/// All 6 parametrisations of `test_empty_and_dot_path_segments_rejected` (raw paths, as Python sends raw ASGI).
#[tokio::test]
async fn test_empty_and_dot_path_segments_rejected() {
    let hub = Hub::new();
    let auth = format!("Bearer {}", hub.admin);
    for path in [
        "//tokens",
        "/namespaces//query",
        "/tokens//",
        "/namespaces/./docs",
        "/a/..",
        "/x\\y",
    ] {
        let r = hub
            .raw("GET", path, &[("authorization", auth.as_str())], AxBody::empty())
            .await;
        assert_eq!((r.status, r.code().as_str()), (400, "invalid_path"), "{path}");
    }
}

#[tokio::test]
async fn test_service_boundary_rejects_double_slash_ids() {
    let hub = Hub::new();
    hub.mk_ns("docs", "lancedb", 32).await;
    let r = hub
        .call(
            "POST",
            "/namespaces/docs/ingest",
            None,
            Some(json!({"documents": [{"id": "a//b", "text": "x"}]})),
        )
        .await;
    assert_eq!(r.status, 422);
}
