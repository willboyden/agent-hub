//! Port of backend/tests/adapters/test_generic.py (adapters/generic.rs), plus the generic golden cases recorded from
//! Python (tests/golden/adapters.json: 42 renders, 5 discovers, 470 spec validations with their caps, the substituter
//! and the TOML writer). Owner: WS-A.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod adapter_fakes;

use adapter_fakes::*;
use agent_hub::adapters::_util as u;
use agent_hub::adapters::base::{ClientAdapter, ClientConfig, Expected, McpItem, PermissionSet};
use agent_hub::adapters::generic::{
    self as g, GenericSpecAdapter, Vals, caps_for_spec, dry_render, subst, toml_tables, validate_spec,
};
use hub_core::value::PyValue;
use serde_json::{Value, json};

const ROOT: &str = "/w/acme";

fn adapter() -> GenericSpecAdapter {
    GenericSpecAdapter
}

/// A brand-new client that exists ONLY as data: no module, no registry change.
fn acme() -> Value {
    json!({
        "version": 1,
        "tool_map": {"read": "fs.read", "write": "fs.write", "shell": "sh", "web_fetch": null},
        "model_tiers": {"fast": "acme-small", "deep": "acme-large"},
        "skills": {"root": "project", "path": ".acme/skills", "layout": "dir_per_skill", "filename": "SKILL.md"},
        "agents": {"root": "project", "path": ".acme/agents", "filename": "{name}.json", "format": "json",
                   "fields": {"id": "name", "about": "description", "prompt": "body"}, "static": {"kind": "helper"},
                   "tools_field": "allowedTools", "tools_format": "bool_map", "model_field": "model"},
        "instructions": {"root": "project", "file": "ACME.md", "mode": "block"},
        "mcp": {"root": "project", "file": ".acme/mcp.yaml", "format": "yaml", "key_path": "tools.mcp",
                "wrapper": ["./run-sandboxed.sh", "--"],
                "entry": {"stdio": {"exec": "{argv}", "environment": "{env}", "label": "srv-{name}"},
                          "http": {"endpoint": "{url}"}}},
        "permissions": {"root": "project", "file": ".acme/policy.json", "format": "json",
                        "lists": {"allow": "rules.allow", "deny": "rules.deny"},
                        "rule_templates": {"command": "shell:{match}", "tool": "tool:{tool}",
                                           "path_read": ["read:{match}", "stat:{match}"]}},
        "memory": {"root": "project", "path": ".acme/memory", "filename": "{id}.md"},
    })
}

fn acme_cfg_with(spec: &Value) -> ClientConfig {
    ClientConfig {
        id: "acme".into(),
        adapter: "generic".into(),
        display_name: "Acme".into(),
        roots: roots(&[("project", ROOT)]),
        spec: Some(dec(spec)),
        strict: false,
        ..ClientConfig::default()
    }
}

fn acme_cfg() -> ClientConfig {
    acme_cfg_with(&acme())
}

fn messages(spec: &Value) -> Vec<String> {
    validate_spec(&dec(spec)).into_iter().map(|d| d.message).collect()
}

fn bad(mutate: impl FnOnce(&mut Value)) -> Vec<String> {
    let mut sp = acme();
    mutate(&mut sp);
    messages(&sp)
}

#[test]
fn golden_render_cases_equal_python() {
    let (n, bad) = render_golden("generic/");
    assert_none("generic render", n, &bad);
}

#[test]
fn golden_discover_cases_equal_python() {
    let (n, bad) = discover_golden("generic/");
    assert_none("generic discover", n, &bad);
}

#[test]
fn golden_meta_equals_python() {
    assert_eq!(meta_golden("generic"), Vec::<String>::new());
}

#[test]
fn golden_spec_validation_and_caps_equal_python() {
    let cases = GOLDEN["spec"].as_array().unwrap();
    let mut failures = Vec::new();
    for c in cases {
        let spec = dec(&c["spec"]);
        let diags = serde_json::to_value(validate_spec(&spec)).unwrap();
        if diags != c["diagnostics"] {
            failures.push(format!(
                "{}: diags rust {diags} != python {}",
                c["name"], c["diagnostics"]
            ));
        }
        let caps = caps_for_spec(&spec).unwrap();
        let got = if c["caps"].is_null() {
            (caps.notes == "invalid spec" && !caps.skills && caps.tool_map.is_empty()).then(|| Value::Null)
        } else {
            Some(serde_json::to_value(&caps).unwrap())
        };
        if got.as_ref() != Some(&c["caps"]) {
            failures.push(format!("{}: caps rust {got:?} != python {}", c["name"], c["caps"]));
        }
    }
    assert_none("spec validation", cases.len(), &failures);
}

