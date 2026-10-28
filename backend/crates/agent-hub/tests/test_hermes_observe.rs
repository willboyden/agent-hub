//! Port of backend/tests/adapters/test_hermes_observe.py: HermesAdapter.observe (Addendum F2): argv form, constant
//! program, untrusted-output handling, and the program itself run locally with python3. Plus the observation-parsing
//! golden cases recorded from Python (tests/golden/adapters.json "observe"). Owner: WS-A. Security control S-OBSERVE.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod adapter_fakes;

use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

use adapter_fakes::*;
use agent_hub::adapters::base::{ClientAdapter, ClientConfig, Observation};
use agent_hub::adapters::hermes::{self as h, HermesAdapter, OBSERVE_PROGRAM, parse_observation};
use base64::Engine as _;
use hub_core::value::PyValue;
use serde_json::{Value, json};

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn md(name: &str, desc: &str) -> String {
    format!("---\nname: {name}\ndescription: {desc}\n---\nbody\n")
}

fn adapter() -> HermesAdapter {
    HermesAdapter
}

fn cfg() -> ClientConfig {
    let mut c = adapter().default_config();
    for (k, v) in [
        ("compose_project", "proj"),
        ("compose_service", "hermes"),
        ("docker_context", "rootless"),
        ("exec_user", "hermes"),
        ("skills_dest", "/opt/data/skills/hub"),
    ] {
        set_param(&mut c, k, s(v));
    }
    c
}

fn doc(files: &Value, manifest: &Value, truncated: bool) -> String {
    json!({"manifest": manifest, "files": files, "truncated": truncated}).to_string()
}

fn observe(out: &str, rc: i32, err: &str) -> (Observation, FakeRunner) {
    let r = FakeRunner::new(&[(0, "ctr-1\n", ""), (rc, out, err)]);
    let obs = adapter().observe(&cfg(), &FakeFs::default(), &r).unwrap();
    (obs, r)
}

#[test]
fn golden_observe_program_is_pythons_constant() {
    assert_eq!(
        OBSERVE_PROGRAM,
        GOLDEN["constants"]["observe_program"].as_str().unwrap()
    );
}

#[test]
fn golden_parse_observation_equals_python() {
    let mut bad = Vec::new();
    let cases = GOLDEN["observe"].as_array().unwrap();
    for c in cases {
        let out = match c["out"].as_str() {
            Some(o) => o.to_string(),
            None => " ".repeat(usize::try_from(c["out_spaces"].as_u64().unwrap()).unwrap()),
        };
        let (ctr, root) = if c["name"] == "oversize" {
            ("c", "/r")
        } else {
            ("ctr-1", "/opt/data/skills")
        };
        let obs = parse_observation(&out, ctr, root, c["skills_dest"].as_str().unwrap()).unwrap();
        let got = json!({
            "skills": obs.skills.iter().map(|sk| json!({"path": sk.path, "name": sk.name, "description": sk.description,
                "origin": sk.origin, "files": sk.files.iter().map(|(k, v)| (k.clone(), blob(v))).collect::<serde_json::Map<_, _>>()}))
                .collect::<Vec<_>>(),
            "source": obs.source, "notes": obs.notes, "complete": obs.complete});
        if got != c["result"] {
            bad.push(
                format!("{}: rust {} != python {}", c["name"], got, c["result"])
                    .chars()
                    .take(600)
                    .collect(),
            );
        }
    }
    assert_none("observe", cases.len(), &bad);
}

#[test]
fn test_argv_and_constant_program() {
    let (obs, r) = observe(
        &doc(&json!({"a/SKILL.md": b64(md("a", "d").as_bytes())}), &json!([]), false),
        0,
        "",
    );
    let calls = r.calls();
    assert_eq!(calls[0][..3], ["docker", "--context", "rootless"]);
    assert_eq!(calls[0][3], "ps");
    assert!(calls[0].contains(&"label=com.docker.compose.service=hermes".to_string()));
    assert_eq!(
        calls[1],
        [
            "docker",
            "--context",
            "rootless",
            "exec",
            "--user",
            "hermes",
            "ctr-1",
            "python3",
            "-c",
            OBSERVE_PROGRAM,
            "/opt/data/skills"
        ]
    );
    assert_eq!(*r.timeouts.lock().unwrap(), vec![15.0, 120.0]);
    assert!(obs.complete && obs.source == "container ctr-1: /opt/data/skills");
}

