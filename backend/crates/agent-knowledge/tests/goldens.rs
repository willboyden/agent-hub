//! Golden comparisons against the PYTHON implementation (regenerate with tests/gen/gen_goldens.py):
//! * `json_loads.json`: CPython 3.13 `json.loads(bytes)` (values via `repr`, decode-error positions and messages, and
//!   which failures are not `JSONDecodeError`);
//! * `bodies.json`: every body route's 422/400 through FastAPI + pydantic (decode, then model validation);
//! * `config.json`: `load_config` (pydantic's `str(ValidationError)` or the resulting fields).

#![cfg(test)]

use std::path::Path;

use agent_knowledge::api;
use agent_knowledge::app::decode_body;
use agent_knowledge::config;
use agent_knowledge::env;
use agent_knowledge::pyjson::{self, DecodeError};
use serde_json::Value;

fn golden(name: &str) -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

#[test]
fn json_loads_matches_cpython() {
    let cases = golden("json_loads.json");
    let cases = cases.as_array().cloned().unwrap_or_default();
    assert!(cases.len() > 90, "golden file missing");
    let mut failures = Vec::new();
    for c in &cases {
        let bytes = hex::decode(s(c, "hex")).unwrap_or_default();
        let got = pyjson::loads_bytes(&bytes);
        let want_err = s(c, "err");
        let ok = match (&got, want_err.as_str()) {
            (Ok(v), "") => c["surrogate"].as_bool() == Some(true) || v.repr() == s(c, "repr"),
            (Err(DecodeError::Json { pos, msg }), "JSONDecodeError") => {
                Some(*pos as u64) == c["pos"].as_u64() && *msg == s(c, "msg")
            }
            (Err(DecodeError::Unicode), "UnicodeDecodeError") | (Err(DecodeError::IntDigits), "ValueError") => true,
            _ => false,
        };
        if !ok {
            failures.push(format!("{} -> {got:?}, want {c}", s(c, "hex")));
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn json_nesting_limit_is_the_measured_one() {
    let deep = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    // run on a thread with a generous stack: dropping the value is recursive
    let r = std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            (
                pyjson::loads_bytes(deep(pyjson::MAX_DEPTH).as_bytes()).is_ok(),
                pyjson::loads_bytes(deep(pyjson::MAX_DEPTH + 1).as_bytes()),
            )
        })
        .map(|h| h.join());
    let Ok(Ok((ok, over))) = r else { panic!("thread failed") };
    assert!(ok);
    assert_eq!(over.err(), Some(DecodeError::Recursion));
    assert_eq!(pyjson::MAX_DEPTH, 9991);
}

#[test]
fn request_bodies_match_fastapi() {
    let cases = golden("bodies.json");
    let cases = cases.as_array().cloned().unwrap_or_default();
    assert!(cases.len() > 150, "golden file missing");
    let mut failures = Vec::new();
    for c in &cases {
        let bytes = hex::decode(s(c, "body_hex")).unwrap_or_default();
        let ctype = s(c, "ctype");
        let endpoint = s(c, "endpoint");
        let Some(model) = api::model_for(&endpoint) else {
            failures.push(format!("no model for {endpoint}"));
            continue;
        };
        let got = decode_body(&bytes, Some(ctype.as_str()).filter(|x| !x.is_empty()))
            .and_then(|b| api::body_model(&b, model));
        let ok = match (&got, &c["status"]) {
            (Ok(_), Value::String(v)) => v == "valid",
            (Err(e), Value::Number(n)) => {
                Some(u64::from(e.status)) == n.as_u64() && e.code == s(c, "code") && e.detail == s(c, "detail")
            }
            _ => false,
        };
        if !ok {
            let shown = String::from_utf8_lossy(&bytes).chars().take(120).collect::<String>();
            failures.push(format!(
                "{endpoint} {ctype:?} {shown}\n   got  {:?}\n   want {} {}",
                got.as_ref()
                    .map(|_| "valid")
                    .map_err(|e| (e.status, &e.code, &e.detail)),
                c["status"],
                s(c, "detail")
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn config_loading_matches_pydantic() {
    let cases = golden("config.json");
    let cases = cases.as_array().cloned().unwrap_or_default();
    assert!(cases.len() > 10, "golden file missing");
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let envm = env::from_pairs(&[("HOME", "/home/golden")]);
    let mut failures = Vec::new();
    for (i, c) in cases.iter().enumerate() {
        let p = dir.path().join(format!("c{i}.yaml"));
        if std::fs::write(&p, s(c, "yaml")).is_err() {
            failures.push(format!("cannot write {p:?}"));
            continue;
        }
        let got = config::load_config_with(p.to_str(), &envm);
        let ok = match (&got, c.get("error"), c.get("fields")) {
            (Err(e), Some(Value::String(want)), _) => e == want,
            (Ok(cfg), None, Some(f)) => {
                let path_s = |x: &std::path::Path| x.to_string_lossy().into_owned();
                cfg.host == s(f, "host")
                    && Some(cfg.port) == f["port"].as_i64()
                    && cfg.router_url == s(f, "router_url")
                    && cfg.qdrant_url.as_deref() == f["qdrant_url"].as_str()
                    && cfg.qdrant_api_key_env.as_deref() == f["qdrant_api_key_env"].as_str()
                    && path_s(&cfg.data_dir) == s(f, "data_dir")
                    && path_s(&cfg.lance_path()) == s(f, "lance_path")
                    && cfg.ingest_roots.iter().map(|x| path_s(x)).collect::<Vec<_>>()
                        == f["ingest_roots"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                    && Some(cfg.body_cap_bytes) == f["body_cap_bytes"].as_i64()
                    && Some(cfg.qdrant_vectors_on_disk) == f["qdrant_vectors_on_disk"].as_bool()
            }
            _ => false,
        };
        if !ok {
            failures.push(format!("{:?}\n   got  {got:?}\n   want {c}", s(c, "yaml")));
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
