//! Port of knowledge/tests/test_units.py: chunking, the filter grammar (validation, semantics, Qdrant translation,
//! injection strings as data), the hash embedder, the router embedder (batching, retries, key handling, no
//! redirects) against a wiremock router, and the router_url env default/validation.

#![cfg(test)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_knowledge::chunking::chunk_text;
use agent_knowledge::config::load_config_with;
use agent_knowledge::embed::{Embedder, HashEmbedder, RouterEmbedder, Sleep};
use agent_knowledge::env;
use agent_knowledge::filters::{FilterError, Node, Op, evaluate, parse_filter, to_qdrant};
use agent_knowledge::pyjson::{self, Style};
use hub_core::PyValue;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn pj(text: &str) -> PyValue {
    pyjson::loads_str(text).unwrap_or_else(|e| panic!("bad test JSON {text}: {e:?}"))
}

fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

#[test]
fn test_chunk_overlap_and_coverage() {
    let text = (0..600).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
    let chunks = chunk_text(&text, 200, 50).unwrap_or_default();
    assert!(chunks.len() > 3 && chunks.iter().all(|c| c.chars().count() <= 200));
    // consecutive chunks share text (the overlap) and together cover every word
    let all: std::collections::HashSet<String> = chunks.iter().flat_map(|c| words(c)).collect();
    assert!(words(&text).iter().all(|w| all.contains(w)));
    assert!((0..chunks.len() - 1).any(|i| {
        let tail: String = chunks[i]
            .chars()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        words(&tail).last().is_some_and(|w| chunks[i + 1].contains(w.as_str()))
    }));
}

#[test]
fn test_chunk_edge_cases() {
    assert_eq!(chunk_text("   ", 1000, 200), Ok(vec![]));
    assert_eq!(chunk_text("short", 100, 10), Ok(vec!["short".to_string()]));
    assert!(chunk_text(&"x".repeat(500), 100, 100).is_err());
    assert!(chunk_text("x", 10, 0).is_err());
}