#[test]
fn test_no_container_is_incomplete_and_nothing_started() {
    let r = FakeRunner::new(&[(0, "", "")]);
    let obs = adapter().observe(&cfg(), &FakeFs::default(), &r).unwrap();
    assert!(!obs.complete && obs.skills.is_empty() && !obs.notes.is_empty() && r.calls().len() == 1);
}

#[test]
fn test_extra_services_in_order() {
    let mut c = cfg();
    set_param(&mut c, "observe_services", list(&["desktop"]));
    let r = FakeRunner::new(&[
        (0, "", ""),
        (0, "c2\n", ""),
        (0, &doc(&json!({}), &json!([]), false), ""),
    ]);
    let obs = adapter().observe(&c, &FakeFs::default(), &r).unwrap();
    let calls = r.calls();
    assert!(calls[1].contains(&"label=com.docker.compose.service=desktop".to_string()));
    let ex = calls[2].iter().position(|x| x == "exec").unwrap();
    assert_eq!(calls[2][ex + 3], "c2");
    assert!(obs.complete);
}

#[test]
fn test_docker_missing() {
    let obs = adapter()
        .observe(&cfg(), &FakeFs::default(), &FakeRunner::raising())
        .unwrap();
    assert!(!obs.complete && !obs.notes.is_empty());
}

#[test]
fn test_placeholder_project_never_runs() {
    let r = FakeRunner::default();
    assert!(
        !adapter()
            .observe(&adapter().default_config(), &FakeFs::default(), &r)
            .unwrap()
            .complete
    );
    assert!(r.calls().is_empty());
}

#[test]
fn test_param_validation_refuses_before_running() {
    for (key, val) in [
        ("observe_python", s("py thon; rm")),
        ("observe_root", s("relative/x")),
        ("observe_root", s("/a/../b")),
        ("observe_root", s("/a b")),
        ("exec_user", s("x y")),
        ("compose_service", s("a;b")),
        ("observe_services", list(&["bad name"])),
    ] {
        let mut c = cfg();
        set_param(&mut c, key, val);
        let r = FakeRunner::default();
        let obs = adapter().observe(&c, &FakeFs::default(), &r).unwrap();
        assert!(!obs.complete && r.calls().is_empty(), "{key}");
    }
}

#[test]
fn test_grouping_nesting_and_origins() {
    let files = json!({
        "devops/foo/SKILL.md": b64(md("foo-real", "Foo").as_bytes()), "devops/foo/scripts/x.sh": b64(b"x"),
        "devops/foo/refs/inner/SKILL.md": b64(md("inner", "d").as_bytes()), "devops/foo/refs/inner/y.md": b64(b"y"),
        "mine/SKILL.md": b64(b"no frontmatter"),
        "lab/l/SKILL.md": b64(md("l", "d").as_bytes()), "hub/h/SKILL.md": b64(md("h", "d").as_bytes()),
        ".archive/old/SKILL.md": b64(md("old", "d").as_bytes()), ".archive/bundledold/SKILL.md": b64(md("bundledold", "d").as_bytes()),
        "stray.txt": b64(b"ignored"),
    });
    let (obs, _) = observe(&doc(&files, &json!(["foo:abc123", "bundledold:ff"]), false), 0, "");
    let by = |p: &str| obs.skills.iter().find(|s| s.path == p).unwrap().clone();
    let foo = by("devops/foo");
    assert!(foo.name == "foo-real" && foo.description == "Foo" && foo.origin == "shipped");
    assert_eq!(foo.files.keys().collect::<Vec<_>>(), vec!["SKILL.md", "scripts/x.sh"]);
    assert_eq!(
        by("devops/foo/refs/inner").files.keys().collect::<Vec<_>>(),
        vec!["SKILL.md", "y.md"]
    );
    assert!(by("mine").name == "mine" && by("mine").origin == "agent");
    assert!(by("lab/l").origin == "lab_sync" && by("hub/h").origin == "hub");
    assert!(by(".archive/old").origin == "archived" && by(".archive/bundledold").origin == "archived");
    assert!(obs.complete);
}

