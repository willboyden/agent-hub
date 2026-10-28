//! Port of backend/tests/adapters/test_turnstone.py (adapters/turnstone.rs), plus the turnstone golden cases recorded
//! from Python (tests/golden/adapters.json: 16 renders, 1 discover, 57 console URLs, resource rules). Owner: WS-A.
//! DEVIATION D-TURNSTONE: verify runs `HUB_PYTHON`, else /usr/bin/python3 or /bin/python3 (never a PATH lookup),
//! instead of Python's `sys.executable`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod adapter_fakes;

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use adapter_fakes::*;
use agent_hub::adapters::base::{Check, ClientAdapter, ClientConfig, Expected, Files};
use agent_hub::adapters::turnstone::{
    MAX_RESOURCE_CHARS, MAX_RESOURCES_PER_SKILL, RESOURCE_DIRS, SYSTEM_PYTHONS, TurnstoneAdapter, console_problem,
    default_deliver_script, interpreter, interpreter_from, undelivered_resources,
};
use hub_core::value::PyValue;
use serde_json::{Value, json};

const STAGE: &str = "/w/stage";

struct Env {
    _tmp: tempfile::TempDir,
    token_dir: PathBuf,
    adapter: TurnstoneAdapter,
    cfg: ClientConfig,
}

/// The fixtures: a token dir under a temp dir (Python monkeypatches TOKEN_DIR) and a client pointed at it.
fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let token_dir = tmp.path().join("agent-hub");
    std::fs::create_dir(&token_dir).unwrap();
    let adapter = TurnstoneAdapter {
        token_dir: Some(token_dir.clone()),
        deliver_script: None,
        python: None,
    };
    let mut cfg = adapter.default_config();
    cfg.roots = roots(&[("stage", STAGE)]);
    set_param(&mut cfg, "console_url", s("http://127.0.0.1:9999"));
    set_param(
        &mut cfg,
        "token_file",
        s(&token_dir.join("ts.token").display().to_string()),
    );
    Env {
        _tmp: tmp,
        token_dir,
        adapter,
        cfg,
    }
}

fn expected_all(e: &Env) -> (FakeFs, Vec<Expected>) {
    let res = e.adapter.render(&sample_ctx(&e.cfg)).unwrap();
    (
        FakeFs::new(tree(&res, STAGE)),
        res.artifacts.iter().map(expected).collect(),
    )
}

fn console(e: &Env, run: &FakeRunner) -> Check {
    let (fs, exp) = expected_all(e);
    e.adapter.verify(&e.cfg, &exp, &fs, run).unwrap().pop().unwrap()
}

#[test]
fn test_default_config_is_neutral_and_the_delivery_script_ships() {
    let d = TurnstoneAdapter::default().default_config();
    assert!(d.id == "turnstone" && d.adapter == "turnstone" && d.roots["stage"].contains("~/path/to"));
    let m = d.param("manage").unwrap();
    assert_eq!(m.get("skills"), Some(&PyValue::Bool(true)));
    assert_eq!(m.get("instructions"), Some(&PyValue::Bool(false)));
    let script = default_deliver_script();
    assert!(script.is_file() && script.parent().unwrap().file_name().unwrap() == "deploy");
}

#[test]
fn test_golden() {
    let (n, bad) = render_golden("turnstone/sample");
    assert_none("turnstone sample", n, &bad);
}

#[test]
fn golden_render_cases_equal_python() {
    let (n, bad) = render_golden("turnstone/");
    assert_none("turnstone render", n, &bad);
}

#[test]
fn golden_discover_cases_equal_python() {
    let (n, bad) = discover_golden("turnstone/");
    assert_none("turnstone discover", n, &bad);
}

#[test]
fn golden_meta_equals_python() {
    assert_eq!(meta_golden("turnstone"), Vec::<String>::new());
}

#[test]
fn golden_console_problem_equals_python() {
    let cases = GOLDEN["turnstone"]["console_problem"].as_array().unwrap();
    let bad: Vec<String> = cases
        .iter()
        .filter_map(|c| {
            let url = c["url"].as_str().unwrap();
            let got = console_problem(url).map(Value::from).unwrap_or(Value::Null);
            (got != c["problem"]).then(|| format!("{url:?}: rust {got} != python {}", c["problem"]))
        })
        .collect();
    assert_none("console_problem", cases.len(), &bad);
}