#[test]
fn golden_subst_equals_python() {
    let mut vals: Vals = Vals::new();
    vals.insert("name", s("N"));
    vals.insert("args", list(&["1", "2"]));
    vals.insert("env", dict(vec![("A", s("${A}"))]));
    vals.insert("empty", dict(vec![]));
    vals.insert("elist", PyValue::List(vec![]));
    vals.insert("none", PyValue::None);
    vals.insert("num", s("7"));
    for c in GOLDEN["util"]["subst"].as_array().unwrap() {
        let got = match subst(&dec(&c["obj"]), &vals) {
            Ok(None) => json!({"drop": true}),
            Ok(Some(v)) => json!({"value": enc(&v)}),
            Err(e) => json!({"error": e.message}),
        };
        assert_eq!(got, c["out"], "{}", c["obj"]);
    }
}

#[test]
fn golden_toml_tables_bytes_equal_python() {
    for c in GOLDEN["util"]["toml_tables"].as_array().unwrap() {
        let servers: Vec<(String, PyValue)> = match dec(&c["servers"]) {
            PyValue::Dict(d) => d.into_iter().map(|(k, v)| (k.py_str(), v)).collect(),
            other => panic!("{other:?}"),
        };
        let got = toml_tables(c["prefix"].as_str().unwrap(), &servers).unwrap();
        assert_eq!(got, c["text"].as_str().unwrap());
    }
}

#[test]
fn test_brand_new_client_purely_via_spec_end_to_end() {
    assert!(messages(&acme()).is_empty());
    let (n, bad) = render_golden("generic/acme");
    assert_none("generic_acme", n, &bad);
    let res = adapter().render(&sample_ctx(&acme_cfg())).unwrap();
    let paths: std::collections::BTreeSet<&str> = res.artifacts.iter().map(|a| a.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            ".acme/agents/builder.json",
            ".acme/agents/reviewer.json",
            ".acme/mcp.yaml",
            ".acme/memory/m1.md",
            ".acme/policy.json",
            ".acme/skills/example-skill/SKILL.md",
            ".acme/skills/example-skill/scripts/run.sh",
            ".acme/skills/example-other-skill/SKILL.md",
            "ACME.md"
        ]
        .into()
    );
    let agent = json_load(&art(&res, ".acme/agents/reviewer.json").content);
    assert_eq!(
        agent["allowedTools"],
        json!({"fs.read": true, "fs.write": false, "sh": true})
    );
    assert!(
        agent
            .get("model")
            .is_none_or(|m| m == "acme-small" || m == "acme-large")
    );
    let mcp = yaml_load(&art(&res, ".acme/mcp.yaml").content);
    let serena = mcp
        .get("tools")
        .and_then(|t| t.get("mcp"))
        .and_then(|m| m.get("serena"))
        .unwrap();
    assert_eq!(
        enc(serena),
        json!({"exec": ["./run-sandboxed.sh", "--", "serena", "start-mcp-server"],
               "environment": {"SERENA_HOME": "${SERENA_HOME}"}, "label": "srv-serena"})
    );
    let m = art(&res, ".acme/mcp.yaml");
    assert!(m.merge == "yaml_keys" && m.managed_keys == ["tools.mcp"]);
    let pol = json_load(&art(&res, ".acme/policy.json").content)["rules"].clone();
    assert_eq!(pol["allow"], json!(["shell:docker compose *"]));
    assert_eq!(
        pol["deny"],
        json!(["read:**/private-keys/**", "stat:**/private-keys/**"])
    );
    assert_eq!(art(&res, "ACME.md").merge, "block");
    assert!(has_diag(&res, "unsupported_capability", None, Some("builder"))); // web_fetch -> None
    assert!(codes(&res).contains("unsupported_rule")); // ask/egress not templated / no list
}

#[test]
fn test_deterministic_and_no_absolute_paths() {
    let ctx = sample_ctx(&acme_cfg());
    let a = adapter().render(&ctx).unwrap();
    assert_eq!(a, adapter().render(&shuffled(&ctx)).unwrap());
    for x in &a.artifacts {
        assert_eq!(u::safe_relpath(&[&x.path]).unwrap(), x.path);
        assert!(!text(x).contains(ROOT));
    }
}