#[test]
fn test_truncated_flag_makes_incomplete() {
    let (obs, _) = observe(
        &doc(&json!({"a/SKILL.md": b64(md("a", "d").as_bytes())}), &json!([]), true),
        0,
        "",
    );
    assert!(!obs.complete && obs.skills.len() == 1);
}

#[test]
fn test_non_object_or_bad_json_rejected() {
    for out in ["[]", "null", "not json", r#"{"files": []}"#, "", r#"{"files""#] {
        let (obs, _) = observe(out, 0, "");
        assert!(
            !obs.complete && obs.skills.is_empty() && !obs.notes.is_empty(),
            "{out:?}"
        );
    }
}

#[test]
fn test_bad_paths_and_base64_dropped() {
    let files = json!({"ok/SKILL.md": b64(md("ok", "d").as_bytes()), "../evil/SKILL.md": b64(b"x"),
                       "/abs/SKILL.md": b64(b"x"), "a//b/SKILL.md": b64(b"x"), "bad/SKILL.md": "not base64!!",
                       "pad/SKILL.md": "QUJD\nREVG", "num/SKILL.md": 5});
    let (obs, _) = observe(&doc(&files, &json!([]), false), 0, "");
    assert_eq!(
        obs.skills.iter().map(|s| s.path.as_str()).collect::<Vec<_>>(),
        vec!["ok"]
    );
    assert!(!obs.complete && obs.notes.join(" ").contains("rejected"));
}

#[test]
fn test_oversize_output_and_file_caps() {
    let (obs, _) = observe(&" ".repeat(h::MAX_OBSERVE_OUT + 1), 0, "");
    assert!(!obs.complete && obs.skills.is_empty());
    let big = b64(&vec![b'x'; h::MAX_FILE + 1]);
    let (obs, _) = observe(
        &doc(
            &json!({"a/SKILL.md": b64(md("a", "d").as_bytes()), "a/big.bin": big}),
            &json!([]),
            false,
        ),
        0,
        "",
    );
    assert!(!obs.complete);
    assert_eq!(obs.skills[0].files.keys().collect::<Vec<_>>(), vec!["SKILL.md"]);
}

#[test]
fn test_long_name_and_description_capped_and_bad_yaml() {
    let files = json!({"a/SKILL.md": b64(md(&"n".repeat(5000), &"d".repeat(9000)).as_bytes()),
                       "b/SKILL.md": b64(b"---\nname: &x [*x\n---\n")});
    let (obs, _) = observe(&doc(&files, &json!([]), false), 0, "");
    let by = |p: &str| obs.skills.iter().find(|s| s.path == p).unwrap().clone();
    assert_eq!(by("a").name.chars().count(), h::MAX_SKILL_NAME);
    assert_eq!(by("a").description.chars().count(), h::MAX_SKILL_DESC);
    assert_eq!(by("b").name, "b");
}

#[test]
fn test_stderr_note_is_capped_and_single_line() {
    let err = format!("line1\nSECRET\x1b[31m{}", "z".repeat(1000));
    let (obs, _) = observe("", 1, &err);
    assert!(!obs.complete && obs.notes[0].chars().count() < 260);
    assert!(!obs.notes[0].contains('\n') && !obs.notes[0].contains('\x1b'));
}

