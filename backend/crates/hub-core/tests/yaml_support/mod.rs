//! Shared helpers for the YAML tests: the typed JSON encoding of backend/parity/gen/yaml_codec.py <-> PyValue, and the
//! `DumpOpts` behind each golden option-set name.
// Test-support code: helpers outside `#[test]` functions that report through panics and stderr by design, shared by
// two test binaries (each uses a subset).
#![allow(dead_code, unreachable_pub, clippy::panic, clippy::print_stderr)]

use base64::Engine as _;
use hub_core::PyValue;
use hub_core::py::float;
use hub_core::value::DateTime;
use hub_core::yaml::{self, DumpOpts, Loader};
use hub_core::yamlsafe;
use serde_json::{Map, Value, json};

pub fn opts_by_name(name: &str) -> Option<DumpOpts> {
    Some(match name {
        "content" => DumpOpts::CONTENT,
        "frontmatter" => DumpOpts::FRONTMATTER,
        "adapter" => DumpOpts::ADAPTER,
        "flow" => DumpOpts {
            sort_keys: false,
            allow_unicode: true,
            default_flow_style: true,
            width: 80,
        },
        "ascii" => DumpOpts {
            sort_keys: false,
            allow_unicode: false,
            default_flow_style: false,
            width: 100,
        },
        _ => return None,
    })
}

fn num<T: TryFrom<i64> + Default>(v: &Value) -> T {
    v.as_i64().and_then(|i| T::try_from(i).ok()).unwrap_or_default()
}

/// yaml_codec encoding -> PyValue (tuples become lists).
pub fn dec(j: &Value) -> PyValue {
    match j {
        Value::Null | Value::Number(_) | Value::Array(_) => PyValue::None,
        Value::Bool(b) => PyValue::Bool(*b),
        Value::String(s) => PyValue::Str(s.clone()),
        Value::Object(o) => {
            let Some((k, v)) = o.iter().next() else {
                return PyValue::None;
            };
            match k.as_str() {
                "int" => {
                    let s = v.as_str().unwrap_or("0");
                    s.parse::<i64>()
                        .map_or_else(|_| PyValue::BigInt(s.to_string()), PyValue::Int)
                }
                "float" => PyValue::Float(match v.as_str().unwrap_or("nan") {
                    "nan" => f64::NAN,
                    "inf" => f64::INFINITY,
                    "-inf" => f64::NEG_INFINITY,
                    s => s.parse().unwrap_or(f64::NAN),
                }),
                "bytes" => PyValue::Bytes(
                    base64::engine::general_purpose::STANDARD
                        .decode(v.as_str().unwrap_or_default())
                        .unwrap_or_default(),
                ),
                "date" => PyValue::Date {
                    year: num(&v[0]),
                    month: num(&v[1]),
                    day: num(&v[2]),
                },
                "datetime" => PyValue::DateTime(DateTime {
                    year: num(&v[0]),
                    month: num(&v[1]),
                    day: num(&v[2]),
                    hour: num(&v[3]),
                    minute: num(&v[4]),
                    second: num(&v[5]),
                    microsecond: num(&v[6]),
                    offset_s: v[7].as_i64().and_then(|i| i32::try_from(i).ok()),
                }),
                "list" | "tuple" => {
                    PyValue::List(v.as_array().map(|a| a.iter().map(dec).collect()).unwrap_or_default())
                }
                "set" => PyValue::Set(v.as_array().map(|a| a.iter().map(dec).collect()).unwrap_or_default()),
                "dict" => PyValue::Dict(
                    v.as_array()
                        .map(|a| a.iter().map(|p| (dec(&p[0]), dec(&p[1]))).collect())
                        .unwrap_or_default(),
                ),
                _ => PyValue::None,
            }
        }
    }
}

