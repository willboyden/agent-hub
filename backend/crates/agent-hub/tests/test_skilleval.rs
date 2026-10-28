//! Port of backend/tests/core/test_skilleval.py: SkillEvaluator integration (advisory): launcher, argv, private copy,
//! verdicts, staleness, the one-at-a-time lock, the environment contract, the API, the CLI and the real runner. No real
//! SkillEvaluator runs: the runner is a fake that writes canned reports (real report shapes, tests/fixtures/skilleval).
//! Service-level tests use a standalone service (no Hub, like `SkillEvalService(root, content_dir, ...)` in Python);
//! tests that need the content lock (Hub.mutating -> require_repo), the content store or the planner wait for
//! WS-B1/WS-B2 (`pending` ignores).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod wsc_support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_hub::services::skilleval::EvalRunner as _;
use agent_hub::services::skilleval::{
    INSTALL_HINT, RealEvalRunner, SAFE_CHECKS, SkillEvalService, combine_verdicts, content_hash, summarize_tier1,
    top_findings, valid_run_id,
};
use indexmap::IndexMap;
use serde_json::{Value, json};
use wsc_support::*;

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0)
}

fn code(r: Result<Value, hub_core::Problem>) -> String {
    r.err().map(|p| p.code).unwrap_or_default()
}

// ---- launcher -------------------------------------------------------------------------------------------------------
#[test]
fn test_launcher_prefers_the_wrapper_then_the_plain_cli_then_none() {
    let s = standalone();
    use_default(&s.svc);
    assert_eq!(
        s.svc.find_launcher(),
        Some((
            "skillevaluator-local".into(),
            format!("{FAKE_BIN}/skillevaluator-local")
        ))
    );
    use_(&s.svc, FakeEvalRunner::new(), &["skillevaluator"], None, None);
    assert_eq!(
        s.svc.find_launcher(),
        Some(("skillevaluator".into(), format!("{FAKE_BIN}/skillevaluator")))
    );
    use_(&s.svc, FakeEvalRunner::new(), &[], None, None);
    assert_eq!(s.svc.find_launcher(), None);
}

#[test]
fn test_launcher_search_path_adds_local_bin_once() {
    let s = standalone();
    let home = s.root.join("h");
    use_(
        &s.svc,
        FakeEvalRunner::new(),
        LAUNCHERS,
        Some(&[("PATH", "/usr/bin:/bin")]),
        Some(&home),
    );
    let seen: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::default();
    let s2 = seen.clone();
    s.svc.set_which(Some(Arc::new(move |name: &str, path: &str| {
        s2.lock().unwrap().push((name.into(), path.into()));
        None
    })));
    let _ = s.svc.find_launcher();
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
        ["skillevaluator-local", "skillevaluator"]
    );
    assert_eq!(seen[0].1, format!("/usr/bin:/bin:{}/.local/bin", home.display()));
    s.svc.set_environ(Some(vec![(
        "PATH".into(),
        format!("{}/.local/bin:/usr/bin", home.display()),
    )]));
    assert_eq!(s.svc.search_path(), format!("{}/.local/bin:/usr/bin", home.display()));
}

// ---- argv, cwd, copy --------------------------------------------------------------------------------------------------
#[test]
fn test_static_argv_no_llm_output_into_run_dir_cwd_is_the_private_copy() {
    let s = standalone();
    let r = use_default(&s.svc);
    let src = s.skill("alpha");
    let rec = s.svc.evaluate("alpha", "static", "t").unwrap();
    let call = r.tier1_call().unwrap();
    let run_id = rec["run_id"].as_str().unwrap();
    let run_dir = s.eval_root().join("alpha").join(run_id);
    assert_eq!(
        call.argv,
        vec![
            format!("{FAKE_BIN}/skillevaluator-local"),
            "tier1".into(),
            call.cwd.join("alpha").to_string_lossy().into_owned(),
            "--no-llm".into(),
            "--checks".into(),
            SAFE_CHECKS.into(),
            "-r".into(),
            "json,markdown".into(),
            "-o".into(),
            run_dir.join("static").to_string_lossy().into_owned(),
        ]
    );
    assert_eq!(call.cwd.parent(), Some(s.eval_root().join(".work").as_path()));
    assert!(!call.cwd.starts_with(&src) && !call.cwd.starts_with(s.content()));
    assert!(!call.cwd.exists(), "the copy is deleted after the run");
    assert_eq!(r.tool_calls().len(), 1);
    assert!((call.timeout - 540.0).abs() < f64::EPSILON);
    assert_eq!(rec["launcher"], "skillevaluator-local");
    assert_eq!(rec["model_calls"], false);
    assert!(valid_run_id(run_id));
}

