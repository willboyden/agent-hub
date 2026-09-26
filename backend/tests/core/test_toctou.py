"""M4: dirfd-relative writes and compare-and-swap. Attacks are simulated between plan and commit."""
from __future__ import annotations

import os
import shutil
from pathlib import Path
from typing import Any

import pytest

from agent_hub.domain.errors import Conflict
from agent_hub.services import applier as ap
from agent_hub.services.hub import Hub

from .conftest import add_client, seed_content


@pytest.fixture
def setup(hub: Hub, home: Path) -> Hub:
    add_client(hub, home)
    seed_content(hub)
    hub.commit("c", "t")
    return hub


def snapshot(p: Path) -> dict[str, bytes]:
    return {f.relative_to(p).as_posix(): f.read_bytes() for f in sorted(p.rglob("*")) if f.is_file()}


def test_symlink_swapped_in_before_staging_is_refused(setup: Hub, home: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    outside = tmp_path / "outside"
    outside.mkdir()
    (home / "agents").mkdir()
    plan = setup.plan(None)                                   # planned while `agents` is a normal directory
    shutil.rmtree(home / "agents")
    (home / "agents").symlink_to(outside)                     # swapped for a symlink between plan and apply
    with pytest.raises(Conflict):                             # the pre-pass replan no longer matches the reviewed plan
        setup.apply(plan.id, True, [], "t")
    assert list(outside.iterdir()) == []


def test_symlink_swap_after_staging_cannot_redirect_the_write(setup: Hub, home: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    outside = tmp_path / "outside"
    outside.mkdir()
    plan = setup.plan(None)
    real = ap.Applier._commit
    state = {"swapped": False}

    def swap_then_commit(self: Any, op: Any) -> None:
        if not state["swapped"]:
            state["swapped"] = True
            d = home / "agents"
            if d.exists():
                d.rename(home / "agents.moved")
            d.symlink_to(outside)                             # attacker swaps the directory for a link mid-apply
        real(self, op)

    monkeypatch.setattr(ap.Applier, "_commit", swap_then_commit)
    setup.apply(plan.id, True, [], "t")
    assert list(outside.iterdir()) == []                      # the held dirfd still points at the ORIGINAL directory


def test_live_file_edited_between_plan_and_commit_aborts_the_whole_client(setup: Hub, home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    setup.apply(setup.plan(None).id, True, [], "t")
    before = snapshot(home)
    setup.work.put("agent", "reviewer", {"description": "d", "capabilities": ["read"], "body": "v2\n"})
    setup.work.put("instruction", "style", {"title": "Style", "order": 10, "body": "v2\n"})
    setup.commit("v2", "t")
    plan = setup.plan(None)
    real = ap.Applier._commit
    calls = {"n": 0}

    def edit_then_commit(self: Any, op: Any) -> None:
        calls["n"] += 1
        if calls["n"] == 2:                                   # after one file was already replaced
            (home / "agents/reviewer.md").write_text("edited by a human at the worst moment\n")
        real(self, op)

    monkeypatch.setattr(ap.Applier, "_commit", edit_then_commit)
    res = setup.apply(plan.id, True, [], "t")["results"][0]
    assert res["status"] == "failed" and res["error"].startswith("conflict") and "nothing was changed" in res["error"]
    after = snapshot(home)
    assert after.pop("agents/reviewer.md") == b"edited by a human at the worst moment\n"    # their edit is never clobbered
    before.pop("agents/reviewer.md")
    assert after == before                                                                # everything else rolled back
    assert not list(home.rglob(".hub-tmp-*"))


def test_removal_is_also_compare_and_swap(setup: Hub, home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    setup.apply(setup.plan(None).id, True, [], "t")
    setup.organizer.toggle("fake1", "agent", "reviewer", False)
    setup.commit("off", "t")
    plan = setup.plan(None)
    real = ap.Applier._commit

    def edit_first(self: Any, op: Any) -> None:
        if op.name == "reviewer.md":
            (home / "agents/reviewer.md").write_text("late edit")
        real(self, op)

    monkeypatch.setattr(ap.Applier, "_commit", edit_first)
    assert setup.apply(plan.id, True, [], "t")["results"][0]["status"] == "failed"
    assert (home / "agents/reviewer.md").read_text() == "late edit"


def test_stage_time_race_and_dir_swap_detection(setup: Hub, home: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    plan = setup.plan(None)
    real = ap.Applier._stage
    n = {"i": 0}

    def swap_stage(self: Any, op: Any, bdir: Path) -> None:
        n["i"] += 1
        if n["i"] == 1:
            (home / "skills").mkdir(exist_ok=True)
            os.symlink(tmp_path, home / "skills" / "alpha")      # a link where the skill directory will be created
        real(self, op, bdir)

    monkeypatch.setattr(ap.Applier, "_stage", swap_stage)
    res = setup.apply(plan.id, True, [], "t")["results"][0]
    assert res["status"] == "failed"
    assert not list(tmp_path.glob("SKILL.md")) and not (tmp_path / "ref").exists()
