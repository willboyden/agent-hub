"""Addendum F: observed inventory (store, comparison), evaluation subjects, reuse by content hash, the FIFO queue, the gate, the
import gate (observed import and /import), enum settings, roles and subject path safety. Fakes only: no SkillEvaluator, no docker."""
from __future__ import annotations

import json
import os
import stat
import subprocess
import threading
from pathlib import Path
from typing import Any

import pytest
from fastapi.testclient import TestClient

from agent_hub.adapters.base import Observation, ObservedSkill
from agent_hub.cli import run as cli_run
from agent_hub.domain.errors import BadRequest, NotFound, Unprocessable
from agent_hub.services import fs as fs_mod
from agent_hub.services.hub import Hub
from agent_hub.services.observe import KEY_RE, ObserveService, make_key
from agent_hub.services.skilleval import SAFE_CHECKS, normalize_subject, parse_subject

from .conftest import CSRF, add_client, bearer
from .fakes import Canned, FakeAdapter, FakeEvalRunner, report
from .test_skilleval import admin_h, skill, tool_calls, use

API = "/api/v1"


# ---- helpers -------------------------------------------------------------------------------------------------------------------
class ObsAdapter(FakeAdapter):
    """A fake client whose `observe` returns whatever the test put in `.skills` (no filesystem, no docker)."""
    id = "fakeobs"

    def __init__(self) -> None:
        super().__init__()
        self.skills: list[ObservedSkill] = []
        self.complete = True

    def observe(self, cfg: Any, fs: Any, run: Any) -> Observation:
        return Observation(skills=list(self.skills), source="fake container", notes=["note one"], complete=self.complete)


def md(name: str, body: str = "Do it.\n", desc: str = "A skill") -> bytes:
    return f"---\nname: {name}\ndescription: {desc}\n---\n{body}".encode()


def oskill(path: str, name: str | None = None, origin: str = "agent", body: str = "Do it.\n",
           extra: dict[str, bytes] | None = None) -> ObservedSkill:
    n = name or path.rsplit("/", 1)[-1]
    return ObservedSkill(path=path, name=n, description="A skill", origin=origin,  # type: ignore[arg-type]
                         files={"SKILL.md": md(n, body), **(extra or {})})


@pytest.fixture
def obs(hub: Hub, home: Path) -> ObsAdapter:
    a = ObsAdapter()
    hub.registry.register(a)
    hub.put_client("obs1", {"adapter": "fakeobs", "display_name": "Obs", "roots": {"home": str(home)}}, create=True)
    return a


def results(**validators: tuple[str, list[tuple[str, str]]]) -> list[dict[str, Any]]:
    """tier1 `results`: validator -> (status, [(severity, message)]). The keyword name uses `_` for spaces."""
    out = []
    for name, (status, findings) in validators.items():
        out.append({"validator": name if name.isupper() else name.replace("_", " "), "passed": status == "passed", "status": status, "incomplete_scans": [],
                    "findings": [{"category": "X", "severity": sev, "check": None, "message": msg} for sev, msg in findings]})
    return out


def canned(**validators: tuple[str, list[tuple[str, str]]]) -> Canned:
    base = {"Security_Scan": ("passed", []), "PII_Scan": ("passed", []), "Code_Risk_Analysis": ("passed", []),
            "Secrets_Detection": ("passed", []), "Code_Integrity_&_Hygiene": ("passed", []),
            "Unicode_Smuggling_Detection": ("passed", []),
            "Schema_&_Repository_Governance": ("failed", [("high", "Author not specified in metadata")])}
    base.update(validators)
    return Canned(rc=1, report=report("tier1_incomplete.json", overall_status="failed", results=results(**base)))


def evaluated(hub: Hub, runner: FakeEvalRunner, name: str = "alpha", tiers: str = "static") -> str:
    use(hub, runner)
    skill(hub, name)
    rec = hub.skilleval.evaluate(name, tiers, "t")
    assert isinstance(rec["content_hash"], str)
    return str(rec["content_hash"])


def names_in_order(r: FakeEvalRunner) -> list[str]:
    return [Path(c["argv"][2]).name for c in tool_calls(r) if "--no-llm" in c["argv"]]


# ---- subjects ------------------------------------------------------------------------------------------------------------------
@pytest.mark.parametrize("bad", ["hub:../x", "a:b/c", "hub:A", "hub:", ":x", "hub:x:y", "..:x", "hub:x\n", "a:b c", "hub:" + "x" * 65,
                                 "client:" + "k" * 82])
def test_subject_fullmatch_rejects_traversal_and_junk(bad: str) -> None:
    with pytest.raises(BadRequest):
        parse_subject(bad)
    with pytest.raises(BadRequest):
        normalize_subject(bad)


def test_subjects_parse_and_bare_names_mean_hub() -> None:
    assert parse_subject("hub:alpha") == ("hub", "alpha") and parse_subject("obs1:foo-0a1b2c3d") == ("obs1", "foo-0a1b2c3d")
    assert normalize_subject("alpha") == "hub:alpha" and normalize_subject("obs1:foo-0a1b2c3d") == "obs1:foo-0a1b2c3d"


def test_subject_routes_refuse_traversal(api: TestClient, hub: Hub) -> None:
    r = use(hub)
    skill(hub)
    h = admin_h(hub)
    for bad in ("hub:..%2Fx", "a:b%2Fc", "hub:A", "..%2F..%2Fetc"):
        assert api.get(f"{API}/skill-evaluations/{bad}", headers=h).status_code in (400, 404), bad
        assert api.get(f"{API}/skill-evaluations/{bad}/runs/20261006T201500123Z-0a1b2c3d/report", headers=h).status_code in (400, 404)
    resp = api.post(f"{API}/skill-evaluations/run", json={"subjects": ["hub:../x"]}, headers=h)
    assert resp.status_code == 400 and tool_calls(r) == []


# ---- the observation store -----------------------------------------------------------------------------------------------------
def test_key_format_and_stability() -> None:
    k = make_key("devops/My Skill!")
    assert KEY_RE.fullmatch(k) and k.startswith("my-skill-") and k == make_key("devops/My Skill!") and k != make_key("other/My Skill!")
    assert KEY_RE.fullmatch(make_key("!!!")) and KEY_RE.fullmatch(make_key("x" * 400))


