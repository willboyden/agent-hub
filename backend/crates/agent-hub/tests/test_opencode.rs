//! Port of backend/tests/adapters/test_opencode.py (adapters/opencode.rs), plus the opencode golden cases recorded from
//! Python (tests/golden/adapters.json: 24 renders, 9 discovers, caps/default_config/docs). Owner: WS-A.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod adapter_fakes;

use adapter_fakes::*;
use agent_hub::adapters::_util as u;
use agent_hub::adapters::base::{ClientAdapter, ClientConfig};
use agent_hub::adapters::opencode::OpenCodeAdapter;
use hub_core::value::PyValue;
use serde_json::json;

const ROOT: &str = "/w/proj";

fn adapter() -> OpenCodeAdapter {
    OpenCodeAdapter
}

fn cfg() -> ClientConfig {
    let mut c = adapter().default_config();
    c.roots = roots(&[("project", ROOT)]);
    set_param(&mut c, "sandbox_wrapper", list(&["./run-sandboxed.sh", "--"]));
    set_param(&mut c, "model_tiers", dict(vec![("fast", s("provider/fast-model"))])); // 'standard' unmapped
    c
}

#[test]
fn test_golden() {
    let (n, bad) = render_golden("opencode/sample");
    assert_none("opencode sample", n, &bad);
}

#[test]
fn golden_render_cases_equal_python() {
    let (n, bad) = render_golden("opencode/");
    assert_none("opencode render", n, &bad);
}

#[test]
fn golden_discover_cases_equal_python() {
    let (n, bad) = discover_golden("opencode/");
    assert_none("opencode discover", n, &bad);
}

#[test]
fn golden_meta_equals_python() {
    assert_eq!(meta_golden("opencode"), Vec::<String>::new());
}

#[test]
fn test_deterministic() {
    let ctx = sample_ctx(&cfg());
    assert_eq!(
        adapter().render(&ctx).unwrap(),
        adapter().render(&shuffled(&ctx)).unwrap()
    );
}

#[test]
fn test_skills_off_by_default_and_diag() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    assert!(!res.artifacts.iter().any(|a| a.path.contains("skills")));
    let d: Vec<_> = res
        .diagnostics
        .iter()
        .filter(|x| x.code == "unsupported_capability" && x.message.contains("skills"))
        .collect();
    assert!(!d.is_empty() && d[0].severity == "warn"); // default config is non-strict
    let mut strict = cfg();
    strict.strict = true;
    let res = adapter().render(&sample_ctx(&strict)).unwrap();
    assert!(
        res.diagnostics
            .iter()
            .any(|x| x.severity == "error" && x.message.contains("skills"))
    );
}

#[test]
fn test_skills_via_claude_dir_param() {
    let mut c = cfg();
    set_param(&mut c, "skills_via_claude_dir", PyValue::Bool(true));
    let res = adapter().render(&sample_ctx(&c)).unwrap();
    assert!(
        res.artifacts
            .iter()
            .any(|a| a.path == ".claude/skills/example-skill/SKILL.md")
    );
    assert!(codes(&res).contains("skills_via_claude_dir"));
}

#[test]
fn test_agent_frontmatter_tools_permission() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    let (fm, body) = u::parse_frontmatter(&text(art(&res, ".opencode/agent/reviewer.md"))).unwrap();
    assert_eq!(fm.get("mode"), Some(&s("subagent")));
    assert_eq!(fm.get("temperature"), Some(&PyValue::Float(0.1)));
    assert_eq!(body.trim(), "You review.");
    let tools = fm.get("tools").unwrap();
    assert_eq!(tools.get("write"), Some(&PyValue::Bool(false)));
    assert_eq!(tools.get("edit"), Some(&PyValue::Bool(false)));
    assert!(tools.get("bash").is_none());
    assert_eq!(
        fm.get("permission"),
        Some(&dict(vec![("bash", s("ask")), ("edit", s("deny"))]))
    );
    assert!(!fm.contains_key("model")); // standard tier unmapped
    let (fm2, _) = u::parse_frontmatter(&text(art(&res, ".opencode/agent/builder.md"))).unwrap();
    assert_eq!(fm2.get("model"), Some(&s("provider/fast-model")));
    assert_eq!(fm2.get("permission").unwrap().get("webfetch"), Some(&s("ask")));
    assert!(fm2.get("tools").is_none_or(|t| t.get("write").is_none()));
    assert!(has_diag(&res, "no_model_mapping", Some("info"), Some("reviewer")));
}

