//! Port of knowledge/tests/test_lance_limits.py: LanceStore abuse limits against the real embedded Lance engine in a
//! temp dir (offline): the scan cap for broad filters (doc_id filters exempt), the per-call timeout and per-namespace
//! busy lock (a slow namespace never blocks another), and the filter complexity cap at the service boundary.

#![cfg(test)]
#![cfg(feature = "lance")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_knowledge::config::Config;
use agent_knowledge::db::{Db, NsRow};
use agent_knowledge::embed::HashEmbedder;
use agent_knowledge::filters::{Node, parse_filter};
use agent_knowledge::pyjson;
use agent_knowledge::service::Service;
use agent_knowledge::stores::lance::{BeforeOp, LanceOptions, LanceStore};
use agent_knowledge::stores::{Record, VectorStore};
use hub_core::PyValue;

fn recs(n: usize, doc: &str) -> Vec<Record> {
    (0..n)
        .map(|i| Record {
            id: format!("{doc}#{i}"),
            text: format!("t{i}"),
            vector: vec![1.0, (i % 7) as f64, 0.0, 0.0],
            meta: PyValue::Dict(vec![
                (PyValue::str("doc_id"), PyValue::Str(format!("{doc}{}", i % 50))),
                (PyValue::str("n"), PyValue::Int(i64::try_from(i).unwrap_or(0))),
            ]),
        })
        .collect()
}

fn flt(text: &str) -> Node {
    parse_filter(&pyjson::loads_str(text).unwrap_or_default(), 32).unwrap_or_else(|e| panic!("{e}"))
}

fn lance(path: &std::path::Path, opts: LanceOptions) -> LanceStore {
    LanceStore::new(path, opts).unwrap_or_else(|e| panic!("{e}"))
}

#[tokio::test]
async fn test_scan_cap_rejects_broad_filters_but_not_doc_id() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let s = lance(
        tmp.path(),
        LanceOptions {
            max_scan_rows: 100,
            ..LanceOptions::default()
        },
    );
    assert!(s.ensure("big", 4).await.is_ok());
    assert_eq!(s.upsert("big", recs(300, "d")).await.ok(), Some(300));
    let e = s
        .query(
            "big",
            &[1.0, 0.0, 0.0, 0.0],
            3,
            Some(&flt(r#"{"n": {"$gt": 5}}"#)),
            None,
        )
        .await
        .err();
    let e = e.unwrap_or_else(|| panic!("expected scan_cap"));
    assert_eq!((e.status, e.code.as_str()), (422, "scan_cap"));
    assert!(s.delete("big", &flt(r#"{"n": {"$gt": 5}}"#)).await.is_err());
    // unfiltered search and doc_id filters (ingest's replace path) still work on a table over the cap
    assert_eq!(
        s.query("big", &[1.0, 0.0, 0.0, 0.0], 3, None, None)
            .await
            .map(|h| h.len())
            .ok(),
        Some(3)
    );
    let hits = s
        .query(
            "big",
            &[1.0, 0.0, 0.0, 0.0],
            50,
            Some(&Node::eq_str("doc_id", "d3")),
            None,
        )
        .await
        .unwrap_or_default();
    assert!(!hits.is_empty() && hits.iter().all(|h| h.meta.get("doc_id") == Some(&PyValue::str("d3"))));
    assert_eq!(s.delete("big", &Node::eq_str("doc_id", "d3")).await.ok(), Some(6));
    assert_eq!(s.stats("big").await.map(|st| st.count).ok(), Some(294));
    // under the cap, arbitrary filters work
    let s2 = lance(
        &tmp.path().join("b"),
        LanceOptions {
            max_scan_rows: 1000,
            ..LanceOptions::default()
        },
    );
    assert!(s2.ensure("small", 4).await.is_ok());
    assert!(s2.upsert("small", recs(30, "d")).await.is_ok());
    let hits = s2
        .query(
            "small",
            &[1.0, 0.0, 0.0, 0.0],
            50,
            Some(&flt(r#"{"n": {"$lt": 10}}"#)),
            None,
        )
        .await;
    assert_eq!(hits.map(|h| h.len()).ok(), Some(10));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_slow_query_times_out_and_other_namespaces_are_not_blocked() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    // the Python test monkeypatches `_query` with a 1 s sleep for namespace "slow"
    let slow: BeforeOp = Arc::new(|ns: &str, op: &str| {
        let slow = ns == "slow" && op == "query";
        Box::pin(async move {
            if slow {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    });
    let s = Arc::new(lance(
        tmp.path(),
        LanceOptions {
            timeout_s: 0.3,
            lock_wait_s: 0.2,
            before_op: Some(slow),
            ..LanceOptions::default()
        },
    ));
    for ns in ["slow", "fast"] {
        assert!(s.ensure(ns, 4).await.is_ok());
        assert!(s.upsert(ns, recs(5, "d")).await.is_ok());
    }
    let t0 = Instant::now();
    let s1 = s.clone();
    let first = tokio::spawn(async move { s1.query("slow", &[1.0, 0.0, 0.0, 0.0], 3, None, None).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    // same namespace, lock held by the slow query: bounded wait then 429, not a stall
    let busy = s
        .query("slow", &[1.0, 0.0, 0.0, 0.0], 3, None, None)
        .await
        .err()
        .unwrap_or_else(|| panic!("busy"));
    assert_eq!((busy.status, busy.code.as_str()), (429, "store_busy"));
    // a different namespace answers immediately while "slow" is stuck
    assert_eq!(
        s.query("fast", &[1.0, 0.0, 0.0, 0.0], 3, None, None)
            .await
            .map(|h| h.len())
            .ok(),
        Some(3)
    );
    let to = first
        .await
        .ok()
        .and_then(Result::err)
        .unwrap_or_else(|| panic!("expected a timeout"));
    assert_eq!((to.status, to.code.as_str()), (429, "store_timeout"));
    assert!(t0.elapsed() < Duration::from_millis(900)); // caller released well before the 1 s query finished
    tokio::time::sleep(Duration::from_secs(1)).await; // let the worker drain
}

#[tokio::test]
async fn test_filter_complexity_cap_via_api() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut cfg = Config::with_data_dir(tmp.path());
    cfg.max_filter_nodes = 4;
    let db = Arc::new(Db::open(tmp.path()).unwrap_or_else(|e| panic!("{e}")));
    // Python replaces ns_row with a stub row; here the row is real and its backend is not configured
    let row = NsRow {
        name: "ns".into(),
        backend: "x".into(),
        embedding_model: "m".into(),
        dim: 4,
        description: String::new(),
        created: 0.0,
    };
    assert!(db.ns_insert(&row).is_ok());
    let svc = Service::new(Arc::new(cfg), db, Vec::new(), Arc::new(HashEmbedder::new(4)), None);
    let big = pyjson::loads_str(&format!(
        r#"{{"$or": [{}]}}"#,
        (0..6)
            .map(|i| format!(r#"{{"a": {i}}}"#))
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .unwrap_or_default();
    let e = svc
        .query("ns", None, Some(vec![0.0; 4]), 3, Some(&big), None, None)
        .await
        .err();
    let e = e.unwrap_or_else(|| panic!("expected invalid_filter"));
    assert_eq!((e.status, e.code.as_str()), (422, "invalid_filter"));
}