def test_observe_stores_index_and_private_files(hub: Hub, obs: ObsAdapter) -> None:
    obs.skills = [oskill("devops/foo", extra={"ref/a.md": b"A"}), oskill("bar", origin="shipped")]
    out = hub.observe("obs1")
    assert out["total"] == 2 and out["by_origin"] == {"agent": 1, "shipped": 1} and out["complete"] is True
    assert out["by_hub_status"] == {"in_hub_same": 0, "in_hub_differs": 0, "not_in_hub": 2} and out["notes"] == ["note one"]
    root = hub.cfg.data_dir / "observed" / "obs1"
    idx = json.loads((root / "index.json").read_text())
    assert idx["client"] == "obs1" and idx["source"] == "fake container" and len(idx["skills"]) == 2
    foo = next(s for s in idx["skills"] if s["path"] == "devops/foo")
    assert KEY_RE.fullmatch(foo["key"]) and foo["files"] == 2 and foo["bytes"] == len(md("foo")) + 1
    assert (root / "skills" / foo["key"] / "ref" / "a.md").read_bytes() == b"A"
    for p in [root, root / "skills", root / "skills" / foo["key"] / "ref"]:
        assert stat.S_IMODE(p.stat().st_mode) == 0o700, p
    for p in [root / "index.json", root / "skills" / foo["key"] / "SKILL.md", root / "skills" / foo["key"] / "ref" / "a.md"]:
        assert stat.S_IMODE(p.stat().st_mode) == 0o600, p
    assert hub.skilleval.current_hash(f"obs1:{foo['key']}") == foo["content_hash"]


def test_a_new_observation_replaces_the_old_atomically(hub: Hub, obs: ObsAdapter, monkeypatch: pytest.MonkeyPatch) -> None:
    obs.skills = [oskill("one"), oskill("two")]
    hub.observe("obs1")
    root = hub.cfg.data_dir / "observed"
    obs.skills = [oskill("three")]
    hub.observe("obs1")
    idx = json.loads((root / "obs1" / "index.json").read_text())
    assert [s["path"] for s in idx["skills"]] == ["three"]
    assert sorted(p.name for p in (root / "obs1" / "skills").iterdir()) == [s["key"] for s in idx["skills"]]
    assert sorted(p.name for p in root.iterdir()) == ["obs1"]                             # no temp or .old directories left
    # a failure while building leaves the previous observation untouched and leaves no temp directory
    obs.skills = [oskill("four")]
    real_open = os.open
    calls = {"n": 0}

    def flaky(path: Any, flags: int, mode: int = 0o777, **kw: Any) -> int:
        if str(path).endswith("SKILL.md") and "/.tmp-" in str(path):
            calls["n"] += 1
            raise OSError("disk full")
        return real_open(path, flags, mode, **kw)

    monkeypatch.setattr(os, "open", flaky)
    with pytest.raises(Exception, match="could not store"):
        hub.observe("obs1")
    monkeypatch.undo()
    assert calls["n"] == 1 and [s["path"] for s in json.loads((root / "obs1" / "index.json").read_text())["skills"]] == ["three"]
    assert sorted(p.name for p in root.iterdir()) == ["obs1"]


def test_caps_unsafe_paths_and_secrets(hub: Hub, obs: ObsAdapter) -> None:
    svc = ObserveService(hub.cfg.data_dir / "observed-caps", hub._hub_view, max_skills=2, max_bytes=100)
    leak = "sk-" + "A1b2C3d4E5f6G7h8I9j0"
    sk = [oskill("a", body="x" * 10), oskill("../escape"), oskill("b", extra={"../../evil": b"x", "ok.md": b"y"}),
          ObservedSkill(path="c", name="c", description=f"key {leak}", files={"SKILL.md": b"z" * 200}),
          oskill("d"), oskill("e")]
    idx = svc.store("obs1", Observation(skills=sk, notes=[f"token: {leak}"]))
    assert [s["path"] for s in idx["skills"]] == ["a", "b"]                               # unsafe path skipped, cap of 2 stops the rest
    assert idx["complete"] is False and any("unsafe" in n for n in idx["notes"]) and any("2 skills" in n for n in idx["notes"])
    assert leak not in json.dumps(idx)
    b = next(s for s in idx["skills"] if s["path"] == "b")
    assert b["files"] == 2                                                                 # ../../evil dropped, ok.md kept
    svc2 = ObserveService(hub.cfg.data_dir / "observed-caps2", hub._hub_view, max_bytes=100)
    big = svc2.store("obs1", Observation(skills=[oskill("small"), ObservedSkill(path="big", name="big", files={"SKILL.md": b"z" * 500})]))
    assert [s["path"] for s in big["skills"]] == ["small"] and big["complete"] is False
    with pytest.raises(BadRequest):
        svc.store("../x", Observation())
    assert svc.entry("obs1", "../x") is None and svc.index("A") is None


def test_incomplete_adapter_flag_is_kept(hub: Hub, obs: ObsAdapter) -> None:
    obs.skills, obs.complete = [oskill("one")], False
    assert hub.observe("obs1")["complete"] is False
    assert hub.observed_view("obs1")["complete"] is False


def test_default_observe_wraps_discover(hub: Hub, home: Path) -> None:
    add_client(hub, home)
    (home / "skills" / "found").mkdir(parents=True)
    (home / "skills" / "found" / "SKILL.md").write_bytes(md("found"))
    out = hub.observe("fake1")
    assert out["total"] == 1 and out["by_origin"] == {"client": 1}
    assert hub.observed_view("fake1")["items"][0]["path"] == "found"


# ---- comparison with the hub -----------------------------------------------------------------------------------------------------
def test_hub_status_same_differs_not_in_hub_and_enabled(hub: Hub, obs: ObsAdapter) -> None:
    skill(hub, "alpha")
    skill(hub, "beta")
    same_files = hub.skilleval.snapshot("hub:alpha").files
    obs.skills = [ObservedSkill(path="x/alpha", name="alpha", origin="agent", files=same_files),
                  oskill("beta", body="Different body.\n"), oskill("gamma")]
    hub.work.put_profile("obs1", {"enable": {"skills": ["alpha"]}})
    out = hub.observe("obs1")
    assert out["by_hub_status"] == {"in_hub_same": 1, "in_hub_differs": 1, "not_in_hub": 1}
    items = {i["name"]: i for i in hub.observed_view("obs1")["items"]}
    assert items["alpha"]["hub_status"] == "in_hub_same" and items["alpha"]["hub_name"] == "alpha"
    assert items["alpha"]["enabled_for_client"] is True and items["alpha"]["content_hash"] == hub.skilleval.current_hash("hub:alpha")
    assert items["beta"]["hub_status"] == "in_hub_differs" and items["beta"]["hub_name"] == "beta"
    assert items["beta"]["enabled_for_client"] is False
    assert items["gamma"]["hub_status"] == "not_in_hub" and items["gamma"]["hub_name"] is None and items["gamma"]["evaluation"] is None
    # computed on read: editing the hub skill changes the answer without a new observation
    hub.work.put("skill", "gamma", {"description": "now in hub", "body": "Do it.\n", "files": {}})
    assert {i["name"]: i["hub_status"] for i in hub.observed_view("obs1")["items"]}["gamma"] in ("in_hub_differs", "in_hub_same")
    assert hub.observed_view("obs1", hub_status="in_hub_same")["items"][0]["name"] == "alpha"
    assert [i["name"] for i in hub.observed_view("obs1", origin="agent", q="ALP")["items"]] == ["alpha"]
    with pytest.raises(BadRequest):
        hub.observed_view("obs1", origin="nope")


