//! Port of knowledge/tests/test_qdrant_rest.py: QdrantStore against a faked REST surface (wiremock). Proves request
//! shapes, the API key header from the env var NAME, error messages that never relay the body, and the refusal of a
//! Qdrant that answers without a key. Real behaviour is in test_live.rs (opt-in).

#![cfg(test)]

use std::sync::Arc;

use agent_knowledge::api::startup_warnings;
use agent_knowledge::config::Config;
use agent_knowledge::env::{self, Env};
use agent_knowledge::filters::{Node, parse_filter};
use agent_knowledge::pyjson;
use agent_knowledge::stores::qdrant::{QdrantStore, point_id};
use agent_knowledge::stores::{Record, VectorStore};
use hub_core::PyValue;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn store(url: &str, key_env: Option<&str>, allow_unauth: bool, env: Env) -> QdrantStore {
    QdrantStore::new(url, key_env, true, 30.0, allow_unauth, env)
}

fn no_env() -> Env {
    env::from_pairs(&[])
}

async fn mock(server: &MockServer, m: &str, p: &str, status: u16, body: Option<Value>) {
    let mut t = ResponseTemplate::new(status);
    if let Some(b) = body {
        t = t.set_body_json(b);
    }
    Mock::given(method(m)).and(path(p)).respond_with(t).mount(server).await;
}

async fn requests_to(server: &MockServer, m: &str, p: &str) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method.as_str() == m && r.url.path() == p)
        .collect()
}

fn body_of(r: &wiremock::Request) -> Value {
    serde_json::from_slice(&r.body).unwrap_or(Value::Null)
}

#[tokio::test]
async fn test_ensure_creates_on_disk_collection() {
    let s = MockServer::start().await;
    mock(&s, "GET", "/collections/ak_docs", 404, None).await;
    mock(&s, "PUT", "/collections/ak_docs", 200, Some(json!({"result": true}))).await;
    mock(&s, "PUT", "/collections/ak_docs/index", 200, Some(json!({}))).await;
    let st = store(&s.uri(), None, true, no_env());
    assert!(st.ensure("docs", 768).await.is_ok());
    let put = requests_to(&s, "PUT", "/collections/ak_docs").await;
    let body = body_of(&put[0]);
    assert_eq!(
        body["vectors"],
        json!({"size": 768, "distance": "Cosine", "on_disk": true})
    );
    assert_eq!(body["hnsw_config"], json!({"on_disk": true}));
    assert_eq!(body["on_disk_payload"], json!(true));
    let idx = requests_to(&s, "PUT", "/collections/ak_docs/index").await;
    assert_eq!(
        body_of(&idx[0]),
        json!({"field_name": "meta.doc_id", "field_schema": "keyword"})
    );
}

#[tokio::test]
async fn test_ensure_existing_dim_mismatch() {
    let s = MockServer::start().await;
    mock(
        &s,
        "GET",
        "/collections/ak_docs",
        200,
        Some(json!({"result": {"config": {"params": {"vectors": {"size": 384}}}}})),
    )
    .await;
    let st = store(&s.uri(), None, true, no_env());
    assert!(st.ensure("docs", 384).await.is_ok());
    let e = st
        .ensure("docs", 768)
        .await
        .err()
        .unwrap_or_else(|| panic!("expected dim mismatch"));
    assert_eq!((e.code.as_str(), e.status), ("dim_mismatch", 409));
}

