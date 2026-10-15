"""Audit round for Addendum F: a streaming observe runner with hard caps, the fail-closed gate details, per-call process tracking,
validate-before-destroy imports, failed-observe handling, secret detection at import, reserved client ids and override auditing."""
from __future__ import annotations

import os
import sys
import threading
import time
from pathlib import Path
from typing import Any

import pytest

from agent_hub.domain.errors import Unavailable, Unprocessable
from agent_hub.services import hub as hub_mod
from agent_hub.services.fs import StreamingRunner
from agent_hub.services.hub import Hub
from agent_hub.services.skilleval import RealEvalRunner

from .fakes import FakeEvalRunner
from .test_observed_eval import ObsAdapter, _observe_one, canned, md, obs, oskill  # noqa: F401
from .test_skilleval import skill, use


def _gone(pid: int, secs: float = 5) -> bool:
    end = time.monotonic() + secs
    while time.monotonic() < end:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return True
        with open(f"/proc/{pid}/stat") as fh:
            if fh.read().split()[2] == "Z":
                return True
        time.sleep(0.05)
    return False


# ---- H1 --------------------------------------------------------------------------------------------------------------------------
def test_streaming_runner_kills_a_child_that_floods_stdout(tmp_path: Path) -> None:
    pidfile = tmp_path / "pid"
    code = (f"import os, sys\nopen({str(pidfile)!r}, 'w').write(str(os.getpid()))\n"
            "chunk = 'x' * 65536\nwhile True:\n    sys.stdout.write(chunk); sys.stdout.flush()\n")
    t0 = time.monotonic()
    rc, out, err = StreamingRunner(max_timeout=60, max_stdout=10_000).run([sys.executable, "-c", code], timeout=30)
    assert (rc, out, err) == (125, "", "output exceeded cap") and time.monotonic() - t0 < 20
    assert _gone(int(pidfile.read_text()))                                        # the whole group was killed


def test_streaming_runner_caps_stderr_too_and_keeps_normal_semantics(tmp_path: Path) -> None:
    flood = "import sys\nwhile True:\n    sys.stderr.write('e' * 65536); sys.stderr.flush()\n"
    assert StreamingRunner(max_stderr=5_000).run([sys.executable, "-c", flood], timeout=30)[0] == 125
    assert StreamingRunner().run([sys.executable, "-c", "print('hi')"], timeout=30) == (0, "hi\n", "")
    assert StreamingRunner().run([sys.executable, "-c", "import sys; sys.stderr.write('bad'); sys.exit(3)"], timeout=30) == (3, "", "bad")
    rc, _, err = StreamingRunner(max_timeout=1).run([sys.executable, "-c", "import time; time.sleep(60)"], timeout=50)
    assert (rc, err) == (124, "timed out")                                        # min(timeout, max_timeout)
    assert StreamingRunner().run(["/nonexistent/prog"], timeout=1)[0] == 127 and StreamingRunner().run([], timeout=1)[0] == 127
    # exactly at the cap is fine, one byte more is not
    ok = StreamingRunner(max_stdout=100).run([sys.executable, "-c", "import sys; sys.stdout.write('a' * 100)"], timeout=30)
    assert ok[0] == 0 and len(ok[1]) == 100
    assert StreamingRunner(max_stdout=100).run([sys.executable, "-c", "import sys; sys.stdout.write('a' * 101)"], timeout=30)[0] == 125


def test_hermes_style_observe_runs_through_the_streaming_runner(hub: Hub, obs: ObsAdapter) -> None:  # noqa: F811
    assert isinstance(hub.observe_runner, StreamingRunner)


# ---- M3 --------------------------------------------------------------------------------------------------------------------------
def test_cancel_kills_every_live_child_even_after_a_version_probe(tmp_path: Path) -> None:
    r = RealEvalRunner()
    sleeper = [sys.executable, "-c", "import time; time.sleep(60)"]
    results: list[Any] = []
    threads = [threading.Thread(target=lambda: results.append(r.run(sleeper, cwd=tmp_path, env={"PATH": os.environ["PATH"]},
                                                                    timeout=60))) for _ in range(2)]
    for t in threads:
        t.start()
    deadline = time.monotonic() + 10
    while len(r._procs) < 2 and time.monotonic() < deadline:
        time.sleep(0.02)
    assert len(r._procs) == 2
    probe = r.run([sys.executable, "-c", "print('version 1')"], cwd=tmp_path, env={"PATH": os.environ["PATH"]}, timeout=30)
    assert probe.returncode == 0 and len(r._procs) == 2                            # a probe neither hides nor removes the others
    t0 = time.monotonic()
    r.cancel()
    for t in threads:
        t.join(15)
    assert time.monotonic() - t0 < 12 and len(results) == 2 and all(x.returncode != 0 and not x.timed_out for x in results)
    assert r._procs == set()