def test_observed_api_clients_and_drift_carry_the_summary(api: TestClient, hub: Hub, obs: ObsAdapter) -> None:
    h = admin_h(hub)
    assert api.get(f"{API}/clients/obs1/observed", headers=h).json()["code"] == "not_observed"
    assert api.get(f"{API}/clients/obs1", headers=h).json()["observed_summary"] is None
    obs.skills = [oskill("one"), oskill("two", origin="shipped")]
    posted = api.post(f"{API}/clients/obs1/observe", headers=h).json()
    assert posted["total"] == 2 and posted["client"] == "obs1"
    body = api.get(f"{API}/clients/obs1/observed?origin=shipped", headers=h).json()
    assert body["summary"]["total"] == 2 and [i["name"] for i in body["items"]] == ["two"] and body["next_cursor"] is None
    assert body["observed_at"] and body["source"] == "fake container"
    summ = api.get(f"{API}/clients/obs1", headers=h).json()["observed_summary"]
    assert summ["total"] == 2 and summ["not_in_hub"] == 2 and summ["in_hub_differs"] == 0 and summ["by_origin"] == {"agent": 1, "shipped": 1}
    assert api.get(f"{API}/clients", headers=h).json()["items"][0]["observed_summary"] is not None
    hub.work.put_profile("obs1", {})
    hub.commit("c", "t")
    assert api.get(f"{API}/clients/obs1/drift", headers=h).json()["observed"]["total"] == 2
    assert api.get(f"{API}/clients/nope/observed", headers=h).status_code == 404
    assert api.get(f"{API}/clients/obs1/observed?hub_status=bad", headers=h).status_code == 400


# ---- reuse by content hash -------------------------------------------------------------------------------------------------------
def test_identical_content_is_reused_without_a_process_and_force_reruns(hub: Hub, obs: ObsAdapter) -> None:
    r = use(hub)
    skill(hub, "alpha")
    obs.skills = [ObservedSkill(path="alpha", name="alpha", origin="agent", files=hub.skilleval.snapshot("hub:alpha").files)]
    hub.observe("obs1")
    key = hub.observed_view("obs1")["items"][0]["key"]
    first = hub.skilleval.evaluate("alpha", "tier1", "t")
    n = len(tool_calls(r))
    assert first["reused_from"] is None and first["evaluator_version"] == "0.5.0" and first["subject"] == "hub:alpha"
    out = hub.skilleval.enqueue([f"obs1:{key}", "hub:alpha"], "static", False, "t")
    assert out["queued"] == [f"obs1:{key}", "hub:alpha"]
    hub.skilleval.join(10)
    assert len(tool_calls(r)) == n                                                        # both reused: no process started
    rec = hub.skilleval.records(f"obs1:{key}")[0]
    assert rec["reused_from"] == {"subject": "hub:alpha", "run_id": first["run_id"]} and rec["tiers"] == "tier1"
    assert rec["subject"] == f"obs1:{key}" and rec["source"] == "obs1" and rec["path"] == "alpha" and rec["origin"] == "agent"
    assert rec["verdict"] == first["verdict"] and rec["validators"] == first["validators"]
    assert hub.skilleval.records("hub:alpha")[0]["reused_from"]["run_id"] == first["run_id"]
    rep = hub.skilleval.report(f"obs1:{key}", rec["run_id"])
    assert rep["markdown"] and rep["subject"] == f"obs1:{key}"                            # a reused run shows the original report
    runs = hub.cfg.data_dir / "skilleval" / "@observed" / "obs1" / key
    assert [p.name for p in runs.iterdir()] == [rec["run_id"]]
    hub.skilleval.enqueue([f"obs1:{key}"], "static", True, "t")                           # force: a real run
    hub.skilleval.join(10)
    assert len(tool_calls(r)) == n + 1 and hub.skilleval.records(f"obs1:{key}")[0]["reused_from"] is None


def test_higher_tiers_or_another_version_or_a_changed_skill_are_not_reused(hub: Hub) -> None:
    r = use(hub)
    d = skill(hub)
    hub.skilleval.evaluate("alpha", "static", "t")
    n = len(tool_calls(r))
    hub.skilleval.evaluate("alpha", "static", "t", force=False)
    assert len(tool_calls(r)) == n                                                        # same tiers, same version: reused
    hub.skilleval.evaluate("alpha", "tier1", "t", force=False)
    assert len(tool_calls(r)) == n + 2                                                    # tiers requested are higher: real run
    hub.skilleval._version = (0.0, None, None)
    r.version_out = "skillevaluator, version 0.6.0"
    hub.skilleval.evaluate("alpha", "static", "t", force=False)
    assert len(tool_calls(r)) == n + 3                                                    # another evaluator version: real run
    (d / "SKILL.md").write_bytes((d / "SKILL.md").read_bytes() + b"more\n")
    hub.skilleval.evaluate("alpha", "static", "t", force=False)
    assert len(tool_calls(r)) == n + 4                                                    # other content: real run


def test_error_runs_are_never_reused(hub: Hub) -> None:
    r = use(hub, FakeEvalRunner(tier1=Canned(rc=2, report=None, markdown=None)))
    skill(hub)
    assert hub.skilleval.evaluate("alpha", "static", "t")["verdict"] == "error"
    assert not (hub.cfg.data_dir / "skilleval" / "@by-hash").exists()
    hub.skilleval.evaluate("alpha", "static", "t", force=False)
    assert len(tool_calls(r)) == 2