#[tokio::test]
async fn test_upsert_query_delete_shapes() {
    let s = MockServer::start().await;
    mock(&s, "PUT", "/collections/ak_docs/points", 200, Some(json!({}))).await;
    let st = store(&s.uri(), None, true, no_env());
    let meta = PyValue::Dict(vec![
        (PyValue::str("doc_id"), PyValue::str("d")),
        (PyValue::str("lang"), PyValue::str("en")),
    ]);
    let rec = Record {
        id: "d#0".into(),
        text: "hello".into(),
        vector: vec![1.0, 0.0],
        meta,
    };
    assert_eq!(st.upsert("docs", vec![rec]).await.ok(), Some(1));
    let up = requests_to(&s, "PUT", "/collections/ak_docs/points").await;
    let pt = &body_of(&up[0])["points"][0];
    assert_eq!(pt["id"], json!(point_id("d#0")));
    assert_eq!(
        pt["payload"],
        json!({"_id": "d#0", "text": "hello", "meta": {"doc_id": "d", "lang": "en"}})
    );
    assert_eq!(up[0].url.query(), Some("wait=true"));

    mock(
        &s,
        "POST",
        "/collections/ak_docs/points/query",
        200,
        Some(json!({"result": {"points": [{"id": "x", "score": 0.9, "payload": {"_id": "d#0", "text": "hello", "meta": {"lang": "en"}}}]}})),
    )
    .await;
    let flt = parse_filter(
        &pyjson::loads_str(r#"{"lang": "en x' OR 1=1"}"#).unwrap_or_default(),
        32,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let hits = st
        .query("docs", &[1.0, 0.0], 3, Some(&flt), Some(0.5))
        .await
        .unwrap_or_default();
    let q = requests_to(&s, "POST", "/collections/ak_docs/points/query").await;
    let body = body_of(&q[0]);
    assert_eq!(body["limit"], json!(3));
    assert_eq!(body["score_threshold"], json!(0.5));
    assert_eq!(body["with_payload"], json!(true));
    assert_eq!(
        body["filter"],
        json!({"must": [{"key": "meta.lang", "match": {"value": "en x' OR 1=1"}}]})
    );
    assert_eq!(hits[0].id, PyValue::str("d#0"));
    assert_eq!(hits[0].score, 0.9);

    mock(
        &s,
        "POST",
        "/collections/ak_docs/points/count",
        200,
        Some(json!({"result": {"count": 4}})),
    )
    .await;
    mock(&s, "POST", "/collections/ak_docs/points/delete", 200, Some(json!({}))).await;
    assert_eq!(st.delete("docs", &Node::eq_str("doc_id", "d")).await.ok(), Some(4));
    let de = requests_to(&s, "POST", "/collections/ak_docs/points/delete").await;
    assert_eq!(
        body_of(&de[0]),
        json!({"filter": {"must": [{"key": "meta.doc_id", "match": {"value": "d"}}]}})
    );
}

#[tokio::test]
async fn test_errors_do_not_echo_body_and_unreachable() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/collections/ak_docs/points/query"))
        .respond_with(ResponseTemplate::new(400).set_body_string("payload: PRIVATE TEXT"))
        .mount(&s)
        .await;
    let st = store(&s.uri(), None, true, no_env());
    let e = st
        .query("docs", &[1.0], 1, None, None)
        .await
        .err()
        .unwrap_or_else(|| panic!("expected an error"));
    assert!(!e.message.contains("PRIVATE"));
    assert_eq!(e.message, "qdrant returned HTTP 400");
    // /readyz unreachable: a port nothing listens on
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(9);
    let down = store(&format!("http://127.0.0.1:{closed}"), None, true, no_env());
    let h = down.health().await;
    assert_eq!(h.get("ok"), Some(&PyValue::Bool(false)));
    assert_eq!(h.get("error"), Some(&PyValue::str("ConnectError")));
}

#[tokio::test]
async fn test_api_key_header_from_env_name() {
    let s = MockServer::start().await;
    mock(&s, "GET", "/collections/ak_docs", 404, None).await;
    mock(&s, "PUT", "/collections/ak_docs", 200, Some(json!({}))).await;
    mock(&s, "PUT", "/collections/ak_docs/index", 200, Some(json!({}))).await;
    let st = store(&s.uri(), Some("QD_KEY"), true, env::from_pairs(&[("QD_KEY", "k-123")]));
    assert!(st.ensure("docs", 4).await.is_ok());
    let get = requests_to(&s, "GET", "/collections/ak_docs").await;
    assert_eq!(
        get[0].headers.get("api-key").and_then(|v| v.to_str().ok()),
        Some("k-123")
    );
}

#[tokio::test]
async fn test_keyless_qdrant_is_refused_unless_explicitly_allowed() {
    let s = MockServer::start().await;
    mock(&s, "GET", "/readyz", 200, None).await;
    mock(&s, "GET", "/collections", 200, Some(json!({}))).await;
    mock(&s, "GET", "/collections/ak_docs", 404, None).await;
    let st = store(&s.uri(), Some("KNOWLEDGE_QDRANT_API_KEY"), false, no_env());
    let h = st.health().await;
    assert_eq!(h.get("unauthenticated_access"), Some(&PyValue::Bool(true)));
    assert_eq!(h.get("api_key"), Some(&PyValue::str("[unset]")));
    assert_eq!(h.get("refused"), Some(&PyValue::Bool(true)));
    assert_eq!(h.get("ok"), Some(&PyValue::Bool(false)));
    let reason = h
        .get("reason")
        .and_then(PyValue::as_str)
        .unwrap_or_default()
        .to_string();
    assert!(!reason.contains("WITHOUT") && reason.contains("API key"));
    let col = requests_to(&s, "GET", "/collections").await;
    assert!(col.iter().all(|r| r.headers.get("api-key").is_none())); // the probe never sends a key
    let e = st
        .ensure("docs", 4)
        .await
        .err()
        .unwrap_or_else(|| panic!("expected a refusal"));
    assert_eq!((e.code.as_str(), e.status), ("qdrant_unauthenticated", 503));
    assert!(requests_to(&s, "GET", "/collections/ak_docs").await.is_empty());
    let stores: Vec<(String, Arc<dyn VectorStore>)> = vec![("qdrant".into(), Arc::new(st))];
    let w = startup_warnings(&stores, &Config::defaults("/h"), &no_env()).await;
    assert_eq!(w.len(), 1);
    assert!(w[0].contains("REFUSED"));
    // explicit opt-in restores use
    mock(&s, "PUT", "/collections/ak_docs", 200, Some(json!({}))).await;
    mock(&s, "PUT", "/collections/ak_docs/index", 200, Some(json!({}))).await;
    let ok = store(&s.uri(), None, true, no_env());
    assert!(ok.ensure("docs", 4).await.is_ok());
    // a Qdrant that enforces the key passes the probe
    s.reset().await;
    mock(&s, "GET", "/readyz", 200, None).await;
    mock(&s, "GET", "/collections", 401, None).await;
    let keyed = store(&s.uri(), Some("KNOWLEDGE_QDRANT_API_KEY"), false, no_env());
    assert_eq!(keyed.health().await.get("refused"), Some(&PyValue::Bool(false)));
    assert_eq!(keyed.refusal_reason().await, None);
}

#[tokio::test]
async fn test_embed_key_value_equal_to_master_key_warns_without_leaking() {
    let env = env::from_pairs(&[
        ("KNOWLEDGE_EMBED_API_KEY", "sk-same-value-123"),
        ("LITELLM_MASTER_KEY", "sk-same-value-123"),
        ("LITELLM_API_KEY", "sk-different"),
    ]);
    let w = startup_warnings(&[], &Config::defaults("/h"), &env).await;
    assert_eq!(w.len(), 1);
    assert!(w[0].contains("LITELLM_MASTER_KEY") && !w[0].contains("sk-same"));
    let env = env::from_pairs(&[
        ("KNOWLEDGE_EMBED_API_KEY", "sk-own-virtual"),
        ("LITELLM_MASTER_KEY", "sk-same-value-123"),
    ]);
    assert!(startup_warnings(&[], &Config::defaults("/h"), &env).await.is_empty());
}