#[test]
fn test_tier1_has_no_no_llm_and_tier1_plus_2_runs_tier2_json_only() {
    let s = standalone();
    let r = use_default(&s.svc);
    s.skill("alpha");
    let rec1 = s.svc.evaluate("alpha", "tier1", "t").unwrap();
    let calls = r.tool_calls();
    assert_eq!(
        calls
            .iter()
            .map(|c| c.argv.contains(&"--no-llm".into()))
            .collect::<Vec<_>>(),
        [true, false]
    );
    assert!(calls.iter().all(|c| c.argv[1] == "tier1"));
    assert!(calls[0].argv.last().unwrap().ends_with("/static"));
    assert!(
        calls[1]
            .argv
            .last()
            .unwrap()
            .ends_with(rec1["run_id"].as_str().unwrap())
    );
    let r2 = use_default(&s.svc);
    let rec = s.svc.evaluate("alpha", "tier1+2", "t").unwrap();
    let c2 = r2.tool_calls();
    assert_eq!(
        c2.iter().map(|c| c.argv[1].as_str()).collect::<Vec<_>>(),
        ["tier1", "tier1", "tier2"]
    );
    let t2 = &c2[2].argv;
    assert_eq!(t2[2], c2[0].argv[2]);
    assert_eq!(t2[3..5], ["-r".to_string(), "json".to_string()]);
    assert_eq!(t2[5], "-o");
    assert!(t2[6].ends_with(rec["run_id"].as_str().unwrap()));
    assert_eq!(c2[2].cwd, c2[0].cwd);
    assert!(
        !t2.contains(&"--checks".to_string()),
        "tier2 is catalog-less and intra-skill"
    );
    assert_eq!(rec["tier2"]["status"], "passed");
    let mut reports: Vec<String> = rec["reports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    reports.sort();
    assert_eq!(
        reports,
        [
            "skillevaluator-tier1.json",
            "skillevaluator-tier1.md",
            "skillevaluator-tier2.json",
            "static/skillevaluator-tier1.json",
            "static/skillevaluator-tier1.md"
        ]
    );
}

#[test]
fn test_every_tier1_argv_passes_exactly_the_safe_checks_and_never_dependency() {
    for tiers in ["static", "tier1", "tier1+2"] {
        let s = standalone();
        let r = use_default(&s.svc);
        s.skill("alpha");
        s.svc.evaluate("alpha", tiers, "t").unwrap();
        let t1: Vec<Vec<String>> = r
            .tool_calls()
            .into_iter()
            .filter(|c| c.argv[1] == "tier1")
            .map(|c| c.argv)
            .collect();
        assert_eq!(t1.len(), if tiers == "static" { 1 } else { 2 }, "{tiers}");
        for argv in t1 {
            assert_eq!(argv.iter().filter(|a| *a == "--checks").count(), 1);
            let i = argv.iter().position(|a| a == "--checks").unwrap();
            assert_eq!(argv[i + 1], SAFE_CHECKS);
            assert!(!argv.iter().any(|a| a.contains("dependency")));
        }
    }
    assert_eq!(
        SAFE_CHECKS,
        "schema,version,security,pii,license,code-integrity,unicode,quality,lint"
    );
}

#[test]
fn test_copy_excludes_hub_yaml_symlinks_and_git_and_hash_covers_exactly_the_copied_files() {
    let s = standalone();
    let r = use_default(&s.svc);
    let src = s.skill_with(
        "alpha",
        "Do alpha.\n",
        &[("ref/notes.md", "notes"), ("scripts/run.sh", "#!/bin/sh\necho hi\n")],
    );
    std::fs::set_permissions(src.join("scripts/run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(src.join("hub.yaml").is_file());
    let secret = s.root.join("outside-secret.txt");
    std::fs::write(&secret, "TOP SECRET").unwrap();
    std::os::unix::fs::symlink(&secret, src.join("leak.txt")).unwrap();
    std::os::unix::fs::symlink(&s.root, src.join("linkdir")).unwrap();
    std::fs::create_dir(src.join(".git")).unwrap();
    std::fs::write(src.join(".git/config"), "[core]\n").unwrap();
    std::fs::write(src.join("ref/hub.yaml"), "nested: a regular skill file\n").unwrap();
    let rec = s.svc.evaluate("alpha", "static", "t").unwrap();
    let seen = &r.seen()[0];
    let mut names: Vec<&String> = seen.files.keys().collect();
    names.sort();
    assert_eq!(names, ["SKILL.md", "ref/hub.yaml", "ref/notes.md", "scripts/run.sh"]);
    assert!(seen.symlinks.is_empty());
    assert!(
        !seen
            .files
            .values()
            .any(|b| String::from_utf8_lossy(b).contains("TOP SECRET"))
    );
    assert_eq!(seen.modes["scripts/run.sh"], 0o700);
    assert_eq!(seen.modes["SKILL.md"], 0o600);
    let mut expected: IndexMap<String, Vec<u8>> = IndexMap::new();
    for rel in seen.files.keys() {
        expected.insert(rel.clone(), std::fs::read(src.join(rel)).unwrap());
        assert_eq!(seen.files[rel], expected[rel]);
    }
    assert_eq!(rec["content_hash"], content_hash(&expected));
    assert_eq!(rec["files"], 4);
}

#[test]
fn test_content_hash_is_framed_and_order_independent() {
    let f = |pairs: &[(&str, &[u8])]| -> IndexMap<String, Vec<u8>> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), v.to_vec())).collect()
    };
    assert_ne!(content_hash(&f(&[("a", b"bc")])), content_hash(&f(&[("ab", b"c")])));
    assert_eq!(
        content_hash(&f(&[("x", b"1"), ("y", b"2")])),
        content_hash(&f(&[("y", b"2"), ("x", b"1")]))
    );
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(1u64.to_be_bytes());
    h.update(b"a");
    h.update(1u64.to_be_bytes());
    h.update(b"1");
    assert_eq!(content_hash(&f(&[("a", b"1")])), hex::encode(h.finalize()));
}

#[test]
fn test_copy_caps_refuse_too_many_files_or_bytes() {
    let s = standalone();
    use_default(&s.svc);
    let files: Vec<(String, String)> = (0..5).map(|i| (format!("r/{i}.md"), "x".to_string())).collect();
    let fr: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    s.skill_with("alpha", "Do alpha.\n", &fr);
    s.svc.set_limits(3, 4 * 1024 * 1024);
    assert_eq!(code(s.svc.evaluate("alpha", "static", "t")), "skill_too_large");
    s.svc.set_limits(200, 20);
    assert_eq!(code(s.svc.evaluate("alpha", "static", "t")), "skill_too_large");
    assert!(s.svc.records("alpha", 20).is_empty());
}

#[test]
fn test_skill_without_skill_md_and_unknown_skill_are_refused() {
    let s = standalone();
    let r = use_default(&s.svc);
    let d = s.skill("alpha");
    std::fs::remove_file(d.join("SKILL.md")).unwrap();
    assert_eq!(code(s.svc.evaluate("alpha", "static", "t")), "skill_invalid");
    let e = s.svc.evaluate("nope", "static", "t").err().unwrap();
    assert_eq!(e.status, 404);
    let e = s.svc.evaluate("alpha", "tier3", "t").err().unwrap();
    assert_eq!((e.code.as_str(), e.status), ("bad_tiers", 422));
    assert!(r.calls().is_empty());
}

#[test]
fn test_symlinked_skill_dir_is_not_a_skill() {
    let s = standalone();
    use_default(&s.svc);
    let other = s.root.join("elsewhere");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("SKILL.md"), "---\nname: evil\ndescription: x\n---\n").unwrap();
    std::os::unix::fs::symlink(&other, s.content().join("skills/evil")).unwrap();
    assert_eq!(s.svc.evaluate("evil", "static", "t").err().map(|p| p.status), Some(404));
}

// ---- verdicts -------------------------------------------------------------------------------------------------------
#[test]
fn test_verdict_comes_from_the_json_not_the_exit_code() {
    for (rc, status) in [(1, "passed"), (0, "failed"), (1, "incomplete"), (0, "passed")] {
        let s = standalone();
        use_(
            &s.svc,
            FakeEvalRunner::tier1(Canned::report(
                rc,
                report("tier1_incomplete.json", &[("overall_status", json!(status))]),
            )),
            LAUNCHERS,
            None,
            None,
        );
        s.skill("alpha");
        assert_eq!(s.svc.evaluate("alpha", "static", "t").unwrap()["verdict"], status);
    }
}

#[test]
fn test_tier2_failure_downgrades_a_passing_verdict() {
    let s = standalone();
    let passed = Canned::report(
        0,
        report("tier1_incomplete.json", &[("overall_status", json!("passed"))]),
    );
    let failed2 = Canned::report(1, report("tier2_passed.json", &[("overall_status", json!("failed"))]));
    use_(
        &s.svc,
        FakeEvalRunner::with(Some(passed.clone()), Some(failed2), None, None),
        LAUNCHERS,
        None,
        None,
    );
    s.skill("alpha");
    let rec = s.svc.evaluate("alpha", "tier1+2", "t").unwrap();
    assert_eq!(
        (
            rec["verdict"].as_str(),
            rec["tier1"]["status"].as_str(),
            rec["tier2"]["status"].as_str()
        ),
        (Some("failed"), Some("passed"), Some("failed"))
    );
    use_(
        &s.svc,
        FakeEvalRunner::with(
            Some(passed.clone()),
            Some(Canned::none(2, "no embeddings model")),
            None,
            None,
        ),
        LAUNCHERS,
        None,
        None,
    );
    let rec = s.svc.evaluate("alpha", "tier1+2", "t").unwrap();
    assert_eq!(rec["verdict"], "incomplete");
    assert_eq!(rec["tier2"]["status"], "error");
    assert!(rec["tier2"]["error"].as_str().unwrap().contains("no embeddings"));
    use_(&s.svc, FakeEvalRunner::tier1(passed), LAUNCHERS, None, None);
    assert_eq!(s.svc.evaluate("alpha", "tier1+2", "t").unwrap()["verdict"], "passed");
    assert_eq!(combine_verdicts("failed", "passed"), "failed");
    assert_eq!(combine_verdicts("incomplete", "failed"), "failed");
    assert_eq!(combine_verdicts("error", "passed"), "error");
    assert_eq!(combine_verdicts("passed", "incomplete"), "incomplete");
}

