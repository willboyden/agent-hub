//! Port of backend/tests/adapters/test_claude_code.py (adapters/claude_code.rs), plus the claude-code golden cases
//! recorded from Python (tests/golden/adapters.json: 35 renders, 28 discovers, caps/default_config/docs). Owner: WS-A.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod adapter_fakes;

use adapter_fakes::*;
use agent_hub::adapters::_util as u;
use agent_hub::adapters::base::{AgentItem, ClientAdapter, ClientConfig, Expected, McpItem, PermissionSet};
use agent_hub::adapters::claude_code::ClaudeCodeAdapter;
use hub_core::value::PyValue;
use serde_json::json;

const ROOT: &str = "/w/proj";

fn adapter() -> ClaudeCodeAdapter {
    ClaudeCodeAdapter
}

fn cfg() -> ClientConfig {
    let mut c = adapter().default_config();
    c.roots = roots(&[("project", ROOT)]);
    set_param(&mut c, "sandbox_wrapper", list(&["./run-sandboxed.sh", "--"]));
    c
}

#[test]
fn test_golden() {
    let (n, bad) = render_golden("claude-code/sample");
    assert_none("claude-code sample", n, &bad);
}

#[test]
fn golden_render_cases_equal_python() {
    let (n, bad) = render_golden("claude-code/");
    assert_none("claude-code render", n, &bad);
}

#[test]
fn golden_discover_cases_equal_python() {
    let (n, bad) = discover_golden("claude-code/");
    assert_none("claude-code discover", n, &bad);
}

#[test]
fn golden_meta_equals_python() {
    assert_eq!(meta_golden("claude-code"), Vec::<String>::new());
}

#[test]
fn test_deterministic_regardless_of_input_order() {
    let ctx = sample_ctx(&cfg());
    assert_eq!(
        adapter().render(&ctx).unwrap(),
        adapter().render(&shuffled(&ctx)).unwrap()
    );
}

#[test]
fn test_paths_relative_and_no_absolute_paths_in_artifacts() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    for a in &res.artifacts {
        assert_eq!(u::safe_relpath(&[&a.path]).unwrap(), a.path);
        assert!(!text(a).contains(ROOT) && !text(a).contains("/home/"));
    }
}

#[test]
fn test_default_manage_makes_mcp_and_permissions_advisory() {
    let d = adapter().default_config();
    let m = d.param("manage").unwrap();
    for k in ["skills", "agents", "instructions"] {
        assert_eq!(m.get(k), Some(&PyValue::Bool(true)));
    }
    assert_eq!(m.get("mcp"), Some(&PyValue::Bool(false)));
    assert_eq!(m.get("permissions"), Some(&PyValue::Bool(false)));
}

#[test]
fn test_only_writes_project_level_files() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    assert!(res.artifacts.iter().all(|a| a.root == "project"));
    assert!(
        !res.artifacts
            .iter()
            .any(|a| a.path.starts_with('~') || a.path.get(..2).is_some_and(|p| p.contains("/.")))
    );
}

#[test]
fn test_agent_tools_model_and_read_only() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    let ro = text(art(&res, ".claude/agents/reviewer.md"));
    assert!(ro.contains("tools: Read, Grep, Glob, Bash") && ro.contains("model: sonnet"));
    let full = text(art(&res, ".claude/agents/builder.md"));
    for t in ["Write", "Edit", "WebFetch", "model: haiku"] {
        assert!(full.contains(t));
    }
    let (fm, body) = u::parse_frontmatter(&ro).unwrap();
    assert_eq!(fm.get("name"), Some(&s("reviewer")));
    assert_eq!(body.trim(), "You review.");
}

#[test]
fn test_read_only_removes_write_tools_with_info() {
    let mut ag = agent("r", "d", &["read", "write", "edit"], "b");
    ag.read_only = true;
    let mut ctx = empty_ctx(&cfg());
    ctx.agents = vec![ag];
    let res = adapter().render(&ctx).unwrap();
    let t = text(&res.artifacts[0]);
    assert!(!t.contains("Write") && !t.contains("Edit"));
    assert!(has_diag(&res, "read_only_applied", Some("info"), None));
}