# ---- the queue ---------------------------------------------------------------------------------------------------------------------
def test_queue_is_fifo_dedupes_and_cancels(hub: Hub) -> None:
    gate = threading.Event()
    r = use(hub, FakeEvalRunner(block=gate))
    for n in ("a1", "b1", "c1", "d1"):
        skill(hub, n)
    out = hub.skilleval.enqueue(["hub:a1", "hub:b1", "hub:c1", "hub:b1"], "static", False, "t")
    assert out["queued"] == ["hub:a1", "hub:b1", "hub:c1"] and out["skipped"] == [{"subject": "hub:b1", "reason": "queued"}]
    assert r.started.wait(5)
    again = hub.skilleval.enqueue(["hub:a1", "hub:c1", "hub:d1", "hub:nope"], "static", False, "t")
    assert again["queued"] == ["hub:d1"]
    assert {(s["subject"], s["reason"]) for s in again["skipped"]} == {("hub:a1", "running"), ("hub:c1", "queued"),
                                                                       ("hub:nope", "not_found")}
    q = hub.skilleval.queue()
    assert q["running"]["subject"] == "hub:a1" and [x["subject"] for x in q["queued"]] == ["hub:b1", "hub:c1", "hub:d1"]
    assert q["queued_total"] == 3 and q["finished_since_start"] == 0
    cancelled = hub.skilleval.cancel()
    assert cancelled == {"cancelled_queued": 3, "killed_running": True} and r.cancelled == 1
    gate.set()
    hub.skilleval.join(10)
    assert names_in_order(r) == ["a1"]
    q = hub.skilleval.queue()
    assert q["running"] is None and q["queued"] == [] and q["errors_since_start"] == 1                  # the killed one: "cancelled"
    assert hub.skilleval.records("hub:a1")[0]["error"] == "cancelled"
    # order of a fresh batch
    gate2 = threading.Event()
    gate2.set()
    r2 = use(hub, FakeEvalRunner(block=gate2))
    hub.skilleval.enqueue(["hub:d1", "hub:b1", "hub:c1"], "static", False, "t")
    hub.skilleval.join(10)
    assert names_in_order(r2) == ["d1", "b1", "c1"] and hub.skilleval.queue()["finished_since_start"] == 4


def test_enqueue_validation_and_missing_evaluator(hub: Hub) -> None:
    from agent_hub.domain.errors import Unavailable
    use(hub, launchers=())
    skill(hub)
    with pytest.raises(Unavailable):
        hub.skilleval.enqueue(["hub:alpha"], "static", False, "t")
    use(hub)
    with pytest.raises(Unprocessable):
        hub.skilleval.enqueue(["hub:alpha"], "tier9", False, "t")
    with pytest.raises(BadRequest):
        hub.skilleval.enqueue(["hub:alpha", "hub:../x"], "static", False, "t")
    assert hub.skilleval.queue()["queued_total"] == 0


def test_interrupted_runs_are_recorded_as_errors(hub: Hub) -> None:
    use(hub)
    skill(hub)
    ghost = hub.cfg.data_dir / "skilleval" / "alpha" / "20260101T000000000Z-0a0a0a0a"
    ghost.mkdir(parents=True, mode=0o700)
    hub.skilleval.enqueue(["hub:alpha"], "static", True, "t")
    hub.skilleval.join(10)
    rec = next(x for x in hub.skilleval.records("hub:alpha") if x["run_id"] == ghost.name)
    assert rec["verdict"] == "error" and rec["error"] == "interrupted"


# ---- the gate ------------------------------------------------------------------------------------------------------------------------
def test_gate_missing_blocked_warn_ok(hub: Hub) -> None:
    assert hub.skilleval.gate("0" * 64)["status"] == "missing" and hub.skilleval.gate("junk")["status"] == "missing"
    # ok: the publication validator has a HIGH finding (Author not specified) and does not matter
    h = evaluated(hub, FakeEvalRunner(tier1=canned()), "okskill")
    g = hub.skilleval.gate(h)
    assert g["status"] == "ok" and g["reasons"] == [] and g["verdict"] == "failed" and g["tiers"] == "static"
    # warn: medium security findings ALONE
    h = evaluated(hub, FakeEvalRunner(tier1=canned(PII_Scan=("failed", [("medium", "email address")]))), "medskill")
    g = hub.skilleval.gate(h)
    assert g["status"] == "warn" and g["reasons"] == ["PII Scan: 1 medium finding(s)"]
    # M1 fail closed: a security scan that did not finish (incomplete / error / skipped / anything else) BLOCKS, naming it
    for st in ("incomplete", "error", "skipped", "weird"):
        h = evaluated(hub, FakeEvalRunner(tier1=canned(Security_Scan=(st, []))), "inc" + st)
        g = hub.skilleval.gate(h)
        assert g["status"] == "blocked" and g["reasons"] == [f"Security Scan: status {st} (the scan did not finish)"], st
    # blocked: a high finding of a security validator, with reasons "validator: message"
    h = evaluated(hub, FakeEvalRunner(tier1=canned(Code_Risk_Analysis=("failed", [("high", "eval of a remote string")]))), "badskill")
    g = hub.skilleval.gate(h)
    assert g["status"] == "blocked" and g["reasons"] == ["Code Risk Analysis: eval of a remote string"] and g["run_id"]
    h = evaluated(hub, FakeEvalRunner(tier1=canned(Secrets_Detection=("failed", [("critical", "private key")]))), "keyskill")
    assert hub.skilleval.gate(h)["status"] == "blocked"


def test_gate_requires_the_expected_security_validators_to_be_present(hub: Hub) -> None:
    for gone in ("Security Scan", "PII Scan", "Code Risk Analysis", "Secrets Detection", "Code Integrity & Hygiene",
                 "Unicode Smuggling Detection"):
        c = canned()
        assert c.report is not None
        c.report["results"] = [x for x in c.report["results"] if x["validator"] != gone]
        h = evaluated(hub, FakeEvalRunner(tier1=c), "no" + gone.split()[0].lower())
        g = hub.skilleval.gate(h)
        assert g["status"] == "blocked" and g["reasons"] == [f"{gone}: validator missing from the report"], gone
    empty = canned()
    assert empty.report is not None
    empty.report["results"] = []
    h = evaluated(hub, FakeEvalRunner(tier1=empty), "emptyreport")
    assert hub.skilleval.gate(h)["status"] == "missing"                                     # no validators at all: not evaluated