#[test]
fn golden_undelivered_resources_equal_python() {
    for c in GOLDEN["turnstone"]["undelivered_resources"].as_array().unwrap() {
        let files: Files = c["files"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), unblob(v)))
            .collect();
        let got: Vec<Value> = undelivered_resources(&files)
            .into_iter()
            .map(|(p, w)| json!([p, w]))
            .collect();
        assert_eq!(Value::Array(got), c["out"]);
    }
}

#[test]
fn test_deterministic() {
    let e = env();
    let ctx = sample_ctx(&e.cfg);
    assert_eq!(
        e.adapter.render(&ctx).unwrap(),
        e.adapter.render(&shuffled(&ctx)).unwrap()
    );
}

#[test]
fn test_stage_layout() {
    let e = env();
    let res = e.adapter.render(&sample_ctx(&e.cfg)).unwrap();
    let paths: std::collections::BTreeSet<&str> = res.artifacts.iter().map(|a| a.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "skills/example-skill/SKILL.md",
            "skills/example-skill/scripts/run.sh",
            "skills/example-other-skill/SKILL.md",
            "instructions.md",
            "mcp.json"
        ]
        .into()
    );
    assert!(res.artifacts.iter().all(|a| a.root == "stage" && a.merge == "own"));
    let instr = text(art(&res, "instructions.md"));
    assert!(instr.find("One.").unwrap() < instr.find("Two.").unwrap());
    let mcp = json_load(&art(&res, "mcp.json").content)["mcpServers"].clone();
    assert_eq!(
        mcp["docs"],
        json!({"type": "streamable-http", "url": "http://mcpo:8000/docs"})
    );
    assert_eq!(
        mcp["serena"],
        json!({"command": "serena", "args": ["start-mcp-server"]})
    );
    assert!(!text(art(&res, "mcp.json")).contains("SERENA_HOME")); // env names never rendered
}

#[test]
fn test_diagnostics() {
    let mut e = env();
    let res = e.adapter.render(&sample_ctx(&e.cfg)).unwrap();
    assert!(has_diag(&res, "unsupported_capability", Some("error"), None)); // agents and memory, strict client
    assert!(has_diag(&res, "mcp_stdio_in_node", Some("warn"), Some("serena")));
    assert!(
        res.diagnostics
            .iter()
            .any(|d| d.code == "permissions_not_rendered" && d.severity == "info" && d.item.is_none())
    );
    e.cfg.strict = false;
    let res = e.adapter.render(&sample_ctx(&e.cfg)).unwrap();
    assert!(has_diag(&res, "unsupported_capability", Some("warn"), None));
}

#[test]
fn test_nothing_but_skills_renders_only_skills() {
    let e = env();
    let mut ctx = empty_ctx(&e.cfg);
    ctx.skills = sample_ctx(&e.cfg).skills;
    let res = e.adapter.render(&ctx).unwrap();
    assert!(res.artifacts.iter().all(|a| a.path.starts_with("skills/")) && res.diagnostics.is_empty());
}

#[test]
fn test_oversized_skill_and_auto_approve_frontmatter_are_flagged() {
    let e = env();
    let mut big_md = b"---\nname: big-skill\ndescription: Big. Use when asked.\n---\n".to_vec();
    big_md.extend(vec![b'x'; 40000]);
    let big = skill_with("big-skill", DESC, &[("SKILL.md", &big_md)]);
    let auto = skill_with(
        "auto-skill",
        DESC,
        &[(
            "SKILL.md",
            b"---\nname: auto-skill\ndescription: A. Use when asked.\nauto_approve: true\n---\nBody.\n",
        )],
    );
    let mut ctx = sample_ctx(&e.cfg);
    ctx.skills = vec![big, auto];
    let res = e.adapter.render(&ctx).unwrap();
    assert!(has_diag(&res, "skill_too_large", Some("error"), Some("big-skill")));
    assert!(has_diag(&res, "auto_approve_ignored", None, Some("auto-skill")));
    let body_only = skill_with(
        "talks-about-it",
        DESC,
        &[(
            "SKILL.md",
            b"---\nname: talks-about-it\ndescription: T. Use when asked.\n---\nauto_approve: is a field.\n",
        )],
    );
    ctx.skills = vec![body_only];
    assert!(!codes(&e.adapter.render(&ctx).unwrap()).contains("auto_approve_ignored"));
}