#[test]
fn test_empty_toolset_is_an_error_not_an_inherit_all() {
    let mut ctx = sample_ctx(&cfg());
    ctx.agents = vec![agent("x", "d", &["browser"], "b")];
    let res = adapter().render(&ctx).unwrap();
    assert!(!res.artifacts.iter().any(|a| a.kind == "agent"));
    assert!(codes(&res).contains("empty_toolset") && codes(&res).contains("unsupported_capability"));
}

#[test]
fn test_unsupported_capability_strict_vs_lenient() {
    let ag = agent("x", "d", &["read", "browser"], "b");
    let mut ctx = sample_ctx(&cfg());
    ctx.agents = vec![ag];
    let sev = |strict: bool| {
        let mut c = ctx.clone();
        c.client.strict = strict;
        adapter()
            .render(&c)
            .unwrap()
            .diagnostics
            .iter()
            .filter(|d| d.code == "unsupported_capability" && d.item.as_deref() == Some("x"))
            .map(|d| d.severity.clone())
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(sev(true), ["error".to_string()].into());
    assert_eq!(sev(false), ["warn".to_string()].into());
}

#[test]
fn test_no_model_tier_omits_model_line() {
    let mut ctx = sample_ctx(&cfg());
    ctx.agents = vec![agent("x", "d", &["read"], "b")];
    let res = adapter().render(&ctx).unwrap();
    assert!(!text(res.artifacts.iter().find(|a| a.kind == "agent").unwrap()).contains("model:"));
}

#[test]
fn test_instructions_block_preserves_handwritten_text() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    let a = art(&res, "CLAUDE.md");
    assert_eq!(a.merge, "block");
    let t = text(a);
    assert!(t.find("## First").unwrap() < t.find("## Second").unwrap()); // ordered by (order, id)
    let merged = u::replace_block("# My notes\n\nkeep me\n", &t, "md");
    assert!(merged.contains("keep me") && merged.contains("## First"));
    let again = u::replace_block(&merged, &u::wrap_block("## Only\n\nnew", "md").unwrap(), "md");
    assert!(again.contains("keep me") && !again.contains("## First"));
    assert_eq!(again.matches("agent-hub:begin").count(), 1);
}

#[test]
fn test_mcp_json_shape_and_merge_keeps_other_servers() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    let a = art(&res, ".mcp.json");
    assert_eq!(
        (a.merge.as_str(), a.managed_keys.clone()),
        ("json_keys", vec!["mcpServers".to_string()])
    );
    let doc = json_load(&a.content);
    let srv = &doc["mcpServers"]["serena"];
    assert_eq!(srv["command"], "./run-sandboxed.sh");
    assert_eq!(srv["args"][0], "--");
    assert_eq!(srv["args"][1], "serena");
    assert_eq!(srv["env"], json!({"SERENA_HOME": "${SERENA_HOME}"})); // names only
    assert_eq!(
        doc["mcpServers"]["docs"],
        json!({"type": "http", "url": "http://mcpo:8000/docs"})
    );
    assert!(has_diag(&res, "mcp_not_clean", None, Some("docs")));
    let merged = u::merge_keys(&json!({"mcpServers": {"old": {}}, "other": 1}), &doc, &["mcpServers"]);
    assert_eq!(merged["other"], 1);
    assert!(merged["mcpServers"].get("old").is_none());
}