def test_unknown_validators_count_as_security_and_the_judge_never_gates(hub: Hub) -> None:
    h = evaluated(hub, FakeEvalRunner(tier1=canned(Brand_New_Scanner=("failed", [("high", "something new")]))), "unk")
    g = hub.skilleval.gate(h)
    assert g["status"] == "blocked" and g["reasons"] == ["Brand New Scanner: something new"]
    for name in ("QUALITY", "SCRIPT_LINT", "RUBRIC_EVAL", "License_Compliance", "Semantic_Version_Validation"):
        h = evaluated(hub, FakeEvalRunner(tier1=canned(**{name: ("failed", [("critical", "nope")])})), "n" + name.lower().replace("_", "-"))
        assert hub.skilleval.gate(h)["status"] == "ok", name


def test_llm_pass_can_only_add_to_the_gate_never_clear_it(hub: Hub) -> None:
    clean = canned()
    judge_noise = canned(RUBRIC_EVAL=("failed", [("critical", "judge says no")]), Security_Scan=("incomplete", [("medium", "meh")]))
    h = evaluated(hub, FakeEvalRunner(tier1=clean, llm=judge_noise), "judged", "tier1")
    assert hub.skilleval.gate(h)["status"] == "ok"                                           # judge + LLM medium/incomplete: ignored
    blocking_llm = canned(Code_Risk_Analysis=("failed", [("high", "llm found exec"), ("medium", "x")]))
    h = evaluated(hub, FakeEvalRunner(tier1=clean, llm=blocking_llm), "llmblock", "tier1")
    g = hub.skilleval.gate(h)
    assert g["status"] == "blocked" and g["reasons"] == ["Code Risk Analysis: llm found exec"] and g["tiers"] == "tier1"
    static_high = canned(Code_Risk_Analysis=("failed", [("high", "static hit")]))
    downgraded = canned(Code_Risk_Analysis=("passed", []))                                    # the LLM verifier "cleared" it
    h = evaluated(hub, FakeEvalRunner(tier1=static_high, llm=downgraded), "cleared", "tier1")
    g = hub.skilleval.gate(h)
    assert g["status"] == "blocked" and g["reasons"] == ["Code Risk Analysis: static hit"]
    rec = hub.skilleval.records("hub:cleared")[0]
    assert rec["validators"] and rec["validators_llm"] and rec["static"]["status"] == "failed"
    assert {"validator", "status", "severity"} <= set(rec["validators"][0])


def test_gate_min_tiers_and_error_runs(hub: Hub) -> None:
    h = evaluated(hub, FakeEvalRunner(tier1=canned()), "gm")
    assert hub.skilleval.gate(h)["status"] == "ok"
    hub.update_settings({"eval_gate_min_tiers": "tier1"})
    assert hub.skilleval.gate(h)["status"] == "missing"                                      # a static run is below tier1
    use(hub, FakeEvalRunner(tier1=canned()))
    hub.skilleval.evaluate("gm", "tier1", "t")
    assert hub.skilleval.gate(h)["status"] == "ok" and hub.skilleval.gate(h)["tiers"] == "tier1"
    hub.skilleval.evaluate("gm", "static", "t")                                              # a later weaker run keeps the better one
    assert hub.skilleval.gate(h)["tiers"] == "tier1"


# ---- settings ------------------------------------------------------------------------------------------------------------------------
def test_enum_settings_validate_persist_and_default(api: TestClient, hub: Hub) -> None:
    h = admin_h(hub)
    cur = api.get(f"{API}/settings", headers=h).json()
    assert cur["editable"]["eval_gate"] == "advisory" and cur["editable"]["eval_gate_min_tiers"] == "static"
    assert cur["editable"]["eval_gate_observed"] == "require" and cur["enums"]["eval_gate_observed"] == ["off", "advisory", "require"]
    assert cur["enums"]["eval_gate"] == ["off", "advisory", "require"]
    for bad in ({"eval_gate": "strict"}, {"eval_gate_min_tiers": "tier2"}, {"eval_gate": 1}, {"eval_gate": ["off"]},
                {"eval_gate": "require", "plan_retention": 0}, {"nope": "x"}):
        assert api.put(f"{API}/settings", json=bad, headers=h).status_code == 422, bad
    assert api.get(f"{API}/settings", headers=h).json()["editable"]["eval_gate"] == "advisory"     # a bad patch changed nothing
    ok = api.put(f"{API}/settings", json={"eval_gate": "require", "eval_gate_min_tiers": "tier1+2"}, headers=h)
    assert ok.status_code == 200 and ok.json()["editable"]["eval_gate"] == "require"
    assert hub._setting_str("eval_gate_min_tiers") == "tier1+2"


def test_env_defaults_for_the_gate(cfg: Any, monkeypatch: pytest.MonkeyPatch) -> None:
    from agent_hub.config import Settings
    monkeypatch.setenv("HUB_EVAL_GATE", "require")
    monkeypatch.setenv("HUB_EVAL_GATE_MIN_TIERS", "tier1")
    s = Settings()
    assert (s.eval_gate, s.eval_gate_min_tiers) == ("require", "tier1")
    monkeypatch.setenv("HUB_EVAL_GATE", "nonsense")
    with pytest.raises(ValueError):
        Settings()


# ---- observed import with the gate --------------------------------------------------------------------------------------------------
def _observe_one(hub: Hub, obs: ObsAdapter, path: str = "devops/foo", **kw: Any) -> tuple[str, str]:
    obs.skills = [oskill(path, **kw)]
    hub.observe("obs1")
    item = hub.observed_view("obs1")["items"][0]
    return str(item["key"]), str(item["content_hash"])


def _eval_observed(hub: Hub, key: str, runner: FakeEvalRunner) -> dict[str, Any]:
    use(hub, runner)
    return hub.skilleval.evaluate(f"obs1:{key}", "static", "t")