#[test]
fn test_no_report_is_an_error_run_and_stderr_is_redacted() {
    let s = standalone();
    let leak = format!("sk-{}", "A1b2C3d4E5f6G7h8I9j0");
    let stderr = format!(
        "\x1b[31mTraceback\x1b[0m in @COPY@/SKILL.md\nprovider said: Authorization: Bearer {leak}\napi_key={leak}\n"
    );
    use_(
        &s.svc,
        FakeEvalRunner::tier1(Canned::none(2, &stderr)),
        LAUNCHERS,
        None,
        None,
    );
    s.skill("alpha");
    let rec = s.svc.evaluate("alpha", "static", "t").unwrap();
    assert_eq!(rec["verdict"], "error");
    assert!(rec["tier1"].is_null());
    let err = rec["error"].as_str().unwrap().to_string();
    assert!(err.starts_with("exit 2, no report: Traceback"), "{err}");
    let stored = std::fs::read_to_string(
        s.eval_root()
            .join("alpha")
            .join(rec["run_id"].as_str().unwrap())
            .join("run.json"),
    )
    .unwrap();
    for text in [&err, &stored] {
        assert!(
            !text.contains(&leak) && text.contains("[redacted]") && !text.contains("\x1b["),
            "{text}"
        );
        assert!(text.contains("<skill-copy>/SKILL.md"));
        assert!(!text.contains(&s.data().to_string_lossy().into_owned()));
    }
}

#[test]
fn test_timeout_is_an_error_run() {
    let s = standalone();
    let c = Canned {
        timed_out: true,
        report: None,
        markdown: None,
        ..Canned::default()
    };
    use_(&s.svc, FakeEvalRunner::tier1(c), LAUNCHERS, None, None);
    s.skill("alpha");
    let rec = s.svc.evaluate("alpha", "static", "t").unwrap();
    assert_eq!(rec["verdict"], "error");
    assert!(rec["error"].as_str().unwrap().contains("timed out after 540 s"));
}

#[test]
fn test_summary_fields_and_top_findings_from_a_real_report() {
    let mut d = report("tier1_incomplete.json", &[]);
    let s = summarize_tier1(&d);
    assert_eq!(
        s,
        json!({"status": "incomplete", "quality_score": 83.0, "grade": "B", "judge_status": "succeeded",
               "judge_score": 83.6, "errors": 2, "warnings": 23,
               "severity": {"critical": 0, "high": 1, "medium": 6, "low": 15}, "incomplete": ["skillspector"]})
    );
    assert!(s["quality_score"].is_f64());
    d["rubric_eval"]["execution_status"] = json!("skipped");
    assert!(
        summarize_tier1(&d)["judge_score"].is_null(),
        "only a judge that ran counts"
    );
    let (rows, total) = top_findings(&report("tier1_incomplete.json", &[]), 12);
    assert_eq!(total, 5);
    assert_eq!(
        rows[0],
        json!({"severity": "high", "validator": "Schema & Repository Governance", "message": "Author not specified in metadata"})
    );
    assert!(
        rows.iter()
            .all(|x| x["severity"] == "high" || x["severity"] == "medium")
    );
    assert!(summarize_tier1(&json!({}))["status"].is_null());
    assert_eq!(top_findings(&json!({"results": "junk"}), 12), (vec![], 0));
}

