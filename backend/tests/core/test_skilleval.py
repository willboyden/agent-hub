"""SkillEvaluator integration (advisory): launcher, argv, private copy, verdicts, staleness, the one-at-a-time lock, the
environment contract, the API (roles, CSRF, audit), the CLI and the doctor entry. No real SkillEvaluator runs here: the runner is
a fake that writes canned reports (real report shapes from tests/core/fixtures/skilleval)."""
from __future__ import annotations

import fcntl
import hashlib
import json
import os
import stat
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

import pytest
from fastapi.testclient import TestClient

from agent_hub import cli as cli_mod
from agent_hub.cli import run as cli_run
from agent_hub.config import Settings
from agent_hub.domain.errors import BadRequest, Conflict, NotFound, Unavailable, Unprocessable
from agent_hub.services import fs as fs_mod
from agent_hub.services.doctor import check_skill_evaluator
from agent_hub.services.hub import Hub
from agent_hub.services.skilleval import (
    INSTALL_HINT,
    RUN_ID_RE,
    SAFE_CHECKS,
    RealEvalRunner,
    SkillEvalService,
    combine_verdicts,
    content_hash,
    summarize_tier1,
    top_findings,
)

from .conftest import CSRF, add_client, bearer
from .fakes import Canned, FakeEvalRunner, report

API = "/api/v1"
FAKE_BIN = "/opt/fake-bin"


def use(hub: Hub, runner: FakeEvalRunner | None = None, *, launchers: tuple[str, ...] = ("skillevaluator-local", "skillevaluator"),
        environ: dict[str, str] | None = None, home: Path | None = None) -> FakeEvalRunner:
    """Point the hub's service at a fake SkillEvaluator: `launchers` are the names `which` finds."""
    r = runner or FakeEvalRunner()
    s = hub.skilleval
    s.runner = r
    s.which = lambda name, _path: f"{FAKE_BIN}/{name}" if name in launchers else None
    s.environ = environ if environ is not None else {"PATH": "/usr/bin:/bin", "HOME": "/home/operator"}
    if home is not None:
        s.home = home
    return r


def skill(hub: Hub, name: str = "alpha", body: str = "Do alpha.\n", files: dict[str, str] | None = None) -> Path:
    hub.work.put("skill", name, {"description": "Alpha skill", "body": body, "files": files or {"ref/notes.md": "notes"},
                                 "tags": ["core"]})
    return hub.cfg.content_dir / "skills" / name


def tier1_call(r: FakeEvalRunner) -> dict[str, Any]:
    return next(c for c in r.calls if c["argv"][1] == "tier1")


def tool_calls(r: FakeEvalRunner) -> list[dict[str, Any]]:
    """Calls that evaluated something (the cached `--version` probe is not one)."""
    return [c for c in r.calls if c["argv"][1:] != ["--version"]]


def admin_h(hub: Hub) -> dict[str, str]:
    return bearer(str(hub.auth.create_key("ev-admin", "admin")["secret"]))


# ---- launcher ------------------------------------------------------------------------------------------------------------
def test_launcher_prefers_the_wrapper_then_the_plain_cli_then_none(hub: Hub) -> None:
    use(hub)
    assert hub.skilleval.find_launcher() == ("skillevaluator-local", f"{FAKE_BIN}/skillevaluator-local")
    use(hub, launchers=("skillevaluator",))
    assert hub.skilleval.find_launcher() == ("skillevaluator", f"{FAKE_BIN}/skillevaluator")
    use(hub, launchers=())
    assert hub.skilleval.find_launcher() is None


def test_launcher_search_path_adds_local_bin_once(hub: Hub, tmp_path: Path) -> None:
    seen: list[tuple[str, str]] = []
    use(hub, environ={"PATH": "/usr/bin:/bin"}, home=tmp_path)
    hub.skilleval.which = lambda name, path: seen.append((name, path)) or None   # type: ignore[func-returns-value]
    hub.skilleval.find_launcher()
    assert [n for n, _ in seen] == ["skillevaluator-local", "skillevaluator"]          # bare names only, in this order
    assert seen[0][1] == f"/usr/bin:/bin:{tmp_path}/.local/bin"
    hub.skilleval.environ = {"PATH": f"{tmp_path}/.local/bin:/usr/bin"}
    assert hub.skilleval.search_path() == f"{tmp_path}/.local/bin:/usr/bin"


# ---- argv, cwd, copy -------------------------------------------------------------------------------------------------------
def test_static_argv_no_llm_output_into_run_dir_cwd_is_the_private_copy(hub: Hub) -> None:
    r = use(hub)
    src = skill(hub)
    rec = hub.skilleval.evaluate("alpha", "static", "t")
    call = tier1_call(r)
    argv, cwd = call["argv"], call["cwd"]
    run_dir = hub.cfg.data_dir / "skilleval" / "alpha" / rec["run_id"]
    assert argv == [f"{FAKE_BIN}/skillevaluator-local", "tier1", str(cwd / "alpha"), "--no-llm", "--checks", SAFE_CHECKS,
                    "-r", "json,markdown", "-o", str(run_dir / "static")]
    assert cwd.parent == hub.cfg.data_dir / "skilleval" / ".work"                     # private temp dir under the state dir
    assert src.resolve() not in [cwd, *cwd.parents] and hub.cfg.content_dir.resolve() not in cwd.resolve().parents
    assert not cwd.exists()                                                           # the copy is deleted after the run
    assert len(tool_calls(r)) == 1 and call["timeout"] == 540.0
    assert rec["launcher"] == "skillevaluator-local" and rec["model_calls"] is False and RUN_ID_RE.fullmatch(rec["run_id"])


