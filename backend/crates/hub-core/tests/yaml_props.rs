//! Property tests for hub-core's PyYAML port:
//! * `load(dump(v)) == v` for random Python-like values, with each of the hub's three dump option sets and both
//!   loaders (PORTING.md §12.3);
//! * the loader never panics on arbitrary text (it returns PyYAML's value or PyYAML's error);
//! * a LIVE oracle: random values and texts from a deterministic proptest RNG are dumped/loaded by PyYAML itself
//!   (backend/parity/gen/yaml_oracle.py, run with the backend venv when it exists) and must give the same bytes, values
//!   and errors. Without the venv that test only checks the frozen oracle batch in golden/yaml_random.json, which
//!   `yaml_golden::golden_random_batch_matches_pyyaml` always checks too.

mod yaml_support;

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use hub_core::PyValue;
use hub_core::value::DateTime;
use hub_core::yaml::{self, DumpOpts, Loader};
use hub_core::yamlsafe;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
use serde_json::{Value, json};
use yaml_support::{big_stack, enc, report, run_dumps, run_loads};

const TOKENS: &[&str] = &[
    "yes",
    "no",
    "on",
    "off",
    "null",
    "Null",
    "~",
    "true",
    "False",
    "1",
    "-1",
    "0x1F",
    "017",
    "1:30",
    "1.0",
    "1e5",
    ".inf",
    ".nan",
    "2024-01-02",
    "2024-01-02 10:20:30",
    "<<",
    "=",
    "- ",
    ": ",
    " #",
    "#",
    "---",
    "...",
    "'",
    "\"",
    "\n",
    "\n\n",
    "  ",
    " ",
    "\t",
    "\r\n",
    "é",
    "日本",
    "😀",
    "\u{a0}",
    "\u{2028}",
    "\u{85}",
    "\u{feff}",
    "\x00",
    "\x07",
    "\x1b",
    "\u{7f}",
    "[",
    "]",
    "{",
    "}",
    ",",
    "?",
    "|",
    ">",
    "&a",
    "*a",
    "!",
    "!!str",
    "%",
    "@",
    "`",
    "\\",
    "x",
    "word",
    "a long phrase that keeps going ",
];

fn text_piece() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => prop::sample::select(TOKENS).prop_map(str::to_string),
        2 => "[a-z0-9 .:_-]{1,12}",
        1 => any::<char>().prop_map(|c| c.to_string()),
    ]
}

fn yaml_string() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => prop::collection::vec(text_piece(), 0..8).prop_map(|v| v.concat()),
        1 => prop::collection::vec(text_piece(), 20..60).prop_map(|v| v.concat()),
    ]
}

fn leaf() -> impl Strategy<Value = PyValue> {
    prop_oneof![
        6 => yaml_string().prop_map(PyValue::Str),
        1 => Just(PyValue::None),
        1 => any::<bool>().prop_map(PyValue::Bool),
        2 => any::<i64>().prop_map(PyValue::Int),
        1 => prop::sample::select(&["18446744073709551616", "-99999999999999999999999"][..])
            .prop_map(|s| PyValue::BigInt(s.to_string())),
        2 => any::<f64>().prop_map(PyValue::Float),
        1 => prop::sample::select(&[0.0, -0.0, 1e16, 1e17, 1e-7, f64::INFINITY, f64::NEG_INFINITY, f64::NAN][..])
            .prop_map(PyValue::Float),
        1 => prop::collection::vec(any::<u8>(), 0..90).prop_map(PyValue::Bytes),
        1 => (1i32..=9999, 1u8..=12, 1u8..=28).prop_map(|(year, month, day)| PyValue::Date { year, month, day }),
        1 => (
            (1i32..=9999, 1u8..=12, 1u8..=28),
            (0u8..24, 0u8..60, 0u8..60, prop_oneof![Just(0u32), 0u32..1_000_000]),
            prop::option::of(prop::sample::select(&[0, 3600, -19_800, 86_340][..])),
        )
            .prop_map(|((year, month, day), (hour, minute, second, microsecond), offset_s)| {
                PyValue::DateTime(DateTime { year, month, day, hour, minute, second, microsecond, offset_s })
            }),
    ]
}

fn key() -> impl Strategy<Value = PyValue> {
    prop_oneof![
        8 => yaml_string().prop_map(PyValue::Str),
        1 => (-5i64..5).prop_map(PyValue::Int),
        1 => (2000i32..2030, 1u8..=12, 1u8..=28).prop_map(|(year, month, day)| PyValue::Date { year, month, day }),
    ]
}

/// Python dict/set semantics: keys unique under `==`.
fn unique<T>(items: Vec<(PyValue, T)>) -> Vec<(PyValue, T)> {
    let mut out: Vec<(PyValue, T)> = Vec::new();
    for (k, v) in items {
        if !out.iter().any(|(e, _)| *e == k) {
            out.push((k, v));
        }
    }
    out
}

