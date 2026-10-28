//! Port of backend/tests/core/test_skilleval_partial.py: record revision 2. A static Security Scan that SkillSpector
//! called `partial` only because the skill mentions local paths it does not bundle is a warning, not a block. Every other
//! partial reason, a missing scanner, a timeout or an unreadable report still blocks (fail closed). The SkillSpector
//! report shape is the one captured from SkillSpector 2.12.0 (only the fields the hub reads).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod wsc_support;

use std::path::PathBuf;

use agent_hub::services::skilleval::{
    ENV_PREFIXES, EVAL_REV, PARTIAL_ERROR, SKILLSPECTOR_REPORT, SKILLSPECTOR_TIMEOUT_S, UNRESOLVED_REFERENCES,
    classify_partial_scan,
};
use serde_json::{Value, json};
use wsc_support::*;

fn partial_msg() -> String {
    format!("{PARTIAL_ERROR}; security scan did not complete")
}

fn reference() -> Value {
    json!({"outcome": "partial", "phase": "reference_resolution", "reason_code": "reference_missing", "path": "SKILL.md",
           "start_line": 8, "end_line": 8, "fatal": false,
           "message": "A local path-like reference does not match any bundled artifact, such as a file the skill writes at runtime."})
}

fn with(base: &Value, changes: &[(&str, Value)]) -> Value {
    let mut v = base.clone();
    for (k, x) in changes {
        v[*k] = x.clone();
    }
    v
}

fn analyzers() -> Value {
    json!([
        {"analyzer_id": "artifact_integrity", "status": "completed"},
        {"analyzer_id": "behavioral_ast", "status": "not_applicable", "reason_code": "no_applicable_files"},
        {"analyzer_id": "semantic_developer_intent", "status": "disabled", "reason_code": "disabled_by_configuration"},
        {"analyzer_id": "semantic_security_discovery", "status": "disabled", "reason_code": "disabled_by_configuration"},
        {"analyzer_id": "meta_analyzer", "status": "disabled", "reason_code": "disabled_by_configuration"},
    ])
}

const LLM_IDS: [&str; 5] = [
    "mcp_tool_poisoning",
    "meta_analyzer",
    "semantic_developer_intent",
    "semantic_quality_policy",
    "semantic_security_discovery",
];

/// `ss_report(**ac)`.
fn ss_report(ac: &[(&str, Value)]) -> Value {
    let mut completeness = json!({"total_components": 1, "scanned_components": 1, "coverage_percent": 100.0,
        "is_complete": false, "status": "partial", "execution_successful": true, "fully_inspected_files": 1,
        "partially_inspected_files": 0, "entirely_uninspected_files": 0, "limitations": [],
        "ledger_exceptions": [reference(), with(&reference(), &[("start_line", json!(32)), ("end_line", json!(32))])],
        "analyzer_statuses": analyzers()});
    for (k, v) in ac {
        completeness[*k] = v.clone();
    }
    let llm: Vec<Value> = LLM_IDS.iter().map(|a| json!({"analyzer_id": a})).collect();
    json!({"skill": {"name": "alpha", "source": "@COPY@"}, "execution_successful": true,
           "metadata": {"skillspector_version": "2.12.0", "llm_requested": false, "llm_provenance": {"analyzers": llm}},
           "analysis_completeness": completeness})
}

// ---- the classifier -------------------------------------------------------------------------------------------------
#[test]
fn test_references_only_is_classified() {
    assert_eq!(
        classify_partial_scan(&ss_report(&[])),
        Some(
            json!({"reason": UNRESOLVED_REFERENCES, "count": 2, "skillspector_version": "2.12.0",
                    "references": [{"path": "SKILL.md", "line": 8}, {"path": "SKILL.md", "line": 32}]})
        )
    );
}

fn excluded() -> Value {
    with(
        &reference(),
        &[
            ("phase", json!("exclusion_audit")),
            ("reason_code", json!("excluded_executable_content")),
            ("path", json!("scripts/__pycache__/x.pyc")),
        ],
    )
}

#[test]
fn test_every_other_partial_reason_is_not_classified() {
    let mut degraded = analyzers().as_array().unwrap().clone();
    degraded.push(json!({"analyzer_id": "behavioral_taint_tracking", "status": "degraded"}));
    let mut core_off = analyzers().as_array().unwrap().clone();
    core_off.push(
        json!({"analyzer_id": "artifact_integrity", "status": "disabled", "reason_code": "disabled_by_configuration"}),
    );
    let changes: Vec<Vec<(&str, Value)>> = vec![
        vec![("partially_inspected_files", json!(1))],
        vec![("entirely_uninspected_files", json!(1))],
        vec![("fully_inspected_files", json!(0)), ("coverage_percent", json!(0.0))],
        vec![("coverage_percent", json!(99.0))],
        vec![("limitations", json!(["file too large"]))],
        vec![("status", json!("failed"))],
        vec![("status", json!("complete")), ("is_complete", json!(true))],
        vec![("execution_successful", json!(false))],
        vec![("ledger_exceptions", json!([]))],
        vec![("ledger_exceptions", json!([reference(), excluded()]))],
        vec![(
            "ledger_exceptions",
            json!([with(
                &reference(),
                &[("reason_code", json!("static_parse_limit")), ("phase", json!("static"))]
            )]),
        )],
        vec![(
            "ledger_exceptions",
            json!([with(&reference(), &[("fatal", json!(true))])]),
        )],
        vec![(
            "ledger_exceptions",
            json!([with(&reference(), &[("outcome", json!("skipped"))])]),
        )],
        vec![("analyzer_statuses", Value::Array(degraded))],
        vec![("analyzer_statuses", Value::Array(core_off))],
        vec![("analyzer_statuses", json!([]))],
        vec![("total_components", json!(true))],
    ];
    assert_eq!(changes.len(), 17);
    for c in changes {
        assert_eq!(classify_partial_scan(&ss_report(&c)), None, "{c:?}");
    }
}