// ---- staleness ------------------------------------------------------------------------------------------------------
fn states(svc: &SkillEvalService) -> std::collections::HashMap<String, String> {
    svc.list_latest()
        .into_iter()
        .map(|e| {
            (
                e["skill"].as_str().unwrap().to_string(),
                e["state"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn test_staleness_flips_when_skill_md_changes_and_back() {
    let s = standalone();
    use_default(&s.svc);
    let d = s.skill("alpha");
    assert_eq!(s.svc.detail("alpha", 20).unwrap()["state"], "none");
    s.svc.evaluate("alpha", "static", "t").unwrap();
    assert_eq!(states(&s.svc)["alpha"], "current");
    assert_eq!(s.svc.detail("alpha", 20).unwrap()["state"], "current");
    let original = std::fs::read(d.join("SKILL.md")).unwrap();
    let mut changed = original.clone();
    changed.extend_from_slice(b"\nOne more line.\n");
    std::fs::write(d.join("SKILL.md"), &changed).unwrap();
    assert_eq!(states(&s.svc)["alpha"], "stale");
    let det = s.svc.detail("alpha", 20).unwrap();
    assert_eq!(
        (det["state"].as_str(), det["history"][0]["state"].as_str()),
        (Some("stale"), Some("stale"))
    );
    std::fs::write(d.join("SKILL.md"), &original).unwrap();
    assert_eq!(states(&s.svc)["alpha"], "current", "same content again: current again");
    std::fs::write(d.join("ref/new.md"), "an extra file is part of the skill").unwrap();
    assert_eq!(states(&s.svc)["alpha"], "stale");
}

/// Python re-puts the skill through the content store (WS-B1) to change hub.yaml; the sidecar file is edited directly
/// here, which is what that put does to the files the evaluation reads.
#[test]
fn test_editing_the_hub_yaml_sidecar_does_not_make_an_evaluation_stale() {
    let s = standalone();
    use_default(&s.svc);
    let d = s.skill("alpha");
    s.svc.evaluate("alpha", "static", "t").unwrap();
    std::fs::write(d.join("hub.yaml"), "tags:\n- other\n- tags\n").unwrap();
    assert!(std::fs::read_to_string(d.join("hub.yaml")).unwrap().contains("other"));
    assert_eq!(s.svc.detail("alpha", 20).unwrap()["state"], "current");
}

// ---- path safety ----------------------------------------------------------------------------------------------------
#[test]
fn test_bad_skill_ids_are_refused() {
    let s = standalone();
    use_default(&s.svc);
    for bad in [
        "../alpha",
        "..",
        "a/b",
        "A",
        "alpha/../../x",
        "",
        ".work",
        &"x".repeat(65),
    ] {
        for st in [
            s.svc.evaluate(bad, "static", "t").err().map(|p| p.status),
            s.svc.detail(bad, 20).err().map(|p| p.status),
            s.svc
                .report(bad, "20261006T201500123Z-0a1b2c3d")
                .err()
                .map(|p| p.status),
        ] {
            assert!(matches!(st, Some(400 | 404)), "{bad}: {st:?}");
        }
    }
}

#[test]
fn test_bad_run_ids_are_refused() {
    let s = standalone();
    use_default(&s.svc);
    s.skill("alpha");
    for bad in [
        "../../hub.db",
        "..",
        "run.json",
        "20261006T201500123Z-0a1b2c3d/../x",
        "20261006T2015Z-abc",
        "20261006T201500123Z-0A1B2C3D",
        "20261006T201500123Z-0a1b2c3d\n",
    ] {
        assert_eq!(s.svc.report("alpha", bad).err().map(|p| p.status), Some(400), "{bad:?}");
    }
}

#[tokio::test]
async fn test_traversal_over_http() {
    let r = FakeEvalRunner::new();
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r.clone()),
            ..Default::default()
        },
        |_| {},
    );
    use_(&t.hub.skilleval, r.clone(), LAUNCHERS, None, None);
    alpha(&t);
    let a = app(&t);
    let h = bearer(&t.key("ev-admin", "admin"));
    let hdr = [("authorization", h.as_str())];
    for url in [
        "/api/v1/skills/..%2F..%2Fetc/evaluations",
        "/api/v1/skills/alpha/evaluations/..%2F..%2Fhub.db/report",
        "/api/v1/skills/alpha/evaluations/20261006T201500123Z-0a1b2c3d/report",
        "/api/v1/skills/A/evaluations",
    ] {
        let st = api(&a, "GET", url, &hdr, None).await.status;
        assert!(st == 400 || st == 404, "{url}: {st}");
    }
    for url in [
        "/api/v1/skills/..%2Fx/evaluate",
        "/api/v1/skills/..%2F..%2Fetc/evaluate",
        "/api/v1/skills/A/evaluate",
    ] {
        let st = api(
            &a,
            "POST",
            url,
            &[("authorization", h.as_str()), ("x-agent-hub", "1")],
            Some(json!({})),
        )
        .await
        .status;
        assert!(matches!(st, 400 | 404 | 405), "{url}: {st}");
    }
    assert!(r.calls().is_empty());
}

// ---- one evaluation at a time ---------------------------------------------------------------------------------------
#[test]
fn test_second_evaluation_is_refused_while_one_runs() {
    let s = standalone();
    let gate = Event::default();
    let r = use_(
        &s.svc,
        FakeEvalRunner::with(None, None, None, Some(gate.clone())),
        LAUNCHERS,
        None,
        None,
    );
    s.skill("alpha");
    s.skill("beta");
    let started = s.svc.start("alpha", "static", "t").unwrap();
    assert_eq!(started["status"], "running");
    assert!(r.started.wait(5.0));
    let second = s.svc.start("beta", "static", "t").unwrap();
    assert_eq!(second["status"], "queued", "F6: queued, no longer a 409");
    assert_eq!(s.svc.start("alpha", "static", "t").unwrap()["status"], "running");
    let e = s.svc.evaluate("beta", "static", "t").err().unwrap();
    assert_eq!(
        (e.code.as_str(), e.status),
        ("evaluation_running", 409),
        "a synchronous (CLI) evaluation still excludes"
    );
    assert_eq!(s.svc.running().unwrap()["skill"], "alpha");
    assert!(
        s.svc
            .list_latest()
            .iter()
            .any(|e| e["skill"] == "alpha" && e["running"] == true)
    );
    gate.set();
    s.svc.join(Some(15.0));
    assert!(s.svc.running().is_none());
    assert_eq!(s.svc.records("alpha", 20)[0]["skill"], "alpha");
    assert_eq!(s.svc.records("beta", 20)[0]["skill"], "beta");
}

#[test]
fn test_another_process_holding_the_lock_blocks_and_is_reported() {
    let s = standalone();
    use_default(&s.svc);
    s.skill("alpha");
    std::fs::create_dir_all(s.eval_root()).unwrap();
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(s.eval_root().join(".lock"))
        .unwrap();
    let held = nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusiveNonblock).unwrap();
    assert_eq!(
        s.svc.evaluate("alpha", "static", "t").err().map(|p| p.status),
        Some(409)
    );
    assert_eq!(
        s.svc.running(),
        Some(
            json!({"subject": null, "skill": null, "run_id": null, "tiers": null, "started_at": null, "elsewhere": true})
        )
    );
    drop(held);
    assert!(s.svc.running().is_none());
    assert_eq!(s.svc.evaluate("alpha", "static", "t").unwrap()["verdict"], "incomplete");
}

#[tokio::test]
async fn test_concurrent_post_is_queued_not_409_over_http() {
    let gate = Event::default();
    let r = FakeEvalRunner::with(None, None, None, Some(gate.clone()));
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r.clone()),
            ..Default::default()
        },
        |_| {},
    );
    t.hub.init_content().unwrap();
    use_(&t.hub.skilleval, r.clone(), LAUNCHERS, None, None);
    alpha(&t);
    let a = app(&t);
    let h = bearer(&t.key("ev-admin", "admin"));
    let hdr = [("authorization", h.as_str())];
    let post = |n: &str| format!("/api/v1/skills/{n}/evaluate");
    assert_eq!(
        api(&a, "POST", &post("alpha"), &hdr, Some(json!({"tiers": "static"})))
            .await
            .status,
        202
    );
    assert!(r.started.wait(5.0));
    skill(&t, "beta", "Do beta.\n", &[]);
    let busy = api(&a, "POST", &post("alpha"), &hdr, Some(json!({"tiers": "static"}))).await;
    assert_eq!((busy.status, busy.json()["status"].as_str()), (202, Some("running")));
    let queued = api(&a, "POST", &post("beta"), &hdr, Some(json!({"tiers": "static"}))).await;
    assert_eq!((queued.status, queued.json()["status"].as_str()), (202, Some("queued")));
    let q = api(&a, "GET", "/api/v1/skill-evaluations/queue", &hdr, None)
        .await
        .json();
    assert_eq!(q["running"]["subject"], "hub:alpha");
    assert_eq!(q["queued"], json!([{"subject": "hub:beta", "tiers": "static"}]));
    let ev = api(&a, "GET", "/api/v1/skills/alpha/evaluations", &hdr, None)
        .await
        .json();
    assert_eq!(ev["running"]["skill"], "alpha");
    gate.set();
    t.hub.skilleval.join(Some(5.0));
}

// ---- environment ----------------------------------------------------------------------------------------------------
#[test]
fn test_env_passthrough_only_for_the_evaluator() {
    let s = standalone();
    let home = s.root.join("h");
    let environ: &[(&str, &str)] = &[
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/home/operator"),
        ("SKILL_EVAL_PROVIDER", "openai-compatible"),
        ("SKILL_EVAL_API_KEY", "value-not-checked-here"),
        ("SKILLEVAL_TIMEOUT", "30"),
        ("OPENAI_API_KEY", "x"),
        ("HUB_API_KEY", "y"),
        ("AWS_SECRET_ACCESS_KEY", "z"),
        ("XSKILL_EVAL_NOPE", "n"),
    ];
    let r = use_(&s.svc, FakeEvalRunner::new(), LAUNCHERS, Some(environ), Some(&home));
    s.skill("alpha");
    s.svc.evaluate("alpha", "static", "t").unwrap();
    let env = r.tier1_call().unwrap().env;
    let want: std::collections::HashMap<String, String> = [
        ("PATH", format!("/usr/bin:/bin:{}/.local/bin", home.display())),
        ("HOME", "/home/operator".to_string()),
        ("LC_ALL", "C".to_string()),
        ("SKILL_EVAL_PROVIDER", "openai-compatible".to_string()),
        ("SKILL_EVAL_API_KEY", "value-not-checked-here".to_string()),
        ("SKILLEVAL_TIMEOUT", "30".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(env, want);
    assert_eq!(
        s.svc.env_names(),
        ["SKILLEVAL_TIMEOUT", "SKILL_EVAL_API_KEY", "SKILL_EVAL_PROVIDER"]
    );
}

/// The second half of Python's env test: every OTHER hub subprocess keeps the plain minimal environment.
#[test]
fn test_env_passthrough_only_for_the_evaluator_real_runner() {
    use agent_hub::adapters::base::CommandRunner as _;
    let r = agent_hub::services::fs::RealRunner::default();
    let (rc, out, _) = r.run(&["/usr/bin/env".to_string()], 5.0).unwrap();
    assert_eq!(rc, 0);
    let mut names: Vec<&str> = out.lines().filter_map(|l| l.split('=').next()).collect();
    names.sort_unstable();
    assert_eq!(names, ["HOME", "LC_ALL", "PATH"]);
}

#[tokio::test]
async fn test_status_shows_env_names_never_values() {
    let r = FakeEvalRunner::new();
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r.clone()),
            ..Default::default()
        },
        |_| {},
    );
    let key = format!("sk-{}", "Z".repeat(24));
    use_(
        &t.hub.skilleval,
        r,
        LAUNCHERS,
        Some(&[("PATH", "/usr/bin"), ("SKILL_EVAL_API_KEY", key.as_str())]),
        None,
    );
    let a = app(&t);
    let h = bearer(&t.key("ev-admin", "admin"));
    let body = api(
        &a,
        "GET",
        "/api/v1/skill-evaluator",
        &[("authorization", h.as_str())],
        None,
    )
    .await
    .text();
    assert!(body.contains("SKILL_EVAL_API_KEY") && !body.contains("ZZZZ"), "{body}");
}