// ---- the program itself, run for real against a temp tree -----------------------------------------------------------------
fn run_program(root: &Path) -> (Option<i32>, Value) {
    let p = Command::new("python3")
        .args(["-c", OBSERVE_PROGRAM])
        .arg(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let v = serde_json::from_slice(&p.stdout).unwrap_or(Value::Null);
    (p.status.code(), v)
}

#[test]
fn test_program_walks_safely() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path().join("root");
    std::fs::create_dir(&t).unwrap();
    std::fs::create_dir(t.join("a")).unwrap();
    std::fs::write(t.join("a/SKILL.md"), "x").unwrap();
    std::fs::write(t.join("a/.hidden"), "h").unwrap();
    std::fs::create_dir(t.join(".curator_backups")).unwrap();
    std::fs::write(t.join(".curator_backups/SKILL.md"), "b").unwrap();
    std::fs::create_dir_all(t.join(".archive/o")).unwrap();
    std::fs::write(t.join(".archive/o/SKILL.md"), "o").unwrap();
    std::fs::create_dir(t.join(".other")).unwrap();
    std::fs::write(t.join(".other/SKILL.md"), "o").unwrap();
    std::fs::write(t.join(".bundled_manifest"), "a:123\n").unwrap();
    let outside = tmp.path().join("root-out");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("SKILL.md"), "secret").unwrap();
    symlink(&outside, t.join("link")).unwrap();
    symlink(outside.join("SKILL.md"), t.join("a/flink")).unwrap();
    let (rc, out) = run_program(&t);
    assert_eq!(rc, Some(0));
    let mut keys: Vec<&String> = out["files"].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, vec![".archive/o/SKILL.md", "a/SKILL.md"]);
    assert_eq!(out["manifest"], json!(["a:123"]));
    assert_eq!(out["truncated"], json!(false));
}

#[test]
fn test_program_caps_and_missing_root() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    std::fs::create_dir(t.join("s")).unwrap();
    std::fs::write(t.join("s/SKILL.md"), "x").unwrap();
    std::fs::write(t.join("s/big"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    let (_, out) = run_program(t);
    assert_eq!(out["truncated"], json!(true));
    assert_eq!(
        out["files"].as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["s/SKILL.md"]
    );
    let (rc, _) = run_program(&t.join("nope"));
    assert_eq!(rc, Some(3));
}

#[test]
fn test_surrogate_and_control_paths_rejected() {
    // json.dumps(..., ensure_ascii=True): a lone surrogate arrives as a \ud800 escape
    let ok = b64(md("ok", "d").as_bytes());
    let x = b64(b"x");
    let esc = |cp: &str| format!("a/\\u{cp}.txt"); // a JSON \uXXXX escape, as ASCII text
    let out = format!(
        r#"{{"manifest": [], "files": {{"ok/SKILL.md": "{ok}", "{}": "{x}", "{}": "{x}", "{}": "{x}", "{}": "{x}"}}, "truncated": false}}"#,
        esc("d800"),
        esc("007f"),
        esc("202e"),
        esc("2028")
    );
    let (obs, _) = observe(&out, 0, "");
    assert_eq!(
        obs.skills.iter().map(|s| s.path.as_str()).collect::<Vec<_>>(),
        vec!["ok"]
    );
    assert!(!obs.complete && obs.notes.join(" ").contains("rejected"));
}

#[test]
fn test_file_dir_prefix_collision_skipped() {
    let files = json!({"a/SKILL.md": b64(md("a", "d").as_bytes()), "a/x": b64(b"f"), "a/x/y": b64(b"g")});
    let (obs, _) = observe(&doc(&files, &json!([]), false), 0, "");
    assert!(!obs.complete && obs.notes.join(" ").contains("collide"));
    assert_eq!(obs.skills[0].files.keys().collect::<Vec<_>>(), vec!["SKILL.md", "x/y"]);
}

#[test]
fn a2b_base64_strict_follows_binascii() {
    assert_eq!(h::a2b_base64_strict(b"QUJD"), Some(b"ABC".to_vec()));
    assert_eq!(h::a2b_base64_strict(b"QR=="), Some(b"A".to_vec())); // trailing bits are not checked
    for bad in [&b"QQ="[..], b"Q", b"=QUJD", b"QQ==QQ==", b"QQ=A", b"QU JD", b"QUJD\n"] {
        assert_eq!(h::a2b_base64_strict(bad), None, "{:?}", String::from_utf8_lossy(bad));
    }
    assert_eq!(h::a2b_base64_strict(b""), Some(vec![]));
    let _ = PyValue::None;
}