/// PyValue -> yaml_codec encoding (as Python's `enc` would write it).
pub fn enc(v: &PyValue) -> Value {
    match v {
        PyValue::None => Value::Null,
        PyValue::Bool(b) => Value::Bool(*b),
        PyValue::Int(i) => json!({ "int": i.to_string() }),
        PyValue::BigInt(s) => json!({ "int": s }),
        PyValue::Float(f) => json!({ "float": float::repr(*f) }),
        PyValue::Str(s) => Value::String(s.clone()),
        PyValue::Bytes(b) => json!({ "bytes": base64::engine::general_purpose::STANDARD.encode(b) }),
        PyValue::Date { year, month, day } => json!({ "date": [year, month, day] }),
        PyValue::DateTime(d) => {
            json!({ "datetime": [d.year, d.month, d.day, d.hour, d.minute, d.second, d.microsecond, d.offset_s] })
        }
        PyValue::List(l) => json!({ "list": l.iter().map(enc).collect::<Vec<_>>() }),
        PyValue::Set(l) => json!({ "set": l.iter().map(enc).collect::<Vec<_>>() }),
        PyValue::Dict(d) => json!({ "dict": d.iter().map(|(k, v)| json!([enc(k), enc(v)])).collect::<Vec<_>>() }),
    }
}

/// A golden value normalised for comparison with [`enc`]: `{"tuple": [...]}` becomes `{"list": [...]}` (DEVIATION
/// D-YAML-TUPLE) and every `{"float": repr}` is re-formatted from its parsed value, so floats compare by value.
pub fn tuples_as_lists(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(tuples_as_lists).collect()),
        Value::Object(o) => {
            let mut m = Map::new();
            for (k, x) in o {
                if k == "float"
                    && let Some(PyValue::Float(f)) = Some(dec(&json!({ "float": x })))
                {
                    m.insert(k.clone(), json!(float::repr(f)));
                    continue;
                }
                let k = if k == "tuple" { "list".to_string() } else { k.clone() };
                m.insert(k, tuples_as_lists(x));
            }
            Value::Object(m)
        }
        other => other.clone(),
    }
}

// ---- comparisons against PyYAML outcomes (shared by the golden and the property tests) ----