// ---- API: roles, CSRF, audit, cheap reads ------------------------------------------------------------------------------
#[tokio::test]
async fn test_post_evaluate_needs_admin_and_the_csrf_header() {
    let r = FakeEvalRunner::new();
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r.clone()),
            ..Default::default()
        },
        |_| {},
    );
    t.hub.init_content().unwrap();
    use_(&t.hub.skilleval, r.clone(), LAUNCHERS, None, None);
    alpha(&t);
    let a = app(&t);
    let url = "/api/v1/skills/alpha/evaluate";
    let viewer = bearer(&t.key("v", "viewer"));
    let resp = api(
        &a,
        "POST",
        url,
        &[("authorization", viewer.as_str())],
        Some(json!({"tiers": "static"})),
    )
    .await;
    assert_eq!((resp.status, resp.code()), (403, "forbidden".to_string()));
    assert_eq!(
        api(&a, "POST", url, &[("x-agent-hub", "1")], Some(json!({})))
            .await
            .status,
        401
    );
    let resp = api(
        &a,
        "POST",
        url,
        &[("x-agent-hub", "1"), ("origin", "http://evil.example")],
        Some(json!({})),
    )
    .await;
    assert_eq!(resp.status, 403);
    assert!(r.calls().is_empty(), "nothing started");
    let adm = bearer(&t.key("ev-admin", "admin"));
    let ok = api(&a, "POST", url, &[("authorization", adm.as_str())], Some(json!({}))).await;
    assert_eq!(ok.status, 202, "{}", ok.text());
    assert_eq!(
        (ok.json()["tiers"].as_str(), ok.json()["status"].as_str()),
        (Some("static"), Some("running"))
    );
    assert_eq!(ok.json()["subject"], "hub:alpha");
    t.hub.skilleval.join(Some(5.0));
    assert_eq!(
        api(
            &a,
            "POST",
            url,
            &[("authorization", adm.as_str())],
            Some(json!({"tiers": "tier9"}))
        )
        .await
        .status,
        422
    );
    assert_eq!(
        api(
            &a,
            "POST",
            url,
            &[("authorization", adm.as_str())],
            Some(json!({"tiers": "static", "x": 1}))
        )
        .await
        .status,
        422
    );
}

#[tokio::test]
async fn test_csrf_header_on_loopback_trust_without_bearer() {
    let r = FakeEvalRunner::new();
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r.clone()),
            ..Default::default()
        },
        |c| {
            c.trust_loopback = true;
        },
    );
    t.hub.init_content().unwrap();
    use_(&t.hub.skilleval, r, LAUNCHERS, None, None);
    alpha(&t);
    let a = app(&t);
    // the loopback peer: security::handle from 127.0.0.1
    use http_body_util::BodyExt;
    let send = |hdrs: Vec<(&'static str, &'static str)>| {
        let a = a.clone();
        async move {
            let mut b = http::Request::builder()
                .method("POST")
                .uri("/api/v1/skills/alpha/evaluate")
                .header("host", "127.0.0.1:8792");
            for (k, v) in hdrs {
                b = b.header(k, v);
            }
            let req = b
                .header("content-type", "application/json")
                .body(axum::body::Body::from("{}"))
                .unwrap();
            let resp =
                agent_hub::api::security::handle(a, std::net::SocketAddr::from(([127, 0, 0, 1], 5000)), 8792, req)
                    .await;
            let (p, body) = resp.into_parts();
            (
                p.status.as_u16(),
                body.collect().await.map(|c| c.to_bytes().to_vec()).unwrap_or_default(),
            )
        }
    };
    let (st, body) = send(vec![]).await;
    assert_eq!(st, 403);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["code"],
        "csrf_header_required"
    );
    assert_eq!(send(vec![("x-agent-hub", "1")]).await.0, 202);
    t.hub.skilleval.join(Some(5.0));
}

#[tokio::test]
async fn test_post_evaluate_is_audited() {
    let r = FakeEvalRunner::new();
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r.clone()),
            ..Default::default()
        },
        |_| {},
    );
    t.hub.init_content().unwrap();
    use_(&t.hub.skilleval, r, LAUNCHERS, None, None);
    alpha(&t);
    let a = app(&t);
    let h = bearer(&t.key("ev-admin", "admin"));
    let hdr = [("authorization", h.as_str())];
    let run_id = api(
        &a,
        "POST",
        "/api/v1/skills/alpha/evaluate",
        &hdr,
        Some(json!({"tiers": "static"})),
    )
    .await
    .json()["run_id"]
        .clone();
    t.hub.skilleval.join(Some(5.0));
    let items = api(&a, "GET", "/api/v1/audit", &hdr, None).await.json()["items"].clone();
    let items = items.as_array().unwrap();
    let req = items
        .iter()
        .find(|i| i["method"] == "POST" && i["path"] == "/api/v1/skills/alpha/evaluate")
        .unwrap();
    assert_eq!(
        (req["status"].as_i64(), req["role"].as_str()),
        (Some(202), Some("admin"))
    );
    assert_eq!(req["params"]["body"], json!({"tiers": "static"}));
    let ev = items
        .iter()
        .find(|i| i["method"] == "EVENT" && i["path"] == "skill.evaluate")
        .unwrap();
    assert_eq!(ev["actor"], "ev-admin");
    assert_eq!(ev["params"]["run_id"], run_id);
    assert_eq!(ev["params"]["verdict"], "incomplete");
}

#[test]
fn test_cli_evaluation_is_audited_as_hubctl() {
    let r = FakeEvalRunner::new();
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r.clone()),
            ..Default::default()
        },
        |_| {},
    );
    t.hub.init_content().unwrap();
    use_(&t.hub.skilleval, r, LAUNCHERS, None, None);
    alpha(&t);
    let out = cli(&t, &["evaluate-skill", "alpha"]);
    assert_eq!(out.code, 2, "{}", out.err);
    let page = t.hub.audit_svc.list_page(50, None).unwrap();
    let ev = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["path"] == "skill.evaluate")
        .cloned()
        .unwrap();
    assert_eq!(ev["actor"], "hubctl");
}

