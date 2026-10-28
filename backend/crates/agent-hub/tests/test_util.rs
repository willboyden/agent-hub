//! Port of backend/tests/adapters/test_util.py (adapters/_util.rs), plus the `_util` golden cases recorded from Python
//! (tests/golden/adapters.json "util": frontmatter render/parse, YAML/JSON dumps, marker regions, wrap_block,
//! safe_relpath, slice digests, oneline). Owner: WS-A.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod adapter_fakes;

use adapter_fakes::*;
use agent_hub::adapters::_util as u;
use agent_hub::adapters::base::{Check, ClientConfig, Expected};
use hub_core::value::PyValue;
use serde_json::{Value, json};

fn cfg_r() -> ClientConfig {
    ClientConfig {
        id: "c".into(),
        adapter: "x".into(),
        display_name: "c".into(),
        roots: roots(&[("r", "/r")]),
        ..ClientConfig::default()
    }
}

fn exp(root: &str, path: &str, sha: &str, kind: &str, merge: &str, keys: &[&str]) -> Expected {
    Expected {
        root: root.into(),
        path: path.into(),
        sha256: sha.into(),
        kind: kind.into(),
        source_ids: vec![],
        merge: merge.into(),
        managed_keys: keys.iter().map(|k| (*k).to_string()).collect(),
    }
}

#[test]
fn test_safe_relpath_rejects() {
    for bad in [
        "../x", "a/../b", "/abs", "a\\b", "a\x00b", "", ".", "a//b", "C:\\x", "a/\x07",
    ] {
        assert!(u::safe_relpath(&[bad]).is_err(), "{bad:?}");
    }
}

#[test]
fn test_safe_relpath_joins() {
    assert_eq!(
        u::safe_relpath(&[".claude/skills", "x", "sub/f.md"]).ok().as_deref(),
        Some(".claude/skills/x/sub/f.md")
    );
}

#[test]
fn test_names_use_fullmatch() {
    assert!(u::valid_name("ok-name.1") && !u::valid_name("ok\n") && !u::valid_name("Bad"));
    assert!(!u::valid_name(&"a".repeat(65)) && u::valid_name(&"a".repeat(64)));
}

#[test]
fn test_frontmatter_round_trip_and_hostile_values() {
    let fields = dict(vec![
        ("name", s("n")),
        ("description", s("a: b # c '\" --- {x}")),
        ("tools", s("Read, Bash")),
    ]);
    let rendered = u::render_frontmatter(&fields, "body\n\n---\nnot frontmatter");
    let (fm, body) = u::parse_frontmatter(&rendered).unwrap();
    let got = PyValue::Dict(fm.into_iter().map(|(k, v)| (PyValue::Str(k), v)).collect());
    assert_eq!(got, fields);
    assert!(body.starts_with("body"));
}

#[test]
fn test_frontmatter_parse_is_tolerant() {
    let (fm, body) = u::parse_frontmatter("no fm").unwrap();
    assert!(fm.is_empty() && body == "no fm");
    assert!(u::parse_frontmatter("---\n: : [\n---\nx").unwrap().0.is_empty());
    assert!(u::parse_frontmatter("---\n- a\n---\nx").unwrap().0.is_empty());
    assert!(u::parse_frontmatter("---\nunterminated: 1\n").unwrap().0.is_empty());
}

#[test]
fn test_oneline_kills_multiline_scalars() {
    let mut out = Vec::new();
    assert_eq!(u::oneline("a\n---\nb", "i", &mut out), "a --- b");
    assert_eq!(out[0].code, "description_normalized");
    let mut none = Vec::new();
    assert_eq!(u::oneline("plain", "i", &mut none), "plain");
    assert!(none.is_empty());
}

#[test]
fn test_block_helpers() {
    let blk = u::wrap_block("hello", "md").unwrap();
    assert_eq!(
        u::extract_block(&format!("x\n{blk}y"), "md").as_deref(),
        Some(blk.as_str())
    );
    assert!(u::extract_block("nothing", "md").is_none());
    assert!(u::extract_block(&format!("{}\nunterminated", u::MD_BEGIN), "md").is_none());
    assert_eq!(u::replace_block("", &blk, "md"), blk);
    let hb = u::wrap_block("k = 1", "hash").unwrap();
    assert!(hb.starts_with(u::HASH_BEGIN));
    assert_eq!(u::extract_block(&hb, "hash").as_deref(), Some(hb.as_str()));
}

#[test]
fn test_dotted_helpers_and_merge() {
    let mut d = serde_json::Map::new();
    u::set_dotted(&mut d, "a.b.c", json!(1));
    let v = Value::Object(d.clone());
    assert_eq!(v, json!({"a": {"b": {"c": 1}}}));
    assert_eq!(u::get_dotted(&v, "a.b.c"), Some(&json!(1)));
    assert_eq!(u::get_dotted(&v, "a.x"), None);
    u::del_dotted(&mut d, "a.b.c");
    u::del_dotted(&mut d, "zz.y");
    assert_eq!(Value::Object(d), json!({"a": {"b": {}}}));
    assert_eq!(
        u::merge_keys(
            &json!({"k": 1, "p": {"a": 1, "b": 2}}),
            &json!({"p": {"a": 9}}),
            &["p.a", "p.b"]
        ),
        json!({"k": 1, "p": {"a": 9}})
    );
}