#[test]
fn test_verify_asks_the_console_through_the_script_without_the_token_on_argv() {
    let e = env();
    let out = json!({"skills": ["example-skill", "example-other-skill", "theirs"],
                     "hub_skills": ["example-skill", "example-other-skill"]})
    .to_string();
    let run = FakeRunner::new(&[(0, &out, "")]);
    let (fs, exp) = expected_all(&e);
    let checks = e.adapter.verify(&e.cfg, &exp, &fs, &run).unwrap();
    let c = checks.last().unwrap();
    assert!(c.ok && c.detail.contains("2 staged skill") && checks.iter().all(|c| c.ok));
    let argv = &run.calls()[0];
    assert_eq!(argv[0], interpreter().unwrap()); // D-TURNSTONE: never a PATH lookup
    assert!(argv[0].starts_with('/'));
    assert_eq!(argv[1], default_deliver_script().display().to_string());
    assert_eq!(argv[2], "list");
    let at = |flag: &str| argv[argv.iter().position(|x| x == flag).unwrap() + 1].clone();
    assert_eq!(at("--console"), "http://127.0.0.1:9999");
    assert_eq!(at("--token-file"), e.token_dir.join("ts.token").display().to_string());
    assert_eq!(*run.timeouts.lock().unwrap(), vec![30.0]);
}

#[test]
fn test_hostile_params_never_reach_the_command_line() {
    let cases: [(&str, &str, Option<&str>); 8] = [
        ("python_bin", "/bin/sh", None), // ignored: the interpreter is never a param
        ("console_url", "http://evil.example:8796", Some("loopback")),
        ("console_url", "https://10.0.0.5", Some("loopback")),
        ("console_url", "http://user:pw@127.0.0.1:8796", Some("bare base URL")),
        ("console_url", "http://127.0.0.1:8796/v1/api?x=1", Some("bare base URL")),
        ("console_url", "ftp://127.0.0.1", Some("http(s)")),
        ("token_file", "/etc/passwd", Some("must be inside")),
        ("token_file", "~/.config/other-app/x.token", Some("must be inside")),
    ];
    for (key, val, fragment) in cases {
        let mut e = env();
        set_param(&mut e.cfg, key, s(val));
        let out = json!({"skills": [], "hub_skills": ["example-skill", "example-other-skill"]}).to_string();
        let run = FakeRunner::new(&[(0, &out, "")]);
        let c = console(&e, &run);
        match fragment {
            None => {
                let calls = run.calls();
                assert!(
                    !calls.is_empty()
                        && calls[0][0] == interpreter().unwrap()
                        && !calls[0].contains(&"/bin/sh".to_string())
                );
            }
            Some(f) => {
                assert!(
                    !c.ok && c.detail.contains(f) && c.detail.contains("not checked"),
                    "{val}: {}",
                    c.detail
                );
                assert!(run.calls().is_empty());
            }
        }
    }
}

#[test]
fn test_a_non_token_file_in_the_token_dir_is_refused() {
    let mut e = env();
    std::fs::write(e.token_dir.join("env"), "HUB_API_KEY=placeholder-not-a-token\n").unwrap(); // the hub's own env file
    let p = e.token_dir.join("env").display().to_string();
    set_param(&mut e.cfg, "token_file", s(&p));
    let run = FakeRunner::default();
    let c = console(&e, &run);
    assert!(!c.ok && c.detail.contains("*.token file") && run.calls().is_empty());
}

#[test]
fn test_a_symlinked_token_file_is_refused() {
    let e = env();
    let elsewhere = e.token_dir.parent().unwrap().join("elsewhere");
    std::fs::write(&elsewhere, "x").unwrap();
    symlink(&elsewhere, e.token_dir.join("ts.token")).unwrap();
    let run = FakeRunner::default();
    let c = console(&e, &run);
    assert!(!c.ok && c.detail.contains("symlink") && run.calls().is_empty());
}

#[test]
fn test_verify_fails_when_a_hub_skill_was_switched_to_auto_approve() {
    let e = env();
    let out = json!({"skills": ["example-skill", "example-other-skill"],
                     "hub_skills": ["example-skill", "example-other-skill"], "unsafe": ["example-skill"]})
    .to_string();
    let c = console(&e, &FakeRunner::new(&[(0, &out, "")]));
    assert!(!c.ok && c.detail.contains("example-skill") && c.detail.contains("auto_approve"));
}