def test_tier1_has_no_no_llm_and_tier1_plus_2_runs_tier2_json_only(hub: Hub) -> None:
    r = use(hub)
    skill(hub)
    rec1 = hub.skilleval.evaluate("alpha", "tier1", "t")
    calls = tool_calls(r)                                   # F14: a static pass first, then the LLM pass
    assert [("--no-llm" in c["argv"]) for c in calls] == [True, False] and all(c["argv"][1] == "tier1" for c in calls)
    assert calls[0]["argv"][-1].endswith("/static") and calls[1]["argv"][-1].endswith(rec1["run_id"])
    r2 = use(hub, FakeEvalRunner())
    rec = hub.skilleval.evaluate("alpha", "tier1+2", "t")
    c2 = tool_calls(r2)
    assert [c["argv"][1] for c in c2] == ["tier1", "tier1", "tier2"]
    t2 = c2[2]["argv"]
    assert t2[2] == c2[0]["argv"][2] and t2[3:5] == ["-r", "json"] and t2[5] == "-o" and t2[6].endswith(rec["run_id"])
    assert c2[2]["cwd"] == c2[0]["cwd"] and "--checks" not in t2                       # tier2 is catalog-less and intra-skill
    assert rec["tier2"]["status"] == "passed" and set(rec["reports"]) == {
        "skillevaluator-tier1.json", "skillevaluator-tier1.md", "skillevaluator-tier2.json", "static/skillevaluator-tier1.json",
        "static/skillevaluator-tier1.md"}


@pytest.mark.parametrize("tiers", ["static", "tier1", "tier1+2"])
def test_every_tier1_argv_passes_exactly_the_safe_checks_and_never_dependency(hub: Hub, tiers: str) -> None:
    """F14: the tool's default check set includes `dependency` (pip-audit installs packages a skill chose)."""
    r = use(hub)
    skill(hub)
    hub.skilleval.evaluate("alpha", tiers, "t")
    t1 = [c["argv"] for c in tool_calls(r) if c["argv"][1] == "tier1"]
    assert len(t1) == (1 if tiers == "static" else 2)
    for argv in t1:
        assert argv.count("--checks") == 1 and argv[argv.index("--checks") + 1] == SAFE_CHECKS
        assert not any("dependency" in a for a in argv)
    assert SAFE_CHECKS == "schema,version,security,pii,license,code-integrity,unicode,quality,lint"


def test_copy_excludes_hub_yaml_symlinks_and_git_and_hash_covers_exactly_the_copied_files(hub: Hub, tmp_path: Path) -> None:
    r = use(hub)
    src = skill(hub, files={"ref/notes.md": "notes", "scripts/run.sh": "#!/bin/sh\necho hi\n"})
    (src / "scripts" / "run.sh").chmod(0o755)
    assert (src / "hub.yaml").is_file()                                               # the sidecar exists in content
    secret = tmp_path / "outside-secret.txt"
    secret.write_text("TOP SECRET")
    (src / "leak.txt").symlink_to(secret)
    (src / "linkdir").symlink_to(tmp_path)
    (src / ".git").mkdir()
    (src / ".git" / "config").write_text("[core]\n")
    (src / "ref" / "hub.yaml").write_text("nested: a regular skill file\n")             # only the TOP-LEVEL sidecar is excluded
    rec = hub.skilleval.evaluate("alpha", "static", "t")
    seen = r.seen[0]
    assert set(seen["files"]) == {"SKILL.md", "ref/notes.md", "ref/hub.yaml", "scripts/run.sh"}
    assert seen["symlinks"] == [] and "TOP SECRET" not in json.dumps({k: v.decode() for k, v in seen["files"].items()})
    assert seen["modes"]["scripts/run.sh"] == 0o700 and seen["modes"]["SKILL.md"] == 0o600
    expected = {rel: (src / rel).read_bytes() for rel in seen["files"]}
    assert seen["files"] == expected
    assert rec["content_hash"] == content_hash(expected) and rec["files"] == 4


def test_content_hash_is_framed_and_order_independent() -> None:
    assert content_hash({"a": b"bc"}) != content_hash({"ab": b"c"})
    assert content_hash({"x": b"1", "y": b"2"}) == content_hash({"y": b"2", "x": b"1"})
    h = hashlib.sha256()
    for rel, data in (("a", b"1"),):
        h.update(len(rel).to_bytes(8, "big") + rel.encode() + len(data).to_bytes(8, "big") + data)
    assert content_hash({"a": b"1"}) == h.hexdigest()


def test_copy_caps_refuse_too_many_files_or_bytes(hub: Hub) -> None:
    use(hub)
    skill(hub, files={f"r/{i}.md": "x" for i in range(5)})
    hub.skilleval.max_files = 3
    with pytest.raises(Unprocessable) as e:
        hub.skilleval.evaluate("alpha", "static", "t")
    assert e.value.code == "skill_too_large"
    hub.skilleval.max_files, hub.skilleval.max_bytes = 200, 20
    with pytest.raises(Unprocessable) as e:
        hub.skilleval.evaluate("alpha", "static", "t")
    assert e.value.code == "skill_too_large" and hub.skilleval.records("alpha") == []


def test_skill_without_skill_md_and_unknown_skill_are_refused(hub: Hub) -> None:
    r = use(hub)
    d = skill(hub)
    (d / "SKILL.md").unlink()
    with pytest.raises(Unprocessable) as e:
        hub.skilleval.evaluate("alpha", "static", "t")
    assert e.value.code == "skill_invalid"
    with pytest.raises(NotFound):
        hub.skilleval.evaluate("nope", "static", "t")
    with pytest.raises(Unprocessable) as e2:
        hub.skilleval.evaluate("alpha", "tier3", "t")
    assert e2.value.code == "bad_tiers" and r.calls == []