#[tokio::test]
async fn test_reads_never_start_a_process() {
    let r = FakeEvalRunner::new();
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r.clone()),
            ..Default::default()
        },
        |_| {},
    );
    t.hub.init_content().unwrap();
    use_(&t.hub.skilleval, r.clone(), LAUNCHERS, None, None);
    alpha(&t);
    skill(&t, "beta", "Do beta.\n", &[]);
    t.hub.skilleval.evaluate("alpha", "static", "t").unwrap();
    let n = r.calls().len();
    let a = app(&t);
    let h = bearer(&t.key("ev-admin", "admin"));
    let hdr = [("authorization", h.as_str())];
    let lst = api(&a, "GET", "/api/v1/skill-evaluations", &hdr, None).await.json();
    let states: std::collections::HashMap<String, String> = lst["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["skill"].as_str().unwrap().into(), e["state"].as_str().unwrap().into()))
        .collect();
    assert_eq!(
        states,
        [("alpha".into(), "current".into()), ("beta".into(), "none".into())]
            .into_iter()
            .collect()
    );
    assert!(lst["running"].is_null());
    let alpha_latest = lst["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["skill"] == "alpha")
        .unwrap()["latest"]
        .clone();
    assert_eq!(alpha_latest["tier1"]["grade"], "B");
    assert_eq!(alpha_latest["verdict"], "incomplete");
    assert_eq!(alpha_latest["findings"][0]["severity"], "high");
    let det = api(&a, "GET", "/api/v1/skills/alpha/evaluations", &hdr, None)
        .await
        .json();
    assert_eq!(det["state"], "current");
    assert_eq!(det["history"].as_array().unwrap().len(), 1);
    assert_eq!(det["content_hash"], det["latest"]["content_hash"]);
    let rid = det["latest"]["run_id"].as_str().unwrap().to_string();
    api(
        &a,
        "GET",
        &format!("/api/v1/skills/alpha/evaluations/{rid}/report"),
        &hdr,
        None,
    )
    .await;
    assert_eq!(r.calls().len(), n, "listing, detail and the report spawn nothing");
    let viewer = bearer(&t.key("v", "viewer"));
    assert_eq!(
        api(
            &a,
            "GET",
            "/api/v1/skill-evaluations",
            &[("authorization", viewer.as_str())],
            None
        )
        .await
        .status,
        200
    );
}

/// Python reads the report over HTTP; the service's `report()` is what the route returns (a standalone service runs
/// the evaluation, so the hub's content lock is not needed).
#[test]
fn test_report_is_redacted_and_paths_are_scrubbed() {
    let s = standalone();
    let leak = format!("ghp_{}", "Q".repeat(30));
    let md = format!("# Report\n\nScanned `@COPY@/SKILL.md`.\n\ntoken: {leak}\n\n<script>alert(1)</script>\n");
    let c = Canned {
        rc: 1,
        report: Some(report("tier1_incomplete.json", &[("note", json!("@COPY@"))])),
        markdown: Some(md),
        ..Canned::default()
    };
    use_(&s.svc, FakeEvalRunner::tier1(c), LAUNCHERS, None, None);
    s.skill("alpha");
    let rec = s.svc.evaluate("alpha", "static", "t").unwrap();
    let rid = rec["run_id"].as_str().unwrap();
    let out = s.svc.report("alpha", rid).unwrap();
    let mdout = out["markdown"].as_str().unwrap();
    assert_eq!(out["truncated"], false);
    assert!(!mdout.contains(&leak) && mdout.contains("[redacted]"));
    assert!(
        mdout.contains("`<skill-copy>/SKILL.md`"),
        "the deleted copy's path is not shown"
    );
    let run_dir = s.eval_root().join("alpha").join(rid);
    for name in ["static/skillevaluator-tier1.md", "static/skillevaluator-tier1.json"] {
        assert!(
            !std::fs::read_to_string(run_dir.join(name)).unwrap().contains(".work"),
            "also scrubbed on disk"
        );
    }
    let j: Value =
        serde_json::from_str(&std::fs::read_to_string(run_dir.join("static/skillevaluator-tier1.json")).unwrap())
            .unwrap();
    assert_eq!(j["note"], "<skill-copy>", "JSON stays valid");
    assert_eq!(
        s.svc
            .report("alpha", "20000101T000000000Z-00000000")
            .err()
            .map(|p| p.status),
        Some(404)
    );
}

#[tokio::test]
async fn test_missing_evaluator_is_503_with_the_install_hint() {
    let r = FakeEvalRunner::new();
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r.clone()),
            ..Default::default()
        },
        |_| {},
    );
    use_(&t.hub.skilleval, r, &[], None, None);
    alpha(&t);
    let s = standalone();
    use_(&s.svc, FakeEvalRunner::new(), &[], None, None);
    s.skill("alpha");
    let e = s.svc.evaluate("alpha", "static", "t").err().unwrap();
    assert_eq!(e.code, "evaluator_missing");
    assert_eq!(e.extra.get("fix"), Some(&json!(INSTALL_HINT)));
    let a = app(&t);
    let h = bearer(&t.key("ev-admin", "admin"));
    let resp = api(
        &a,
        "POST",
        "/api/v1/skills/alpha/evaluate",
        &[("authorization", h.as_str())],
        Some(json!({})),
    )
    .await;
    assert_eq!(
        (resp.status, resp.code()),
        (503, "evaluator_missing".to_string()),
        "{}",
        resp.text()
    );
    assert_eq!(resp.json()["fix"], INSTALL_HINT);
    let st = api(
        &a,
        "GET",
        "/api/v1/skill-evaluator",
        &[("authorization", h.as_str())],
        None,
    )
    .await
    .json();
    assert_eq!(
        (st["installed"].clone(), st["launcher"].clone()),
        (json!(false), Value::Null)
    );
    assert_eq!(st["install_hint"], INSTALL_HINT);
}

