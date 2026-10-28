//! The LanceDB verdict (PORTING.md §15) as tests, both directions:
//! * Rust reads a table the PYTHON knowledge service wrote (knowledge/src/agent_knowledge/stores/lance.py through
//!   lancedb 0.39.0: ensure, upsert x2, delete; fixture tests/fixtures/lance-py-lancedb-0.39.0, generated 2026-10-27
//!   by backend/parity/gen/mklance.py): row count, and the same hits, order and float scores Python's LanceStore gives
//!   (tests/fixtures/lance-py-lancedb-0.39.0.expected.json, recorded with tests/py/lance_roundtrip.py);
//! * Python reads what Rust wrote (create, merge-insert, update, delete), then writes to it (merge-insert, delete, a
//!   new table) and Rust reads Python's writes back. This half runs the Python venv (knowledge/.venv) when it is
//!   present, hermetically (cleared environment, temp HOME, only the temp table directory); without the venv it
//!   reports that it did not run and checks only the Rust side.

#![cfg(test)]
#![cfg(feature = "lance")]

use std::path::{Path, PathBuf};

use agent_knowledge::filters::{Node, parse_filter};
use agent_knowledge::pyjson;
use agent_knowledge::stores::lance::{LanceOptions, LanceStore, table_names};
use agent_knowledge::stores::{Hit, Record, VectorStore};
use hub_core::PyValue;
use serde_json::{Value, json};

const Q: [f64; 4] = [0.1, 0.2, 0.3, 0.4];

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lance-py-lancedb-0.39.0")
}

/// What Python's LanceStore answers on the fixture (recorded with tests/py/lance_roundtrip.py `read`).
fn expected_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lance-py-lancedb-0.39.0.expected.json")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap_or_else(|e| panic!("{e}"));
    for e in std::fs::read_dir(from).unwrap_or_else(|e| panic!("{e}")).flatten() {
        let t = to.join(e.file_name());
        if e.file_type().is_ok_and(|f| f.is_dir()) {
            copy_dir(&e.path(), &t);
        } else {
            std::fs::copy(e.path(), &t).unwrap_or_else(|e| panic!("{e}"));
        }
    }
}

fn hits_json(hs: &[Hit]) -> Value {
    Value::Array(
        hs.iter()
            .map(
                |h| json!({"id": h.id.to_json(), "score": h.score, "text": h.text.to_json(), "meta": h.meta.to_json()}),
            )
            .collect(),
    )
}