#[test]
fn test_strict_vs_lenient_unsupported() {
    let ag = agent("x", "d", &["read", "browser", "web_search"], "b");
    let sevs = |strict: bool| {
        let mut c = cfg();
        c.strict = strict;
        let mut ctx = sample_ctx(&c);
        ctx.agents = vec![ag.clone()];
        let mut v: Vec<String> = adapter()
            .render(&ctx)
            .unwrap()
            .diagnostics
            .into_iter()
            .filter(|d| d.code == "unsupported_capability" && d.item.as_deref() == Some("x"))
            .map(|d| d.severity)
            .collect();
        v.sort();
        v
    };
    assert_eq!(sevs(false), vec!["warn", "warn"]);
    assert_eq!(sevs(true), vec!["error", "error"]);
}

#[test]
fn test_opencode_json_keys() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    let arts: Vec<_> = res.artifacts.iter().filter(|a| a.path == "opencode.json").collect();
    let shape: Vec<(String, Vec<String>)> = arts.iter().map(|a| (a.kind.clone(), a.managed_keys.clone())).collect();
    assert_eq!(
        shape,
        vec![
            ("mcp".into(), vec!["mcp".into()]),
            ("rule".into(), vec!["permission".into()])
        ] // instructions opt-in
    );
    assert!(arts.iter().all(|a| a.merge == "json_keys"));
    let mut doc = json_load(&arts[0].content);
    doc.as_object_mut()
        .unwrap()
        .extend(json_load(&arts[1].content).as_object().unwrap().clone());
    assert_eq!(
        doc["mcp"]["serena"],
        json!({"type": "local", "enabled": true, "command": ["./run-sandboxed.sh", "--", "serena", "start-mcp-server"],
               "environment": {"SERENA_HOME": "{env:SERENA_HOME}"}})
    );
    assert_eq!(doc["mcp"]["docs"]["type"], "remote");
    let bash = &doc["permission"]["bash"];
    assert!(bash["docker compose *"] == "allow" && bash["sudo *"] == "ask" && bash["* **/private-keys/**"] == "deny");
    assert_eq!(bash.as_object().unwrap().keys().next().map(String::as_str), Some("*"));
    assert_eq!(doc["permission"]["webfetch"], "ask");
    let live = json!({"provider": {"litellm": {}}, "model": "m", "mcp": {"old": {}}});
    let merged = u::merge_keys(&live, &doc, &["mcp", "permission"]);
    assert_eq!(merged["provider"], json!({"litellm": {}}));
    assert!(merged["mcp"].get("old").is_none());
}

#[test]
fn test_instruction_files_param_is_opt_in() {
    let mut c = cfg();
    set_param(&mut c, "instruction_files", list(&["docs/b.md", "AGENTS.md"]));
    let res = adapter().render(&sample_ctx(&c)).unwrap();
    let a = res
        .artifacts
        .iter()
        .find(|a| a.kind == "instruction" && a.path == "opencode.json")
        .unwrap();
    assert_eq!(
        json_load(&a.content),
        json!({"instructions": ["AGENTS.md", "docs/b.md"]})
    );
    assert_eq!(a.managed_keys, vec!["instructions"]);
}

#[test]
fn test_agents_md_block() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    let a = art(&res, "AGENTS.md");
    assert!(a.merge == "block" && text(a).starts_with(u::MD_BEGIN));
}

#[test]
fn test_hostile_and_huge() {
    let mut ctx = empty_ctx(&cfg());
    ctx.agents = vec![
        agent("../up", "d", &["read"], "b"),
        agent("ok", &"d".repeat(9000), &["read"], "b"),
    ];
    let res = adapter().render(&ctx).unwrap();
    assert!(res.artifacts.is_empty());
    assert!(codes(&res).contains("invalid_name") && codes(&res).contains("description_too_long"));
}