#[test]
fn test_status_caches_the_version_probe() {
    let s = standalone();
    let r = use_default(&s.svc);
    let st = s.svc.status().unwrap();
    assert_eq!(st["installed"], true);
    assert_eq!(st["launcher"], "skillevaluator-local");
    assert_eq!(st["version"], "0.5.0");
    assert_eq!(st["wrapper"], true);
    assert_eq!(
        st["tiers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["static", "tier1", "tier1+2"]
    );
    assert_eq!(st["timeout_s"], json!(540.0));
    assert!(st["timeout_s"].is_f64());
    s.svc.status().unwrap();
    assert_eq!(
        r.calls().iter().map(|c| c.argv.clone()).collect::<Vec<_>>(),
        vec![vec![
            format!("{FAKE_BIN}/skillevaluator-local"),
            "--version".to_string()
        ]]
    );
    use_(&s.svc, r.clone(), &["skillevaluator"], None, None);
    assert_eq!(
        s.svc.status().unwrap()["launcher"],
        "skillevaluator",
        "the launcher changed: probe again"
    );
    assert_eq!(r.calls().len(), 2);
    assert_eq!(s.svc.doctor_info().unwrap()["version"], "0.5.0");
    assert_eq!(r.calls().len(), 2, "doctor never spawns");
}

// ---- storage --------------------------------------------------------------------------------------------------------
#[test]
fn test_history_keeps_the_newest_n_runs() {
    let s = standalone();
    use_default(&s.svc);
    s.skill("alpha");
    s.svc.set_keep(3);
    let mut ids: Vec<String> = (0..5)
        .map(|_| {
            let id = s.svc.evaluate("alpha", "static", "t").unwrap()["run_id"]
                .as_str()
                .unwrap()
                .to_string();
            std::thread::sleep(std::time::Duration::from_millis(2)); // distinct millisecond stamps, like a real run
            id
        })
        .collect();
    ids.sort();
    let mut left: Vec<String> = std::fs::read_dir(s.eval_root().join("alpha"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ids[2..]);
    let recs: Vec<String> = s
        .svc
        .records("alpha", 20)
        .iter()
        .map(|r| r["run_id"].as_str().unwrap().to_string())
        .collect();
    let mut want = ids[2..].to_vec();
    want.reverse();
    assert_eq!(recs, want);
}

#[test]
fn test_state_is_private() {
    let s = standalone();
    use_default(&s.svc);
    s.skill("alpha");
    let rec = s.svc.evaluate("alpha", "tier1+2", "t").unwrap();
    let root = s.eval_root();
    let run_dir = root.join("alpha").join(rec["run_id"].as_str().unwrap());
    for d in [
        root.clone(),
        root.join("alpha"),
        run_dir.clone(),
        run_dir.join("static"),
    ] {
        assert_eq!(mode(&d), 0o700, "{}", d.display());
    }
    for dir in [run_dir.clone(), run_dir.join("static")] {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if !p.is_dir() {
                assert_eq!(mode(&p), 0o600, "{}", p.display());
            }
        }
    }
    assert_eq!(mode(&root.join(".lock")), 0o600);
    assert_eq!(std::fs::read_dir(root.join(".work")).unwrap().count(), 0);
}

#[test]
fn test_symlinks_left_in_the_output_dir_are_dropped() {
    let s = standalone();
    let target = s.root.join("target.txt");
    std::fs::write(&target, "x").unwrap();
    let r = use_default(&s.svc);
    let tgt = target.clone();
    r.state().after_run = Some(Arc::new(move |argv: &[String]| {
        if let Some(i) = argv.iter().position(|a| a == "-o") {
            let _ = std::os::unix::fs::symlink(&tgt, PathBuf::from(&argv[i + 1]).join("evil-link"));
        }
    }));
    s.skill("alpha");
    let rec = s.svc.evaluate("alpha", "static", "t").unwrap();
    let run_dir = s.eval_root().join("alpha").join(rec["run_id"].as_str().unwrap());
    assert!(std::fs::symlink_metadata(run_dir.join("evil-link")).is_err());
    assert!(std::fs::symlink_metadata(run_dir.join("static/evil-link")).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
}

fn tree(root: &Path) -> std::collections::BTreeMap<String, String> {
    use sha2::Digest as _;
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                out.insert(rel, "dir".into());
                stack.push(p);
            } else {
                out.insert(rel, hex::encode(sha2::Sha256::digest(std::fs::read(&p).unwrap())));
            }
        }
    }
    out
}

/// The content tree is byte-for-byte unchanged (Python also asserts `hub.repo.status() == []`: see the WS-B2 test below).
#[test]
fn test_an_evaluation_never_touches_the_content_repo() {
    let s = standalone();
    let r = use_default(&s.svc);
    s.skill("alpha");
    let before = tree(&s.content());
    r.state().write_into_cwd = true; // the tool writes next to itself and into its copy
    s.svc.evaluate("alpha", "static", "t").unwrap();
    assert_eq!(tree(&s.content()), before);
    assert_ne!(r.seen()[0].files["SKILL.md"], b"tampered by the evaluator".to_vec());
    assert_eq!(std::fs::read_dir(s.eval_root().join(".work")).unwrap().count(), 0);
}

#[test]
fn test_an_evaluation_never_touches_the_content_repo_git_status() {
    let r = FakeEvalRunner::new();
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r.clone()),
            ..Default::default()
        },
        |_| {},
    );
    t.hub.init_content().unwrap();
    use_(&t.hub.skilleval, r.clone(), LAUNCHERS, None, None);
    alpha(&t);
    t.hub.commit("skill", "t").unwrap();
    r.state().write_into_cwd = true;
    t.hub.skilleval.evaluate("alpha", "static", "t").unwrap();
    assert!(t.hub.repo.status().unwrap().is_empty());
}

#[test]
fn test_evaluations_are_advisory_plan_and_floor_unchanged() {
    let r = FakeEvalRunner::new();
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r),
            ..Default::default()
        },
        |_| {},
    );
    t.hub.init_content().unwrap();
    let floor_before = std::fs::read(t.hub.cfg().policy_file).unwrap();
    let home = t.root.join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    put_client(
        &t,
        "fake1",
        &json!({"adapter": "claude-code", "display_name": "F", "roots": {"project": home.to_string_lossy()}}),
    )
    .unwrap();
    alpha(&t);
    put_profile(&t, "fake1", &json!({"enable": {"skills": ["alpha"]}})).unwrap();
    t.hub.commit("c", "t").unwrap();
    let before = t.hub.plan(None, "committed").unwrap().clients[0].clone();
    use_(
        &t.hub.skilleval,
        FakeEvalRunner::tier1(Canned::report(
            1,
            report("tier1_incomplete.json", &[("overall_status", json!("failed"))]),
        )),
        LAUNCHERS,
        None,
        None,
    );
    assert_eq!(
        t.hub.skilleval.evaluate("alpha", "static", "t").unwrap()["verdict"],
        "failed"
    );
    let after = t.hub.plan(None, "committed").unwrap().clients[0].clone();
    assert!(!after.blocked);
    assert_eq!(after.digest, before.digest);
    assert!(
        !after
            .diagnostics
            .iter()
            .any(|d| d["code"].as_str().unwrap_or_default().contains("eval"))
    );
    assert_eq!(std::fs::read(t.hub.cfg().policy_file).unwrap(), floor_before);
}

// ---- CLI ------------------------------------------------------------------------------------------------------------
fn cli_hub(r: Arc<FakeEvalRunner>) -> T {
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r.clone()),
            ..Default::default()
        },
        |_| {},
    );
    t.hub.init_content().unwrap();
    use_(&t.hub.skilleval, r, LAUNCHERS, None, None);
    t
}

#[test]
fn test_cli_evaluate_skill_exit_codes() {
    for (status, code) in [("passed", 0), ("failed", 2), ("incomplete", 2)] {
        let t = cli_hub(FakeEvalRunner::tier1(Canned::report(
            1,
            report("tier1_incomplete.json", &[("overall_status", json!(status))]),
        )));
        alpha(&t);
        let out = cli(&t, &["evaluate-skill", "alpha", "--tiers", "static"]);
        assert_eq!(out.code, code, "{status}: {}", out.err);
        assert!(out.out.contains(&format!("alpha: {status}")) && out.out.contains("quality 83.0 grade B"));
        assert!(out.out.contains("[HIGH] Schema & Repository Governance") && out.out.contains("advisory only"));
    }
}

#[test]
fn test_cli_evaluate_skill_errors_exit_1() {
    let t = cli_hub(FakeEvalRunner::tier1(Canned::none(2, "boom")));
    alpha(&t);
    assert_eq!(cli(&t, &["evaluate-skill", "alpha"]).code, 1, "no report");
    assert_eq!(cli(&t, &["evaluate-skill", "nope"]).code, 1, "unknown skill");
    use_(&t.hub.skilleval, FakeEvalRunner::new(), &[], None, None);
    let out = cli(&t, &["evaluate-skill", "alpha"]);
    assert_eq!(out.code, 1, "not installed");
    assert!(out.err.contains("evaluator_missing") && out.err.contains(INSTALL_HINT));
    assert_eq!(cli(&t, &["evaluate-skill", "alpha", "--tiers", "tier3"]).code, 2);
}