# ---- M4 --------------------------------------------------------------------------------------------------------------------------
@pytest.mark.parametrize("bad", ["hub.yaml", ".git/hooks/x"])
def test_replace_validates_before_deleting(hub: Hub, obs: ObsAdapter, bad: str) -> None:  # noqa: F811
    hub.update_settings({"eval_gate_observed": "off"})
    skill(hub, "foo", body="Hub version.\n")
    before = (hub.cfg.content_dir / "skills" / "foo" / "SKILL.md").read_bytes()
    obs.skills = [oskill("foo", body="New.\n", extra={bad: b"x"})]
    hub.observe("obs1")
    key = hub.observed_view("obs1")["items"][0]["key"]
    row = hub.import_observed("obs1", [{"key": key}], "replace", False, None)["results"][0]
    assert row["status"] == "error" and row["code"] == "validation_error"
    assert (hub.cfg.content_dir / "skills" / "foo" / "SKILL.md").read_bytes() == before
    assert (hub.cfg.content_dir / "skills" / "foo" / "hub.yaml").is_file()           # the old sidecar survived too


def test_content_caps_apply_to_observed_import(hub: Hub, obs: ObsAdapter) -> None:  # noqa: F811
    hub.update_settings({"eval_gate_observed": "off"})
    obs.skills = [oskill("many", extra={f"f{i}.md": b"x" for i in range(210)}),
                  oskill("fat", extra={"big.bin": b"x" * (hub.cfg.max_skill_bytes + 1)})]
    hub.observe("obs1")
    rows = hub.import_observed("obs1", [{"key": i["key"]} for i in hub.observed_view("obs1")["items"]], "skip", False, None)["results"]
    assert [r["status"] for r in rows] == ["error", "error"] and {r["code"] for r in rows} == {"skill_too_large"}
    assert hub.work.names("skill") == []


def test_a_failed_replace_restores_the_old_skill(hub: Hub, obs: ObsAdapter, monkeypatch: pytest.MonkeyPatch) -> None:  # noqa: F811
    hub.update_settings({"eval_gate_observed": "off"})
    skill(hub, "foo", body="Hub version.\n")
    key, _ = _observe_one(hub, obs, path="foo", body="New.\n")
    real = hub.work.write_skill_files

    def boom(name: str, files: dict[str, bytes], sidecar: Any) -> None:
        real(name, {"SKILL.md": files["SKILL.md"]}, sidecar)                         # partial write, then failure
        raise OSError("disk full")

    monkeypatch.setattr(hub.work, "write_skill_files", boom)
    row = hub.import_observed("obs1", [{"key": key}], "replace", False, None)["results"][0]
    assert row["status"] == "error"
    assert "Hub version." in (hub.cfg.content_dir / "skills" / "foo" / "SKILL.md").read_text()
    assert sorted(p.name for p in (hub.cfg.content_dir / "skills").iterdir()) == [".gitkeep", "foo"]


# ---- M5 --------------------------------------------------------------------------------------------------------------------------
def test_incomplete_and_empty_observation_keeps_the_previous_one(hub: Hub, obs: ObsAdapter) -> None:  # noqa: F811
    obs.skills = [oskill("one")]
    hub.observe("obs1")
    obs.skills, obs.complete = [], False
    with pytest.raises(Unavailable) as e:
        hub.observe("obs1")
    assert e.value.code == "observe_failed" and "note one" in e.value.detail
    assert [i["path"] for i in hub.observed_view("obs1")["items"]] == ["one"]
    obs.complete = True                                                                # an empty but COMPLETE result is real
    assert hub.observe("obs1")["total"] == 0


def test_unprintable_unencodable_and_colliding_paths_are_skipped(hub: Hub, obs: ObsAdapter) -> None:  # noqa: F811
    rlo, sep, lone = chr(0x202E), chr(0x2028), chr(0xD800)
    skills = [oskill("good"), oskill(f"bad{lone}name", "bad"), oskill(f"rlo{rlo}name", "rlo"), oskill(f"sep{sep}x", "sep"),
              oskill("del\x7fx", "del"),
              oskill("fine/file", extra={"a/x": b"1", "a/x/y": b"2", "ok.md": b"3", f"bad{rlo}.md": b"4", f"lone{lone}": b"5"})]
    obs.skills = skills
    out = hub.observe("obs1")
    assert out["total"] == 2
    items = {i["path"]: i for i in hub.observed_view("obs1")["items"]}
    assert set(items) == {"good", "fine/file"}
    root = hub.cfg.data_dir / "observed" / "obs1" / "skills" / items["fine/file"]["key"]
    assert sorted(p.relative_to(root).as_posix() for p in root.rglob("*") if p.is_file()) == ["SKILL.md", "a/x/y", "ok.md"]
    text = " ".join(out["notes"])
    assert "‮" not in text and "\ud800" not in text and "also used as a directory" in text