fn value() -> impl Strategy<Value = PyValue> {
    leaf().prop_recursive(4, 48, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(PyValue::List),
            prop::collection::vec((key(), inner), 0..6).prop_map(|kv| PyValue::Dict(unique(kv))),
            prop::collection::vec(key(), 0..4).prop_map(|ks| PyValue::Set(
                unique(ks.into_iter().map(|k| (k, ())).collect())
                    .into_iter()
                    .map(|(k, ())| k)
                    .collect()
            )),
        ]
    })
}

/// Python `==` up to dict/set order (enc + sorted entries).
fn canon(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut m = serde_json::Map::new();
            for (k, x) in o {
                let mut x = canon(x);
                if (k == "dict" || k == "set")
                    && let Value::Array(a) = &mut x
                {
                    a.sort_by_key(|e| e.to_string());
                }
                m.insert(k.clone(), x);
            }
            Value::Object(m)
        }
        Value::Array(a) => Value::Array(a.iter().map(canon).collect()),
        other => other.clone(),
    }
}

proptest! {
    #![proptest_config(Config { cases: 400, failure_persistence: None, ..Config::default() })]

    #[test]
    fn roundtrip_load_of_dump_is_identity(v in value()) {
        // PyYAML itself does not round-trip NEL (U+0085) inside single-quoted scalars (it reads it back as a line
        // break and folds it); the bytes are still PyYAML's (live oracle below), so only the round trip skips it.
        prop_assume!(!enc(&v).to_string().contains('\u{85}'));
        for opts in [DumpOpts::CONTENT, DumpOpts::FRONTMATTER, DumpOpts::ADAPTER] {
            let text = yaml::dump(&v, &opts);
            for loader in [Loader::Safe, Loader::Strict] {
                let back = yaml::load(&text, loader);
                prop_assert!(back.is_ok(), "{loader:?} cannot load {text:?}: {back:?}");
                let back = back.unwrap_or_default();
                prop_assert_eq!(canon(&enc(&back)), canon(&enc(&v)), "dump was {:?}", text);
            }
        }
    }

    #[test]
    fn loader_never_panics(t in prop::collection::vec(text_piece(), 0..40).prop_map(|v| v.concat())) {
        let _ = yaml::load(&t, Loader::Safe);
        let _ = yaml::load(&t, Loader::Strict);
        let _ = yamlsafe::split_frontmatter(&format!("---\n{t}\n---\nbody"));
    }
}

fn backend_python() -> Option<(PathBuf, PathBuf)> {
    // the live PyYAML oracle runs only with a prepared Python release (PYREL=$(node backend/parity/lib/python-release.mjs))
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let python = PathBuf::from(std::env::var("PYREL").ok()?).join("backend/.venv/bin/python");
    let oracle = root.join("backend/parity/gen/yaml_oracle.py");
    (python.exists() && oracle.exists()).then_some((python, oracle))
}

fn sample<T: std::fmt::Debug>(s: &impl Strategy<Value = T>, runner: &mut TestRunner, n: usize) -> Vec<T> {
    (0..n)
        .filter_map(|_| s.new_tree(runner).ok().map(|t| t.current()))
        .collect()
}

#[test]
fn live_pyyaml_oracle_agrees_on_random_values_and_texts() {
    let Some((python, oracle)) = backend_python() else {
        eprintln!("no backend venv: the live oracle is skipped (golden/yaml_random.json is the frozen batch)");
        return;
    };
    let mut runner = TestRunner::new_with_rng(Config::default(), TestRng::deterministic_rng(RngAlgorithm::ChaCha));
    // YAML_ORACLE_SCALE=N multiplies the batch (a longer bug hunt); the default keeps the test at about a second.
    let scale: usize = std::env::var("YAML_ORACLE_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let values: Vec<Value> = sample(&value(), &mut runner, 500 * scale).iter().map(enc).collect();
    let texts_strategy = prop::collection::vec(text_piece(), 0..40).prop_map(|v| v.concat());
    let texts = sample(&texts_strategy, &mut runner, 1500 * scale);
    let input = json!({ "values": values, "texts": texts, "structured": 1500 * scale, "seed": 7 }).to_string();
    let home = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let mut child = Command::new(&python)
        .arg("-I")
        .arg(&oracle)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home.path())
        .env("PYTHONHASHSEED", "0")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", python.display()));
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input.as_bytes())
            .unwrap_or_else(|e| panic!("oracle stdin: {e}"));
    }
    let out = child.wait_with_output().unwrap_or_else(|e| panic!("oracle: {e}"));
    assert!(
        out.status.success(),
        "oracle failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let golden: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("oracle output: {e}"));
    big_stack(move || {
        let mut failures = Vec::new();
        let mut n = run_dumps(golden["dumps"].as_array().map_or(&[][..], Vec::as_slice), &mut failures);
        n += run_loads(golden["loads"].as_array().map_or(&[][..], Vec::as_slice), &mut failures);
        report("live oracle", n, &failures);
    });
}