#[test]
fn test_only_llm_analyzers_may_be_disabled_and_only_without_an_llm() {
    let mut r = ss_report(&[]);
    r["metadata"]["llm_provenance"]["analyzers"] = json!([{"analyzer_id": "semantic_developer_intent"}]); // meta_analyzer not LLM-backed
    assert_eq!(classify_partial_scan(&r), None);
    let mut r = ss_report(&[]);
    r["metadata"]["llm_requested"] = json!(true);
    assert_eq!(classify_partial_scan(&r), None);
}

#[test]
fn test_a_report_without_success_markers_is_not_classified() {
    let r = with(&ss_report(&[]), &[("execution_successful", Value::Null)]);
    assert_eq!(classify_partial_scan(&r), None);
    assert_eq!(classify_partial_scan(&json!({})), None);
}

// ---- through a real evaluation (fake SkillEvaluator, fake SkillSpector) ---------------------------------------------
/// `security(status, errors, findings, incomplete)`: a static tier1 report where every expected security validator
/// passed, except Security Scan as given.
fn security(
    status: &str,
    errors: Option<Vec<String>>,
    findings: &[(&str, &str)],
    incomplete: Option<Vec<&str>>,
) -> Canned {
    let mut results: Vec<Value> = [
        "PII Scan",
        "Code Risk Analysis",
        "Secrets Detection",
        "Code Integrity & Hygiene",
        "Unicode Smuggling Detection",
    ]
    .iter()
    .map(|n| json!({"validator": n, "passed": true, "status": "passed", "incomplete_scans": [], "findings": []}))
    .collect();
    let f: Vec<Value> = findings
        .iter()
        .map(|(sev, msg)| json!({"category": "X", "severity": sev, "check": null, "message": msg}))
        .collect();
    results.push(json!({"validator": "Security Scan", "passed": false, "status": status,
                        "incomplete_scans": incomplete.unwrap_or_else(|| vec!["skillspector"]),
                        "findings": f,
                        "legacy": {"errors": errors.unwrap_or_else(|| vec![partial_msg()]), "warnings": [], "messages": []}}));
    let inc = if status == "incomplete" {
        json!(["skillspector"])
    } else {
        json!([])
    };
    Canned::report(
        1,
        report(
            "tier1_incomplete.json",
            &[
                ("overall_status", json!(status)),
                ("results", Value::Array(results)),
                ("incomplete_scans", inc),
            ],
        ),
    )
}

fn security_default() -> Canned {
    security("incomplete", None, &[], None)
}

/// `run(hub, runner, spector=)`: evaluate `alpha` statically and return (record, gate).
fn run(s: &S, r: std::sync::Arc<FakeEvalRunner>, spector: bool) -> (Value, Value) {
    let launchers: &[&str] = if spector {
        &["skillevaluator-local", "skillevaluator", "skillspector"]
    } else {
        LAUNCHERS
    };
    use_(&s.svc, r, launchers, None, None);
    s.skill("alpha");
    let rec = s.svc.evaluate("alpha", "static", "t").unwrap();
    let gate = s.svc.gate(rec["content_hash"].as_str().unwrap());
    (rec, gate)
}

#[test]
fn test_unresolved_references_warn_instead_of_block() {
    let s = standalone();
    let r = FakeEvalRunner::tier1(security_default());
    r.state().spector = Some(ss_report(&[]));
    let (rec, gate) = run(&s, r.clone(), true);
    assert_eq!(gate["status"], "warn");
    assert_eq!(rec["verdict"], "incomplete");
    assert_eq!(
        gate["reasons"],
        json!([
            "Security Scan: 2 local reference(s) not bundled with the skill (SKILL.md:8, SKILL.md:32); every bundled file was scanned"
        ])
    );
    assert_eq!(rec["eval_rev"], json!(EVAL_REV));
    assert_eq!(rec["scan_partial"]["Security Scan"]["reason"], UNRESOLVED_REFERENCES);
    let calls = r.scan_calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    let copy_dir = PathBuf::from(&call.argv[2]);
    let work = copy_dir.parent().unwrap().to_path_buf();
    assert_eq!(
        call.argv,
        vec![
            format!("{FAKE_BIN}/skillspector"),
            "scan".to_string(),
            copy_dir.to_string_lossy().into_owned(),
            "--format".into(),
            "json".into(),
            "--no-llm".into(),
            "--output".into(),
            work.join(SKILLSPECTOR_REPORT).to_string_lossy().into_owned(),
        ]
    );
    assert_eq!(call.cwd, work);
    assert!((call.timeout - SKILLSPECTOR_TIMEOUT_S).abs() < f64::EPSILON);
    assert!(
        call.env
            .keys()
            .all(|k| ["PATH", "HOME", "LC_ALL"].contains(&k.as_str()) || ENV_PREFIXES.iter().any(|p| k.starts_with(p)))
    );
    assert!(!work.exists(), "the work dir, with the SkillSpector report, is gone");
}