#[test]
fn test_discover_real_layout_and_round_trip() {
    let cj = json!({"instructions": ["AGENTS.md"],
                    "mcp": {"example": {"type": "local", "enabled": false, "command": ["./run-sandboxed.sh", "--", "npx"]}},
                    "permission": {"edit": "allow", "bash": {"*": "ask", "ls *": "allow", "sudo *": "deny"}}});
    let fs = FakeFs::new([
        (
            format!("{ROOT}/.opencode/agent/reviewer.md"),
            b"---\ndescription: Reviews.\nmode: subagent\ntemperature: 0.1\ntools:\n  write: false\n  edit: false\npermission:\n  bash: ask\n---\n\nYou review.\n".to_vec(),
        ),
        (format!("{ROOT}/opencode.json"), cj.to_string().into_bytes()),
        (format!("{ROOT}/AGENTS.md"), b"# Repo\n".to_vec()),
    ]);
    let d = adapter().discover(&cfg(), &fs).unwrap();
    let ag = &d.agents[0];
    assert!(ag.name == "reviewer" && ag.read_only);
    assert!(!ag.capabilities.iter().any(|c| c == "write" || c == "edit"));
    assert!(ag.capabilities.iter().any(|c| c == "shell") && ag.capabilities.iter().any(|c| c == "read"));
    assert_eq!(d.mcp_servers[0].command.as_deref(), Some("./run-sandboxed.sh"));
    let kinds: Vec<(String, String, String)> = d
        .permissions
        .as_ref()
        .unwrap()
        .rules
        .iter()
        .map(|r| (r.kind.clone(), r.matches.clone(), r.decision.clone()))
        .collect();
    for (k, m, dd) in [
        ("command", "ls *", "allow"),
        ("tool", "shell", "ask"),
        ("tool", "edit", "allow"),
    ] {
        assert!(kinds.contains(&(k.into(), m.into(), dd.into())));
    }
    assert_eq!(d.instructions[0].id, "agents-md");
    let mut ctx = empty_ctx(&cfg());
    ctx.agents = d.agents;
    let res = adapter().render(&ctx).unwrap();
    let (fm, _) = u::parse_frontmatter(&text(&res.artifacts[0])).unwrap();
    assert_eq!(
        fm.get("tools"),
        Some(&dict(vec![
            ("edit", PyValue::Bool(false)),
            ("write", PyValue::Bool(false))
        ]))
    );
    assert_eq!(fm.get("permission").unwrap().get("bash"), Some(&s("ask")));
    assert_eq!(fm.get("description"), Some(&s("Reviews.")));
}

#[test]
fn test_verify() {
    let mut ctx = sample_ctx(&cfg());
    ctx.instructions = vec![instruction("a", "A", 0, "b")];
    let res = adapter().render(&ctx).unwrap();
    let ag = res.artifacts.iter().find(|a| a.kind == "agent").unwrap();
    let fs = FakeFs::new([(format!("{ROOT}/{}", ag.path), ag.content.clone())]);
    let e = [expected(ag)];
    assert!(adapter().verify(&cfg(), &e, &fs, &FakeRunner::default()).unwrap()[0].ok);
    fs.set(&format!("{ROOT}/{}", ag.path), b"other");
    assert!(!adapter().verify(&cfg(), &e, &fs, &FakeRunner::default()).unwrap()[0].ok);
}

#[test]
fn test_unsupported_permission_rules_warn() {
    let mut ctx = empty_ctx(&cfg());
    ctx.permissions = rules(&[("egress_host", "x.org", "allow"), ("path_write", "/etc/**", "allow")]);
    let res = adapter().render(&ctx).unwrap();
    assert_eq!(
        res.diagnostics.iter().filter(|d| d.code == "unsupported_rule").count(),
        2
    );
}

#[test]
fn test_own_mode_agents_md_round_trip_and_suggestion() {
    let whole = b"# Canonical\n\nLots of text: with colons.\n";
    let d = adapter()
        .discover(&cfg(), &FakeFs::new([(format!("{ROOT}/AGENTS.md"), whole.to_vec())]))
        .unwrap();
    assert_eq!(d.suggested_params, vec![("instructions_mode".to_string(), s("own"))]);
    let mut c = cfg();
    set_param(&mut c, "instructions_mode", s("own"));
    let mut ctx = empty_ctx(&c);
    ctx.instructions = d.instructions.clone();
    let res = adapter().render(&ctx).unwrap();
    assert!(res.artifacts[0].merge == "own" && res.artifacts[0].content == whole);
    set_param(&mut c, "instructions_mode", s("block"));
    let mut ctx = empty_ctx(&c);
    ctx.instructions = d.instructions;
    assert_eq!(adapter().render(&ctx).unwrap().artifacts[0].merge, "block");
}
