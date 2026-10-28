//! Port of knowledge/tests/test_live.py: the store contract and an end-to-end ingest/query through the API against
//! real backends. LanceDB is embedded and offline, so its half always runs (in a temp dir). The Qdrant half needs a
//! real server and stays opt-in exactly like Python's: AK_LIVE=1 and AK_LIVE_QDRANT_URL (AK_LIVE_QDRANT_KEY_ENV names
//! the env var holding the throwaway key Qdrant was started with); without them it reports that it did not run.

#![cfg(test)]
#![cfg(feature = "lance")]

use std::sync::Arc;
use std::time::Duration;

use agent_knowledge::app::{self, Options};
use agent_knowledge::config::Config;
use agent_knowledge::embed::HashEmbedder;
use agent_knowledge::env;
use agent_knowledge::filters::{Node, parse_filter};
use agent_knowledge::pyjson;
use agent_knowledge::stores::lance::{LanceOptions, LanceStore};
use agent_knowledge::stores::qdrant::QdrantStore;
use agent_knowledge::stores::{Record, VectorStore};
use axum::body::Body as AxBody;
use hub_core::PyValue;
use serde_json::{Value, json};

fn live_qdrant() -> Option<String> {
    if std::env::var("AK_LIVE").ok().as_deref() != Some("1") {
        eprintln!("test_live: set AK_LIVE=1 (and AK_LIVE_QDRANT_URL) for the Qdrant half");
        return None;
    }
    std::env::var("AK_LIVE_QDRANT_URL").ok().filter(|u| !u.is_empty())
}

fn qdrant(url: &str, key_env: Option<&str>) -> QdrantStore {
    QdrantStore::new(url, key_env, true, 30.0, false, env::process())
}

fn uniq() -> String {
    hub_core::common::token_hex(4)
}

fn f(text: &str) -> Node {
    parse_filter(&pyjson::loads_str(text).unwrap_or_default(), 32).unwrap_or_else(|e| panic!("{e}"))
}

fn rec(id: &str, text: &str, v: [f64; 4], meta: &[(&str, PyValue)]) -> Record {
    Record {
        id: id.into(),
        text: text.into(),
        vector: v.to_vec(),
        meta: PyValue::Dict(meta.iter().map(|(k, x)| (PyValue::str(*k), x.clone())).collect()),
    }
}

async fn ids(s: &dyn VectorStore, ns: &str, flt: Option<&Node>, min: Option<f64>) -> Vec<String> {
    s.query(ns, &[1.0, 0.0, 0.0, 0.0], 3, flt, min)
        .await
        .unwrap_or_default()
        .iter()
        .map(|h| h.id.as_str().unwrap_or_default().to_string())
        .collect()
}