#[test]
fn test_files_turnstone_will_refuse_are_warned_in_the_plan() {
    let e = env();
    let refs: Vec<(String, Vec<u8>)> = (0..11)
        .map(|i| (format!("references/r{i:02}.md"), b"r".to_vec()))
        .collect();
    let big_js = vec![b'x'; 100 * 1024 + 1];
    let mut extra: Vec<(&str, &[u8])> = vec![
        (
            "SKILL.md",
            b"---\nname: odd\ndescription: O. Use when asked.\n---\nSee references/r10.md.\n",
        ),
        ("agents/openai.yaml", b"a"),
        ("check.py", b"print()"),
        ("assets/logo.png", b"\x89PNG\xff"),
        ("assets/big.js", &big_js),
    ];
    extra.extend(refs.iter().map(|(k, v)| (k.as_str(), v.as_slice())));
    let odd = skill_with("odd", DESC, &extra);
    let fine = skill_with(
        "fine",
        DESC,
        &[
            ("scripts/run.sh", b"echo"),
            ("references/a.md", b"a"),
            ("assets/t.html", b"<p>"),
        ],
    );
    let mut ctx = sample_ctx(&e.cfg);
    ctx.skills = vec![odd, fine];
    let res = e.adapter.render(&ctx).unwrap();
    let diags: Vec<_> = res
        .diagnostics
        .iter()
        .filter(|d| d.code == "resources_not_delivered")
        .collect();
    assert_eq!(
        diags.iter().map(|d| d.item.clone().unwrap()).collect::<Vec<_>>(),
        vec!["odd"]
    );
    assert_eq!(diags[0].severity, "warn"); // a warning: the plan is not blocked
    let msg = &diags[0].message;
    assert!(
        msg.starts_with("5 file(s)") && msg.contains("agents/openai.yaml (outside scripts/, references/, assets/)")
    );
    assert!(msg.contains("check.py (outside") && msg.contains("assets/logo.png (not UTF-8 text)"));
    assert!(msg.contains("assets/big.js (over 102400"));
    assert!(msg.contains("references/r09.md (past 10 files per skill)") && !msg.contains("r10.md (past")); // SKILL.md names r10
}

#[test]
fn test_the_plan_and_the_delivery_script_apply_the_same_resource_rules() {
    let script = default_deliver_script();
    let code = "import importlib.util, json, sys\n\
spec = importlib.util.spec_from_file_location('turnstone_deliver_rules', sys.argv[1])\n\
m = importlib.util.module_from_spec(spec)\n\
spec.loader.exec_module(m)\n\
print(json.dumps([list(m.RESOURCE_DIRS), m.MAX_RESOURCE_CHARS, m.MAX_RESOURCES_PER_SKILL]))\n";
    let out = std::process::Command::new("python3")
        .args(["-I", "-c", code])
        .arg(&script)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v, json!([RESOURCE_DIRS, MAX_RESOURCE_CHARS, MAX_RESOURCES_PER_SKILL]));
    let consts = &GOLDEN["constants"];
    assert_eq!(consts["resource_dirs"], json!(RESOURCE_DIRS));
    assert_eq!(consts["max_resource_chars"], json!(MAX_RESOURCE_CHARS));
    assert_eq!(consts["max_resources_per_skill"], json!(MAX_RESOURCES_PER_SKILL));
}

#[test]
fn test_allowed_tools_in_frontmatter_is_warned_as_not_delivered() {
    let e = env();
    let tooled = skill_with(
        "tooled",
        DESC,
        &[(
            "SKILL.md",
            b"---\nname: tooled\ndescription: T. Use when asked.\nallowed-tools: Bash\n---\nBody.\n",
        )],
    );
    let mut ctx = sample_ctx(&e.cfg);
    ctx.skills = vec![tooled];
    assert!(has_diag(
        &e.adapter.render(&ctx).unwrap(),
        "allowed_tools_dropped",
        Some("warn"),
        Some("tooled")
    ));
}