#[test]
fn test_cli_evaluate_skills_prints_a_table() {
    let r = FakeEvalRunner::new();
    let t = cli_hub(r.clone());
    alpha(&t);
    skill(&t, "beta", "Do beta.\n", &[]);
    let out = cli(&t, &["evaluate-skills", "--tiers", "static"]);
    assert_eq!(out.code, 2, "incomplete reports exist");
    let lines: Vec<&str> = out.out.lines().collect();
    assert_eq!(
        lines[0].split_whitespace().collect::<Vec<_>>(),
        ["skill", "verdict", "quality", "grade", "judge", "errors", "high"]
    );
    assert_eq!(
        lines[1..]
            .iter()
            .map(|l| l.split_whitespace().take(2).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [["alpha", "incomplete"], ["beta", "incomplete"]]
    );
    assert!(r.tool_calls().iter().all(|c| c.argv.contains(&"--no-llm".to_string())));
    let out = cli(&t, &["--json", "evaluate-skills"]);
    assert_eq!(out.code, 2);
    let v: Value = serde_json::from_str(&out.out).unwrap();
    assert_eq!(
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["skill"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
    use_(&t.hub.skilleval, FakeEvalRunner::new(), &[], None, None);
    assert_eq!(cli(&t, &["evaluate-skills"]).code, 1);
}

// ---- doctor ---------------------------------------------------------------------------------------------------------
#[test]
fn test_doctor_reports_the_evaluator_as_info() {
    let r = FakeEvalRunner::new();
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r.clone()),
            ..Default::default()
        },
        |_| {},
    );
    use_(&t.hub.skilleval, r.clone(), &[], None, None);
    let find = |t: &T| {
        t.hub.doctor(None).unwrap()["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "skill-evaluator")
            .cloned()
            .unwrap()
    };
    let f = find(&t);
    assert_eq!(f["severity"], "info");
    assert!(f["title"].as_str().unwrap().contains("not installed"));
    assert_eq!(f["fix"], INSTALL_HINT);
    use_(&t.hub.skilleval, r.clone(), LAUNCHERS, None, None);
    let f = find(&t);
    assert!(f["title"].as_str().unwrap().contains("skillevaluator-local"));
    assert!(r.calls().is_empty(), "no subprocess");
    use_(
        &t.hub.skilleval,
        r,
        &["skillevaluator"],
        Some(&[("PATH", "/usr/bin"), ("SKILL_EVAL_PROVIDER", "p")]),
        None,
    );
    let f = find(&t);
    assert!(
        f["detail"].as_str().unwrap().contains("SKILL_EVAL_PROVIDER")
            && f["title"].as_str().unwrap().contains("(skillevaluator)")
    );
}

// ---- the real runner (real child processes, offline: /bin/sh) -------------------------------------------------------
#[test]
fn test_real_runner_kills_the_group_on_timeout_and_caps_output() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("grandchild.pid");
    let script = format!(
        "sleep 60 & echo $! > {}; head -c 300000 /dev/zero | tr '\\0' x; sleep 60",
        pidfile.display()
    );
    let r = RealEvalRunner::new(1000);
    let t0 = std::time::Instant::now();
    let res = r.run(
        &["/bin/sh".into(), "-c".into(), script],
        tmp.path(),
        &[("PATH".into(), "/usr/bin:/bin".into())],
        3.0,
    );
    assert!(res.timed_out && t0.elapsed().as_secs() < 20);
    assert!(
        res.stdout.len() <= 1000 + "[...] ".len() && res.stdout.starts_with("[...] "),
        "{}",
        res.stdout.len()
    );
    let gpid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(gpid), None).is_ok();
        let zombie = std::fs::read_to_string(format!("/proc/{gpid}/stat"))
            .map(|s| s.split_whitespace().nth(2) == Some("Z"))
            .unwrap_or(true);
        if !alive || zombie {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the grandchild in the evaluator's process group survived the timeout"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn test_real_runner_passes_cwd_and_only_the_given_env() {
    let tmp = tempfile::tempdir().unwrap();
    let r = RealEvalRunner::default();
    let env = [
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("SKILL_EVAL_X".to_string(), "1".to_string()),
    ];
    let res = r.run(&["/usr/bin/env".into()], tmp.path(), &env, 30.0);
    assert_eq!(res.returncode, 0);
    let mut names: Vec<&str> = res.stdout.lines().filter_map(|l| l.split('=').next()).collect();
    names.sort_unstable();
    assert_eq!(names, ["PATH", "SKILL_EVAL_X"]);
    let res = r.run(&["/bin/pwd".into()], tmp.path(), &env, 30.0);
    assert_eq!(
        res.stdout.trim(),
        std::fs::canonicalize(tmp.path()).unwrap().to_string_lossy()
    );
    assert_eq!(r.run(&[], tmp.path(), &[], 1.0).returncode, 127);
    assert_eq!(
        r.run(&["/nonexistent/skillevaluator".into()], tmp.path(), &[], 1.0)
            .returncode,
        127
    );
}

#[test]
fn test_service_defaults_to_the_real_runner_and_shutil_which() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let s = SkillEvalService::new(
        &root.join("skilleval"),
        &root.join("content"),
        std::sync::Weak::new(),
        None,
        200,
        1 << 22,
        &root.join("observed"),
    );
    s.set_environ(Some(vec![(
        "PATH".into(),
        root.join("bin").to_string_lossy().into_owned(),
    )]));
    s.set_home(Some(root.to_path_buf()));
    std::fs::create_dir(root.join("bin")).unwrap();
    let exe = root.join("bin/skillevaluator");
    // written by a child process: if this process held the write fd, a fork by a parallel test could carry it into
    // its child until exec, and running the script would fail with ETXTBSY ("Text file busy")
    let mut w = std::process::Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(&exe)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        &mut w.stdin.take().unwrap(),
        b"#!/bin/sh\necho 'skillevaluator, version 9.9.9'\n",
    )
    .unwrap();
    assert!(w.wait().unwrap().success());
    assert_eq!(
        s.find_launcher(),
        Some(("skillevaluator".into(), exe.to_string_lossy().into_owned()))
    );
    assert_eq!(
        s.status().unwrap()["version"],
        "9.9.9",
        "a stand-in script on a temp PATH, not the real tool"
    );
}

#[test]
fn test_compiled_bytecode_is_never_evaluated_and_does_not_change_the_hash() {
    let s = standalone();
    let r = use_default(&s.svc);
    let src = s.skill_with(
        "alpha",
        "Do alpha.\n",
        &[("ref/notes.md", "notes"), ("scripts/tool.py", "print('hi')\n")],
    );
    let clean = s.svc.snapshot("alpha").unwrap();
    std::fs::create_dir(src.join("scripts/__pycache__")).unwrap();
    std::fs::write(
        src.join("scripts/__pycache__/tool.cpython-313.pyc"),
        b"\x00\x0d\x0d\x0a bytecode",
    )
    .unwrap();
    std::fs::write(src.join("stray.pyc"), b"\x00 bytecode").unwrap();
    std::fs::write(src.join("old.pyo"), b"\x00 bytecode").unwrap();
    let rec = s.svc.evaluate("alpha", "static", "t").unwrap();
    let mut names: Vec<String> = r.seen()[0].files.keys().cloned().collect();
    names.sort();
    assert_eq!(names, ["SKILL.md", "ref/notes.md", "scripts/tool.py"]);
    assert_eq!(rec["content_hash"], clean.content_hash());
    assert_eq!(rec["files"], 3);
}