pub fn golden(name: &str) -> Value {
    let path = format!("{}/tests/golden/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Compare one loader outcome; returns a failure description.
pub fn check_load(text: &str, loader: Loader, want: &Value) -> Option<String> {
    let got = yaml::load(text, loader);
    if want.get("unencodable").is_some() {
        return None; // a lone surrogate (DEVIATION D-YAML-SURROGATE): not comparable
    }
    if want.get("cyclic").is_some() {
        // Python builds a self-containing object; a PyValue tree cannot hold one (DEVIATION D-YAML-RECURSIVE)
        return match got {
            Err(e) if e.python_exception() == Some("RecursionError") => None,
            other => Some(format!(
                "{loader:?} {text:?}: want an unrepresentable-recursion error, got {other:?}"
            )),
        };
    }
    if let (Some(j), Ok(g)) = (want.get("ok_json"), &got) {
        let ge = serde_json::to_string(&enc(g)).unwrap_or_default();
        return (j.as_str() != Some(ge.as_str())).then(|| format!("{loader:?} {text:?}: deep value differs"));
    }
    match (want.get("ok"), want.get("err"), got) {
        (Some(v), _, Ok(g)) => {
            let ge = enc(&g);
            let want = tuples_as_lists(v);
            (ge != want).then(|| format!("{loader:?} {text:?}:\n  got  {ge}\n  want {want}"))
        }
        (_, Some(e), Err(g)) => {
            let mark = g
                .problem_mark
                .map(|m| json!({ "line": m.line, "column": m.column, "index": m.index }));
            let got = json!({
                "problem": g.problem, "full": g.full, "custom": g.custom, "py_exc": g.python_exception(),
                "problem_mark": mark,
            });
            (&got != e).then(|| format!("{loader:?} {text:?}:\n  got  {got}\n  want {e}"))
        }
        (_, _, got) => Some(format!("{loader:?} {text:?}: want {want}, got {got:?}")),
    }
}

pub fn check_yamlsafe(text: &str, want: &Value) -> Option<String> {
    if want.get("unencodable").is_some() {
        return None;
    }
    let got = yamlsafe::load_exact(text, yamlsafe::DEFAULT_MAX_BYTES);
    if let (Some(j), Ok(g)) = (want.get("ok_json"), &got) {
        let ge = serde_json::to_string(&enc(g)).unwrap_or_default();
        return (j.as_str() != Some(ge.as_str())).then(|| format!("yamlsafe {text:?}: deep value differs"));
    }
    match (want.get("ok"), want.get("err"), got) {
        (Some(v), _, Ok(g)) => {
            let ge = enc(&g);
            let want = tuples_as_lists(v);
            (ge != want).then(|| format!("yamlsafe {text:?}:\n  got  {ge}\n  want {want}"))
        }
        (_, Some(e), Err(g)) => {
            let got = match g {
                yamlsafe::LoadError::Yaml(y) => json!({ "message": y.0 }),
                yamlsafe::LoadError::Python { exc_type, message } => json!({ "py_exc": exc_type, "message": message }),
            };
            (&got != e).then(|| format!("yamlsafe {text:?}:\n  got  {got}\n  want {e}"))
        }
        (_, _, got) => Some(format!("yamlsafe {text:?}: want {want}, got {got:?}")),
    }
}

pub fn run_loads(cases: &[Value], failures: &mut Vec<String>) -> usize {
    let mut n = 0;
    for case in cases {
        let text = case["text"].as_str().unwrap_or_default();
        failures.extend(check_load(text, Loader::Safe, &case["safe"]));
        failures.extend(check_load(text, Loader::Strict, &case["strict"]));
        failures.extend(check_yamlsafe(text, &case["yamlsafe"]));
        n += 3;
    }
    n
}

pub fn run_dumps(cases: &[Value], failures: &mut Vec<String>) -> usize {
    let mut n = 0;
    for case in cases {
        let value = match case.get("construct") {
            Some(c) => construct(c[0].as_str().unwrap_or_default(), c[1].as_u64().unwrap_or(0)),
            None => dec(&case["value"]),
        };
        let Some(outs) = case["out"].as_object() else { continue };
        for (name, want) in outs {
            let Some(opts) = opts_by_name(name) else { continue };
            let Some(want) = want.as_str() else {
                failures.push(format!("{name} {}: Python raised {want}", case["value"]));
                continue;
            };
            n += 1;
            let got = yaml::dump(&value, &opts);
            if got != want {
                failures.push(format!(
                    "dump[{name}] {}:\n  got  {got:?}\n  want {want:?}",
                    case["value"]
                ));
            }
        }
    }
    n
}

/// The deep values the generator builds programmatically (`"leaf"` wrapped `depth` times).
pub fn construct(kind: &str, depth: u64) -> hub_core::PyValue {
    use hub_core::PyValue;
    let mut v = PyValue::str("leaf");
    for i in 0..depth {
        v = if kind == "list" || (kind == "mixed" && i % 2 == 1) {
            PyValue::List(vec![v])
        } else {
            PyValue::Dict(vec![(PyValue::str("k"), v)])
        };
    }
    v
}

/// The comparisons recurse over deeply nested goldens (the harness, not the engine: the engine is iterative and
/// proven on a 2 MiB stack in yaml/mod.rs), so they run on a thread with a large stack.
pub fn big_stack(f: impl FnOnce() + Send + 'static) {
    let h = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(f)
        .unwrap_or_else(|e| panic!("spawn: {e}"));
    if let Err(e) = h.join() {
        std::panic::resume_unwind(e);
    }
}

pub fn report(what: &str, checked: usize, failures: &[String]) {
    for f in failures.iter().take(40) {
        eprintln!("FAIL {f}");
    }
    assert!(
        failures.is_empty(),
        "{what}: {} of {checked} checks differ from PyYAML",
        failures.len()
    );
    eprintln!("{what}: {checked} checks equal to PyYAML");
}