#[test]
fn test_permissions_render_and_merge_keeps_hooks() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    let a = art(&res, ".claude/settings.json");
    assert_eq!(
        a.managed_keys,
        vec!["permissions.allow", "permissions.ask", "permissions.deny"]
    );
    let p = json_load(&a.content)["permissions"].clone();
    assert_eq!(
        p["allow"],
        json!(["Bash(docker compose:*)", "WebFetch(domain:pypi.org)"])
    );
    assert_eq!(p["ask"], json!(["Bash(sudo:*)", "WebFetch"]));
    assert_eq!(p["deny"], json!(["Read(**/private-keys/**)"]));
    let live = json!({"hooks": {"PreToolUse": []}, "permissions": {"allow": ["Bash(ls)"], "deny": ["x"], "other": 1}});
    let keys: Vec<&str> = a.managed_keys.iter().map(String::as_str).collect();
    let merged = u::merge_keys(&live, &json_load(&a.content), &keys);
    assert_eq!(merged["hooks"], json!({"PreToolUse": []}));
    assert_eq!(merged["permissions"]["other"], 1);
    assert_eq!(merged["permissions"]["allow"], p["allow"]);
    assert!(has_diag(&res, "egress_advisory", None, None));
}

#[test]
fn test_hostile_names_and_paths_are_refused() {
    let bad_skill = skill_with("ok-name", DESC, &[("../../evil", b"x")]);
    let mut ctx = empty_ctx(&cfg());
    ctx.skills = vec![skill("../../etc/x"), skill("A B"), bad_skill];
    ctx.agents = vec![
        agent("../../x", "d", &["read"], "b"),
        agent("Fine", "d", &["read"], "b"),
    ];
    let res = adapter().render(&ctx).unwrap();
    assert!(res.artifacts.is_empty());
    assert!(res.diagnostics.iter().filter(|d| d.code == "invalid_name").count() >= 3);
    assert!(codes(&res).contains("unsafe_path"));
}

#[test]
fn test_huge_description_and_body() {
    let mut ctx = empty_ctx(&cfg());
    ctx.skills = vec![skill_with("big", &"x".repeat(9000), &[])];
    ctx.agents = vec![
        agent("warned", &"y".repeat(2000), &["read"], "b"),
        agent("huge-body", "d", &["read"], &"z".repeat(u::MAX_BODY_CHARS + 1)),
    ];
    let res = adapter().render(&ctx).unwrap();
    let paths: Vec<&str> = res.artifacts.iter().map(|a| a.path.as_str()).collect();
    assert!(paths.contains(&".claude/agents/warned.md") && !paths.contains(&".claude/agents/huge-body.md"));
    assert!(!paths.iter().any(|p| p.contains("skills/big")));
    for c in ["description_too_long", "description_long", "body_too_large"] {
        assert!(codes(&res).contains(c), "{c}");
    }
}

#[test]
fn test_frontmatter_injection_in_description_is_quoted() {
    let mut ctx = sample_ctx(&cfg());
    ctx.agents = vec![agent("inj", "ok\n---\ntools: Bash, Write\nmodel: opus", &["read"], "b")];
    let res = adapter().render(&ctx).unwrap();
    let (fm, _) = u::parse_frontmatter(&text(res.artifacts.iter().find(|a| a.kind == "agent").unwrap())).unwrap();
    assert_eq!(fm.get("tools"), Some(&s("Read, Grep, Glob")));
    assert!(!fm.contains_key("model"));
}

#[test]
fn test_duplicate_names_are_errors() {
    let mut ctx = sample_ctx(&cfg());
    ctx.skills = vec![skill("dup"), skill("dup")];
    let res = adapter().render(&ctx).unwrap();
    assert!(codes(&res).contains("duplicate_name"));
    assert!(!res.artifacts.iter().any(|a| a.path.contains("skills/dup")));
}

#[test]
fn test_memory_unsupported_diag() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    assert!(
        res.diagnostics
            .iter()
            .any(|d| d.code == "unsupported_capability" && d.message.contains("memory"))
    );
}