# ---- L1 --------------------------------------------------------------------------------------------------------------------------
def test_forget_and_prune_remove_stored_runs(hub: Hub, obs: ObsAdapter) -> None:  # noqa: F811
    use(hub, FakeEvalRunner(tier1=canned()))
    obs.skills = [oskill("one"), oskill("two")]
    hub.observe("obs1")
    keys = {i["path"]: i["key"] for i in hub.observed_view("obs1")["items"]}
    for k in keys.values():
        hub.skilleval.evaluate(f"obs1:{k}", "static", "t")
    runs = hub.cfg.data_dir / "skilleval" / "@observed" / "obs1"
    assert sorted(p.name for p in runs.iterdir()) == sorted(keys.values())
    obs.skills = [oskill("one")]
    hub.observe("obs1")                                                                # "two" is gone: its runs go with it
    assert [p.name for p in runs.iterdir()] == [keys["one"]]
    hub.delete_client("obs1")
    assert not runs.exists() and not (hub.cfg.data_dir / "observed" / "obs1").exists()


def test_select_is_capped_per_request(hub: Hub, obs: ObsAdapter, monkeypatch: pytest.MonkeyPatch) -> None:  # noqa: F811
    use(hub, FakeEvalRunner(tier1=canned()))
    obs.skills = [oskill(f"s{i}") for i in range(5)]
    hub.observe("obs1")
    monkeypatch.setattr(hub_mod, "SELECT_CAP", 3)
    out = hub.eval_run(None, "all", "obs1", "static", False, "t")
    assert len(out["queued"]) == 3 and [s["reason"] for s in out["skipped"]] == ["request_cap"] * 2
    hub.skilleval.join(10)


# ---- L2 / L4 ---------------------------------------------------------------------------------------------------------------------
def test_secret_looking_text_is_refused_unless_overridden_and_audited(hub: Hub, obs: ObsAdapter) -> None:  # noqa: F811
    hub.update_settings({"eval_gate_observed": "off"})
    leak = "sk-" + "A1b2C3d4E5f6G7h8I9j0"
    key, h = _observe_one(hub, obs, path="leaky", extra={"ref/env.md": f"api_key={leak}\n".encode()})
    row = hub.import_observed("obs1", [{"key": key}], "skip", False, None)["results"][0]
    assert row["status"] == "refused" and row["code"] == "secret_detected" and "ref/env.md" in row["detail"] and leak not in str(row)
    assert hub.work.names("skill") == []
    ok = hub.import_observed("obs1", [{"key": key}], "skip", False, "reviewed: the value is a documented dummy", "alice")["results"][0]
    assert ok["status"] == "created"
    ev = [i for i in hub.audit_svc.list_page(50, None)["items"] if i["path"] == "skill.import.override"]
    assert len(ev) == 1
    p = ev[0]["params"]
    assert ev[0]["actor"] == "alice" and p["key"] == key and p["content_hash"] == h[:12] and p["overrides"] == ["secret_detected"]
    assert p["outcome"] == "written" and p["skill"] == "leaky" and leak not in str(ev)


def test_override_is_audited_even_when_the_write_fails(hub: Hub, obs: ObsAdapter, monkeypatch: pytest.MonkeyPatch) -> None:  # noqa: F811
    key, h = _observe_one(hub, obs)
    use(hub, FakeEvalRunner(tier1=canned(Code_Risk_Analysis=("failed", [("high", "bad")]))))
    hub.skilleval.evaluate(f"obs1:{key}", "static", "t")
    monkeypatch.setattr(hub.work, "write_skill_files", lambda *a, **k: (_ for _ in ()).throw(OSError("disk full")))
    row = hub.import_observed("obs1", [{"key": key}], "skip", False, "known false positive, reviewed by hand", "bob")["results"][0]
    assert row["status"] == "error"
    ev = [i for i in hub.audit_svc.list_page(50, None)["items"] if i["path"] == "skill.import.override"]
    assert len(ev) == 1 and ev[0]["params"]["outcome"] == "failed: OSError" and ev[0]["params"]["overrides"] == ["eval_blocked"]
    assert ev[0]["params"]["key"] == key and ev[0]["params"]["content_hash"] == h[:12]


# ---- L3 / M2 ---------------------------------------------------------------------------------------------------------------------
@pytest.mark.parametrize("cid", ["hub", "a:b"])
def test_reserved_client_ids(hub: Hub, home: Path, cid: str) -> None:
    with pytest.raises(Unprocessable) as e:
        hub.put_client(cid, {"adapter": "fake", "display_name": "x", "roots": {"home": str(home)}}, create=True)
    assert e.value.code == "reserved_client_id"


def test_observed_gate_default_is_require_and_import_gate_is_separate(hub: Hub, home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    assert hub._setting_str("eval_gate") == "advisory" and hub._setting_str("eval_gate_observed") == "require"
    from agent_hub.config import Settings
    monkeypatch.setenv("HUB_EVAL_GATE_OBSERVED", "advisory")
    assert Settings().eval_gate_observed == "advisory"
    d = home / "skills" / "disc"
    d.mkdir(parents=True)
    (d / "SKILL.md").write_bytes(md("disc"))
    from .conftest import add_client
    add_client(hub, home)
    row = hub.import_items("fake1", [("skill", "disc")], None)                       # /import follows eval_gate (advisory): not refused
    assert row["results"][0]["status"] == "created" and row["gate_mode"] == "advisory"