#[test]
fn test_dry_render_ignores_client_spec_and_validates() {
    let mut cfg = acme_cfg();
    cfg.spec = None;
    let ctx = sample_ctx(&cfg);
    assert!(!dry_render(&dec(&acme()), &ctx).unwrap().artifacts.is_empty());
    assert!(
        dry_render(&dec(&json!({"bogus": 1})), &ctx)
            .unwrap()
            .artifacts
            .is_empty()
    );
}

#[test]
fn test_round_trip_skills_agents_via_discover() {
    let mut sp = acme();
    sp["agents"] = json!({"root": "project", "path": ".acme/agents", "filename": "{name}.md", "format": "md_frontmatter",
                          "fields": {"name": "name", "description": "description"}, "tools_field": "tools",
                          "tools_format": "list"});
    let cfg = acme_cfg_with(&sp);
    let mut ctx = sample_ctx(&cfg);
    (ctx.memory, ctx.instructions, ctx.mcp_servers) = (vec![], vec![], vec![]);
    ctx.permissions = PermissionSet::default();
    let res = adapter().render(&ctx).unwrap();
    let fs = FakeFs::new(all_files(&res, ROOT));
    let d = adapter().discover(&cfg, &fs).unwrap();
    let mut names: Vec<&str> = d.skills.iter().map(|x| x.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["example-other-skill", "example-skill"]);
    let caps: std::collections::BTreeMap<&str, Vec<String>> = d
        .agents
        .iter()
        .map(|a| (a.name.as_str(), a.capabilities.clone()))
        .collect();
    assert_eq!(caps["builder"], vec!["read", "write", "shell"]);
    assert_eq!(caps["reviewer"], vec!["read", "shell"]);
    let mut ctx2 = ctx;
    ctx2.skills = d.skills;
    ctx2.agents = d.agents;
    let res2 = adapter().render(&ctx2).unwrap();
    let skills = |r: &agent_hub::adapters::base::RenderResult| -> Vec<(String, Vec<u8>)> {
        r.artifacts
            .iter()
            .filter(|a| a.kind == "skill")
            .map(|a| (a.path.clone(), a.content.clone()))
            .collect()
    };
    assert_eq!(skills(&res2), skills(&res));
}

// ---- validation: unknown keys and precise paths --------------------------------------------------------------------------
#[test]
fn test_unknown_keys_are_errors_with_paths() {
    assert!(bad(|s| s["skills"]["layoutt"] = json!("x")).contains(&"spec.skills.layoutt: unknown key".to_string()));
    assert!(bad(|s| s["wat"] = json!(1)).contains(&"spec.wat: unknown key".to_string()));
    assert!(
        bad(|s| s["agents"]["format"] = json!("xml"))
            .iter()
            .any(|m| m.starts_with("spec.agents.format:"))
    );
    assert!(messages(&json!({"skills": "nope"}))[0].starts_with("spec.skills:"));
    assert_eq!(messages(&json!([])), vec!["spec: must be a mapping"]);
}

#[test]
fn test_semantic_errors_have_precise_paths() {
    let any = |msgs: Vec<String>, f: &dyn Fn(&str) -> bool| msgs.iter().any(|m| f(m));
    assert!(any(bad(|s| s["tool_map"]["telepathy"] = json!("x")), &|m| m
        .contains("spec.tool_map.telepathy")));
    assert!(any(bad(|s| s["model_tiers"]["huge"] = json!("x")), &|m| m
        .contains("spec.model_tiers.huge")));
    assert!(any(bad(|s| s["skills"]["path"] = json!("../x")), &|m| m
        .starts_with("spec.skills.path: unsafe relative path")));
    assert!(any(bad(|s| s["instructions"]["file"] = json!("/etc/passwd")), &|m| m
        .starts_with("spec.instructions.file: unsafe")));
    assert!(any(bad(|s| s["agents"]["fields"]["x"] = json!("secrets")), &|m| m
        .starts_with("spec.agents.fields.x: unknown source")));
    assert!(any(
        bad(|s| {
            s["agents"]["fields"].as_object_mut().unwrap().remove("prompt");
        }),
        &|m| m.contains("need one field mapped to 'body'")
    ));
    assert!(any(
        bad(|s| s["mcp"]["entry"]["stdio"]["exec"] = json!("{shell}")),
        &|m| m.starts_with("spec.mcp.entry.stdio.exec: unknown placeholder {shell}")
    ));
    assert!(any(bad(|s| s["mcp"]["entry"]["grpc"] = json!({})), &|m| m
        .contains("spec.mcp.entry.grpc")));
    assert!(any(bad(|s| s["mcp"]["key_path"] = json!("a b")), &|m| m
        .contains("spec.mcp.key_path")));
    assert!(any(bad(|s| s["permissions"]["lists"]["maybe"] = json!("x")), &|m| m
        .contains("spec.permissions.lists.maybe")));
    assert!(any(
        bad(|s| s["permissions"]["rule_templates"]["command"] = json!("{cmd}")),
        &|m| m.contains("spec.permissions.rule_templates.command") && m.contains("{cmd}")
    ));
    assert!(any(bad(|s| s["memory"]["filename"] = json!("fixed.md")), &|m| m
        .contains("spec.memory.filename")));
}