#[test]
fn test_other_partial_reasons_still_block() {
    let s = standalone();
    let r = FakeEvalRunner::tier1(security_default());
    r.state().spector = Some(ss_report(&[("ledger_exceptions", json!([reference(), excluded()]))]));
    let (rec, gate) = run(&s, r, true);
    assert_eq!(gate["status"], "blocked");
    assert!(
        gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("Security Scan: status incomplete (the scan did not finish)"))
    );
    assert_eq!(rec["scan_partial"], json!({}));
}

#[test]
fn test_a_high_finding_still_blocks_a_references_only_scan() {
    let s = standalone();
    let r = FakeEvalRunner::tier1(security(
        "incomplete",
        Some(vec![
            partial_msg(),
            "[SECURITY-HIGH] Downloads and runs a remote script in SKILL.md:9".into(),
        ]),
        &[("high", "Downloads and runs a remote script")],
        None,
    ));
    r.state().spector = Some(ss_report(&[]));
    let (rec, gate) = run(&s, r, true);
    assert_eq!(rec["scan_partial"]["Security Scan"]["reason"], UNRESOLVED_REFERENCES);
    assert_eq!(gate["status"], "blocked");
    let reasons: Vec<String> = gate["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    assert!(reasons.iter().any(|x| x.contains("Downloads and runs a remote script")));
    assert!(!reasons.iter().any(|x| x.contains("did not finish")));
}

#[test]
fn test_anything_else_blocks_without_a_classification() {
    let cases: Vec<(Canned, bool, bool)> = vec![
        (security_default(), false, false), // SkillSpector not found: nothing to read
        (security_default(), true, true),   // it timed out
        (
            security(
                "incomplete",
                Some(vec![
                    "skillspector did not return valid JSON (exit code 2); security scan did not complete".into(),
                ]),
                &[],
                None,
            ),
            true,
            false,
        ),
        (
            security(
                "incomplete",
                Some(vec![
                    partial_msg(),
                    "skillspector reported errors; security scan did not complete".into(),
                ]),
                &[],
                None,
            ),
            true,
            false,
        ),
        (
            security("incomplete", None, &[], Some(vec!["skillspector", "gitleaks"])),
            true,
            false,
        ),
    ];
    for (i, (canned, spector, timed_out)) in cases.into_iter().enumerate() {
        let s = standalone();
        let r = FakeEvalRunner::tier1(canned);
        {
            let mut st = r.state();
            st.spector = Some(ss_report(&[]));
            st.spector_timed_out = timed_out;
        }
        let (rec, gate) = run(&s, r, spector);
        assert_eq!(gate["status"], "blocked", "case {i}");
        assert_eq!(rec["scan_partial"], json!({}), "case {i}");
    }
}

#[test]
fn test_an_unreadable_skillspector_report_blocks() {
    let s = standalone();
    let r = FakeEvalRunner::tier1(security_default());
    r.state().spector = None; // it ran but wrote no report
    let (_, gate) = run(&s, r, true);
    assert_eq!(gate["status"], "blocked");
}

#[test]
fn test_older_records_with_an_incomplete_security_scan_are_evaluated_again_once() {
    let s = standalone();
    let r = FakeEvalRunner::tier1(security_default());
    r.state().spector = Some(ss_report(&[]));
    run(&s, r.clone(), true);
    let n = r.tool_calls().len();
    s.svc.evaluate_force("alpha", "static", "t", false).unwrap();
    assert_eq!(r.tool_calls().len(), n, "same revision: reused");
    s.svc.set_eval_rev(EVAL_REV + 1); // the stored record is now an older revision
    s.svc.evaluate_force("alpha", "static", "t", false).unwrap();
    assert!(
        r.tool_calls().len() > n,
        "evaluated again (tier1 + the SkillSpector read)"
    );
}

#[test]
fn test_older_records_whose_security_scan_finished_are_still_reused() {
    let s = standalone();
    let r = FakeEvalRunner::tier1(security("passed", Some(vec![]), &[], Some(vec![])));
    run(&s, r.clone(), true);
    let n = r.tool_calls().len();
    s.svc.set_eval_rev(EVAL_REV + 1);
    s.svc.evaluate_force("alpha", "static", "t", false).unwrap();
    assert_eq!(r.tool_calls().len(), n);
}