def test_observed_import_writes_provenance_and_does_not_enable(hub: Hub, obs: ObsAdapter) -> None:
    key, h = _observe_one(hub, obs, extra={"ref/n.md": b"N"})
    rec = _eval_observed(hub, key, FakeEvalRunner(tier1=canned()))
    out = hub.import_observed("obs1", [{"key": key}], "skip", False, None)
    row = out["results"][0]
    assert row["status"] == "created" and row["final_name"] == "foo" and row["gate"] == "ok" and row["key"] == key
    side = hub.item_detail("skill", "foo")["sidecar"]
    assert side["source"] == "observed:obs1"
    prov = side["provenance"]
    assert prov["client"] == "obs1" and prov["path"] == "devops/foo" and prov["origin"] == "agent" and prov["content_hash"] == h
    assert prov["observed_at"] and prov["evaluation"] == {"run_id": rec["run_id"], "verdict": "failed", "gate": "ok"}
    assert (hub.cfg.content_dir / "skills" / "foo" / "ref" / "n.md").read_bytes() == b"N"
    assert hub.work.get_profile("obs1").enable.skills == [] and out["enabled"] is False        # enable defaults to false
    again = hub.import_observed("obs1", [{"key": key}], "skip", False, None)["results"][0]
    assert again["status"] == "skipped" and "identical" in again["detail"]                      # identical, on_conflict skip
    renamed = hub.import_observed("obs1", [{"key": key, "name": "bar"}], "skip", True, None)["results"][0]
    assert renamed["status"] == "created" and hub.work.get_profile("obs1").enable.skills == ["bar"]
    miss = hub.import_observed("obs1", [{"key": "nothere-00000000"}], "skip", False, None)["results"][0]
    assert miss["status"] == "error"
    with pytest.raises(BadRequest):
        hub.import_observed("obs1", [{"key": key}], "weird", False, None)


def test_observed_import_conflicts_replace_and_rename(hub: Hub, obs: ObsAdapter) -> None:
    hub.update_settings({"eval_gate_observed": "advisory"})
    skill(hub, "foo", body="Hub version.\n")
    key, _ = _observe_one(hub, obs, body="Observed version.\n")
    skipped = hub.import_observed("obs1", [{"key": key}], "skip", False, None)["results"][0]
    assert skipped["status"] == "skipped" and "differs" in skipped.get("reason", "differs")
    renamed = hub.import_observed("obs1", [{"key": key}], "rename", False, None)["results"][0]
    assert renamed["status"] == "renamed" and renamed["final_name"] == "foo-2"
    replaced = hub.import_observed("obs1", [{"key": key}], "replace", False, None)["results"][0]
    assert replaced["status"] == "replaced" and "Observed version" in (hub.cfg.content_dir / "skills" / "foo" / "SKILL.md").read_text()


def test_require_refuses_missing_and_blocked_and_override_is_audited(hub: Hub, obs: ObsAdapter) -> None:
    hub.update_settings({"eval_gate_observed": "require"})
    key, _ = _observe_one(hub, obs)
    refused = hub.import_observed("obs1", [{"key": key}], "skip", False, None)["results"][0]
    assert refused["status"] == "refused" and refused["code"] == "eval_required" and refused["gate"] == "missing"
    assert "foo" not in hub.work.names("skill")
    _eval_observed(hub, key, FakeEvalRunner(tier1=canned(Code_Risk_Analysis=("failed", [("high", "eval of remote string")]))))
    blocked = hub.import_observed("obs1", [{"key": key}], "skip", False, None)["results"][0]
    assert blocked["status"] == "refused" and blocked["code"] == "eval_blocked" and "eval of remote string" in blocked["detail"]
    assert "foo" not in hub.work.names("skill")
    with pytest.raises(Unprocessable):
        hub.import_observed("obs1", [{"key": key}], "skip", False, "too short")
    with pytest.raises(Unprocessable):
        hub.import_observed("obs1", [{"key": key}], "skip", False, "x" * 501)
    ok = hub.import_observed("obs1", [{"key": key}], "skip", False, "reviewed by hand, the eval is a known false positive", "alice")
    row = ok["results"][0]
    assert row["status"] == "created" and row["gate"] == "overridden"
    ev = [i for i in hub.audit_svc.list_page(50, None)["items"] if i["path"] == "skill.import.override"]
    assert len(ev) == 1 and ev[0]["actor"] == "alice" and ev[0]["params"]["skill"] == "foo" and ev[0]["params"]["gate_status"] == "blocked"
    ev_ev = hub.item_detail("skill", "foo")["sidecar"]["provenance"]["evaluation"]
    assert ev_ev["gate"] == "overridden" and ev_ev["override_reason"].startswith("reviewed by hand")


def test_require_passes_warn_and_ok_advisory_never_refuses_off_has_no_gate(hub: Hub, obs: ObsAdapter) -> None:
    key, _ = _observe_one(hub, obs)
    off_status = hub.update_settings({"eval_gate_observed": "off"})["editable"]["eval_gate_observed"]
    assert off_status == "off"
    row = hub.import_observed("obs1", [{"key": key}], "skip", False, None)["results"][0]
    assert row["status"] == "created" and row["gate"] is None
    assert "evaluation" not in hub.item_detail("skill", "foo")["sidecar"]["provenance"]
    hub.work.delete("skill", "foo")
    hub.update_settings({"eval_gate_observed": "advisory"})
    row = hub.import_observed("obs1", [{"key": key}], "skip", False, None)["results"][0]
    assert row["status"] == "created" and row["gate"] == "missing"                              # advisory: shown, never refused
    hub.work.delete("skill", "foo")
    _eval_observed(hub, key, FakeEvalRunner(tier1=canned(PII_Scan=("failed", [("medium", "email address")]))))
    hub.update_settings({"eval_gate_observed": "require"})
    row = hub.import_observed("obs1", [{"key": key}], "skip", False, None)["results"][0]
    assert row["status"] == "created" and row["gate"] == "warn"                                 # warn passes


def test_import_of_discovered_skills_is_gated_too(hub: Hub, home: Path) -> None:
    add_client(hub, home)
    d = home / "skills" / "disc"
    d.mkdir(parents=True)
    (d / "SKILL.md").write_bytes(md("disc"))
    hub.update_settings({"eval_gate": "require"})
    out = hub.import_items("fake1", [("skill", "disc")], None)
    row = out["results"][0]
    assert row["status"] == "refused" and row["code"] == "eval_required" and "disc" not in hub.work.names("skill")
    # evaluate the exact discovered content (as an observed/hub copy would be), then the import passes
    skill(hub, "tmp-copy")
    hub.work.delete("skill", "tmp-copy")
    from agent_hub.services.skilleval import content_hash
    h = content_hash({"SKILL.md": md("disc")})
    use(hub, FakeEvalRunner(tier1=canned()))
    hub.observe_svc.store("fake1", Observation(skills=[ObservedSkill(path="disc", name="disc", files={"SKILL.md": md("disc")})]))
    k = hub.observed_view("fake1")["items"][0]["key"]
    hub.skilleval.evaluate(f"fake1:{k}", "static", "t")
    assert hub.skilleval.gate(h)["status"] == "ok"
    row = hub.import_items("fake1", [("skill", "disc")], None)["results"][0]
    assert row["status"] == "created" and row["gate"] == "ok"
    assert hub.item_detail("skill", "disc")["sidecar"]["provenance"]["evaluation"]["gate"] == "ok"
    # blocked + override via the discover path
    hub.work.delete("skill", "disc")
    use(hub, FakeEvalRunner(tier1=canned(Code_Risk_Analysis=("failed", [("high", "bad")]))))
    hub.skilleval.evaluate(f"fake1:{k}", "static", "t")
    assert hub.import_items("fake1", [("skill", "disc")], None)["results"][0]["code"] == "eval_blocked"
    row = hub.import_items("fake1", [("skill", "disc")], None, override_reason="accepted after a manual review")["results"][0]
    assert row["status"] == "created" and row["gate"] == "overridden"