#[test]
fn test_templating_is_not_format_string_or_eval() {
    for evil in [
        "{0.__class__}",
        "{name.__class__}",
        "{name!r}",
        "{name:>99}",
        "{{name}}",
        "${name}",
        "{__import__}",
    ] {
        let msgs = bad(|s| s["mcp"]["entry"]["stdio"]["exec"] = json!(evil));
        if evil == "${name}" {
            // ${name} is a literal $ + {name}
            assert!(
                msgs.is_empty()
                    || msgs
                        .iter()
                        .any(|m| m.contains("stray") || m.contains("unknown placeholder"))
            );
        } else {
            assert!(!msgs.is_empty(), "{evil}");
        }
    }
    // the substituter only touches allow-listed names and never evaluates
    let mut vals: Vals = Vals::new();
    vals.insert("name", s("N"));
    vals.insert("args", list(&["1"]));
    vals.insert("env", dict(vec![]));
    let obj = dec(&json!({"a": "x{name}y", "b": ["{args}", "{missing}"], "c": "{env}"}));
    assert_eq!(
        enc(&subst(&obj, &vals).unwrap().unwrap()),
        json!({"a": "xNy", "b": [["1"]]})
    );
    let err = subst(&s("pre-{args}"), &vals).err().unwrap();
    assert_eq!(err.py_type, "ValueError");
}

#[test]
fn test_hostile_names_through_templates() {
    let mut ctx = empty_ctx(&acme_cfg());
    ctx.skills = vec![skill("../x")];
    ctx.agents = vec![agent("a/../../b", "d", &["read"], "b")];
    ctx.mcp_servers = vec![McpItem {
        command: Some("c".into()),
        ..mcp("{name}", "stdio")
    }];
    let res = adapter().render(&ctx).unwrap();
    assert!(
        res.artifacts
            .iter()
            .all(|a| u::safe_relpath(&[&a.path]).ok().as_deref() == Some(a.path.as_str()))
    );
    assert!(res.diagnostics.iter().filter(|d| d.code == "invalid_name").count() >= 3);
}

#[test]
fn test_env_command_values_cannot_inject_structure() {
    let m = McpItem {
        command: Some("c\"; rm -rf /; \"".into()),
        args: vec!["{name}".into(), "${X}".into()],
        env_names: vec!["A".into()],
        ..mcp("s", "stdio")
    };
    let mut ctx = empty_ctx(&acme_cfg());
    ctx.mcp_servers = vec![m];
    let res = adapter().render(&ctx).unwrap();
    let doc = yaml_load(&res.artifacts[0].content);
    let srv = doc
        .get("tools")
        .and_then(|t| t.get("mcp"))
        .and_then(|x| x.get("s"))
        .unwrap();
    // data stays data, never re-substituted
    assert_eq!(
        enc(srv.get("exec").unwrap()),
        json!(["c\"; rm -rf /; \"", "{name}", "${X}"])
    );
}

