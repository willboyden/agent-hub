"""Rollback of an apply: restore backups, delete created files, protect later edits, all in a temp dir."""
from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from agent_hub.cli import run as cli
from agent_hub.domain.errors import Conflict, NotFound, Unprocessable
from agent_hub.services import applier as ap
from agent_hub.services.hub import Hub

from .conftest import add_client, seed_content


def snapshot(p: Path) -> dict[str, bytes]:
    return {f.relative_to(p).as_posix(): f.read_bytes() for f in sorted(p.rglob("*")) if f.is_file()}


def do_apply(hub: Hub, adopt: list[str] | None = None) -> dict[str, Any]:
    plan = hub.plan(None)
    return hub.apply(plan.id, True, adopt or [], "t")["results"][0]  # type: ignore[no-any-return]


@pytest.fixture
def setup(hub: Hub, home: Path) -> Hub:
    add_client(hub, home)
    seed_content(hub)
    hub.commit("c", "t")
    return hub


def test_applies_are_recorded_and_listed(setup: Hub, home: Path) -> None:
    assert setup.list_applies("fake1")["items"] == []
    r = do_apply(setup)
    lst = setup.list_applies("fake1")["items"]
    assert lst[0]["apply_id"] == r["apply_id"] and lst[0]["plan_id"].startswith("plan_") and lst[0]["counts"]["add"] >= 4
    assert lst[0]["rolled_back_at"] is None and lst[0]["apply_id"].endswith(lst[0]["plan_id"])
    assert do_apply(setup)["apply_id"] is None                                  # a no-op apply records nothing
    with pytest.raises(NotFound):
        setup.list_applies("ghost")


def test_rollback_restores_bytes_and_removes_added_files(setup: Hub, home: Path) -> None:
    (home / "agents").mkdir()
    (home / "agents/reviewer.md").write_text("original hand-written agent\n")
    (home / "CLAUDE.md").write_text("mine\n")
    before = snapshot(home)
    first = do_apply(setup, adopt=["home:agents/reviewer.md"])                 # changes one file, adds the rest
    assert snapshot(home) != before
    out = setup.rollback("fake1", None, True, False, [], "t")
    assert out["status"] == "rolled_back" and out["apply_id"] == first["apply_id"] and not out["conflicts"]
    assert snapshot(home) == before                                            # byte-identical, added files gone
    assert not (home / "skills").exists()                                      # and the directories they created are pruned
    assert out["verify"]["ok"] is not False
    assert setup.list_applies("fake1")["items"][0]["rolled_back_at"] is not None
    lock = setup.locks.load("fake1")
    assert lock is not None and lock.files == {}                               # lock entries are back to the pre-apply state


def test_later_hand_edit_is_protected_and_force_overrides(setup: Hub, home: Path) -> None:
    do_apply(setup)
    (home / "agents/reviewer.md").write_text("edited after the apply\n")
    dry = setup.rollback("fake1", None, False, True, [], "t")
    assert dry["status"] == "dry_run" and [c["path"] for c in dry["conflicts"]] == ["agents/reviewer.md"]
    assert all("diff" in c for c in dry["changed"]) and (home / "skills/alpha/SKILL.md").exists()          # dry run wrote nothing
    out = setup.rollback("fake1", None, True, False, [], "t")
    assert out["status"] == "partially_rolled_back" and [c["path"] for c in out["conflicts"]] == ["agents/reviewer.md"]
    assert (home / "agents/reviewer.md").read_text() == "edited after the apply\n"        # never clobbered
    assert not (home / "skills").exists()                                                  # the untouched files were rolled back
    again = setup.rollback("fake1", None, True, False, [], "t")                             # the same conflict, nothing else
    assert again["status"] == "conflicts_only" and again["changed"] == []
    final = setup.rollback("fake1", None, True, False, ["home:agents/reviewer.md"], "t")   # explicit override finishes the job
    assert final["status"] == "rolled_back" and not (home / "agents").exists()


def test_force_paths_restores_an_edited_file(setup: Hub, home: Path) -> None:
    (home / "agents").mkdir()
    (home / "agents/reviewer.md").write_text("v0\n")
    do_apply(setup, adopt=["home:agents/reviewer.md"])
    (home / "agents/reviewer.md").write_text("v-edited\n")
    out = setup.rollback("fake1", None, True, False, ["home:agents/reviewer.md"], "t")
    assert not out["conflicts"] and (home / "agents/reviewer.md").read_text() == "v0\n"


def test_second_rollback_is_a_clear_noop(setup: Hub, home: Path) -> None:
    do_apply(setup)
    setup.rollback("fake1", None, True, False, [], "t")
    out = setup.rollback("fake1", None, True, False, [], "t")
    assert out["status"] == "already_rolled_back" and out["changed"] == [] and "nothing to do" in out["message"]