# ---- listing, detail and run over HTTP -------------------------------------------------------------------------------------------------
def test_evaluations_listing_filters_summary_and_run(api: TestClient, hub: Hub, obs: ObsAdapter) -> None:
    r = use(hub, FakeEvalRunner(tier1=canned()))
    skill(hub, "alpha")
    obs.skills = [oskill("one"), oskill("two", origin="shipped")]
    hub.observe("obs1")
    h = admin_h(hub)
    hub_only = api.get(f"{API}/skill-evaluations", headers=h).json()                          # default source=hub: the old shape
    assert [e["skill"] for e in hub_only["items"]] == ["alpha"] and hub_only["items"][0]["state"] == "none"
    allrows = api.get(f"{API}/skill-evaluations?source=all", headers=h).json()
    assert allrows["summary"]["total"] == 3 and allrows["summary"]["by_state"] == {"none": 3}
    assert [e["source"] for e in allrows["items"]] == ["hub", "obs1", "obs1"]
    assert all({"subject", "name", "path", "origin", "content_hash", "state", "latest", "queued", "running", "gate"} <= set(e)
               for e in allrows["items"]) and "hub_status" in allrows["items"][1]
    run = api.post(f"{API}/skill-evaluations/run", json={"select": "none_or_stale", "source": "obs1"}, headers=h).json()
    assert len(run["queued"]) == 2 and run["skipped"] == []
    hub.skilleval.join(10)
    assert len(names_in_order(r)) == 2
    rows = api.get(f"{API}/skill-evaluations?source=obs1&state=current&gate=ok", headers=h).json()
    assert len(rows["items"]) == 2 and rows["items"][0]["latest"]["verdict"] == "failed" and rows["summary"]["by_gate"] == {"ok": 2}
    assert [e["name"] for e in api.get(f"{API}/skill-evaluations?source=obs1&origin=shipped", headers=h).json()["items"]] == ["two"]
    assert api.get(f"{API}/skill-evaluations?source=obs1&q=ONE", headers=h).json()["items"][0]["name"] == "one"
    again = api.post(f"{API}/skill-evaluations/run", json={"select": "none_or_stale", "source": "all"}, headers=h).json()
    assert again["queued"] == ["hub:alpha"]                                                    # only the one never evaluated
    hub.skilleval.join(10)
    subject = rows["items"][0]["subject"]
    det = api.get(f"{API}/skill-evaluations/{subject}", headers=h).json()
    assert det["subject"] == subject and det["source"] == "obs1" and det["state"] == "current" and det["gate"]["status"] == "ok"
    assert det["history"] and det["hub_status"] == "not_in_hub"
    rid = det["latest"]["run_id"]
    assert api.get(f"{API}/skill-evaluations/{subject}/runs/{rid}/report", headers=h).json()["markdown"]
    # exactly one of subjects / select
    for body in ({}, {"subjects": ["hub:alpha"], "select": "all"}, {"select": "bogus"}, {"tiers": "x", "select": "all"}):
        assert api.post(f"{API}/skill-evaluations/run", json=body, headers=h).status_code == 422, body
    assert api.get(f"{API}/skill-evaluations?source=BAD/x", headers=h).status_code == 400
    assert api.get(f"{API}/skill-evaluations?state=weird", headers=h).status_code == 400
    assert api.get(f"{API}/skill-evaluations/obs1:nothere-00000000", headers=h).status_code == 404
    assert api.post(f"{API}/skill-evaluations/queue/cancel", headers=h).json() == {"cancelled_queued": 0, "killed_running": False}


def test_viewer_can_read_but_not_post(api: TestClient, hub: Hub, obs: ObsAdapter) -> None:
    r = use(hub)
    skill(hub)
    obs.skills = [oskill("one")]
    hub.observe("obs1")
    v = bearer(str(hub.auth.create_key("v", "viewer")["secret"]))
    for path in ("/skill-evaluations", "/skill-evaluations/queue", "/clients/obs1/observed", "/skill-evaluations/hub:alpha"):
        assert api.get(f"{API}{path}", headers=v).status_code == 200, path
    for path, body in (("/clients/obs1/observe", None), ("/skill-evaluations/run", {"select": "all"}),
                       ("/skill-evaluations/queue/cancel", None), ("/clients/obs1/observed/import", {"items": [{"key": "x-00000000"}]}),
                       ("/import", {"client": "obs1"})):
        resp = api.post(f"{API}{path}", json=body, headers=v)
        assert resp.status_code == 403 and resp.json()["code"] == "forbidden", path
    anon = api.post(f"{API}/skill-evaluations/run", json={"select": "all"}, headers=CSRF)
    assert anon.status_code == 401 and tool_calls(r) == []


def test_observed_import_over_http(api: TestClient, hub: Hub, obs: ObsAdapter) -> None:
    key, _ = _observe_one(hub, obs)
    h = admin_h(hub)
    resp = api.post(f"{API}/clients/obs1/observed/import", json={"items": [{"key": key, "name": "mine"}]}, headers=h)
    row = resp.json()["results"][0]
    assert resp.status_code == 200 and row["status"] == "refused" and row["code"] == "eval_required" and resp.json()["gate_mode"] == "require"
    hub.update_settings({"eval_gate_observed": "advisory"})
    resp = api.post(f"{API}/clients/obs1/observed/import", json={"items": [{"key": key, "name": "mine"}]}, headers=h)
    assert resp.json()["results"][0]["final_name"] == "mine" and resp.json()["results"][0]["gate"] == "missing"
    assert api.post(f"{API}/clients/obs1/observed/import", json={"items": []}, headers=h).status_code == 400
    assert api.post(f"{API}/clients/obs1/observed/import", json={"items": [{"key": key}], "enable": "x"}, headers=h).status_code == 422
    assert api.post(f"{API}/clients/nope/observed/import", json={"items": [{"key": key}]}, headers=h).status_code == 404