/// All 15 parametrisations of `test_filter_rejects`.
#[test]
fn test_filter_rejects() {
    let in65 = format!("[{}]", (0..65).map(|i| i.to_string()).collect::<Vec<_>>().join(","));
    let bad: Vec<String> = vec![
        "{}".into(),
        "[]".into(),
        "\"x\"".into(),
        r#"{"a b": 1}"#.into(),
        r#"{"a; DROP": 1}"#.into(),
        r#"{"a": {"$regex": "x"}}"#.into(),
        r#"{"a": {"$eq": 1, "$ne": 2}}"#.into(),
        r#"{"a": [1]}"#.into(),
        r#"{"a": {"$in": []}}"#.into(),
        r#"{"$and": []}"#.into(),
        r#"{"a": null}"#.into(),
        format!(r#"{{"a": "{}"}}"#, "x".repeat(600)),
        r#"{"$where": "1"}"#.into(),
        format!(r#"{{"a": {{"$in": {in65}}}}}"#),
        r#"{"$not": {"$not": {"$not": {"$not": {"$not": {"a": 1}}}}}}"#.into(),
    ];
    assert_eq!(bad.len(), 15);
    for b in &bad {
        let r: Result<Node, FilterError> = parse_filter(&pj(b), 32);
        assert!(r.is_err(), "{b} was accepted");
    }
}

#[test]
fn test_filter_semantics_and_qdrant_translation() {
    let f = parse_filter(
        &pj(r#"{"lang": "py", "n": {"$gte": 3}, "$or": [{"tag": {"$in": ["a", "b"]}}, {"$not": {"x": true}}]}"#),
        32,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(evaluate(&f, &pj(r#"{"lang": "py", "n": 3, "tag": "a"}"#)));
    assert!(!evaluate(&f, &pj(r#"{"lang": "py", "n": 2, "tag": "a"}"#)));
    assert!(!evaluate(&f, &pj(r#"{"lang": "py", "n": 5, "tag": "z", "x": true}"#)));
    let q = to_qdrant(&parse_filter(&pj(r#"{"lang": "py", "n": {"$gt": 3}}"#), 32).unwrap_or_else(|e| panic!("{e}")))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        pyjson::dumps(&q, Style::Std),
        r#"{"must": [{"must": [{"key": "meta.lang", "match": {"value": "py"}}]}, {"must": [{"key": "meta.n", "range": {"gt": 3}}]}]}"#
    );
    let float_eq = Node::Cmp {
        field: "a".into(),
        op: Op::Eq,
        value: PyValue::Float(1.5),
    };
    assert!(to_qdrant(&float_eq).is_err());
    let float_in = Node::In {
        field: "a".into(),
        values: vec![PyValue::Float(1.5)],
    };
    assert!(to_qdrant(&float_in).is_err());
    let or = to_qdrant(&Node::Or(vec![Node::eq_str("a", "x")])).unwrap_or_default();
    assert!(
        or.get("should")
            .and_then(PyValue::as_list)
            .is_some_and(|l| !l.is_empty())
    );
}

#[test]
fn test_injection_strings_are_only_ever_data() {
    let nasty = "x' OR 1=1 --\" ]}";
    let f = parse_filter(&PyValue::Dict(vec![(PyValue::str("lang"), PyValue::str(nasty))]), 32)
        .unwrap_or_else(|e| panic!("{e}"));
    let q = to_qdrant(&f).unwrap_or_default();
    let v = q
        .get("must")
        .and_then(PyValue::as_list)
        .and_then(|l| l.first())
        .and_then(|m| m.get("match"))
        .and_then(|m| m.get("value"));
    assert_eq!(v, Some(&PyValue::str(nasty))); // typed JSON slot, never concatenated
    assert!(!evaluate(&f, &pj(r#"{"lang": "x"}"#)));
}

#[tokio::test]
async fn test_hash_embedder_deterministic() {
    let e = HashEmbedder::new(16);
    let texts = vec!["hello world".to_string(), "hello world".to_string()];
    let out = e.embed(&texts, "m").await.unwrap_or_default();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], out[1]);
    assert_eq!(out[0].len(), 16);
}

fn recording_sleep() -> (Sleep, Arc<Mutex<Vec<f64>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let l2 = log.clone();
    let sleep: Sleep = Arc::new(move |s: f64| {
        if let Ok(mut g) = l2.lock() {
            g.push(s);
        }
        Box::pin(async {})
    });
    (sleep, log)
}

/// First call 503, then `{"data": [...]}` with the indexes reversed (the embedder must sort by index).
struct FlakyRouter {
    calls: AtomicUsize,
}

impl Respond for FlakyRouter {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return ResponseTemplate::new(503);
        }
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let n = body["input"].as_array().map_or(0, Vec::len);
        let data: Vec<serde_json::Value> = (0..n)
            .rev()
            .map(|i| serde_json::json!({"index": i, "embedding": [i as f64, 1.0]}))
            .collect();
        ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": data }))
    }
}

#[tokio::test]
async fn test_router_embedder_batches_retries_and_hides_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(FlakyRouter {
            calls: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;
    let (sleep, slept) = recording_sleep();
    let env = env::from_pairs(&[("KEY_ENV", "sekrit-value-123")]);
    let emb = RouterEmbedder::with(&server.uri(), "KEY_ENV", 2, 4, 60.0, sleep, env);
    let texts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let out = emb
        .embed(&texts, "example-embed")
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(out, vec![vec![0.0, 1.0], vec![1.0, 1.0], vec![0.0, 1.0]]);
    let calls = server.received_requests().await.unwrap_or_default();
    assert_eq!(calls.len(), 3); // 1 retry + 2 batches
    assert_eq!(*slept.lock().unwrap_or_else(|p| p.into_inner()), vec![0.5]);
    assert_eq!(
        calls[0].headers.get("authorization").and_then(|v| v.to_str().ok()),
        Some("Bearer sekrit-value-123")
    );
    // the request body is httpx's JSON: compact, model then input
    assert_eq!(
        String::from_utf8_lossy(&calls[1].body),
        r#"{"model":"example-embed","input":["a","b"]}"#
    );
}

#[tokio::test]
async fn test_router_embedder_failure_messages_have_no_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Bearer sekrit-value-123"))
        .mount(&server)
        .await;
    let env = env::from_pairs(&[("KEY_ENV", "sekrit-value-123")]);
    let (sleep, _) = recording_sleep();
    let e = RouterEmbedder::with(&server.uri(), "KEY_ENV", 64, 2, 60.0, sleep.clone(), env.clone())
        .embed(&["a".to_string()], "m")
        .await
        .err()
        .unwrap_or_else(|| panic!("expected an EmbedError"));
    assert!(!e.message.contains("sekrit"));
    assert_eq!(e.message, "router embeddings failed after 3 attempts (HTTP 500)");
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let e2 = RouterEmbedder::with(&server.uri(), "KEY_ENV", 64, 4, 60.0, sleep, env)
        .embed(&["a".to_string()], "m")
        .await
        .err()
        .unwrap_or_else(|| panic!("expected an EmbedError"));
    assert_eq!(e2.status, 400);
}

#[tokio::test]
async fn test_router_embedder_does_not_follow_redirects() {
    let evil = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/steal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})))
        .mount(&evil)
        .await;
    let router = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", format!("{}/steal", evil.uri()).as_str()))
        .mount(&router)
        .await;
    let (sleep, _) = recording_sleep();
    let r = RouterEmbedder::with(&router.uri(), "K", 64, 0, 60.0, sleep, env::from_pairs(&[]))
        .embed(&["a".to_string()], "m")
        .await;
    assert!(r.is_err());
    assert!(evil.received_requests().await.unwrap_or_default().is_empty());
}

#[test]
fn test_router_url_env_default_and_validation() {
    let env = env::from_pairs(&[("KNOWLEDGE_ROUTER_URL", "http://router.example:4000/"), ("HOME", "/h")]);
    assert_eq!(
        load_config_with(None, &env).map(|c| c.router_url),
        Ok("http://router.example:4000".into())
    );
    let env = env::from_pairs(&[("KNOWLEDGE_ROUTER_URL", "http://user:pw@evil/x?y=1"), ("HOME", "/h")]);
    assert!(load_config_with(None, &env).is_err());
}