async fn store_contract(store: &dyn VectorStore, is_lance: bool) {
    let ns = format!("t{}", uniq());
    assert!(store.ensure(&ns, 4).await.is_ok());
    assert!(store.ensure(&ns, 4).await.is_ok()); // idempotent
    assert_eq!(
        store.ensure(&ns, 8).await.err().map(|e| e.code),
        Some("dim_mismatch".into())
    );
    let recs = vec![
        rec(
            "a#0",
            "alpha",
            [1.0, 0.0, 0.0, 0.0],
            &[
                ("doc_id", PyValue::str("a")),
                ("lang", PyValue::str("en")),
                ("n", PyValue::Int(1)),
                ("ok", PyValue::Bool(true)),
            ],
        ),
        rec(
            "b#0",
            "beta",
            [0.0, 1.0, 0.0, 0.0],
            &[
                ("doc_id", PyValue::str("b")),
                ("lang", PyValue::str("fr")),
                ("n", PyValue::Int(2)),
                ("ok", PyValue::Bool(false)),
            ],
        ),
        rec(
            "c#0",
            "it's \"quoted\" ' OR 1=1",
            [0.9, 0.1, 0.0, 0.0],
            &[
                ("doc_id", PyValue::str("c")),
                ("lang", PyValue::str("en")),
                ("n", PyValue::Int(3)),
            ],
        ),
    ];
    assert_eq!(store.upsert(&ns, recs.clone()).await.ok(), Some(3));
    assert!(store.upsert(&ns, vec![recs[0].clone()]).await.is_ok()); // upsert twice must not duplicate
    assert_eq!(store.stats(&ns).await.map(|s| s.count).ok(), Some(3));
    let hits = store
        .query(&ns, &[1.0, 0.0, 0.0, 0.0], 3, None, None)
        .await
        .unwrap_or_default();
    assert_eq!(
        hits.iter().take(2).map(|h| h.id.clone()).collect::<Vec<_>>(),
        vec![PyValue::str("a#0"), PyValue::str("c#0")]
    );
    assert!(hits[0].score > 0.99 && hits[0].meta.get("lang") == Some(&PyValue::str("en")));
    assert_eq!(ids(store, &ns, Some(&f(r#"{"lang": "fr"}"#)), None).await, vec!["b#0"]);
    let mut got = ids(store, &ns, Some(&f(r#"{"n": {"$gte": 2}}"#)), None).await;
    got.sort();
    assert_eq!(got, vec!["b#0", "c#0"]);
    assert_eq!(ids(store, &ns, Some(&f(r#"{"ok": true}"#)), None).await, vec!["a#0"]);
    let mut got = ids(store, &ns, Some(&f(r#"{"$or": [{"lang": "fr"}, {"n": 3}]}"#)), None).await;
    got.sort();
    assert_eq!(got, vec!["b#0", "c#0"]);
    assert_eq!(
        ids(store, &ns, Some(&f(r#"{"$not": {"lang": "en"}}"#)), None).await,
        vec!["b#0"]
    );
    assert!(
        ids(store, &ns, Some(&f(r#"{"lang": "x' OR 1=1 --"}"#)), None)
            .await
            .is_empty()
    );
    assert_eq!(ids(store, &ns, None, Some(0.95)).await, vec!["a#0", "c#0"]);
    let short = store.query(&ns, &[1.0, 0.0], 3, None, None).await;
    if is_lance {
        assert_eq!(short.err().map(|e| e.code), Some("dim_mismatch".into()));
    } else {
        assert!(short.is_err());
    }
    assert_eq!(store.delete(&ns, &Node::eq_str("doc_id", "a")).await.ok(), Some(1));
    let st = store.stats(&ns).await.ok();
    assert_eq!(st.map(|s| s.count), Some(2));
    assert!(st.and_then(|s| s.disk_bytes).is_some_and(|b| b > 0));
    assert_eq!(store.health().await.get("ok"), Some(&PyValue::Bool(true)));
    assert!(store.drop_ns(&ns).await.is_ok());
    assert!(store.drop_ns(&ns).await.is_ok()); // dropping twice is fine
}

/// `test_store_contract[lancedb]` (always) and `[qdrant]` (opt-in).
#[tokio::test]
async fn test_store_contract() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let lance = LanceStore::new(&tmp.path().join("lance"), LanceOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    store_contract(&lance, true).await;
    if let Some(url) = live_qdrant() {
        store_contract(
            &qdrant(&url, std::env::var("AK_LIVE_QDRANT_KEY_ENV").ok().as_deref()),
            false,
        )
        .await;
    }
}

async fn end_to_end(store: Arc<dyn VectorStore>) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap_or_else(|e| panic!("{e}"));
    let _ = std::fs::write(
        root.join("a.md"),
        "The mitochondria is the powerhouse of the cell. ".repeat(60),
    );
    let _ = std::fs::write(root.join("b.md"), "Rust ownership and borrowing keep memory safe.");
    let mut cfg = Config::with_data_dir(&tmp.path().join("data"));
    cfg.ingest_roots = vec![root.clone()];
    cfg.router_url = "http://127.0.0.1:9".into();
    let name = store.name();
    let app = app::create_app_with(
        cfg,
        Options {
            stores: Some(vec![(name.into(), store)]),
            embedder: Some(Arc::new(HashEmbedder::new(32))),
            env: Some(env::from_pairs(&[])),
        },
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let admin = std::fs::read_to_string(tmp.path().join("data/admin.key"))
        .unwrap_or_default()
        .trim()
        .to_string();
    let call = |m: &str, p: String, body: Option<Value>| {
        let app = app.clone();
        let auth = format!("Bearer {admin}");
        let m = m.to_string();
        async move {
            let mut b = http::Request::builder()
                .method(m.as_str())
                .uri(p)
                .header("host", "127.0.0.1:8795")
                .header("authorization", auth);
            let body = match body {
                Some(v) => {
                    b = b.header("content-type", "application/json");
                    AxBody::from(serde_json::to_vec(&v).unwrap_or_default())
                }
                None => AxBody::empty(),
            };
            let resp = app::handle(app, b.body(body).unwrap_or_else(|e| panic!("{e}"))).await;
            let st = resp.status().as_u16();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            (st, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null))
        }
    };
    let ns = format!("e2e{}", &uniq()[..6]);
    let backend = if name == "lancedb" { "lancedb" } else { "qdrant" };
    let (st, body) = call(
        "POST",
        "/namespaces".into(),
        Some(json!({"name": ns, "backend": backend, "dim": 32})),
    )
    .await;
    assert_eq!(st, 201, "{body}");
    let (_, r) = call(
        "POST",
        format!("/namespaces/{ns}/ingest"),
        Some(json!({"path": root.to_string_lossy(), "glob": "*.md", "chunk_size": 300, "chunk_overlap": 50})),
    )
    .await;
    let jid = r["job"]["id"].as_str().unwrap_or_default().to_string();
    let mut j = Value::Null;
    for _ in 0..300 {
        j = call("GET", format!("/jobs/{jid}"), None).await.1;
        if j["state"] == "done" || j["state"] == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        (j["state"].clone(), j["docs_done"].clone()),
        (json!("done"), json!(2)),
        "{j}"
    );
    let q = json!({"text": "rust borrowing memory", "k": 2, "filter": {"source": "b.md"}});
    let (_, r) = call("POST", format!("/namespaces/{ns}/query"), Some(q)).await;
    assert_eq!(r["hits"][0]["metadata"]["source"], "b.md");
    let (_, view) = call("GET", format!("/namespaces/{ns}"), None).await;
    assert!(
        view["count"].as_i64().unwrap_or(0) >= 2 && view["disk_bytes"].as_i64().unwrap_or(0) > 0,
        "{view}"
    );
    assert_eq!(call("DELETE", format!("/namespaces/{ns}"), None).await.0, 204);
}

/// `test_end_to_end_through_api[lancedb]` (always) and `[qdrant]` (opt-in).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_end_to_end_through_api() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let lance = LanceStore::new(&tmp.path().join("lance"), LanceOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    end_to_end(Arc::new(lance)).await;
    if let Some(url) = live_qdrant() {
        end_to_end(Arc::new(qdrant(
            &url,
            std::env::var("AK_LIVE_QDRANT_KEY_ENV").ok().as_deref(),
        )))
        .await;
    }
}

#[tokio::test]
async fn test_qdrant_enforces_key() {
    let Some(url) = live_qdrant() else { return };
    let Some(key_env) = std::env::var("AK_LIVE_QDRANT_KEY_ENV").ok() else {
        eprintln!("test_live: needs a keyed live Qdrant (AK_LIVE_QDRANT_KEY_ENV)");
        return;
    };
    let keyless = qdrant(&url, None);
    assert_eq!(keyless.probe_unauthenticated().await, Some(false)); // a keyless request is refused
    let keyed = qdrant(&url, Some(&key_env));
    let h = keyed.health().await;
    assert_eq!(h.get("unauthenticated_access"), Some(&PyValue::Bool(false)));
    assert_eq!(h.get("refused"), Some(&PyValue::Bool(false)));
    assert_eq!(h.get("ok"), Some(&PyValue::Bool(true)));
    let e = keyless
        .ensure(&format!("t{}", uniq()), 4)
        .await
        .err()
        .map(|e| e.message)
        .unwrap_or_default();
    assert!(e.contains("401") || e.contains("403"), "{e}");
}