#[test]
fn test_verify_fails_closed() {
    let one = json!({"skills": ["example-skill"], "hub_skills": ["example-skill"]}).to_string();
    let two = json!({"skills": ["example-skill", "example-other-skill"], "hub_skills": ["example-skill"]}).to_string();
    type Case<'a> = (Option<(i32, &'a str, &'a str)>, bool, &'a str);
    let cases: [Case<'_>; 5] = [
        (Some((0, &one, "")), false, "example-other-skill"),
        (Some((0, &two, "")), false, "example-other-skill"),
        (
            Some((1, "", "turnstone-deliver: token file /secrets/ts.token not found")),
            false,
            "token file",
        ),
        (Some((0, "not json", "")), false, "no JSON"),
        (None, true, "unverified"),
    ];
    for (response, raises, fragment) in cases {
        let e = env();
        let run = match (response, raises) {
            (_, true) => FakeRunner::raising(),
            (Some(r), false) => FakeRunner::new(&[r]),
            (None, false) => FakeRunner::default(),
        };
        let c = console(&e, &run);
        assert!(!c.ok && c.detail.contains(fragment), "{fragment}: {}", c.detail);
    }
}

#[test]
fn test_discover_reads_only_the_stage() {
    let e = env();
    let (fs, _) = expected_all(&e);
    let res = e.adapter.discover(&e.cfg, &fs).unwrap();
    let mut names: Vec<&str> = res.skills.iter().map(|x| x.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["example-other-skill", "example-skill"]);
    assert!(res.notes.iter().any(|n| n.contains("database")));
}

#[test]
fn test_registry_loads_it() {
    let reg = agent_hub::adapters::load_registry();
    assert!(reg.ids().contains(&"turnstone".to_string()) && !reg.load_errors.contains_key("turnstone"));
    assert_eq!(reg.get("turnstone").unwrap().id(), "turnstone");
    assert!(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/adapters/turnstone.rs")
            .is_file()
    );
}

// ---- labels: skills-meta.json (category from the hub source, plus tags) -------------------------------------------------
fn with_categories(e: &mut Env, param: PyValue) {
    set_param(&mut e.cfg, "category_from_source", param);
}

#[test]
fn test_category_from_source_labels_only_the_mapped_skills() {
    let mut e = env();
    with_categories(&mut e, dict(vec![("import:video-agent", s("video-agent"))]));
    let (mut s1, mut s2, mut s3) = (skill("from-video"), skill("tagged-only"), skill("plain"));
    s1.source = "import:video-agent".into();
    s2.tags = vec!["video".into()];
    s3.source = "import:claude-code".into();
    let mut ctx = sample_ctx(&e.cfg);
    ctx.skills = vec![s3, s1, s2];
    let res = e.adapter.render(&ctx).unwrap();
    let meta = art(&res, "skills-meta.json");
    assert!(meta.root == "stage" && meta.merge == "own" && meta.kind == "skill");
    assert_eq!(
        json_load(&meta.content),
        json!({"from-video": {"category": "video-agent", "tags": ["video-agent"]}, "tagged-only": {"tags": ["video"]}})
    );
    assert_eq!(meta.source_ids, vec!["from-video", "tagged-only"]);
    assert!(!codes(&res).contains("invalid_param"));
}

#[test]
fn test_no_labels_no_meta_file() {
    let mut e = env();
    with_categories(&mut e, dict(vec![("import:video-agent", s("video-agent"))]));
    let res = e.adapter.render(&sample_ctx(&e.cfg)).unwrap(); // sample skills have no source and no tags
    assert!(!res.artifacts.iter().any(|a| a.path == "skills-meta.json"));
}

#[test]
fn test_a_withheld_skill_gets_no_label() {
    let mut e = env();
    with_categories(&mut e, dict(vec![("import:video-agent", s("video-agent"))]));
    let mut bad = skill("Bad Name"); // invalid name: skill_artifacts drops it
    bad.source = "import:video-agent".into();
    let mut ctx = sample_ctx(&e.cfg);
    ctx.skills = vec![bad];
    assert!(
        !e.adapter
            .render(&ctx)
            .unwrap()
            .artifacts
            .iter()
            .any(|a| a.path == "skills-meta.json")
    );
}