#[test]
fn test_missing_root_and_missing_section_diags() {
    let mut cfg = acme_cfg();
    cfg.roots = roots(&[("other", "/x")]);
    let res = adapter().render(&sample_ctx(&cfg)).unwrap();
    assert!(codes(&res).contains("root_missing") && res.artifacts.is_empty());
    let mut cfg2 = acme_cfg_with(&json!({"version": 1, "instructions": acme()["instructions"]}));
    cfg2.strict = true;
    let r2 = adapter().render(&sample_ctx(&cfg2)).unwrap();
    let errs: Vec<_> = r2
        .diagnostics
        .iter()
        .filter(|d| d.code == "unsupported_capability")
        .collect();
    assert!(errs.len() >= 5 && errs.iter().all(|d| d.severity == "error"));
    cfg2.strict = false;
    let r3 = adapter().render(&sample_ctx(&cfg2)).unwrap();
    assert!(
        r3.diagnostics
            .iter()
            .filter(|d| d.code == "unsupported_capability")
            .all(|d| d.severity == "warn")
    );
}

#[test]
fn test_invalid_spec_render_returns_only_errors() {
    let cfg = ClientConfig {
        id: "x".into(),
        adapter: "generic".into(),
        display_name: "x".into(),
        roots: roots(&[("project", ROOT)]),
        spec: Some(dec(&json!({"nope": 1}))),
        ..ClientConfig::default()
    };
    let res = adapter().render(&sample_ctx(&cfg)).unwrap();
    assert!(res.artifacts.is_empty() && res.diagnostics[0].severity == "error");
    let cfg2 = ClientConfig {
        roots: roots(&[]),
        spec: None,
        ..cfg
    };
    assert!(
        adapter().render(&sample_ctx(&cfg2)).unwrap().diagnostics[0]
            .message
            .starts_with("spec:")
    );
}

#[test]
fn test_flat_skills_and_modes_and_toml() {
    let spec = json!({"version": 1,
        "skills": {"root": "p", "path": "cmds", "layout": "flat", "filename": "{name}.md"},
        "instructions": {"root": "p", "file": "I.md", "mode": "concat"},
        "mcp": {"root": "h", "file": "config.toml", "format": "toml", "key_path": "mcp_servers",
                "entry": {"stdio": {"command": "{command}", "args": "{args}", "env": "{env}"}, "http": {"url": "{url}"}},
                "env_style": "names_only"}});
    assert!(messages(&spec).is_empty());
    let cfg = ClientConfig {
        id: "t".into(),
        adapter: "generic".into(),
        display_name: "t".into(),
        roots: roots(&[("p", "/p"), ("h", "/h")]),
        spec: Some(dec(&spec)),
        strict: false,
        ..ClientConfig::default()
    };
    let mut ctx = sample_ctx(&cfg);
    ctx.memory = vec![];
    ctx.agents = vec![];
    ctx.permissions = PermissionSet::default();
    ctx.mcp_servers = vec![
        McpItem {
            command: Some("serena".into()),
            args: vec!["start-mcp-server".into()],
            env_names: vec!["SERENA_HOME".into()],
            ..mcp("serena", "stdio")
        },
        McpItem {
            url: Some("http://mcpo:8000/docs".into()),
            ..mcp("docs", "http")
        },
    ];
    let res = adapter().render(&ctx).unwrap();
    assert!(res.artifacts.iter().any(|a| a.path == "cmds/example-skill.md"));
    assert!(!res.artifacts.iter().any(|a| a.path.contains("cmds/scripts")));
    assert!(codes(&res).contains("extra_files_dropped"));
    let i = art(&res, "I.md");
    assert!(i.merge == "own" && !text(i).contains("## ")); // concat: no headings
    let toml = art(&res, "config.toml");
    assert_eq!(toml.merge, "block");
    let t = text(toml);
    assert!(t.starts_with(u::HASH_BEGIN));
    assert!(t.contains("[mcp_servers.serena]\ncommand = \"serena\"\nargs = [\"start-mcp-server\"]"));
    assert!(t.contains("[mcp_servers.docs]\nurl = ") && !t.contains("SERENA_HOME"));
}

#[test]
fn test_caps_for_spec_reflects_sections() {
    let c = caps_for_spec(&dec(&acme())).unwrap();
    assert!(c.skills && c.agents && c.memory && c.mcp && c.permissions);
    assert_eq!(c.tool_map["read"].as_deref(), Some("fs.read"));
    assert_eq!(c.tool_map["browser"], None);
    assert_eq!(c.model_tiers["deep"], "acme-large");
    assert!(
        !caps_for_spec(&dec(&json!({"version": 1, "instructions": acme()["instructions"]})))
            .unwrap()
            .skills
    );
    assert_eq!(caps_for_spec(&dec(&json!({"bad": 1}))).unwrap().notes, "invalid spec");
    assert!(adapter().caps().skills);
}

