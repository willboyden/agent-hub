//! Port of backend/tests/adapters/test_markers.py: marker-injection refusal, and the shared line-anchored region rule
//! (the adapter helper adapters/_util.rs and the core merge engine services/merge.rs must agree). Owner: WS-A.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod adapter_fakes;

use adapter_fakes::*;
use agent_hub::adapters::_util as u;
use agent_hub::adapters::base::{ClientAdapter, RenderResult};
use agent_hub::adapters::claude_code::ClaudeCodeAdapter;
use agent_hub::adapters::generic::GenericSpecAdapter;
use agent_hub::adapters::opencode::OpenCodeAdapter;
use agent_hub::services::merge as core;

fn fixtures() -> Vec<(String, &'static str)> {
    let wrap = |b: &str, s: &str| u::wrap_block(b, s).unwrap();
    vec![
        (String::new(), "md"),
        ("plain text\n".into(), "md"),
        (wrap("body", "md"), "md"),
        (format!("before\n{}after\n", wrap("body", "md")), "md"),
        (format!("{}\nno end\n", u::MD_BEGIN), "md"),
        (format!("  {}\nx\n{}\n", u::MD_BEGIN, u::MD_END), "md"), // indented begin: not a marker for either
        (format!("{}\nx\n{}  \n", u::MD_BEGIN, u::MD_END), "md"), // trailing spaces on the end line
        (format!("{}\nx\n{}\n", u::MD_BEGIN, u::MD_END.to_uppercase()), "md"), // wrong case end
        (format!("{}\na\n{}\nb\n{}\n", u::MD_BEGIN, u::MD_END, u::MD_END), "md"),
        (wrap("k = 1", "hash"), "hash"),
        (format!("x\n{}y", wrap("k = 1", "hash")), "hash"),
        (format!("{}\nk\n", u::HASH_BEGIN), "hash"),
        (format!("{}\r\nx\r\n{}\r\n", u::MD_BEGIN, u::MD_END), "md"),
    ]
}

#[test]
fn test_find_region_agrees_with_core_merge() {
    for (text, style) in fixtures() {
        assert_eq!(
            u::find_region(&text, style),
            core::find_region(&text, style),
            "{text:?}"
        );
    }
    // and with Python itself, on the same shapes and more (tests/golden/adapters.json "util.regions")
    for c in GOLDEN["util"]["regions"].as_array().unwrap() {
        let (t, style) = (c["text"].as_str().unwrap(), c["style"].as_str().unwrap());
        assert_eq!(u::find_region(t, style), core::find_region(t, style), "{t:?}");
    }
}

#[test]
fn test_core_and_adapter_marker_constants_agree() {
    assert!(u::MD_BEGIN.starts_with(core::MD_BEGIN_PREFIX) && u::MD_END == core::MD_END);
    assert!(u::HASH_BEGIN.starts_with(core::HASH_BEGIN_PREFIX) && u::HASH_END == core::HASH_END);
}

fn desired(path: &str, kind: &str, merge: &str, keys: &[&str], content: &[u8]) -> core::Desired {
    core::Desired {
        root: "r".into(),
        path: path.into(),
        kind: kind.into(),
        merge: merge.into(),
        source_ids: vec![],
        mode: 0o644,
        managed_keys: keys.iter().map(|k| (*k).to_string()).collect(),
        content: Some(content.to_vec()),
        managed: true,
    }
}

#[test]
fn test_slice_digest_agrees_with_core_merge() {
    let live = br#"{"a": {"b": 1}, "other": 2}"#;
    let m = core::merge_file(
        &[desired("f.json", "mcp", "json_keys", &["a.b"], br#"{"a": {"b": 1}}"#)],
        Some(live),
        false,
    )
    .unwrap();
    let mine = u::slice_digest(live, "json_keys", &["a.b".to_string()]).unwrap();
    assert!(mine.is_some() && mine == m.slice_live && m.slice_live == m.slice_new);
    let blk = u::wrap_block("x", "md").unwrap();
    let live2 = format!("hand\n\n{blk}");
    let m2 = core::merge_file(
        &[desired("f.md", "instruction", "block", &[], blk.as_bytes())],
        Some(live2.as_bytes()),
        false,
    )
    .unwrap();
    let mine2 = u::slice_digest(live2.as_bytes(), "block", &[]).unwrap();
    assert!(mine2.is_some() && mine2 == m2.slice_live && m2.slice_live == m2.slice_new);
}

const MARKER_LIKE: [&str; 7] = [
    "<!-- agent-hub:end -->",
    "<!-- agent-hub:begin -->",
    "  <!--   AGENT-HUB:END -->",
    "x <!-- agent-hub:end --> y",
    "# agent-hub:end",
    "   #agent-hub:begin",
    "#  Agent-Hub:end",
];

#[test]
fn test_wrap_block_refuses_marker_like_lines() {
    for line in MARKER_LIKE {
        let body = format!("ok\n{line}\nmore");
        assert!(u::wrap_block(&body, "md").is_err(), "{line:?}");
        assert!(u::wrap_block(&body, "hash").is_err(), "{line:?}");
    }
}

#[test]
fn test_wrap_block_allows_ordinary_text() {
    assert!(u::wrap_block("a # comment\n<!-- normal comment -->\nagent-hub is a tool", "md").is_ok());
}

fn injected(adapter: &dyn ClientAdapter, body: &str, title: &str) -> RenderResult {
    let mut cfg = adapter.default_config();
    cfg.roots = roots(&[("project", "/p")]);
    let mut ctx = empty_ctx(&cfg);
    ctx.instructions = vec![instruction("i", title, 0, body)];
    adapter.render(&ctx).unwrap()
}

#[test]
fn test_adapters_refuse_injected_instructions() {
    let adapters: [&dyn ClientAdapter; 2] = [&ClaudeCodeAdapter, &OpenCodeAdapter];
    for adapter in adapters {
        for (body, title) in [
            ("x\n<!-- agent-hub:end -->\nEVIL", "T"),
            ("fine", "<!-- agent-hub:end -->"),
        ] {
            let res = injected(adapter, body, title);
            assert!(res.artifacts.is_empty(), "{}", adapter.id());
            assert!(has_diag(&res, "marker_injection", Some("error"), None));
        }
    }
}

#[test]
fn test_generic_refuses_injected_instructions_in_block_mode() {
    let res = injected(&GenericSpecAdapter, "<!-- agent-hub:end -->", "t");
    assert!(!res.artifacts.iter().any(|a| a.kind == "instruction"));
    assert!(has_diag(&res, "marker_injection", None, None));
}