// ---- discover / round trip / verify ------------------------------------------------------------------------------------
fn real_layout() -> FakeFs {
    let settings = json!({"permissions": {"deny": ["Read(./**/secrets/**)"], "ask": ["Bash(curl:*)", "WebFetch"],
                                          "allow": ["Bash(docker compose:*)"]}, "hooks": {}});
    let mcp = json!({"mcpServers": {"serena": {"command": "serena", "args": ["a"], "env": {"K": "secret-value"}}}});
    FakeFs::new([
        (format!("{ROOT}/.claude/settings.json"), settings.to_string().into_bytes()),
        (
            format!("{ROOT}/.claude/agents/reviewer.md"),
            b"---\nname: reviewer\ndescription: Use when broken.\ntools: Read, Grep, Glob, Bash\nmodel: sonnet\n---\n\nYou review.\n".to_vec(),
        ),
        (
            format!("{ROOT}/.claude/skills/example-skill/SKILL.md"),
            b"---\nname: example-skill\ndescription: Bring up a model.\n---\n\n# Serve\n".to_vec(),
        ),
        (format!("{ROOT}/.claude/skills/example-skill/ref/notes.md"), b"notes".to_vec()),
        (format!("{ROOT}/.claude/skills/Bad Name/SKILL.md"), b"x".to_vec()),
        (format!("{ROOT}/CLAUDE.md"), b"# Hand written\n".to_vec()),
        (format!("{ROOT}/.mcp.json"), mcp.to_string().into_bytes()),
    ])
}

#[test]
fn test_discover_real_layout() {
    let d = adapter().discover(&cfg(), &real_layout()).unwrap();
    assert_eq!(
        d.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["example-skill"]
    );
    assert!(d.skills[0].files.contains_key("ref/notes.md"));
    assert_eq!(d.skills[0].description, "Bring up a model.");
    let ag = &d.agents[0];
    assert!(ag.name == "reviewer" && ag.capabilities == ["read", "shell"]);
    assert!(ag.model_tier.as_deref() == Some("standard") && ag.read_only);
    assert_eq!(d.instructions[0].id, "claude-md");
    let kinds: Vec<(String, String)> = d
        .permissions
        .as_ref()
        .unwrap()
        .rules
        .iter()
        .map(|r| (r.kind.clone(), r.decision.clone()))
        .collect();
    for want in [("command", "allow"), ("path_read", "deny"), ("tool", "ask")] {
        assert!(kinds.contains(&(want.0.to_string(), want.1.to_string())));
    }
    assert_eq!(d.mcp_servers[0].env_names, vec!["K"]);
    assert!(!format!("{d:?}").contains("secret-value"));
    assert!(d.notes.iter().any(|n| n.contains("Bad Name")));
}

#[test]
fn test_round_trip_discover_then_render_reproduces_files() {
    let fs = real_layout();
    let d = adapter().discover(&cfg(), &fs).unwrap();
    let mut ctx = empty_ctx(&cfg());
    ctx.skills = d.skills;
    ctx.agents = d.agents;
    let res = adapter().render(&ctx).unwrap();
    let got = |p: &str| art(&res, p).content.clone();
    let orig = |p: &str| fs.files.lock().unwrap()[&format!("{ROOT}/{p}")].clone();
    assert_eq!(got(".claude/agents/reviewer.md"), orig(".claude/agents/reviewer.md"));
    assert_eq!(
        got(".claude/skills/example-skill/SKILL.md"),
        orig(".claude/skills/example-skill/SKILL.md")
    );
    assert_eq!(got(".claude/skills/example-skill/ref/notes.md"), b"notes".to_vec());
}

#[test]
fn test_round_trip_permissions() {
    let d = adapter().discover(&cfg(), &real_layout()).unwrap();
    let mut ctx = empty_ctx(&cfg());
    ctx.permissions = d.permissions.unwrap();
    let res = adapter().render(&ctx).unwrap();
    assert_eq!(
        json_load(&res.artifacts[0].content)["permissions"],
        json!({"allow": ["Bash(docker compose:*)"], "ask": ["Bash(curl:*)", "WebFetch"], "deny": ["Read(./**/secrets/**)"]})
    );
}

#[test]
fn test_discover_empty_and_missing_roots() {
    let d = adapter().discover(&cfg(), &FakeFs::default()).unwrap();
    assert!(d.skills.is_empty() && d.agents.is_empty() && d.permissions.is_none());
    let mut cfg2 = cfg();
    cfg2.roots = roots(&[]);
    assert!(adapter().discover(&cfg2, &real_layout()).unwrap().skills.is_empty());
}