#[test]
fn test_json_and_yaml_dump_are_sorted_and_stable() {
    assert_eq!(
        u::dump_json(&json!({"b": 1, "a": [2, 1]})),
        b"{\n  \"a\": [\n    2,\n    1\n  ],\n  \"b\": 1\n}\n".to_vec()
    );
    assert_eq!(
        u::dump_yaml(&dict(vec![("b", PyValue::Int(1)), ("a", PyValue::Int(2))])),
        b"a: 2\nb: 1\n".to_vec()
    );
}

#[test]
fn test_effective_caps() {
    assert_eq!(
        u::effective_caps(["shell", "read", "write"], true),
        vec!["read", "shell"]
    );
    assert_eq!(u::unknown_caps(&["read".into(), "teleport".into()]), vec!["teleport"]);
}

#[test]
fn test_read_tree_bounded_and_skips_caches() {
    let fs = FakeFs::new([
        ("/b/a.txt", b"1".to_vec()),
        ("/b/__pycache__/x.pyc", b"x".to_vec()),
        ("/b/d/e.txt", b"2".to_vec()),
        ("/b/big", vec![b'x'; u::MAX_FILE_BYTES + 1]),
    ]);
    let got = u::read_tree(&fs, "/b", u::MAX_DISCOVER_FILES, &mut Vec::new()).unwrap();
    assert_eq!(
        got.iter().map(|(k, v)| (k.as_str(), v.as_slice())).collect::<Vec<_>>(),
        vec![("a.txt", b"1".as_slice()), ("d/e.txt", b"2".as_slice())]
    );
    assert_eq!(u::read_tree(&fs, "/b", 1, &mut Vec::new()).unwrap().len(), 1);
    assert!(u::read_tree(&fs, "/nope", 400, &mut Vec::new()).unwrap().is_empty());
}

#[test]
fn test_verify_expected_unsafe_and_unconfigured_root() {
    let checks: Vec<Check> = u::verify_expected(
        &cfg_r(),
        &[
            exp("r", "../x", "0", "skill", "own", &[]),
            exp("zz", "a", "0", "skill", "own", &[]),
        ],
        &FakeFs::default(),
    )
    .unwrap();
    assert!(!checks.iter().any(|c| c.ok));
    assert!(checks.iter().any(|c| c.detail.contains("unsafe")));
    assert!(checks.iter().any(|c| c.detail == "root 'zz' not configured"));
}

#[test]
fn test_verify_expected_key_merged_file_compares_only_the_slice() {
    let keys = vec!["mcpServers".to_string()];
    let good = u::slice_digest(
        br#"{"mcpServers": {"a": {"command": "x"}}, "other": 1}"#,
        "json_keys",
        &keys,
    )
    .unwrap()
    .unwrap();
    let e = [exp("r", ".mcp.json", &good, "mcp", "json_keys", &["mcpServers"])];
    let fs = FakeFs::new([(
        "/r/.mcp.json",
        br#"{"other": 2, "mcpServers": {"a": {"command": "x"}}}"#.to_vec(),
    )]);
    assert!(u::verify_expected(&cfg_r(), &e, &fs).unwrap()[0].ok); // unmanaged key changed
    let fs2 = FakeFs::new([(
        "/r/.mcp.json",
        br#"{"mcpServers": {"a": {"command": "EVIL"}}}"#.to_vec(),
    )]);
    let c = &u::verify_expected(&cfg_r(), &e, &fs2).unwrap()[0];
    assert!(!c.ok && c.detail.contains("slice differs"));
    assert_eq!(u::slice_digest(b"not json", "json_keys", &keys).unwrap(), None);
}

#[test]
fn test_verify_yaml_keys_slice() {
    let keys = vec!["agent.disabled_toolsets".to_string()];
    let want = u::slice_digest(b"agent:\n  disabled_toolsets: [a]\nz: 1\n", "yaml_keys", &keys)
        .unwrap()
        .unwrap_or_default();
    let fs = FakeFs::new([(
        "/r/c.yaml",
        b"agent:\n  disabled_toolsets: [a]\nz: 2\nnew: 1\n".to_vec(),
    )]);
    let e = [exp(
        "r",
        "c.yaml",
        &want,
        "rule",
        "yaml_keys",
        &["agent.disabled_toolsets"],
    )];
    assert!(u::verify_expected(&cfg_r(), &e, &fs).unwrap()[0].ok);
}

// ---- golden: _util against Python ----------------------------------------------------------------------------------------
fn util(key: &str) -> &'static Vec<Value> {
    GOLDEN["util"][key].as_array().unwrap()
}

fn char_to_byte(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map_or(s.len(), |(b, _)| b)
}

