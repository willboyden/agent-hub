"""Record revision 2: a static Security Scan that SkillSpector called `partial` only because the skill mentions local paths it
does not bundle is a warning, not a block. Every other partial reason, a missing scanner, a timeout or an unreadable report
still blocks (fail closed). The SkillSpector report shape below was captured from SkillSpector 2.12.0 (`scan --format json
--no-llm` on a real skill; only the fields the hub reads are kept)."""
from __future__ import annotations

import copy as copymod
from pathlib import Path
from typing import Any

import pytest

from agent_hub.services import skilleval as se
from agent_hub.services.hub import Hub
from agent_hub.services.skilleval import PARTIAL_ERROR, UNRESOLVED_REFERENCES, classify_partial_scan

from .fakes import Canned, FakeEvalRunner, report
from .test_skilleval import FAKE_BIN, skill, tool_calls, use

PARTIAL_MSG = PARTIAL_ERROR + "; security scan did not complete"
REF = {"outcome": "partial", "phase": "reference_resolution", "reason_code": "reference_missing", "path": "SKILL.md",
       "start_line": 8, "end_line": 8, "fatal": False,
       "message": "A local path-like reference does not match any bundled artifact, such as a file the skill writes at runtime."}
ANALYZERS = [
    {"analyzer_id": "artifact_integrity", "status": "completed"},
    {"analyzer_id": "behavioral_ast", "status": "not_applicable", "reason_code": "no_applicable_files"},
    {"analyzer_id": "semantic_developer_intent", "status": "disabled", "reason_code": "disabled_by_configuration"},
    {"analyzer_id": "semantic_security_discovery", "status": "disabled", "reason_code": "disabled_by_configuration"},
    {"analyzer_id": "meta_analyzer", "status": "disabled", "reason_code": "disabled_by_configuration"},
]
LLM_IDS = ["mcp_tool_poisoning", "meta_analyzer", "semantic_developer_intent", "semantic_quality_policy",
           "semantic_security_discovery"]


def ss_report(**ac: Any) -> dict[str, Any]:
    completeness = {"total_components": 1, "scanned_components": 1, "coverage_percent": 100.0, "is_complete": False,
                    "status": "partial", "execution_successful": True, "fully_inspected_files": 1,
                    "partially_inspected_files": 0, "entirely_uninspected_files": 0, "limitations": [],
                    "ledger_exceptions": [dict(REF), {**REF, "start_line": 32, "end_line": 32}],
                    "analyzer_statuses": copymod.deepcopy(ANALYZERS)}
    completeness.update(ac)
    return {"skill": {"name": "alpha", "source": "@COPY@"}, "execution_successful": True,
            "metadata": {"skillspector_version": "2.12.0", "llm_requested": False,
                         "llm_provenance": {"analyzers": [{"analyzer_id": a} for a in LLM_IDS]}},
            "analysis_completeness": completeness}


# ---- the classifier ---------------------------------------------------------------------------------------------------------
def test_references_only_is_classified() -> None:
    got = classify_partial_scan(ss_report())
    assert got == {"reason": UNRESOLVED_REFERENCES, "count": 2, "skillspector_version": "2.12.0",
                   "references": [{"path": "SKILL.md", "line": 8}, {"path": "SKILL.md", "line": 32}]}


EXCLUDED = {**REF, "phase": "exclusion_audit", "reason_code": "excluded_executable_content", "path": "scripts/__pycache__/x.pyc"}