#[test]
fn test_verify_hash_missing_and_tampered() {
    let mut ctx = sample_ctx(&cfg());
    (ctx.agents, ctx.instructions, ctx.mcp_servers, ctx.memory) = (vec![], vec![], vec![], vec![]);
    ctx.permissions = PermissionSet::default();
    let res = adapter().render(&ctx).unwrap();
    let fs = FakeFs::new(tree(&res, ROOT));
    let exp: Vec<Expected> = res.artifacts.iter().map(expected).collect();
    let run = FakeRunner::default();
    assert!(adapter().verify(&cfg(), &exp, &fs, &run).unwrap().iter().all(|c| c.ok));
    fs.set(
        &format!("{ROOT}/.claude/skills/example-other-skill/SKILL.md"),
        b"tampered",
    );
    fs.remove(&format!("{ROOT}/.claude/skills/example-skill/SKILL.md"));
    let checks = adapter().verify(&cfg(), &exp, &fs, &run).unwrap();
    let by = |n: &str| checks.iter().find(|c| c.name == n).unwrap().clone();
    assert!(!by("project:.claude/skills/example-other-skill/SKILL.md").ok);
    assert_eq!(
        by("project:.claude/skills/example-skill/SKILL.md").detail,
        "file missing"
    );
}

#[test]
fn test_verify_block_artifact_by_marker_region() {
    let mut ctx = empty_ctx(&cfg());
    ctx.instructions = sample_ctx(&cfg()).instructions;
    let res = adapter().render(&ctx).unwrap();
    let a = &res.artifacts[0];
    let mut live = b"hand\n\n".to_vec();
    live.extend(&a.content);
    live.extend(b"\ntrailer\n");
    let fs = FakeFs::new([(format!("{ROOT}/CLAUDE.md"), live)]);
    let mut e = expected(a);
    e.merge = "block".into();
    assert!(adapter().verify(&cfg(), &[e], &fs, &FakeRunner::default()).unwrap()[0].ok);
}

#[test]
fn test_caps_shape() {
    let c = adapter().caps();
    assert_eq!(c.tool_map["read"].as_deref(), Some("Read"));
    assert_eq!(c.tool_map["browser"], None);
    assert_eq!(c.tool_map["subagent"].as_deref(), Some("Task"));
    assert_eq!(
        serde_json::to_value(&c.model_tiers).unwrap(),
        json!({"fast": "haiku", "standard": "sonnet", "deep": "opus"})
    );
    assert!(!c.memory);
}

#[test]
fn test_unknown_rule_and_mcp_errors() {
    let mut ctx = sample_ctx(&cfg());
    ctx.mcp_servers = vec![mcp("nocmd", "stdio"), mcp("nourl", "sse")];
    ctx.permissions = rules(&[("tool", "browser", "allow")]);
    let res = adapter().render(&ctx).unwrap();
    for c in ["mcp_no_command", "mcp_no_url", "unsupported_rule"] {
        assert!(codes(&res).contains(c), "{c}");
    }
}

#[test]
fn test_instruction_only_ctx() {
    let mut ctx = empty_ctx(&cfg());
    ctx.instructions = vec![instruction("x", "T", 0, "B")];
    let res = adapter().render(&ctx).unwrap();
    assert_eq!(
        res.artifacts.iter().map(|a| a.path.as_str()).collect::<Vec<_>>(),
        vec!["CLAUDE.md"]
    );
}

// ---- instructions_mode / suggested_params / yaml safety --------------------------------------------------------------------
const WHOLE: &[u8] = b"# Repo rules\n\nBe careful.\n\n## Section\n\ntext without trailing newline";