#[test]
fn golden_render_frontmatter_bytes_equal_python() {
    for c in util("render_frontmatter") {
        let got = u::render_frontmatter(&dec(&c["fields"]), c["body"].as_str().unwrap());
        assert_eq!(got, c["text"].as_str().unwrap(), "fields {}", c["fields"]);
    }
}

#[test]
fn golden_parse_frontmatter_equals_python() {
    for c in util("parse_frontmatter") {
        let (fm, body) = u::parse_frontmatter(c["text"].as_str().unwrap()).unwrap();
        let fm = PyValue::Dict(fm.into_iter().map(|(k, v)| (PyValue::Str(k), v)).collect());
        assert_eq!(enc(&fm), c["fm"], "text {:?}", c["text"]);
        assert_eq!(body, c["body"].as_str().unwrap(), "text {:?}", c["text"]);
    }
}

#[test]
fn golden_dump_yaml_and_dump_json_bytes_equal_python() {
    for c in util("dump_yaml") {
        assert_eq!(
            String::from_utf8(u::dump_yaml(&dec(&c["doc"]))).unwrap(),
            c["text"].as_str().unwrap()
        );
    }
    for c in util("dump_json") {
        assert_eq!(
            String::from_utf8(u::dump_json(&c["doc"])).unwrap(),
            c["text"].as_str().unwrap()
        );
        assert_eq!(
            String::from_utf8(u::dump_json_py(&dec(&c["doc"])).unwrap()).unwrap(),
            c["text"].as_str().unwrap()
        );
    }
}

#[test]
fn golden_regions_extract_and_replace_equal_python() {
    for c in util("regions") {
        let t = c["text"].as_str().unwrap();
        let style = c["style"].as_str().unwrap();
        let want = match &c["region"] {
            Value::Null => Ok(None),
            Value::String(_) => Err("broken"),
            Value::Array(a) => {
                let idx = |v: &Value| usize::try_from(v.as_u64().unwrap()).unwrap();
                let (x, y) = (idx(&a[0]), idx(&a[1]));
                Ok(Some((char_to_byte(t, x), char_to_byte(t, y))))
            }
            other => panic!("{other}"),
        };
        assert_eq!(u::find_region(t, style), want, "{t:?} {style}");
        assert_eq!(
            u::extract_block(t, style).map(Value::from).unwrap_or(Value::Null),
            c["extract"],
            "{t:?}"
        );
        let new = u::wrap_block("NEW", style).unwrap();
        assert_eq!(
            u::replace_block(t, &new, style),
            c["replace"].as_str().unwrap(),
            "{t:?} {style}"
        );
    }
}

#[test]
fn golden_wrap_block_equals_python() {
    for c in util("wrap_block") {
        let got = match u::wrap_block(c["body"].as_str().unwrap(), c["style"].as_str().unwrap()) {
            Ok(s) => Value::from(s),
            Err(e) => json!({"error": e.to_string()}),
        };
        assert_eq!(got, c["out"], "{:?} {}", c["body"], c["style"]);
    }
}

#[test]
fn golden_safe_relpath_messages_equal_python() {
    for c in util("safe_relpath") {
        let parts: Vec<&str> = c["parts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect();
        let got = match u::safe_relpath(&parts) {
            Ok(s) => Value::from(s),
            Err(e) => json!({"error": e.to_string()}),
        };
        assert_eq!(got, c["out"], "{parts:?}");
    }
}

#[test]
fn golden_slice_digest_equals_python() {
    for c in util("slice_digest") {
        let keys: Vec<String> = c["managed_keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap().into())
            .collect();
        let got = match u::slice_digest(&unblob(&c["raw"]), c["merge"].as_str().unwrap(), &keys) {
            Ok(d) => d.map(Value::from).unwrap_or(Value::Null),
            Err(e) => json!({"error": e.to_string()}),
        };
        assert_eq!(got, c["digest"], "{} {} {:?}", c["raw"], c["merge"], keys);
    }
}

#[test]
fn golden_oneline_equals_python() {
    for c in util("oneline") {
        let mut out = Vec::new();
        assert_eq!(
            u::oneline(c["text"].as_str().unwrap(), "i", &mut out),
            c["out"].as_str().unwrap()
        );
        assert_eq!(serde_json::to_value(&out).unwrap(), c["diags"]);
    }
}

#[test]
fn cap_str_and_truthiness_follow_python() {
    assert_eq!(u::cap_str(Some(&PyValue::Float(1.5)), 10), "1.5");
    assert_eq!(u::cap_str(Some(&PyValue::Bool(true)), 10), "True");
    assert_eq!(u::cap_str(Some(&PyValue::Set(vec![])), 10), "set()");
    assert!(!u::truthy(&PyValue::Float(0.0)) && u::truthy(&PyValue::Float(f64::NAN)));
    assert_eq!(
        u::py_iter(&PyValue::Int(3)).err().map(|e| e.to_string()),
        Some("TypeError: 'int' object is not iterable".into())
    );
}