def test_symlinked_skill_dir_is_not_a_skill(hub: Hub, tmp_path: Path) -> None:
    use(hub)
    other = tmp_path / "elsewhere"
    other.mkdir()
    (other / "SKILL.md").write_text("---\nname: evil\ndescription: x\n---\n")
    (hub.cfg.content_dir / "skills" / "evil").symlink_to(other)
    with pytest.raises(NotFound):
        hub.skilleval.evaluate("evil", "static", "t")


# ---- verdicts ---------------------------------------------------------------------------------------------------------------
@pytest.mark.parametrize(("rc", "status"), [(1, "passed"), (0, "failed"), (1, "incomplete"), (0, "passed")])
def test_verdict_comes_from_the_json_not_the_exit_code(hub: Hub, rc: int, status: str) -> None:
    use(hub, FakeEvalRunner(tier1=Canned(rc=rc, report=report("tier1_incomplete.json", overall_status=status))))
    skill(hub)
    assert hub.skilleval.evaluate("alpha", "static", "t")["verdict"] == status


def test_tier2_failure_downgrades_a_passing_verdict(hub: Hub) -> None:
    passed = Canned(rc=0, report=report("tier1_incomplete.json", overall_status="passed"))
    use(hub, FakeEvalRunner(tier1=passed, tier2=Canned(rc=1, report=report("tier2_passed.json", overall_status="failed"))))
    skill(hub)
    rec = hub.skilleval.evaluate("alpha", "tier1+2", "t")
    assert rec["verdict"] == "failed" and rec["tier1"]["status"] == "passed" and rec["tier2"]["status"] == "failed"
    use(hub, FakeEvalRunner(tier1=passed, tier2=Canned(rc=2, report=None, stderr="no embeddings model")))
    rec = hub.skilleval.evaluate("alpha", "tier1+2", "t")
    assert rec["verdict"] == "incomplete" and rec["tier2"]["status"] == "error" and "no embeddings" in rec["tier2"]["error"]
    use(hub, FakeEvalRunner(tier1=passed))
    assert hub.skilleval.evaluate("alpha", "tier1+2", "t")["verdict"] == "passed"
    assert combine_verdicts("failed", "passed") == "failed" and combine_verdicts("incomplete", "failed") == "failed"
    assert combine_verdicts("error", "passed") == "error" and combine_verdicts("passed", "incomplete") == "incomplete"


def test_no_report_is_an_error_run_and_stderr_is_redacted(hub: Hub) -> None:
    leak = "sk-" + "A1b2C3d4E5f6G7h8I9j0"
    stderr = f"\x1b[31mTraceback\x1b[0m in @COPY@/SKILL.md\nprovider said: Authorization: Bearer {leak}\napi_key={leak}\n"
    use(hub, FakeEvalRunner(tier1=Canned(rc=2, report=None, markdown=None, stderr=stderr)))
    skill(hub)
    rec = hub.skilleval.evaluate("alpha", "static", "t")
    assert rec["verdict"] == "error" and rec["tier1"] is None and rec["error"].startswith("exit 2, no report: Traceback")
    stored = (hub.cfg.data_dir / "skilleval" / "alpha" / rec["run_id"] / "run.json").read_text()
    for text in (rec["error"], stored):
        assert leak not in text and "[redacted]" in text and "\x1b[" not in text
        assert "<skill-copy>/SKILL.md" in text and str(hub.cfg.data_dir) not in text


def test_timeout_is_an_error_run(hub: Hub) -> None:
    use(hub, FakeEvalRunner(tier1=Canned(timed_out=True, report=None, markdown=None)))
    skill(hub)
    rec = hub.skilleval.evaluate("alpha", "static", "t")
    assert rec["verdict"] == "error" and "timed out after 540 s" in rec["error"]


def test_summary_fields_and_top_findings_from_a_real_report() -> None:
    d = report("tier1_incomplete.json")
    s = summarize_tier1(d)
    assert s == {"status": "incomplete", "quality_score": 83.0, "grade": "B", "judge_status": "succeeded", "judge_score": 83.6,
                 "errors": 2, "warnings": 23, "severity": {"critical": 0, "high": 1, "medium": 6, "low": 15},
                 "incomplete": ["skillspector"]}
    d["rubric_eval"]["execution_status"] = "skipped"
    assert summarize_tier1(d)["judge_score"] is None                                   # only a judge that ran counts
    rows, total = top_findings(report("tier1_incomplete.json"))
    assert total == 5 and rows[0] == {"severity": "high", "validator": "Schema & Repository Governance",
                                      "message": "Author not specified in metadata"}
    assert all(x["severity"] in ("high", "medium") for x in rows)                      # low findings are left out
    assert summarize_tier1({})["status"] is None and top_findings({"results": "junk"}) == ([], 0)


# ---- staleness --------------------------------------------------------------------------------------------------------------
def test_staleness_flips_when_skill_md_changes_and_back(hub: Hub) -> None:
    use(hub)
    d = skill(hub)
    assert hub.skilleval.detail("alpha")["state"] == "none"
    hub.skilleval.evaluate("alpha", "static", "t")
    states = lambda: {e["skill"]: e["state"] for e in hub.skilleval.list_latest()}  # noqa: E731
    assert states()["alpha"] == "current" and hub.skilleval.detail("alpha")["state"] == "current"
    original = (d / "SKILL.md").read_bytes()
    (d / "SKILL.md").write_bytes(original + b"\nOne more line.\n")
    assert states()["alpha"] == "stale"
    det = hub.skilleval.detail("alpha")
    assert det["state"] == "stale" and det["history"][0]["state"] == "stale"
    (d / "SKILL.md").write_bytes(original)                                             # same content again: current again
    assert states()["alpha"] == "current"
    (d / "ref" / "new.md").write_text("an extra file is part of the skill")
    assert states()["alpha"] == "stale"