/// The same view `lance_roundtrip.py` prints.
async fn view(s: &LanceStore, ns: &str) -> Value {
    let n_ge_2 = parse_filter(&pyjson::loads_str(r#"{"n": {"$gte": 2}}"#).unwrap_or_default(), 32)
        .unwrap_or_else(|e| panic!("{e}"));
    json!({
        "count": s.stats(ns).await.map(|st| st.count).unwrap_or(-1),
        "query": hits_json(&s.query(ns, &Q, 10, None, None).await.unwrap_or_default()),
        "doc_d1": hits_json(&s.query(ns, &Q, 10, Some(&Node::eq_str("doc_id", "d1")), None).await.unwrap_or_default()),
        "n_ge_2": hits_json(&s.query(ns, &Q, 10, Some(&n_ge_2), None).await.unwrap_or_default()),
    })
}

async fn full_view(s: &LanceStore, dir: &Path) -> Value {
    let tables = table_names(dir).unwrap_or_default();
    let mut out = json!({"tables": tables, "demo": view(s, "demo").await});
    if tables.iter().any(|t| t == "ns_pynew") {
        out["pynew"] = view(s, "pynew").await;
    }
    out
}

fn store(dir: &Path) -> LanceStore {
    LanceStore::new(dir, LanceOptions::default()).unwrap_or_else(|e| panic!("{e}"))
}

/// Runs the Python half with a prepared Python release (PYREL=$(node backend/parity/lib/python-release.mjs)); `None`
/// without one (the Python-written fixture is still read by the Rust half).
fn python(mode: &str, dir: &Path) -> Option<Value> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let py = std::path::PathBuf::from(std::env::var("PYREL").unwrap_or_default()).join("knowledge/.venv/bin/python");
    if !py.exists() {
        eprintln!("lance_compat: no Python release (PYREL), the Python half did not run");
        return None;
    }
    let home = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let out = std::process::Command::new(&py)
        .arg("-I")
        .arg(manifest.join("tests/py/lance_roundtrip.py"))
        .arg(mode)
        .arg(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home.path())
        .env("LANG", "C.UTF-8")
        .output()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", py.display()));
    assert!(
        out.status.success(),
        "python {mode} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("bad JSON from python: {e}")))
}

#[tokio::test]
async fn lance_reads_python_written_table() -> Result<(), Box<dyn std::error::Error>> {
    let dir = fixture();
    assert_eq!(table_names(&dir)?, vec!["ns_demo".to_string()]);
    // 5 inserted, 1 updated in place, 1 deleted by doc_id
    assert_eq!(agent_knowledge::stores::lance::count_rows(&dir, "ns_demo").await?, 4);
    Ok(())
}

#[tokio::test]
async fn rust_store_reads_python_fixture_like_python() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let dir = tmp.path().join("lance");
    copy_dir(&fixture(), &dir);
    let got = full_view(&store(&dir), &dir).await;
    let want_text = std::fs::read_to_string(expected_file()).unwrap_or_default();
    let want: Value = serde_json::from_str(&want_text).unwrap_or(Value::Null);
    assert_eq!(
        got, want,
        "Rust's view of the Python-written table differs from Python's"
    );
    if let Some(py) = python("read", &dir) {
        assert_eq!(got, py);
    }
}

fn rec(id: &str, text: &str, v: [f64; 4], meta: &[(&str, PyValue)]) -> Record {
    Record {
        id: id.into(),
        text: text.into(),
        vector: v.to_vec(),
        meta: PyValue::Dict(meta.iter().map(|(k, x)| (PyValue::str(*k), x.clone())).collect()),
    }
}

#[tokio::test]
async fn python_reads_rust_writes_and_rust_reads_python_writes() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let dir = tmp.path().join("lance");
    let s = store(&dir);
    // what backend/parity/gen/mklance.py does through the Python store, done by the Rust one
    assert!(s.ensure("demo", 4).await.is_ok());
    let recs: Vec<Record> = (0..5i64)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let x = 0.1 * i as f64;
            rec(
                &format!("d{i}#c0"),
                &format!("text {i} é"),
                [x, 0.2, 0.3, 0.4],
                &[("doc_id", PyValue::Str(format!("d{i}"))), ("n", PyValue::Int(i))],
            )
        })
        .collect();
    assert_eq!(s.upsert("demo", recs).await.ok(), Some(5));
    let upd = rec(
        "d1#c0",
        "updated",
        [1.0, 0.0, 0.0, 0.0],
        &[("doc_id", PyValue::str("d1"))],
    );
    assert_eq!(s.upsert("demo", vec![upd]).await.ok(), Some(1));
    assert_eq!(s.delete("demo", &Node::eq_str("doc_id", "d3")).await.ok(), Some(1));
    let rust_view = full_view(&s, &dir).await;
    assert_eq!(rust_view["demo"]["count"], 4);
    // the Rust-written table answers exactly like the Python-written fixture
    let want_text = std::fs::read_to_string(expected_file()).unwrap_or_default();
    let want: Value = serde_json::from_str(&want_text).unwrap_or(Value::Null);
    assert_eq!(rust_view, want);
    let Some(py_read) = python("read", &dir) else { return };
    assert_eq!(py_read, rust_view, "Python's view of the Rust-written table");
    let Some(py_after) = python("write", &dir) else { return };
    let rust_after = full_view(&store(&dir), &dir).await;
    assert_eq!(rust_after, py_after, "Rust's view after Python's writes");
    assert_eq!(rust_after["demo"]["count"], 4); // +py#0, d1 updated, -d4
    assert_eq!(rust_after["pynew"]["count"], 1);
}