#[test]
fn test_tags_turnstone_cannot_take_are_left_out_with_a_warning() {
    let mut e = env();
    with_categories(&mut e, dict(vec![("import:video-agent", s("video-agent"))]));
    let mut sk = skill("many");
    sk.source = "import:video-agent".into();
    sk.tags = vec!["Video Tools".into(), "x".repeat(65), "ok".into()];
    sk.tags.extend((0..25).map(|i| format!("t{i:02}"))); // 2 bad + 26 good, room for 19
    let mut ctx = sample_ctx(&e.cfg);
    ctx.skills = vec![sk];
    let res = e.adapter.render(&ctx).unwrap();
    let tags: Vec<String> =
        serde_json::from_value(json_load(&art(&res, "skills-meta.json").content)["many"]["tags"].clone()).unwrap();
    assert!(tags.len() == 20 && tags.contains(&"video-agent".to_string()) && tags.contains(&"ok".to_string()));
    assert!(!tags.contains(&"Video Tools".to_string()));
    let warn: Vec<_> = res
        .diagnostics
        .iter()
        .filter(|d| d.code == "tag_not_delivered")
        .collect();
    assert!(warn.len() == 1 && warn[0].severity == "warn" && warn[0].item.as_deref() == Some("many"));
    assert!(warn[0].message.starts_with("9 tag(s)"));
}

#[test]
fn test_invalid_category_param_is_an_error_and_renders_no_category() {
    let params = [
        dict(vec![("import:x", s("Has Caps"))]),
        dict(vec![("import:x", s(&"x".repeat(65)))]),
        dict(vec![("import:x", PyValue::Int(3))]),
        list(&["import:x"]),
        dict(vec![("import:x", s(""))]),
    ];
    for param in params {
        let mut e = env();
        with_categories(&mut e, param.clone());
        let mut one = skill("one");
        one.source = "import:x".into();
        let mut ctx = sample_ctx(&e.cfg);
        ctx.skills = vec![one];
        let res = e.adapter.render(&ctx).unwrap();
        assert!(has_diag(&res, "invalid_param", Some("error"), None), "{param:?}");
        assert!(!res.artifacts.iter().any(|a| a.path == "skills-meta.json"));
    }
}

#[test]
fn resource_size_counts_characters_not_bytes() {
    // 60 000 two-byte characters (120 000 bytes) are under the 102 400-character cap, like Python's len(str)
    let mut files = Files::new();
    files.insert("SKILL.md".into(), b"x".to_vec());
    files.insert("assets/u.txt".into(), "é".repeat(60_000).into_bytes());
    files.insert("assets/over.txt".into(), "é".repeat(102_401).into_bytes());
    assert_eq!(
        undelivered_resources(&files),
        vec![(
            "assets/over.txt".to_string(),
            format!("over {MAX_RESOURCE_CHARS} characters")
        )]
    );
}

/// Audit F6: the interpreter is `HUB_PYTHON` (absolute, executable) or a system python3, never whatever `python3` comes
/// first on PATH; a refused interpreter is a failed check and nothing runs.
#[test]
fn test_interpreter_is_never_looked_up_on_path() {
    let t = tempfile::tempdir().unwrap();
    let exe = t.path().join("python3");
    std::fs::write(&exe, "#!/bin/sh\n").unwrap();
    let plain = t.path().join("not-exec");
    std::fs::write(&plain, "").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let exe_s = exe.to_string_lossy().to_string();
    assert_eq!(interpreter_from(Some(exe_s.clone()), &[]), Ok(exe_s.clone()));
    for bad in [
        "python3",
        "./python3",
        "",
        "/nonexistent/python3",
        plain.to_str().unwrap(),
    ] {
        let r = interpreter_from(Some(bad.to_string()), &["/nonexistent"]);
        if bad.is_empty() {
            assert!(r.unwrap_err().contains("not found in /usr/bin or /bin")); // empty = unset
        } else {
            assert_eq!(
                r,
                Err("HUB_PYTHON must be an absolute path to an executable file".into()),
                "{bad}"
            );
        }
    }
    assert_eq!(
        interpreter_from(None, &["/nonexistent/python3", &exe_s]),
        Ok(exe_s.clone())
    );
    assert_eq!(SYSTEM_PYTHONS, ["/usr/bin/python3", "/bin/python3"]);
    // through verify: a refused interpreter runs nothing
    let mut e = env();
    e.adapter.python = Some("python3".into());
    let run = FakeRunner::new(&[(0, "{}", "")]);
    let c = console(&e, &run);
    assert!(!c.ok && c.detail.contains("HUB_PYTHON must be an absolute path") && c.detail.ends_with("not checked"));
    assert!(run.calls().is_empty());
}