def test_editing_the_hub_yaml_sidecar_does_not_make_an_evaluation_stale(hub: Hub) -> None:
    use(hub)
    skill(hub)
    hub.skilleval.evaluate("alpha", "static", "t")
    hub.work.put("skill", "alpha", {**{k: v for k, v in hub.item_detail("skill", "alpha")["meta"].items()},
                                    "body": "Do alpha.\n", "files": {"ref/notes.md": "notes"}, "tags": ["other", "tags"]})
    assert "other" in (hub.cfg.content_dir / "skills" / "alpha" / "hub.yaml").read_text()
    assert hub.skilleval.detail("alpha")["state"] == "current"


# ---- path safety ------------------------------------------------------------------------------------------------------------
@pytest.mark.parametrize("bad", ["../alpha", "..", "a/b", "A", "alpha/../../x", "", ".work", "x" * 65])
def test_bad_skill_ids_are_refused(hub: Hub, bad: str) -> None:
    use(hub)
    for fn in (lambda: hub.skilleval.evaluate(bad, "static", "t"), lambda: hub.skilleval.detail(bad),
               lambda: hub.skilleval.report(bad, "20261006T201500123Z-0a1b2c3d")):
        with pytest.raises((BadRequest, NotFound)):
            fn()


@pytest.mark.parametrize("bad", ["../../hub.db", "..", "run.json", "20261006T201500123Z-0a1b2c3d/../x", "20261006T2015Z-abc",
                                 "20261006T201500123Z-0A1B2C3D", "20261006T201500123Z-0a1b2c3d\n"])
def test_bad_run_ids_are_refused(hub: Hub, bad: str) -> None:
    use(hub)
    skill(hub)
    with pytest.raises(BadRequest):
        hub.skilleval.report("alpha", bad)


def test_traversal_over_http(api: TestClient, hub: Hub) -> None:
    use(hub)
    skill(hub)
    h = admin_h(hub)
    for url in (f"{API}/skills/..%2F..%2Fetc/evaluations", f"{API}/skills/alpha/evaluations/..%2F..%2Fhub.db/report",
                f"{API}/skills/alpha/evaluations/20261006T201500123Z-0a1b2c3d/report", f"{API}/skills/A/evaluations"):
        assert api.get(url, headers=h).status_code in (400, 404), url
    for url in (f"{API}/skills/..%2Fx/evaluate", f"{API}/skills/..%2F..%2Fetc/evaluate", f"{API}/skills/A/evaluate"):
        assert api.post(url, json={}, headers=h).status_code in (400, 404, 405), url     # 405: no POST route matches at all
    assert hub.skilleval.runner.calls == []                                            # type: ignore[attr-defined]


# ---- one evaluation at a time --------------------------------------------------------------------------------------------------
def test_second_evaluation_is_refused_while_one_runs(hub: Hub) -> None:
    gate = threading.Event()
    r = use(hub, FakeEvalRunner(block=gate))
    skill(hub)
    skill(hub, "beta")
    started = hub.skilleval.start("alpha", "static", "t")
    assert started["status"] == "running" and r.started.wait(5)
    second = hub.skilleval.start("beta", "static", "t")                  # F6: queued, no longer a 409
    assert second["status"] == "queued" and hub.skilleval.start("alpha", "static", "t")["status"] == "running"
    with pytest.raises(Conflict) as e:                                   # a synchronous (CLI) evaluation still excludes
        hub.skilleval.evaluate("beta", "static", "t")
    assert e.value.code == "evaluation_running" and e.value.status == 409
    assert hub.skilleval.running()["skill"] == "alpha"                                   # type: ignore[index]
    assert next(e for e in hub.skilleval.list_latest() if e["skill"] == "alpha")["running"] is True
    gate.set()
    hub.skilleval.join(5)
    assert hub.skilleval.running() is None and [x["skill"] for x in map(lambda s: hub.skilleval.records(s)[0], ("alpha", "beta"))] == ["alpha", "beta"]


def test_another_process_holding_the_lock_blocks_and_is_reported(hub: Hub) -> None:
    use(hub)
    skill(hub)
    hub.skilleval.root.mkdir(mode=0o700, exist_ok=True)
    fd = os.open(hub.skilleval.root / ".lock", os.O_RDWR | os.O_CREAT, 0o600)        # a second open file = another holder
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    try:
        with pytest.raises(Conflict):
            hub.skilleval.evaluate("alpha", "static", "t")
        assert hub.skilleval.running() == {"subject": None, "skill": None, "run_id": None, "tiers": None, "started_at": None,
                                          "elsewhere": True}
    finally:
        os.close(fd)
    assert hub.skilleval.running() is None and hub.skilleval.evaluate("alpha", "static", "t")["verdict"] == "incomplete"


def test_concurrent_post_is_queued_not_409_over_http(api: TestClient, hub: Hub) -> None:
    gate = threading.Event()
    r = use(hub, FakeEvalRunner(block=gate))
    skill(hub)
    h = admin_h(hub)
    assert api.post(f"{API}/skills/alpha/evaluate", json={"tiers": "static"}, headers=h).status_code == 202
    assert r.started.wait(5)
    skill(hub, "beta")
    busy = api.post(f"{API}/skills/alpha/evaluate", json={"tiers": "static"}, headers=h)
    assert busy.status_code == 202 and busy.json()["status"] == "running"                  # already running: no second job
    queued = api.post(f"{API}/skills/beta/evaluate", json={"tiers": "static"}, headers=h)
    assert queued.status_code == 202 and queued.json()["status"] == "queued"
    q = api.get(f"{API}/skill-evaluations/queue", headers=h).json()
    assert q["running"]["subject"] == "hub:alpha" and q["queued"] == [{"subject": "hub:beta", "tiers": "static"}]
    assert api.get(f"{API}/skills/alpha/evaluations", headers=h).json()["running"]["skill"] == "alpha"
    gate.set()
    hub.skilleval.join(5)