@pytest.mark.parametrize("change", [
    {"partially_inspected_files": 1}, {"entirely_uninspected_files": 1}, {"fully_inspected_files": 0, "coverage_percent": 0.0},
    {"coverage_percent": 99.0}, {"limitations": ["file too large"]}, {"status": "failed"}, {"status": "complete", "is_complete": True},
    {"execution_successful": False}, {"ledger_exceptions": []}, {"ledger_exceptions": [dict(REF), EXCLUDED]},
    {"ledger_exceptions": [{**REF, "reason_code": "static_parse_limit", "phase": "static"}]},
    {"ledger_exceptions": [{**REF, "fatal": True}]}, {"ledger_exceptions": [{**REF, "outcome": "skipped"}]},
    {"analyzer_statuses": [*ANALYZERS, {"analyzer_id": "behavioral_taint_tracking", "status": "degraded"}]},
    {"analyzer_statuses": [*ANALYZERS, {"analyzer_id": "artifact_integrity", "status": "disabled",
                                        "reason_code": "disabled_by_configuration"}]},
    {"analyzer_statuses": []}, {"total_components": True},
])
def test_every_other_partial_reason_is_not_classified(change: dict[str, Any]) -> None:
    assert classify_partial_scan(ss_report(**change)) is None


def test_only_llm_analyzers_may_be_disabled_and_only_without_an_llm() -> None:
    r = ss_report()
    r["metadata"]["llm_provenance"]["analyzers"] = [{"analyzer_id": "semantic_developer_intent"}]   # meta_analyzer not LLM-backed
    assert classify_partial_scan(r) is None
    r = ss_report()
    r["metadata"]["llm_requested"] = True
    assert classify_partial_scan(r) is None


def test_a_report_without_success_markers_is_not_classified() -> None:
    assert classify_partial_scan({**ss_report(), "execution_successful": None}) is None
    assert classify_partial_scan({}) is None


# ---- through a real evaluation (fake SkillEvaluator, fake SkillSpector) ------------------------------------------------------
def security(status: str = "incomplete", errors: list[str] | None = None, findings: list[tuple[str, str]] | None = None,
             incomplete: list[str] | None = None) -> Canned:
    """A static tier1 report: every expected security validator passed, except Security Scan as given."""
    names = ("PII Scan", "Code Risk Analysis", "Secrets Detection", "Code Integrity & Hygiene", "Unicode Smuggling Detection")
    results: list[dict[str, Any]] = [{"validator": n, "passed": True, "status": "passed", "incomplete_scans": [], "findings": []}
                                     for n in names]
    results.append({"validator": "Security Scan", "passed": False, "status": status,
                    "incomplete_scans": ["skillspector"] if incomplete is None else incomplete,
                    "findings": [{"category": "X", "severity": sev, "check": None, "message": msg} for sev, msg in findings or []],
                    "legacy": {"errors": [PARTIAL_MSG] if errors is None else errors, "warnings": [], "messages": []}})
    return Canned(rc=1, report=report("tier1_incomplete.json", overall_status=status, results=results,
                                      incomplete_scans=["skillspector"] if status == "incomplete" else []))


def run(hub: Hub, runner: FakeEvalRunner, *, spector: bool = True) -> tuple[dict[str, Any], dict[str, Any]]:
    use(hub, runner, launchers=("skillevaluator-local", "skillevaluator", *(("skillspector",) if spector else ())))
    skill(hub)
    rec = hub.skilleval.evaluate("alpha", "static", "t")
    return rec, hub.skilleval.gate(str(rec["content_hash"]))


def scan_calls(r: FakeEvalRunner) -> list[dict[str, Any]]:
    return [c for c in r.calls if c["argv"][1:2] == ["scan"]]


def test_unresolved_references_warn_instead_of_block(hub: Hub) -> None:
    r = FakeEvalRunner(tier1=security())
    r.spector = ss_report()
    rec, gate = run(hub, r)
    assert gate["status"] == "warn" and rec["verdict"] == "incomplete"
    assert gate["reasons"] == ["Security Scan: 2 local reference(s) not bundled with the skill (SKILL.md:8, SKILL.md:32); "
                               "every bundled file was scanned"]
    assert rec["eval_rev"] == se.EVAL_REV and rec["scan_partial"]["Security Scan"]["reason"] == UNRESOLVED_REFERENCES
    (call,) = scan_calls(r)
    copy_dir = Path(call["argv"][2])
    assert call["argv"] == [f"{FAKE_BIN}/skillspector", "scan", str(copy_dir), "--format", "json", "--no-llm",
                            "--output", str(copy_dir.parent / se.SKILLSPECTOR_REPORT)]
    assert call["cwd"] == copy_dir.parent and call["timeout"] == se.SKILLSPECTOR_TIMEOUT_S
    assert set(call["env"]) <= {"PATH", "HOME", "LC_ALL"} | {k for k in call["env"] if k.startswith(se.ENV_PREFIXES)}
    assert not copy_dir.parent.exists()                                    # the work dir, with the SkillSpector report, is gone