# ---- runner and CLI ---------------------------------------------------------------------------------------------------------------------
def test_real_runner_max_stdout_keyword(monkeypatch: pytest.MonkeyPatch) -> None:
    def fake_run(argv: list[str], **kw: Any) -> subprocess.CompletedProcess[bytes]:
        return subprocess.CompletedProcess(argv, 0, b"x" * 300_000, b"")

    monkeypatch.setattr(fs_mod.subprocess, "run", fake_run)
    assert len(fs_mod.RealRunner().run(["x"], timeout=1)[1]) == 100_000                         # default unchanged
    assert len(fs_mod.RealRunner(max_timeout=120, max_stdout=250_000).run(["x"], timeout=1)[1]) == 250_000


def test_hub_builds_the_observe_runner_with_the_big_cap(hub: Hub) -> None:
    from agent_hub.services.fs import StreamingRunner
    h2 = Hub(hub.cfg, registry=hub.registry, runner=hub.runner)                                 # type: ignore[arg-type]
    try:
        assert isinstance(h2.observe_runner, StreamingRunner) and h2.observe_runner.max_stdout == 48 * 1024 * 1024
        assert h2.observe_runner.max_timeout == 120 and h2.runner is hub.runner
    finally:
        h2.close()


def test_cli_observe_evaluate_and_import_observed(hub: Hub, obs: ObsAdapter, monkeypatch: pytest.MonkeyPatch,
                                                  capsys: pytest.CaptureFixture[str]) -> None:
    monkeypatch.setenv("HUB_API_KEY", str(hub.auth.create_key("k", "admin")["secret"]))
    obs.skills = [oskill("devops/foo"), oskill("bar")]
    assert cli_run(["observe", "--client", "obs1"], hub=hub) == 0
    assert "2 skill(s) observed" in capsys.readouterr().out
    r = use(hub, FakeEvalRunner(tier1=canned(Code_Risk_Analysis=("failed", [("high", "bad thing")]))))
    assert cli_run(["evaluate", "--source", "obs1"], hub=hub) == 0                              # no --wait: nothing runs
    assert "nothing was run" in capsys.readouterr().out and tool_calls(r) == []
    assert cli_run(["evaluate", "--source", "obs1", "--wait"], hub=hub) == 2                    # failed verdicts exist
    out = capsys.readouterr().out
    assert out.count("obs1:") == 2 and len(names_in_order(r)) == 2
    assert cli_run(["evaluate", "--source", "obs1", "--wait"], hub=hub) == 0                    # nothing none_or_stale left
    assert "nothing to evaluate" in capsys.readouterr().out
    assert cli_run(["evaluate", "--source", "obs1", "--select", "all", "--wait"], hub=hub) == 2
    assert len(names_in_order(r)) == 2                                                          # reused: no new process
    capsys.readouterr()
    assert cli_run(["evaluate", "--subject", "obs1:bar-nothere0", "--wait"], hub=hub) == 1
    capsys.readouterr()
    hub.update_settings({"eval_gate_observed": "require"})
    keys = [i["key"] for i in hub.observed_view("obs1")["items"]]
    assert cli_run(["import-observed", "--client", "obs1", "--key", keys[0]], hub=hub) == 1     # blocked
    assert "refused" in capsys.readouterr().out
    assert cli_run(["import-observed", "--client", "obs1", "--key", keys[0], "--override-reason", "known false positive, reviewed"],
                   hub=hub) == 0
    assert "overridden" in capsys.readouterr().out
    assert cli_run(["import-observed", "--client", "obs1", "--key", keys[0], "--key", keys[1], "--name", "x"], hub=hub) == 1
    assert SAFE_CHECKS in " ".join(tool_calls(r)[0]["argv"])


def test_missing_observed_snapshot_is_not_found(hub: Hub, obs: ObsAdapter) -> None:
    use(hub)
    with pytest.raises(NotFound):
        hub.skilleval.snapshot("obs1:nothere-00000000")
    with pytest.raises(NotFound):
        hub.skilleval.detail("obs1:nothere-00000000")


def test_list_rows_carry_security_severity(api: TestClient, hub: Hub) -> None:
    c = canned(Code_Risk_Analysis=("failed", [("high", "a"), ("critical", "b")]), Brand_New_Scanner=("failed", [("high", "c")]),
               QUALITY=("failed", [("critical", "not security")]))
    llm = canned(Code_Risk_Analysis=("failed", [("high", "a")]), Secrets_Detection=("failed", [("high", "llm only")]))
    use(hub, FakeEvalRunner(tier1=c, llm=llm))
    skill(hub, "sev")
    skill(hub, "never")
    hub.skilleval.evaluate("sev", "tier1", "t")
    rows = {r["name"]: r for r in api.get(f"{API}/skill-evaluations", headers=admin_h(hub)).json()["items"]}
    assert rows["sev"]["latest"]["security_severity"] == {"critical": 1, "high": 3}          # CRA 1c+1h, new scanner 1h, secrets 1h
    assert rows["never"]["latest"] is None
    rec = hub.skilleval.records("hub:sev")[0]
    del rec["validators"]                                                                     # an older record: the field is omitted
    from agent_hub.services.skilleval import security_severity
    assert security_severity(rec) is None


def test_observed_bytecode_is_left_out_of_the_store_the_hash_evaluation_and_import(hub: Hub, obs: ObsAdapter) -> None:
    obs.skills = [oskill("tool", extra={"scripts/t.py": b"print(1)\n", "scripts/__pycache__/t.cpython-313.pyc": b"\x00bc",
                                        "x.pyc": b"\x00bc"})]
    out = hub.observe("obs1")
    entry = out["skills"][0] if "skills" in out else None
    idx = json.loads((hub.cfg.data_dir / "observed" / "obs1" / "index.json").read_text())
    s = idx["skills"][0]
    assert s["files"] == 2 and any("2 compiled Python bytecode" in n for n in idx["notes"])
    assert set(hub.observe_svc.files("obs1", s["key"])) == {"SKILL.md", "scripts/t.py"}
    assert hub.skilleval.current_hash(f"obs1:{s['key']}") == s["content_hash"]
    assert entry is None or entry.get("files", 2) == 2