#[test]
fn test_own_mode_whole_file_round_trips_byte_identical() {
    let fs = FakeFs::new([(format!("{ROOT}/CLAUDE.md"), WHOLE.to_vec())]);
    let d = adapter().discover(&cfg(), &fs).unwrap();
    assert_eq!(d.suggested_params, vec![("instructions_mode".to_string(), s("own"))]);
    let mut c = cfg();
    for (k, v) in &d.suggested_params {
        set_param(&mut c, k, v.clone());
    }
    let mut ctx = empty_ctx(&c);
    ctx.instructions = d.instructions;
    let res = adapter().render(&ctx).unwrap();
    assert_eq!(res.artifacts[0].merge, "own");
    assert_eq!(res.artifacts[0].content, WHOLE);
}

#[test]
fn test_own_mode_several_and_default_block() {
    let mut c = cfg();
    assert_eq!(
        adapter()
            .render(&sample_ctx(&c))
            .unwrap()
            .artifacts
            .last()
            .unwrap()
            .merge,
        "block"
    );
    set_param(&mut c, "instructions_mode", s("own"));
    let res = adapter().render(&sample_ctx(&c)).unwrap();
    let a = art(&res, "CLAUDE.md");
    assert_eq!(a.merge, "own");
    assert_eq!(text(a), "## First\n\nOne.\n\n## Second\n\nTwo.\n");
    set_param(&mut c, "instructions_mode", s("bogus"));
    assert!(codes(&adapter().render(&sample_ctx(&c)).unwrap()).contains("invalid_param"));
}

#[test]
fn test_marker_block_import_suggests_nothing() {
    let fs = FakeFs::new([(
        format!("{ROOT}/CLAUDE.md"),
        u::wrap_block("x", "md").unwrap().into_bytes(),
    )]);
    assert!(adapter().discover(&cfg(), &fs).unwrap().suggested_params.is_empty());
}

#[test]
fn test_every_emitted_frontmatter_is_valid_yaml_and_round_trips() {
    use agent_hub::adapters::opencode::OpenCodeAdapter;
    let descs = [
        "Use when: a thing # not a comment",
        "- leading dash",
        "@at & *star !bang",
        "'quoted' \"double\"",
        "multi\nline\n---\nx",
        "{brace}: [x]",
        "%pct",
        "  padded  ",
    ];
    let ags: Vec<AgentItem> = descs
        .iter()
        .enumerate()
        .map(|(i, d)| agent(&format!("a{i}"), d, &["read"], "b"))
        .collect();
    let mut ctx = empty_ctx(&cfg());
    ctx.agents = ags.clone();
    let res = adapter().render(&ctx).unwrap();
    let oc = OpenCodeAdapter;
    let mut ctx2 = empty_ctx(&oc.default_config());
    ctx2.agents = ags;
    let res2 = oc.render(&ctx2).unwrap();
    for r in [res, res2] {
        let agents: Vec<_> = r.artifacts.iter().filter(|a| a.kind == "agent").collect();
        assert_eq!(agents.len(), descs.len());
        for (a, d) in agents.iter().zip(descs) {
            let t = text(a);
            let head = t.split_once("\n---\n").unwrap().0.strip_prefix("---\n").unwrap();
            let doc = yaml_load(head.as_bytes());
            assert_eq!(
                doc.get("description"),
                Some(&PyValue::str(hub_core::py::text::collapse_ws(d)))
            );
        }
    }
}

#[test]
fn test_sandbox_profile_without_wrapper_fails_closed() {
    let mut c = cfg();
    set_param(&mut c, "sandbox_wrapper", PyValue::List(vec![]));
    let res = adapter().render(&sample_ctx(&c)).unwrap();
    assert!(
        json_load(&art(&res, ".mcp.json").content)["mcpServers"]
            .get("serena")
            .is_none()
    );
    assert!(has_diag(&res, "sandbox_wrapper_missing", Some("error"), Some("serena")));
}

#[test]
fn mcp_items_from_discover_keep_pydantic_defaults() {
    let d = adapter().discover(&cfg(), &real_layout()).unwrap();
    let m: &McpItem = &d.mcp_servers[0];
    assert!(m.scan_status == "unscanned" && m.sandbox_profile.is_none() && m.egress_hosts.is_empty());
    assert_eq!(d.agents[0].mode, "subagent");
}