def test_rollback_requires_confirm_and_a_recorded_apply(setup: Hub) -> None:
    with pytest.raises(Unprocessable) as ei:
        setup.rollback("fake1", None, False, False, [], "t")
    assert ei.value.code == "confirm_required"
    with pytest.raises(NotFound):
        setup.rollback("fake1", None, True, False, [], "t")
    with pytest.raises(Exception):  # noqa: B017 - a hostile apply id is a 400
        setup.rollback("fake1", "../../etc/passwd", True, False, [], "t")


def test_older_apply_can_only_be_rolled_back_if_every_file_still_matches(setup: Hub, home: Path) -> None:
    first = do_apply(setup)
    setup.work.put("agent", "reviewer", {"description": "d", "capabilities": ["read"], "body": "v2\n"})
    setup.commit("v2", "t")
    second = do_apply(setup)
    assert first["apply_id"] != second["apply_id"]
    with pytest.raises(Conflict) as ei:                                            # first's reviewer.md was changed by second
        setup.rollback("fake1", first["apply_id"], True, False, [], "t")
    assert ei.value.code == "rollback_not_latest" and (home / "skills/alpha/SKILL.md").exists()
    setup.rollback("fake1", second["apply_id"], True, False, [], "t")              # newest first
    assert "Review carefully" in (home / "agents/reviewer.md").read_text()
    setup.rollback("fake1", first["apply_id"], True, False, [], "t")
    assert not (home / "agents").exists() and not (home / "skills").exists()


def test_rollback_restores_removed_files_and_uses_safe_writes(setup: Hub, home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    do_apply(setup)
    setup.organizer.toggle("fake1", "agent", "reviewer", False)
    setup.commit("off", "t")
    r = do_apply(setup)
    assert not (home / "agents/reviewer.md").exists()
    out = setup.rollback("fake1", r["apply_id"], True, False, [], "t")
    assert out["status"] == "rolled_back" and (home / "agents/reviewer.md").read_text() == "# reviewer\nReview carefully.\n"
    assert "reviewer" in json.dumps(setup.locks.load("fake1").model_dump())          # type: ignore[union-attr]


def test_rollback_aborts_atomically_when_a_file_changes_mid_way(setup: Hub, home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    do_apply(setup)
    after = snapshot(home)
    real = ap.Applier._commit
    n = {"i": 0}

    def edit_late(self: Any, op: Any) -> None:
        n["i"] += 1
        if n["i"] == 2:
            (home / "agents/reviewer.md").write_text("late edit\n")
        real(self, op)

    monkeypatch.setattr(ap.Applier, "_commit", edit_late)
    with pytest.raises(Conflict) as ei:
        setup.rollback("fake1", None, True, False, [], "t")
    assert ei.value.code == "rollback_failed"
    got = snapshot(home)
    assert got.pop("agents/reviewer.md") == b"late edit\n"
    after.pop("agents/reviewer.md")
    assert got == after                                                                # everything else restored to post-apply
    assert setup.list_applies("fake1")["items"][0]["rolled_back_at"] is None


def test_rollback_http_and_cli(api: Any, hub: Hub, home: Path, admin_key: str, viewer_key: str, capsys: pytest.CaptureFixture[str],
                               monkeypatch: pytest.MonkeyPatch) -> None:
    add_client(hub, home)
    seed_content(hub)
    hub.commit("c", "t")
    h = {"Authorization": f"Bearer {admin_key}"}
    r = do_apply(hub)
    assert api.get("/api/v1/clients/fake1/applies", headers=h).json()["items"][0]["apply_id"] == r["apply_id"]
    v = {"Authorization": f"Bearer {viewer_key}"}
    assert api.post("/api/v1/clients/fake1/rollback", json={"confirm": True}, headers=v).status_code == 403
    assert api.post("/api/v1/clients/fake1/rollback", json={}, headers=h).status_code == 422
    d = api.post("/api/v1/clients/fake1/rollback", json={"dry_run": True}, headers=h).json()
    assert d["status"] == "dry_run" and (home / "CLAUDE.md").exists()
    assert api.post("/api/v1/clients/fake1/rollback", json={"confirm": True}, headers=h).json()["status"] == "rolled_back"
    assert not (home / "CLAUDE.md").exists()
    audit = json.dumps(api.get("/api/v1/audit", headers=h).json())
    assert "rollback" in audit
    # CLI
    do_apply(hub)
    monkeypatch.setenv("HUB_API_KEY", admin_key)
    assert cli(["rollback", "--client", "fake1"], hub=hub) == 1                         # needs --yes
    capsys.readouterr()
    assert cli(["rollback", "--client", "fake1", "--dry-run"], hub=hub) == 0 and (home / "CLAUDE.md").exists()
    capsys.readouterr()
    assert cli(["--json", "rollback", "--client", "fake1", "--yes"], hub=hub) == 0
    assert json.loads(capsys.readouterr().out)["status"] == "rolled_back"
    assert not (home / "CLAUDE.md").exists()
    assert cli(["applies", "--client", "fake1"], hub=hub) == 0 and "rolled back" in capsys.readouterr().out