# ---- environment ------------------------------------------------------------------------------------------------------------
def test_env_passthrough_only_for_the_evaluator(hub: Hub, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    environ = {"PATH": "/usr/bin:/bin", "HOME": "/home/operator", "SKILL_EVAL_PROVIDER": "openai-compatible",
               "SKILL_EVAL_API_KEY": "value-not-checked-here", "SKILLEVAL_TIMEOUT": "30", "OPENAI_API_KEY": "x",
               "HUB_API_KEY": "y", "AWS_SECRET_ACCESS_KEY": "z", "XSKILL_EVAL_NOPE": "n"}
    r = use(hub, environ=environ, home=tmp_path)
    skill(hub)
    hub.skilleval.evaluate("alpha", "static", "t")
    assert tier1_call(r)["env"] == {"PATH": f"/usr/bin:/bin:{tmp_path}/.local/bin", "HOME": "/home/operator", "LC_ALL": "C",
                                    "SKILL_EVAL_PROVIDER": "openai-compatible", "SKILL_EVAL_API_KEY": "value-not-checked-here",
                                    "SKILLEVAL_TIMEOUT": "30"}
    assert hub.skilleval.env_names() == ["SKILLEVAL_TIMEOUT", "SKILL_EVAL_API_KEY", "SKILL_EVAL_PROVIDER"]
    # every OTHER hub subprocess keeps the plain minimal environment, even with SKILL_EVAL_* in the process environment
    monkeypatch.setenv("SKILL_EVAL_API_KEY", "must-not-leak")
    seen: dict[str, Any] = {}

    def fake_run(argv: list[str], **kw: Any) -> subprocess.CompletedProcess[bytes]:
        seen.update(kw["env"])
        return subprocess.CompletedProcess(argv, 0, b"", b"")

    monkeypatch.setattr(fs_mod.subprocess, "run", fake_run)
    fs_mod.RealRunner().run(["git", "--version"], timeout=5)
    assert set(seen) == {"PATH", "HOME", "LC_ALL"}


def test_status_shows_env_names_never_values(api: TestClient, hub: Hub) -> None:
    use(hub, environ={"PATH": "/usr/bin", "SKILL_EVAL_API_KEY": "sk-" + "Z" * 24})
    body = api.get(f"{API}/skill-evaluator", headers=admin_h(hub)).text
    assert "SKILL_EVAL_API_KEY" in body and "ZZZZ" not in body


# ---- API: roles, CSRF, audit, cheap reads ----------------------------------------------------------------------------------
def test_post_evaluate_needs_admin_and_the_csrf_header(api: TestClient, hub: Hub) -> None:
    r = use(hub)
    skill(hub)
    viewer = bearer(str(hub.auth.create_key("v", "viewer")["secret"]))
    resp = api.post(f"{API}/skills/alpha/evaluate", json={"tiers": "static"}, headers=viewer)
    assert resp.status_code == 403 and resp.json()["code"] == "forbidden"
    assert api.post(f"{API}/skills/alpha/evaluate", json={}, headers=CSRF).status_code == 401
    hub.cfg.trust_loopback = True                                     # an ambient-credential browser: the header is required
    resp = api.post(f"{API}/skills/alpha/evaluate", json={})
    assert resp.status_code in (401, 403) and resp.json()["code"] in ("csrf_header_required", "unauthorized")
    hub.cfg.trust_loopback = False
    resp = api.post(f"{API}/skills/alpha/evaluate", json={}, headers={**CSRF, "Origin": "http://evil.example"})
    assert resp.status_code == 403
    assert r.calls == []                                              # nothing started
    ok = api.post(f"{API}/skills/alpha/evaluate", json={}, headers=admin_h(hub))
    assert ok.status_code == 202 and ok.json()["tiers"] == "static" and ok.json()["status"] == "running"
    assert ok.json()["subject"] == "hub:alpha"
    hub.skilleval.join(5)
    assert api.post(f"{API}/skills/alpha/evaluate", json={"tiers": "tier9"}, headers=admin_h(hub)).status_code == 422
    assert api.post(f"{API}/skills/alpha/evaluate", json={"tiers": "static", "x": 1}, headers=admin_h(hub)).status_code == 422


def test_csrf_header_on_loopback_trust_without_bearer(api: TestClient, hub: Hub, monkeypatch: pytest.MonkeyPatch) -> None:
    use(hub)
    skill(hub)
    hub.cfg.trust_loopback = True
    from agent_hub.api import security
    monkeypatch.setattr(security, "LOOPBACK", {*security.LOOPBACK, "testclient"})    # the test peer counts as loopback
    no_header = api.post(f"{API}/skills/alpha/evaluate", json={})
    assert no_header.status_code == 403 and no_header.json()["code"] == "csrf_header_required"
    assert api.post(f"{API}/skills/alpha/evaluate", json={}, headers=CSRF).status_code == 202
    hub.skilleval.join(5)


def test_post_evaluate_is_audited(api: TestClient, hub: Hub) -> None:
    use(hub)
    skill(hub)
    h = admin_h(hub)
    run_id = api.post(f"{API}/skills/alpha/evaluate", json={"tiers": "static"}, headers=h).json()["run_id"]
    hub.skilleval.join(5)
    items = api.get(f"{API}/audit", headers=h).json()["items"]
    req = [i for i in items if i["method"] == "POST" and i["path"] == f"{API}/skills/alpha/evaluate"]
    assert req and req[0]["status"] == 202 and req[0]["params"]["body"] == {"tiers": "static"} and req[0]["role"] == "admin"
    ev = [i for i in items if i["method"] == "EVENT" and i["path"] == "skill.evaluate"]
    assert ev and ev[0]["actor"] == "ev-admin" and ev[0]["params"]["run_id"] == run_id and ev[0]["params"]["verdict"] == "incomplete"


def test_cli_evaluation_is_audited_as_hubctl(hub: Hub, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
    use(hub)
    skill(hub)
    monkeypatch.setenv("HUB_API_KEY", str(hub.auth.create_key("k", "admin")["secret"]))
    cli_run(["evaluate-skill", "alpha"], hub=hub)
    capsys.readouterr()
    ev = [i for i in hub.audit_svc.list_page(50, None)["items"] if i["path"] == "skill.evaluate"]
    assert ev and ev[0]["actor"] == "hubctl"


def test_reads_never_start_a_process(api: TestClient, hub: Hub) -> None:
    r = use(hub)
    skill(hub)
    skill(hub, "beta")
    hub.skilleval.evaluate("alpha", "static", "t")
    n = len(r.calls)
    h = admin_h(hub)
    lst = api.get(f"{API}/skill-evaluations", headers=h).json()
    assert {e["skill"]: e["state"] for e in lst["items"]} == {"alpha": "current", "beta": "none"} and lst["running"] is None
    alpha = next(e for e in lst["items"] if e["skill"] == "alpha")["latest"]
    assert alpha["tier1"]["grade"] == "B" and alpha["verdict"] == "incomplete" and alpha["findings"][0]["severity"] == "high"
    det = api.get(f"{API}/skills/alpha/evaluations", headers=h).json()
    assert det["state"] == "current" and len(det["history"]) == 1 and det["content_hash"] == det["latest"]["content_hash"]
    api.get(f"{API}/skills/alpha/evaluations/{det['latest']['run_id']}/report", headers=h)
    assert len(r.calls) == n                                          # listing, detail and the report spawn nothing
    viewer = bearer(str(hub.auth.create_key("v", "viewer")["secret"]))
    assert api.get(f"{API}/skill-evaluations", headers=viewer).status_code == 200      # reading is not admin-only


def test_report_is_redacted_and_paths_are_scrubbed(api: TestClient, hub: Hub) -> None:
    leak = "ghp_" + "Q" * 30
    md = f"# Report\n\nScanned `@COPY@/SKILL.md`.\n\ntoken: {leak}\n\n<script>alert(1)</script>\n"
    use(hub, FakeEvalRunner(tier1=Canned(rc=1, report=report("tier1_incomplete.json", note="@COPY@"), markdown=md)))
    skill(hub)
    rec = hub.skilleval.evaluate("alpha", "static", "t")
    h = admin_h(hub)
    out = api.get(f"{API}/skills/alpha/evaluations/{rec['run_id']}/report", headers=h).json()
    assert out["truncated"] is False and leak not in out["markdown"] and "[redacted]" in out["markdown"]
    assert "`<skill-copy>/SKILL.md`" in out["markdown"]                # the deleted copy's path is not shown
    run_dir = hub.cfg.data_dir / "skilleval" / "alpha" / rec["run_id"]
    for name in ("static/skillevaluator-tier1.md", "static/skillevaluator-tier1.json"):
        assert ".work" not in (run_dir / name).read_text()                              # also scrubbed on disk
    assert json.loads((run_dir / "static" / "skillevaluator-tier1.json").read_text())["note"] == "<skill-copy>"   # JSON stays valid
    missing = api.get(f"{API}/skills/alpha/evaluations/20000101T000000000Z-00000000/report", headers=h)
    assert missing.status_code == 404


def test_missing_evaluator_is_503_with_the_install_hint(api: TestClient, hub: Hub) -> None:
    use(hub, launchers=())
    skill(hub)
    with pytest.raises(Unavailable) as e:
        hub.skilleval.evaluate("alpha", "static", "t")
    assert e.value.code == "evaluator_missing" and e.value.extra["fix"] == INSTALL_HINT
    resp = api.post(f"{API}/skills/alpha/evaluate", json={}, headers=admin_h(hub))
    assert resp.status_code == 503 and resp.json()["code"] == "evaluator_missing" and resp.json()["fix"] == INSTALL_HINT
    st = api.get(f"{API}/skill-evaluator", headers=admin_h(hub)).json()
    assert st["installed"] is False and st["launcher"] is None and st["install_hint"] == INSTALL_HINT


def test_status_caches_the_version_probe(hub: Hub) -> None:
    r = use(hub)
    st = hub.skilleval.status()
    assert st["installed"] and st["launcher"] == "skillevaluator-local" and st["version"] == "0.5.0" and st["wrapper"] is True
    assert [t["id"] for t in st["tiers"]] == ["static", "tier1", "tier1+2"]
    hub.skilleval.status()
    assert [c["argv"] for c in r.calls] == [[f"{FAKE_BIN}/skillevaluator-local", "--version"]]
    use(hub, r, launchers=("skillevaluator",))                         # the launcher changed: probe again
    assert hub.skilleval.status()["launcher"] == "skillevaluator" and len(r.calls) == 2
    assert hub.skilleval.doctor_info()["version"] == "0.5.0" and len(r.calls) == 2   # doctor never spawns


# ---- storage --------------------------------------------------------------------------------------------------------------
def test_history_keeps_the_newest_n_runs(hub: Hub) -> None:
    use(hub)
    skill(hub)
    hub.skilleval.keep = 3
    ids = [hub.skilleval.evaluate("alpha", "static", "t")["run_id"] for _ in range(5)]
    left = sorted(p.name for p in (hub.cfg.data_dir / "skilleval" / "alpha").iterdir())
    assert left == sorted(ids)[-3:] and [r["run_id"] for r in hub.skilleval.records("alpha")] == sorted(ids)[-3:][::-1]


def test_state_is_private(hub: Hub) -> None:
    use(hub)
    skill(hub)
    rec = hub.skilleval.evaluate("alpha", "tier1+2", "t")
    root = hub.cfg.data_dir / "skilleval"
    run_dir = root / "alpha" / rec["run_id"]
    for d in (root, root / "alpha", run_dir, run_dir / "static"):
        assert stat.S_IMODE(d.stat().st_mode) == 0o700, d
    for f in [*run_dir.iterdir(), *(run_dir / "static").iterdir()]:
        if not f.is_dir():
            assert stat.S_IMODE(f.stat().st_mode) == 0o600, f
    assert stat.S_IMODE((root / ".lock").stat().st_mode) == 0o600
    assert not any((root / ".work").iterdir())


def test_symlinks_left_in_the_output_dir_are_dropped(hub: Hub, tmp_path: Path) -> None:
    target = tmp_path / "target.txt"
    target.write_text("x")
    r = use(hub)
    skill(hub)
    orig = r.run

    def run(argv: list[str], **kw: Any) -> Any:
        res = orig(argv, **kw)
        if "-o" in argv:
            (Path(argv[argv.index("-o") + 1]) / "evil-link").symlink_to(target)
        return res

    r.run = run  # type: ignore[method-assign]
    rec = hub.skilleval.evaluate("alpha", "static", "t")
    assert not (hub.cfg.data_dir / "skilleval" / "alpha" / rec["run_id"] / "evil-link").exists() and target.read_text() == "x"


def _tree(root: Path) -> dict[str, str]:
    out = {}
    for p in sorted(root.rglob("*")):
        rel = p.relative_to(root).as_posix()
        out[rel] = "dir" if p.is_dir() else hashlib.sha256(p.read_bytes()).hexdigest()
    return out


def test_an_evaluation_never_touches_the_content_repo(hub: Hub) -> None:
    r = use(hub)
    skill(hub)
    hub.commit("skill", "t")
    before = _tree(hub.cfg.content_dir)
    r.write_into_cwd = True                                            # the tool writes next to itself and into its copy
    hub.skilleval.evaluate("alpha", "static", "t")
    assert _tree(hub.cfg.content_dir) == before and hub.repo.status() == []
    assert r.seen[0]["files"]["SKILL.md"] != b"tampered by the evaluator"     # it got a copy; the copy is gone
    assert not any((hub.cfg.data_dir / "skilleval" / ".work").iterdir())


def test_evaluations_are_advisory_plan_and_floor_unchanged(hub: Hub, home: Path) -> None:
    floor_before = hub.cfg.policy_file.read_bytes()
    add_client(hub, home)
    skill(hub)
    hub.work.put_profile("fake1", {"enable": {"skills": ["alpha"]}})
    hub.commit("c", "t")
    plan_before = hub.plan(None).clients[0]
    use(hub, FakeEvalRunner(tier1=Canned(rc=1, report=report("tier1_incomplete.json", overall_status="failed"))))
    assert hub.skilleval.evaluate("alpha", "static", "t")["verdict"] == "failed"
    plan_after = hub.plan(None).clients[0]
    assert plan_after.blocked is False and plan_after.digest == plan_before.digest
    assert not any("eval" in d.code for d in plan_after.diagnostics)
    assert hub.cfg.policy_file.read_bytes() == floor_before


# ---- CLI ------------------------------------------------------------------------------------------------------------------
@pytest.mark.parametrize(("status", "code"), [("passed", 0), ("failed", 2), ("incomplete", 2)])
def test_cli_evaluate_skill_exit_codes(hub: Hub, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
                                       status: str, code: int) -> None:
    use(hub, FakeEvalRunner(tier1=Canned(rc=1, report=report("tier1_incomplete.json", overall_status=status))))
    skill(hub)
    monkeypatch.setenv("HUB_API_KEY", str(hub.auth.create_key("k", "admin")["secret"]))
    assert cli_run(["evaluate-skill", "alpha", "--tiers", "static"], hub=hub) == code
    out = capsys.readouterr().out
    assert f"alpha: {status}" in out and "quality 83.0 grade B" in out and "[HIGH] Schema & Repository Governance" in out
    assert "advisory only" in out


def test_cli_evaluate_skill_errors_exit_1(hub: Hub, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
    monkeypatch.setenv("HUB_API_KEY", str(hub.auth.create_key("k", "admin")["secret"]))
    use(hub, FakeEvalRunner(tier1=Canned(rc=2, report=None, markdown=None, stderr="boom")))
    skill(hub)
    assert cli_run(["evaluate-skill", "alpha"], hub=hub) == 1                       # no report
    assert cli_run(["evaluate-skill", "nope"], hub=hub) == 1                        # unknown skill
    use(hub, launchers=())
    assert cli_run(["evaluate-skill", "alpha"], hub=hub) == 1                       # not installed
    err = capsys.readouterr().err
    assert "evaluator_missing" in err and INSTALL_HINT in err
    with pytest.raises(SystemExit):
        cli_mod.build_parser().parse_args(["evaluate-skill", "alpha", "--tiers", "tier3"])


def test_cli_evaluate_skills_prints_a_table(hub: Hub, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
    r = use(hub)
    skill(hub)
    skill(hub, "beta")
    monkeypatch.setenv("HUB_API_KEY", str(hub.auth.create_key("k", "admin")["secret"]))
    assert cli_run(["evaluate-skills", "--tiers", "static"], hub=hub) == 2           # incomplete reports exist
    out = capsys.readouterr().out.splitlines()
    assert out[0].split() == ["skill", "verdict", "quality", "grade", "judge", "errors", "high"]
    assert [ln.split()[:2] for ln in out[1:]] == [["alpha", "incomplete"], ["beta", "incomplete"]]
    assert all("--no-llm" in c["argv"] for c in tool_calls(r))
    assert cli_run(["--json", "evaluate-skills"], hub=hub) == 2
    assert [x["skill"] for x in json.loads(capsys.readouterr().out)["items"]] == ["alpha", "beta"]
    use(hub, launchers=())
    assert cli_run(["evaluate-skills"], hub=hub) == 1


# ---- doctor ---------------------------------------------------------------------------------------------------------------
def test_doctor_reports_the_evaluator_as_info(hub: Hub, tmp_path: Path) -> None:
    from .test_doctor import FakeFS
    use(hub, launchers=())
    f = next(x for x in hub.doctor(None, fs=FakeFS({}), home=tmp_path)["findings"] if x["id"] == "skill-evaluator")
    assert f["severity"] == "info" and "not installed" in f["title"] and f["fix"] == INSTALL_HINT
    assert INSTALL_HINT == 'uv tool install --python 3.13 "skillevaluator[all] @ git+https://github.com/NVIDIA/SkillEvaluator.git"'
    r = use(hub)
    f = next(x for x in hub.doctor(None, fs=FakeFS({}), home=tmp_path)["findings"] if x["id"] == "skill-evaluator")
    assert f["severity"] == "info" and "skillevaluator-local" in f["title"] and r.calls == []       # no subprocess
    use(hub, launchers=("skillevaluator",), environ={"PATH": "/usr/bin", "SKILL_EVAL_PROVIDER": "p"})
    f = next(x for x in hub.doctor(None, fs=FakeFS({}), home=tmp_path)["findings"] if x["id"] == "skill-evaluator")
    assert "SKILL_EVAL_PROVIDER" in f["detail"] and "(skillevaluator)" in f["title"]
    from agent_hub.services.doctor import DoctorContext
    ctx = DoctorContext(cfg=Settings(data_dir=tmp_path / "s"), fs=FakeFS({}), env={}, home=tmp_path, app_root=tmp_path, uid=0)
    assert check_skill_evaluator(ctx) == []                                          # not checked: nothing reported


# ---- the real runner (a real child process, offline: the Python interpreter itself) ------------------------------------------
def test_real_runner_kills_the_group_on_timeout_and_caps_output(tmp_path: Path) -> None:
    pidfile = tmp_path / "grandchild.pid"
    code = ("import subprocess, sys, time\n"
            f"p = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])\n"
            f"open({str(pidfile)!r}, 'w').write(str(p.pid))\n"
            "sys.stdout.write('x' * 300000); sys.stdout.flush()\n"
            "time.sleep(60)\n")
    r = RealEvalRunner(output_cap=1000)
    t0 = time.monotonic()
    res = r.run([sys.executable, "-c", code], cwd=tmp_path, env={"PATH": os.environ.get("PATH", "")}, timeout=1.5)
    assert res.timed_out and time.monotonic() - t0 < 20
    assert len(res.stdout) <= 1000 + len("[...] ") and res.stdout.startswith("[...] ")
    gpid = int(pidfile.read_text())
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            os.kill(gpid, 0)
        except ProcessLookupError:
            break
        with open(f"/proc/{gpid}/stat") as fh:                          # a zombie (killed, not yet reaped by init) counts as gone
            if fh.read().split()[2] == "Z":
                break
        time.sleep(0.05)
    else:
        pytest.fail("the grandchild in the evaluator's process group survived the timeout")


def test_real_runner_passes_cwd_and_only_the_given_env(tmp_path: Path) -> None:
    r = RealEvalRunner()
    code = "import os, json; print(json.dumps({'cwd': os.getcwd(), 'env': sorted(os.environ)}))"
    res = r.run([sys.executable, "-c", code], cwd=tmp_path, env={"PATH": os.environ.get("PATH", ""), "SKILL_EVAL_X": "1"},
                timeout=30)
    out = json.loads(res.stdout)
    assert res.returncode == 0 and out["cwd"] == str(tmp_path)
    assert set(out["env"]) - {"LC_CTYPE"} == {"PATH", "SKILL_EVAL_X"}          # Python may add LC_CTYPE (PEP 538)
    assert r.run([], cwd=tmp_path, env={}, timeout=1).returncode == 127
    assert r.run(["/nonexistent/skillevaluator"], cwd=tmp_path, env={}, timeout=1).returncode == 127


def test_service_defaults_to_the_real_runner_and_shutil_which(tmp_path: Path) -> None:
    s = SkillEvalService(tmp_path / "skilleval", tmp_path / "content", environ={"PATH": str(tmp_path / "bin")}, home=tmp_path)
    assert isinstance(s.runner, RealEvalRunner)
    (tmp_path / "bin").mkdir()
    exe = tmp_path / "bin" / "skillevaluator"
    exe.write_text("#!/bin/sh\necho 'skillevaluator, version 9.9.9'\n")
    exe.chmod(0o755)
    assert s.find_launcher() == ("skillevaluator", str(exe))
    assert s.status()["version"] == "9.9.9"                           # a stand-in script on a temp PATH, not the real tool


def test_compiled_bytecode_is_never_evaluated_and_does_not_change_the_hash(hub: Hub) -> None:
    """A `__pycache__/` copied into a skill used to reach SkillSpector, which cannot read it: partial coverage, a block."""
    r = use(hub)
    src = skill(hub, files={"ref/notes.md": "notes", "scripts/tool.py": "print('hi')\n"})
    clean = hub.skilleval.snapshot("alpha")
    (src / "scripts" / "__pycache__").mkdir()
    (src / "scripts" / "__pycache__" / "tool.cpython-313.pyc").write_bytes(b"\x00\x0d\x0d\x0a bytecode")
    (src / "stray.pyc").write_bytes(b"\x00 bytecode")
    (src / "old.pyo").write_bytes(b"\x00 bytecode")
    rec = hub.skilleval.evaluate("alpha", "static", "t")
    assert set(r.seen[0]["files"]) == {"SKILL.md", "ref/notes.md", "scripts/tool.py"}
    assert rec["content_hash"] == clean.content_hash and rec["files"] == 3