#[test]
fn test_default_config_spec_is_valid() {
    assert!(validate_spec(&adapter().default_config().spec.unwrap()).is_empty());
}

#[test]
fn test_verify_uses_hash() {
    let mut ctx = empty_ctx(&acme_cfg());
    ctx.skills = sample_ctx(&acme_cfg()).skills;
    let res = adapter().render(&ctx).unwrap();
    let fs = FakeFs::new(all_files(&res, ROOT));
    let exp: Vec<Expected> = res.artifacts.iter().map(expected).collect();
    assert!(
        adapter()
            .verify(&acme_cfg(), &exp, &fs, &FakeRunner::default())
            .unwrap()
            .iter()
            .all(|c| c.ok)
    );
}

#[test]
fn test_permission_decision_without_list_is_reported() {
    let mut ctx = empty_ctx(&acme_cfg());
    ctx.permissions = rules(&[("command", "ls *", "ask")]);
    let res = adapter().render(&ctx).unwrap();
    assert!(codes(&res).contains("unsupported_rule") || codes(&res).contains("unsupported_decision"));
}

fn example_cfg(name: &str) -> (String, ClientConfig) {
    let path = format!("{}/../../../examples/clients/{name}.yaml", env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read_to_string(path).unwrap();
    let data = hub_core::yaml::load(&raw, hub_core::yaml::Loader::Safe).unwrap();
    let get_s = |k: &str| data.get(k).and_then(PyValue::as_str).unwrap_or_default().to_string();
    let cfg = ClientConfig {
        id: get_s("id"),
        adapter: get_s("adapter"),
        display_name: get_s("display_name"),
        description: get_s("description"),
        roots: data
            .get("roots")
            .and_then(PyValue::as_dict)
            .unwrap()
            .iter()
            .map(|(k, v)| (k.py_str(), v.py_str()))
            .collect(),
        params: data.get("params").cloned().unwrap_or(PyValue::Dict(vec![])),
        spec: data.get("spec").cloned(),
        strict: data.get("strict") != Some(&PyValue::Bool(false)),
        ..ClientConfig::default()
    };
    (raw, cfg)
}

#[test]
fn test_example_specs_are_valid_labelled_and_render() {
    for name in ["cursor", "codex-cli", "gemini-cli"] {
        let (raw, cfg) = example_cfg(name);
        assert!(raw.lines().next().unwrap().contains("Example spec"));
        assert!(cfg.adapter == "generic" && cfg.spec.is_some());
        assert!(validate_spec(cfg.spec.as_ref().unwrap()).is_empty(), "{name}");
        let res = adapter().render(&sample_ctx(&cfg)).unwrap();
        assert!(!res.artifacts.is_empty(), "{name}");
        assert!(
            !res.diagnostics
                .iter()
                .any(|d| d.code == "spec_invalid" || d.code == "root_missing")
        );
        assert!(
            res.artifacts
                .iter()
                .all(|a| u::safe_relpath(&[&a.path]).ok().as_deref() == Some(a.path.as_str()))
        );
    }
    let (n, bad) = render_golden("generic/example-");
    assert_none("example specs", n, &bad);
}

#[test]
fn test_sandbox_profile_without_spec_wrapper_fails_closed() {
    let mut sp = acme();
    sp["mcp"].as_object_mut().unwrap().remove("wrapper");
    let mut ctx = empty_ctx(&acme_cfg_with(&sp));
    ctx.mcp_servers = sample_ctx(&acme_cfg()).mcp_servers;
    let res = adapter().render(&ctx).unwrap();
    let doc = yaml_load(&res.artifacts[0].content);
    assert!(
        doc.get("tools")
            .and_then(|t| t.get("mcp"))
            .unwrap()
            .get("serena")
            .is_none()
    );
    assert!(codes(&res).contains("sandbox_wrapper_missing"));
}

#[test]
fn render_errors_surface_as_the_python_exception() {
    // a flat filename template that escapes the root raises UnsafePath out of render(), as in Python
    let mut sp = acme();
    sp["skills"] = json!({"root": "project", "path": "c", "layout": "flat", "filename": "../{name}.md"});
    let e = adapter().render(&sample_ctx(&acme_cfg_with(&sp))).err().unwrap();
    assert_eq!(e.py_type, "UnsafePath");
    assert!(g::parse_spec(Some(&dec(&sp))).0.is_some());
}
