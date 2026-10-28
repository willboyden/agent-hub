//! Port of backend/tests/adapters/test_hardening.py (M6: hardened YAML loading and bounded reads on the discover path;
//! security control S-ADAPTER-PATHS). Owner: WS-A.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod adapter_fakes;

use std::collections::BTreeMap;
use std::sync::Mutex;

use adapter_fakes::*;
use agent_hub::adapters::_util as u;
use agent_hub::adapters::base::ClientAdapter;
use agent_hub::adapters::claude_code::ClaudeCodeAdapter;
use agent_hub::adapters::hermes::HermesAdapter;
use hub_core::value::PyValue;

const ALIAS_BOMB: &str = "a: &a [1,2,3,4,5,6,7,8,9]\nb: &b [*a,*a,*a,*a,*a,*a,*a,*a,*a]\nc: &c [*b,*b,*b,*b,*b,*b,*b,*b,*b]\nd: [*c,*c,*c,*c,*c,*c,*c,*c,*c]\n";

fn deep() -> String {
    format!("x: {}{}\n", "[".repeat(200), "]".repeat(200))
}

#[test]
fn test_frontmatter_rejects_alias_bomb_and_anchors_and_depth() {
    for head in [
        ALIAS_BOMB.to_string(),
        "a: &x 1\nb: *x\n".into(),
        deep(),
        "k: 1\nk: 2\n".into(),
    ] {
        let (fm, _) = u::parse_frontmatter(&format!("---\n{head}---\nbody\n")).unwrap();
        assert!(fm.is_empty(), "{head:?}");
    }
    let (fm, _) = u::parse_frontmatter("---\nname: ok\n---\nb").unwrap();
    assert_eq!(fm.get("name"), Some(&PyValue::str("ok")));
    assert_eq!(fm.len(), 1);
}

#[test]
fn test_frontmatter_rejects_oversized_head() {
    let text = format!("---\ndescription: {}\n---\nb", "x".repeat(300_000));
    assert!(u::parse_frontmatter(&text).unwrap().0.is_empty());
}

#[test]
fn test_cap_str_bounds_everything() {
    let big = PyValue::str("x".repeat(1_000_000));
    assert_eq!(u::cap_str(Some(&big), u::MAX_DESC_ERR).chars().count(), u::MAX_DESC_ERR);
    assert_eq!(
        u::cap_str(Some(&PyValue::List(vec![PyValue::str("a")])), u::MAX_DESC_ERR),
        ""
    );
    assert_eq!(u::cap_str(None, u::MAX_DESC_ERR), "");
    assert_eq!(u::cap_str(Some(&PyValue::None), u::MAX_DESC_ERR), "");
    assert_eq!(u::cap_str(Some(&PyValue::Int(7)), u::MAX_DESC_ERR), "7");
}

#[test]
fn test_discover_agent_with_alias_bomb_is_skipped_not_expanded() {
    let a = ClaudeCodeAdapter;
    let mut cfg = a.default_config();
    cfg.roots = roots(&[("project", "/p")]);
    let fs = FakeFs::new([(
        "/p/.claude/agents/bomb.md",
        format!("---\n{ALIAS_BOMB}---\nbody").into_bytes(),
    )]);
    let d = a.discover(&cfg, &fs).unwrap();
    assert_eq!(d.agents[0].description, "");
    assert!(d.agents[0].capabilities.is_empty());
}

#[test]
fn test_hermes_config_alias_bomb_and_oversize_rejected() {
    let a = HermesAdapter;
    let mut cfg = a.default_config();
    cfg.roots = roots(&[("stage", "/s")]);
    set_param(&mut cfg, "source_root", s("/lab"));
    set_param(
        &mut cfg,
        "source_dirs",
        PyValue::List(vec![dict(vec![("kind", s("config")), ("path", s("c.yaml"))])]),
    );
    let d = a
        .discover(&cfg, &FakeFs::new([("/lab/c.yaml", ALIAS_BOMB.as_bytes().to_vec())]))
        .unwrap();
    assert!(d.mcp_servers.is_empty() && d.notes.iter().any(|n| n.contains("safe YAML loader")));
    let mut big = b"mcp_servers: {}\n".to_vec();
    big.extend(vec![b'#'; u::MAX_READ_FILE_BYTES + 1]);
    let d2 = a.discover(&cfg, &FakeFs::new([("/lab/c.yaml", big)])).unwrap();
    assert!(d2.notes.iter().any(|n| n.contains("read cap")));
}

#[test]
fn test_read_tree_stats_before_reading() {
    let fs = SizedFs {
        inner: FakeFs::new([("/b/ok.txt", b"1".to_vec()), ("/b/big.bin", b"x".to_vec())]),
        sizes: BTreeMap::from([("/b/big.bin".to_string(), (u::MAX_READ_FILE_BYTES + 1) as u64)]),
        read: Mutex::new(Vec::new()),
    };
    let mut notes = Vec::new();
    let got = u::read_tree(&fs, "/b", u::MAX_DISCOVER_FILES, &mut notes).unwrap();
    assert_eq!(got.keys().collect::<Vec<_>>(), vec!["ok.txt"]);
    assert!(!fs.read.lock().unwrap().contains(&"/b/big.bin".to_string()));
    assert!(notes.iter().any(|n| n.contains("big.bin")));
}

#[test]
fn test_read_tree_caps_without_size_port_file_count_and_total() {
    let mut notes = Vec::new();
    let fs = FakeFs::new([
        ("/b/big", vec![b'x'; u::MAX_READ_FILE_BYTES + 1]),
        ("/b/s", b"y".to_vec()),
    ]);
    let got = u::read_tree(&fs, "/b", u::MAX_DISCOVER_FILES, &mut notes).unwrap();
    assert_eq!(got.keys().collect::<Vec<_>>(), vec!["s"]);
    assert!(notes.iter().any(|n| n.contains("read cap")));
    let many = FakeFs::new((0..450).map(|i| (format!("/m/f{i:04}"), b"1".to_vec())));
    let mut n2 = Vec::new();
    assert_eq!(
        u::read_tree(&many, "/m", u::MAX_DISCOVER_FILES, &mut n2).unwrap().len(),
        u::MAX_DISCOVER_FILES
    );
    assert!(n2.iter().any(|n| n.contains("file limit")));
    let chunk = vec![b'z'; u::MAX_READ_FILE_BYTES - 1];
    let tot = FakeFs::new((0..40).map(|i| (format!("/t/f{i:02}"), chunk.clone())));
    let mut n3 = Vec::new();
    let got = u::read_tree(&tot, "/t", u::MAX_DISCOVER_FILES, &mut n3).unwrap();
    assert!(got.values().map(Vec::len).sum::<usize>() <= u::MAX_READ_TOTAL_BYTES);
    assert!(n3.iter().any(|n| n.contains("total read cap")));
}

/// Python's parse_frontmatter catches only yamlsafe.YamlError: a loader exception escapes, as it does in Python
/// (here `ValueError` for an impossible date), and render reports it instead of treating the skill as unlabelled.
#[test]
fn loader_exceptions_escape_parse_frontmatter_like_python() {
    let e = u::parse_frontmatter("---\nd: 2024-02-30\n---\nx").err();
    assert_eq!(e.map(|e| e.py_type), Some("ValueError".to_string()));
}