def test_other_partial_reasons_still_block(hub: Hub) -> None:
    r = FakeEvalRunner(tier1=security())
    r.spector = ss_report(ledger_exceptions=[dict(REF), EXCLUDED])
    rec, gate = run(hub, r)
    assert gate["status"] == "blocked" and "Security Scan: status incomplete (the scan did not finish)" in gate["reasons"]
    assert rec["scan_partial"] == {}


def test_a_high_finding_still_blocks_a_references_only_scan(hub: Hub) -> None:
    # SkillEvaluator lists the findings among the scan's error lines too; they do not stop the classification, and the
    # finding still blocks.
    r = FakeEvalRunner(tier1=security(findings=[("high", "Downloads and runs a remote script")],
                                      errors=[PARTIAL_MSG, "[SECURITY-HIGH] Downloads and runs a remote script in SKILL.md:9"]))
    r.spector = ss_report()
    rec, gate = run(hub, r)
    assert rec["scan_partial"]["Security Scan"]["reason"] == UNRESOLVED_REFERENCES
    assert gate["status"] == "blocked" and any("Downloads and runs a remote script" in x for x in gate["reasons"])
    assert not any("did not finish" in x for x in gate["reasons"])


@pytest.mark.parametrize("canned,spector,timed_out", [
    (security(), False, False),                                            # SkillSpector not found: nothing to read
    (security(), True, True),                                              # it timed out
    (security(errors=["skillspector did not return valid JSON (exit code 2); security scan did not complete"]), True, False),
    (security(errors=[PARTIAL_MSG, "skillspector reported errors; security scan did not complete"]), True, False),
    (security(incomplete=["skillspector", "gitleaks"]), True, False),
])
def test_anything_else_blocks_without_a_classification(hub: Hub, canned: Canned, spector: bool, timed_out: bool) -> None:
    r = FakeEvalRunner(tier1=canned)
    r.spector = ss_report()
    r.spector_timed_out = timed_out
    rec, gate = run(hub, r, spector=spector)
    assert gate["status"] == "blocked" and rec["scan_partial"] == {}


def test_an_unreadable_skillspector_report_blocks(hub: Hub) -> None:
    r = FakeEvalRunner(tier1=security())
    r.spector = None                                                       # it ran but wrote no report
    _, gate = run(hub, r)
    assert gate["status"] == "blocked"


def test_older_records_with_an_incomplete_security_scan_are_evaluated_again_once(hub: Hub,
                                                                                monkeypatch: pytest.MonkeyPatch) -> None:
    r = FakeEvalRunner(tier1=security())
    r.spector = ss_report()
    run(hub, r)
    n = len(tool_calls(r))
    hub.skilleval.evaluate("alpha", "static", "t", force=False)
    assert len(tool_calls(r)) == n                                         # same revision: reused
    monkeypatch.setattr(se, "EVAL_REV", se.EVAL_REV + 1)                   # the stored record is now an older revision
    hub.skilleval.evaluate("alpha", "static", "t", force=False)
    assert len(tool_calls(r)) > n                                          # evaluated again (tier1 + the SkillSpector read)


def test_older_records_whose_security_scan_finished_are_still_reused(hub: Hub, monkeypatch: pytest.MonkeyPatch) -> None:
    r = FakeEvalRunner(tier1=security(status="passed", errors=[], incomplete=[]))
    run(hub, r)
    n = len(tool_calls(r))
    monkeypatch.setattr(se, "EVAL_REV", se.EVAL_REV + 1)
    hub.skilleval.evaluate("alpha", "static", "t", force=False)
    assert len(tool_calls(r)) == n
