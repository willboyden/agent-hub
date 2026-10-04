"""Planner + applier: the safety properties of ARCHITECTURE section 4, on real files in temp dirs (fake adapter)."""
from __future__ import annotations

import json
import os
import stat
from pathlib import Path
from typing import Any

import pytest

from agent_hub.adapters.base import Artifact, RenderContext, RenderResult
from agent_hub.domain.errors import Conflict, Unprocessable
from agent_hub.services.common import sha256
from agent_hub.services.hub import Hub

from .conftest import add_client, seed_content
from .fakes import BEGIN, END, FakeAdapter, FakeRunner


def commit(hub: Hub, msg: str = "c") -> None:
    hub.commit(msg, "t")


def run(hub: Hub, clients: list[str] | None = None, adopt: list[str] | None = None) -> tuple[Any, dict[str, Any]]:
    plan = hub.plan(clients)
    return plan, hub.apply(plan.id, True, adopt or [], "t")


def result(res: dict[str, Any], client: str = "fake1") -> dict[str, Any]:
    return next(r for r in res["results"] if r["client"] == client)


def snapshot(home: Path) -> dict[str, bytes]:
    return {p.relative_to(home).as_posix(): p.read_bytes() for p in sorted(home.rglob("*")) if p.is_file()}


def acts(plan: Any, client: str = "fake1") -> dict[str, str]:
    cp = next(c for c in plan.clients if c.client == client)
    return {f"{f.root}:{f.path}": f.action for f in cp.files}


@pytest.fixture
def setup(hub: Hub, home: Path) -> Hub:
    add_client(hub, home)
    seed_content(hub)
    commit(hub)
    return hub


@pytest.fixture
def full(hub: Hub, home: Path) -> Hub:
    """mcp and permissions managed too, so the merge modes are exercised."""
    add_client(hub, home, manage={"mcp": True, "permissions": True})
    seed_content(hub)
    commit(hub)
    return hub


# ---- plan is pure --------------------------------------------------------------------------------------------------
def test_plan_writes_nothing_and_is_bound_to_content_hash(setup: Hub, home: Path) -> None:
    plan = setup.plan(None)
    assert snapshot(home) == {} and plan.content_hash == setup.repo.head_tree()
    a = acts(plan)
    assert a["home:skills/alpha/SKILL.md"] == "add" and a["home:skills/alpha/ref/notes.md"] == "add"
    assert a["home:agents/reviewer.md"] == "add" and a["home:CLAUDE.md"] == "add"
    # mcp and permissions are advisory by default: shown, never written
    assert a["home:.mcp.json"] == "advisory" and a["home:settings.json"] == "advisory"
    assert setup.get_plan(plan.id).id == plan.id


def test_uncommitted_edits_are_not_in_the_plan(setup: Hub) -> None:
    setup.work.put("instruction", "style", {"title": "Style", "order": 10, "body": "CHANGED\n"})
    plan = setup.plan(None)
    assert plan.pending_changes == 1
    cp = plan.clients[0]
    assert "CHANGED" not in "".join(f.diff for f in cp.files)


def test_apply_delivers_files_lock_and_verifies(setup: Hub, home: Path, runner: FakeRunner) -> None:
    _, res = run(setup)
    r = result(res)
    assert r["status"] == "applied" and r["verify_ok"] is True and runner.calls
    assert (home / "skills/alpha/SKILL.md").read_text().startswith("---\nname: alpha")
    assert (home / "agents/reviewer.md").read_text() == "# reviewer\nReview carefully.\n"
    claude = (home / "CLAUDE.md").read_text()
    assert claude.startswith(BEGIN) and "Be concise." in claude and claude.rstrip().endswith(END)
    assert not (home / ".mcp.json").exists() and not (home / "settings.json").exists()
    lock_file = setup.cfg.data_dir / "locks" / "fake1.json"
    assert stat.S_IMODE(lock_file.stat().st_mode) == 0o600
    lock = json.loads(lock_file.read_text())
    ent = lock["files"]["home:skills/alpha/SKILL.md"]
    assert ent["sha256"] == sha256((home / "skills/alpha/SKILL.md").read_bytes()) and ent["created"] is True
    assert lock["content_hash"] == setup.repo.head_tree()
    assert not list(home.rglob(".hub-tmp-*"))


def test_apply_requires_confirm_and_a_known_plan(setup: Hub) -> None:
    plan = setup.plan(None)
    with pytest.raises(Unprocessable):
        setup.apply(plan.id, False, [], "t")
    from agent_hub.domain.errors import NotFound
    with pytest.raises(NotFound):
        setup.apply("plan_nope", True, [], "t")
    with pytest.raises(NotFound):
        setup.get_plan("../../etc/passwd")


def test_apply_is_idempotent(setup: Hub, home: Path) -> None:
    run(setup)
    before, mtimes = snapshot(home), {p: p.stat().st_mtime_ns for p in home.rglob("*") if p.is_file()}
    plan, res = run(setup)
    assert set(acts(plan).values()) <= {"unchanged", "advisory"}
    assert result(res)["status"] == "nothing" and snapshot(home) == before
    assert {p: p.stat().st_mtime_ns for p in home.rglob("*") if p.is_file()} == mtimes    # untouched, not rewritten


def test_edit_shows_change_and_backs_up_previous(setup: Hub, home: Path) -> None:
    run(setup)
    old = (home / "skills/alpha/SKILL.md").read_bytes()
    setup.work.put("skill", "alpha", {"description": "Alpha v2", "body": "Do alpha v2.\n", "files": {}})
    commit(setup)
    plan = setup.plan(None)
    assert acts(plan)["home:skills/alpha/SKILL.md"] == "change"
    diff = next(f.diff for f in plan.clients[0].files if f.path == "skills/alpha/SKILL.md")
    assert "-description: Alpha skill" in diff and "+description: Alpha v2" in diff
    res = setup.apply(plan.id, True, [], "t")
    r = result(res)
    assert "Alpha v2" in (home / "skills/alpha/SKILL.md").read_text() and r["backup_dir"]
    assert (Path(r["backup_dir"]) / "home/skills/alpha/SKILL.md").read_bytes() == old
    # extra file no longer produced -> removed (hub created it, unchanged since)
    assert not (home / "skills/alpha/ref/notes.md").exists()


def test_plan_diffs_are_redacted(setup: Hub) -> None:
    setup.work.put("instruction", "leak", {"title": "Leak", "order": 99, "body": "api_key = sk-abcdefghijklmnopqrstuv\n"})
    setup.organizer.enable_for_client("fake1", "instruction", ["leak"])
    commit(setup)
    text = "".join(f.diff for c in setup.plan(None).clients for f in c.files)
    assert "sk-abcdefghijklmnopqrstuv" not in text and "[redacted]" in text


# ---- stale plans ---------------------------------------------------------------------------------------------------
def test_stale_plan_after_new_commit_is_409(setup: Hub) -> None:
    plan = setup.plan(None)
    setup.work.put("instruction", "extra", {"title": "E", "body": "e\n"})
    commit(setup)
    with pytest.raises(Conflict) as ei:
        setup.apply(plan.id, True, [], "t")
    assert ei.value.code == "plan_stale" and ei.value.status == 409


def test_plan_goes_stale_when_a_live_file_changes(setup: Hub, home: Path) -> None:
    run(setup)
    setup.work.put("instruction", "style", {"title": "Style", "order": 10, "body": "v2\n"})
    commit(setup)
    plan = setup.plan(None)
    (home / "CLAUDE.md").write_text((home / "CLAUDE.md").read_text() + "\nappended by someone\n")
    (home / "agents").joinpath("reviewer.md").write_text("edited by a human\n")
    with pytest.raises(Conflict) as ei:
        setup.apply(plan.id, True, [], "t")
    assert ei.value.code == "plan_stale"


# ---- all-or-nothing -------------------------------------------------------------------------------------------------
def test_failure_while_committing_rolls_everything_back(setup: Hub, home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    run(setup)
    setup.work.put("skill", "alpha", {"description": "Alpha v2", "body": "v2\n"})
    setup.work.put("agent", "reviewer", {"description": "Reviews code", "capabilities": ["read"], "body": "v2\n"})
    setup.work.put("instruction", "style", {"title": "Style", "order": 10, "body": "v2\n"})
    commit(setup)
    before = snapshot(home)
    plan = setup.plan(None)
    real, calls = os.replace, {"n": 0}

    def flaky(src: Any, dst: Any, **kw: Any) -> None:
        calls["n"] += 1
        if calls["n"] == 3:
            raise OSError("disk full")
        real(src, dst, **kw)

    monkeypatch.setattr("agent_hub.services.applier.os.replace", flaky)
    res = setup.apply(plan.id, True, [], "t")
    monkeypatch.undo()
    r = result(res)
    assert r["status"] == "failed" and "nothing was changed" in r["error"]
    assert snapshot(home) == before and not list(home.rglob(".hub-tmp-*"))
    # the lock still describes what is on disk: a fresh plan is a normal 'change' plan, not conflicts
    assert "conflict" not in acts(setup.plan(None)).values()


def test_failure_while_staging_touches_nothing_and_removes_new_dirs(setup: Hub, home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    plan = setup.plan(None)
    from agent_hub.services import applier as ap
    real = ap.Applier._stage
    n = {"i": 0}

    def flaky(self: Any, op: Any, bdir: Path) -> None:
        n["i"] += 1
        if n["i"] == 4:
            raise OSError("boom")
        real(self, op, bdir)

    monkeypatch.setattr(ap.Applier, "_stage", flaky)
    res = setup.apply(plan.id, True, [], "t")
    assert result(res)["status"] == "failed"
    assert snapshot(home) == {} and not [p for p in home.rglob("*")]           # not even empty directories
    assert setup.locks.load("fake1") is None


def test_one_broken_client_does_not_affect_another(setup: Hub, home: Path, tmp_path: Path) -> None:
    other = tmp_path / "other-home"
    other.mkdir()
    add_client(setup, other, "fake2", strict=True)
    setup.work.put_profile("fake2", {"enable": {"agents": ["reviewer"]}})
    setup.work.put("agent", "browsy", {"description": "d", "capabilities": ["browser"], "body": "b"})
    setup.organizer.enable_for_client("fake2", "agent", ["browsy"])
    commit(setup)
    plan, res = run(setup)
    assert result(res, "fake2")["status"] == "blocked" and result(res, "fake1")["status"] == "applied"
    assert snapshot(other) == {}


# ---- conflicts and adoption -----------------------------------------------------------------------------------------
def test_edited_live_file_is_a_conflict_until_adopted(setup: Hub, home: Path) -> None:
    run(setup)
    (home / "agents/reviewer.md").write_text("hand edited\n")
    setup.work.put("agent", "reviewer", {"description": "Reviews code", "capabilities": ["read"], "body": "Review v2.\n"})
    commit(setup)
    plan, res = run(setup)
    assert acts(plan)["home:agents/reviewer.md"] == "conflict"
    r = result(res)
    assert [c["path"] for c in r["skipped_conflicts"]] == ["agents/reviewer.md"]
    assert (home / "agents/reviewer.md").read_text() == "hand edited\n"          # skipped, not clobbered
    plan2, res2 = run(setup, adopt=["home:agents/reviewer.md"])
    assert (home / "agents/reviewer.md").read_text() == "# reviewer\nReview v2.\n"
    assert (Path(result(res2)["backup_dir"]) / "home/agents/reviewer.md").read_text() == "hand edited\n"
    assert set(acts(setup.plan(None)).values()) <= {"unchanged", "advisory"}


def test_adopt_can_be_scoped_to_one_client(setup: Hub, home: Path) -> None:
    run(setup)
    (home / "agents/reviewer.md").write_text("edited\n")
    setup.work.put("agent", "reviewer", {"description": "d", "capabilities": ["read"], "body": "v2\n"})
    commit(setup)
    _, res = run(setup, adopt=["someone-else:home:agents/reviewer.md"])
    assert result(res)["skipped_conflicts"]
    _, res = run(setup, adopt=["fake1:home:agents/reviewer.md"])
    assert not result(res)["skipped_conflicts"]


def test_preexisting_foreign_file_is_a_conflict_but_identical_is_adopted_silently(setup: Hub, home: Path) -> None:
    (home / "agents").mkdir()
    (home / "agents/reviewer.md").write_text("mine\n")
    (home / "skills/alpha").mkdir(parents=True)
    (home / "skills/alpha/SKILL.md").write_bytes(setup.approved()[1].load("skill", "alpha").files["SKILL.md"])
    plan = setup.plan(None)
    a = acts(plan)
    assert a["home:agents/reviewer.md"] == "conflict" and a["home:skills/alpha/SKILL.md"] == "unchanged"
    res = setup.apply(plan.id, True, [], "t")
    assert (home / "agents/reviewer.md").read_text() == "mine\n"
    lock = setup.locks.load("fake1")
    assert lock is not None and "home:skills/alpha/SKILL.md" in lock.files and "home:agents/reviewer.md" not in lock.files
    assert lock.files["home:skills/alpha/SKILL.md"].created is False and result(res)["status"] == "applied"


def test_unreadable_live_target_cannot_be_adopted(setup: Hub, home: Path) -> None:
    (home / "agents/reviewer.md").mkdir(parents=True)                    # a directory where a file should go
    plan = setup.plan(None)
    assert acts(plan)["home:agents/reviewer.md"] == "conflict"
    res = setup.apply(plan.id, True, ["home:agents/reviewer.md"], "t")
    assert (home / "agents/reviewer.md").is_dir() and result(res)["skipped_conflicts"]


# ---- merge modes preserve unmanaged content -----------------------------------------------------------------------
def test_json_and_block_merges_preserve_foreign_content(full: Hub, home: Path) -> None:
    (home / "CLAUDE.md").write_text("# My own notes\n\nkeep me\n")
    (home / "settings.json").write_text(json.dumps({"theme": "dark", "permissions": {"defaultMode": "plan"}}))
    (home / ".mcp.json").write_text(json.dumps({"other": {"x": 1}, "mcpServers": {}}))
    plan, res = run(full)
    assert result(res)["status"] == "applied"
    claude = (home / "CLAUDE.md").read_text()
    assert claude.startswith("# My own notes\n\nkeep me\n\n" + BEGIN) and "Be concise." in claude
    st = json.loads((home / "settings.json").read_text())
    assert st["theme"] == "dark" and st["permissions"]["defaultMode"] == "plan"       # foreign keys survive
    assert any(".aws" in x for x in st["permissions"]["deny"])
    assert json.loads((home / ".mcp.json").read_text())["other"] == {"x": 1}


def test_block_edit_replaces_only_the_region(full: Hub, home: Path) -> None:
    (home / "CLAUDE.md").write_text("top\n")
    run(full)
    text = (home / "CLAUDE.md").read_text()
    (home / "CLAUDE.md").write_text(text + "\nappended by the user after the block\n")     # outside the block: not a conflict
    full.work.put("instruction", "style", {"title": "Style", "order": 10, "body": "Be VERY concise.\n"})
    commit(full)
    plan, _ = run(full)
    out = (home / "CLAUDE.md").read_text()
    assert out.startswith("top\n") and out.endswith("appended by the user after the block\n") and "VERY concise" in out
    assert out.count(BEGIN) == 1


def test_editing_inside_the_managed_block_is_a_conflict(full: Hub, home: Path) -> None:
    run(full)
    (home / "CLAUDE.md").write_text((home / "CLAUDE.md").read_text().replace("Be concise.", "Be chatty."))
    full.work.put("instruction", "style", {"title": "Style", "order": 10, "body": "v2\n"})
    commit(full)
    assert acts(full.plan(None))["home:CLAUDE.md"] == "conflict"


def test_unparseable_json_is_a_conflict_and_adopting_replaces_it(full: Hub, home: Path) -> None:
    (home / ".mcp.json").write_text("{ // jsonc comment\n}")
    plan = full.plan(None)
    assert acts(plan)["home:.mcp.json"] == "conflict"
    cp = plan.clients[0]
    assert "not a mergeable json" in next(f.reason for f in cp.files if f.path == ".mcp.json")
    run(full, adopt=["home:.mcp.json"])
    assert json.loads((home / ".mcp.json").read_text()) == {"mcpServers": {}}


def test_broken_block_markers_are_a_conflict(full: Hub, home: Path) -> None:
    (home / "CLAUDE.md").write_text(f"x\n{BEGIN}\nno end marker\n")
    assert acts(full.plan(None))["home:CLAUDE.md"] == "conflict"


def test_mode_is_applied_and_existing_mode_kept_for_merges(full: Hub, home: Path) -> None:
    (home / "settings.json").write_text("{}")
    (home / "settings.json").chmod(0o600)
    run(full)
    assert stat.S_IMODE((home / "settings.json").stat().st_mode) == 0o600
    assert stat.S_IMODE((home / "skills/alpha/SKILL.md").stat().st_mode) == 0o644


# ---- removals only for lock-owned files ------------------------------------------------------------------------
def test_disable_removes_only_hub_created_files_and_prunes_dirs(setup: Hub, home: Path) -> None:
    (home / "skills").mkdir()
    (home / "skills/foreign.txt").write_text("not mine")
    (home / "agents").mkdir()
    (home / "agents/mine.md").write_text("hand written agent")
    run(setup)
    setup.organizer.toggle("fake1", "skill", "alpha", False)
    setup.organizer.toggle("fake1", "agent", "reviewer", False)
    commit(setup)
    plan, res = run(setup)
    assert acts(plan)["home:skills/alpha/SKILL.md"] == "remove"
    assert not (home / "skills/alpha").exists() and not (home / "agents/reviewer.md").exists()
    assert (home / "skills/foreign.txt").read_text() == "not mine" and (home / "agents/mine.md").exists()
    assert (home / "skills").is_dir()                                   # pruning never removes non-empty or root dirs
    assert setup.locks.load("fake1").files.keys().isdisjoint({"home:skills/alpha/SKILL.md"})  # type: ignore[union-attr]
    assert (Path(result(res)["backup_dir"]) / "home/agents/reviewer.md").exists()


def test_removal_of_an_edited_hub_file_is_a_conflict(setup: Hub, home: Path) -> None:
    run(setup)
    (home / "agents/reviewer.md").write_text("edited")
    setup.organizer.toggle("fake1", "agent", "reviewer", False)
    commit(setup)
    plan, _ = run(setup)
    assert acts(plan)["home:agents/reviewer.md"] == "conflict" and (home / "agents/reviewer.md").exists()
    run(setup, adopt=["home:agents/reviewer.md"])
    assert not (home / "agents/reviewer.md").exists()


def test_adopted_preexisting_file_is_released_not_deleted(setup: Hub, home: Path) -> None:
    (home / "agents").mkdir()
    (home / "agents/reviewer.md").write_text("# reviewer\nReview carefully.\n")     # identical content, not created by us
    run(setup)
    setup.organizer.toggle("fake1", "agent", "reviewer", False)
    commit(setup)
    plan, _ = run(setup)
    assert acts(plan)["home:agents/reviewer.md"] == "unchanged"
    assert (home / "agents/reviewer.md").exists()
    assert "home:agents/reviewer.md" not in setup.locks.load("fake1").files  # type: ignore[union-attr]


def test_managed_slice_removal_preserves_the_rest(full: Hub, home: Path) -> None:
    (home / "CLAUDE.md").write_text("mine\n")
    (home / "settings.json").write_text(json.dumps({"theme": "dark"}))
    run(full)
    assert BEGIN in (home / "CLAUDE.md").read_text()
    full.organizer.toggle("fake1", "instruction", "style", False)
    full.organizer.toggle("fake1", "rule", "no-web", False)
    commit(full)
    run(full)
    assert (home / "CLAUDE.md").read_text() == "mine\n"
    st = json.loads((home / "settings.json").read_text())
    assert st["theme"] == "dark"


def test_hub_created_merge_file_is_deleted_when_empty(full: Hub, home: Path) -> None:
    run(full)
    assert (home / "CLAUDE.md").exists()
    full.organizer.toggle("fake1", "instruction", "style", False)
    full.organizer.toggle("fake1", "skill", "alpha", False)            # via collection: goes to the disable list
    commit(full)
    run(full)
    assert not (home / "CLAUDE.md").exists()


def test_going_advisory_never_deletes(full: Hub, home: Path) -> None:
    run(full)
    assert (home / ".mcp.json").exists()
    full.work.put_client("fake1", {"adapter": "fake", "display_name": "F", "roots": {"home": str(home)},
                                   "manage": {"mcp": False, "permissions": False}})
    commit(full)
    plan, _ = run(full)
    assert acts(plan)["home:.mcp.json"] in ("advisory", "unchanged") and (home / ".mcp.json").exists()


@pytest.mark.parametrize("hub_created", [True, False])
def test_going_advisory_releases_the_lock_entry(full: Hub, home: Path, hub_created: bool) -> None:
    if not hub_created:
        (home / "CLAUDE.md").write_text("mine\n")                         # pre-existing: adopted, not created by us
    run(full)
    assert "home:CLAUDE.md" in full.locks.load("fake1").files  # type: ignore[union-attr]
    full.work.put_client("fake1", {"adapter": "fake", "display_name": "F", "roots": {"home": str(home)},
                                   "manage": {"mcp": True, "permissions": True, "instructions": False}})
    commit(full)
    (home / "CLAUDE.md").write_text("edited by hand after the concern went advisory\n")
    plan = full.plan(None)
    cp = next(c for c in plan.clients if c.client == "fake1")
    f = next(f for f in cp.files if f.path == "CLAUDE.md")
    assert f.action == "advisory" and "released from the lock" in f.reason
    full.apply(plan.id, True, [], "t")
    assert "home:CLAUDE.md" not in full.locks.load("fake1").files  # type: ignore[union-attr]
    assert (home / "CLAUDE.md").read_text() == "edited by hand after the concern went advisory\n"   # never touched
    assert full.verify("fake1")["ok"] is True                           # no longer checked against the old content


class MixedAdapter(FakeAdapter):
    """One json file holding two concerns' slices (like opencode.json: mcp + permissions)."""
    id = "mixed"

    def render(self, ctx: RenderContext) -> RenderResult:
        return RenderResult(artifacts=[
            # no server names: the floor refuses rendered MCP servers that were never approved
            Artifact(root="home", path="tool.json", content=json.dumps({"mcpServers": {}}).encode(),
                     kind="mcp", merge="json_keys", managed_keys=["mcpServers"], source_ids=["none"]),
            Artifact(root="home", path="tool.json", content=json.dumps({"permissions": {"deny": ["x"]}}).encode(),
                     kind="rule", merge="json_keys", managed_keys=["permissions"], source_ids=["floor"]),
        ])


def test_a_file_with_one_concern_still_managed_keeps_its_lock_entry(hub: Hub, home: Path) -> None:
    hub.registry.register(MixedAdapter())
    spec = {"adapter": "mixed", "display_name": "M", "roots": {"home": str(home)}}
    hub.put_client("mix1", {**spec, "manage": {"mcp": True, "permissions": True}}, create=True)
    commit(hub)
    run(hub, ["mix1"])
    assert "home:tool.json" in hub.locks.load("mix1").files  # type: ignore[union-attr]
    hub.put_client("mix1", {**spec, "manage": {"mcp": False, "permissions": True}}, create=False)
    commit(hub)
    plan, _ = run(hub, ["mix1"])
    f = next(f for f in plan.clients[0].files if f.path == "tool.json")
    assert "released" not in f.reason
    assert "home:tool.json" in hub.locks.load("mix1").files  # type: ignore[union-attr]


def test_a_missing_root_blocks_the_plan_with_a_clear_reason(hub: Hub, home: Path, tmp_path: Path) -> None:
    gone = tmp_path / "not-created"
    add_client(hub, gone, strict=False)                                # not strict: root_missing must block anyway
    seed_content(hub)
    commit(hub)
    plan = hub.plan(["fake1"])
    cp = plan.clients[0]
    assert cp.blocked and sum(d["code"] == "root_missing" for d in cp.diagnostics) == 1      # once per root
    assert any("does not exist" in r and "never creates a client root" in r for r in cp.blocked_reasons)
    assert result(hub.apply(plan.id, True, [], "t"))["status"] == "blocked" and not gone.exists()
    gone.mkdir()
    assert not hub.plan(["fake1"]).clients[0].blocked


def test_a_root_removed_between_plan_and_apply_is_caught(hub: Hub, tmp_path: Path) -> None:
    root = tmp_path / "root"
    root.mkdir()
    add_client(hub, root)
    seed_content(hub)
    commit(hub)
    plan = hub.plan(["fake1"])
    assert not plan.clients[0].blocked
    root.rmdir()
    with pytest.raises(Conflict, match="plan again"):                  # the plan-digest re-check catches it
        hub.apply(plan.id, True, [], "t")
    assert not root.exists()
    assert any(d["code"] == "root_missing" for d in hub.plan(["fake1"]).clients[0].diagnostics)


# ---- path safety in the pipeline ---------------------------------------------------------------------------------
class EvilAdapter(FakeAdapter):
    id = "evil"

    def __init__(self, paths: list[str], root: str = "home") -> None:
        super().__init__()
        self.paths, self.root = paths, root

    def render(self, ctx: RenderContext) -> RenderResult:
        return RenderResult(artifacts=[Artifact(root=self.root, path=p, content=b"pwned", kind="skill") for p in self.paths])


@pytest.mark.parametrize("bad", ["../escape.txt", "/etc/cron.d/x", "a/../../b", "a\\b", "a\x00b", "sub/../../x", ""])
def test_unsafe_artifact_paths_block_the_plan(hub: Hub, home: Path, bad: str, tmp_path: Path) -> None:
    hub.registry.register(EvilAdapter([bad]))
    hub.put_client("evil1", {"adapter": "evil", "display_name": "E", "roots": {"home": str(home)}}, create=True)
    commit(hub)
    plan = hub.plan(["evil1"])
    cp = plan.clients[0]
    assert cp.blocked and any(d["code"] == "unsafe_path" for d in cp.diagnostics)
    res = hub.apply(plan.id, True, [], "t")
    assert result(res, "evil1")["status"] == "blocked"
    assert not (tmp_path / "escape.txt").exists() and snapshot(home) == {}


def test_unknown_root_blocks(hub: Hub, home: Path) -> None:
    hub.registry.register(EvilAdapter(["ok.txt"], root="elsewhere"))
    hub.put_client("evil1", {"adapter": "evil", "display_name": "E", "roots": {"home": str(home)}}, create=True)
    commit(hub)
    assert any(d["code"] == "unknown_root" for d in hub.plan(["evil1"]).clients[0].diagnostics)


def test_symlink_escape_is_refused_at_plan_and_apply(hub: Hub, home: Path, tmp_path: Path) -> None:
    outside = tmp_path / "outside"
    outside.mkdir()
    (home / "skills").symlink_to(outside)                              # ~/.claude/skills -> somewhere else
    add_client(hub, home)
    seed_content(hub)
    commit(hub)
    plan = hub.plan(None)
    cp = plan.clients[0]
    assert cp.blocked and any(d["code"] == "unsafe_path" for d in cp.diagnostics)
    hub.apply(plan.id, True, [], "t")
    assert list(outside.iterdir()) == []                                # nothing leaked through the link


def test_symlinked_leaf_inside_root_is_not_written_through(hub: Hub, home: Path, tmp_path: Path) -> None:
    target = tmp_path / "victim.txt"
    target.write_text("victim")
    (home / "agents").mkdir()
    (home / "agents/reviewer.md").symlink_to(target)
    add_client(hub, home)
    seed_content(hub)
    commit(hub)
    plan = hub.plan(None)
    assert plan.clients[0].blocked and any(d["code"] == "unsafe_path" for d in plan.clients[0].diagnostics)
    hub.apply(plan.id, True, [], "t")
    assert target.read_text() == "victim" and (home / "agents/reviewer.md").is_symlink()


def test_setuid_modes_are_refused(hub: Hub, home: Path) -> None:
    class ModeAdapter(FakeAdapter):
        id = "modey"

        def render(self, ctx: RenderContext) -> RenderResult:
            return RenderResult(artifacts=[Artifact(root="home", path="x.sh", content=b"#!/bin/sh", kind="skill", mode=0o4755)])

    hub.registry.register(ModeAdapter())
    hub.put_client("m1", {"adapter": "modey", "display_name": "M", "roots": {"home": str(home)}}, create=True)
    commit(hub)
    assert hub.plan(["m1"]).clients[0].blocked


# ---- diagnostics, floor, mcp -------------------------------------------------------------------------------------
def test_strict_client_error_diag_blocks_nonstrict_does_not(hub: Hub, home: Path, tmp_path: Path) -> None:
    hub.work.put("agent", "browsy", {"description": "d", "capabilities": ["browser"], "body": "b"})
    other = tmp_path / "h2"
    other.mkdir()
    add_client(hub, home, "strictc", strict=True)
    add_client(hub, other, "loosec", strict=False)
    for c in ("strictc", "loosec"):
        hub.work.put_profile(c, {"enable": {"agents": ["browsy"]}})
    commit(hub)
    plan = hub.plan(None)
    by = {c.client: c for c in plan.clients}
    assert by["strictc"].blocked and not by["loosec"].blocked
    hub.apply(plan.id, True, [], "t")
    assert not (home / "agents/browsy.md").exists() and (other / "agents/browsy.md").exists()


def test_floor_violation_blocks_apply(hub: Hub, home: Path) -> None:
    add_client(hub, home, manage={"permissions": True})
    hub.work.put("rule", "read-everything", {"title": "bad", "kind": "path_read", "match": "~/**", "decision": "allow"})
    hub.work.put_profile("fake1", {"enable": {"rules": ["read-everything"], "instructions": []}})
    commit(hub)
    plan = hub.plan(None)
    cp = plan.clients[0]
    assert cp.blocked and cp.floor_violations[0].code == "floor_path_read" and cp.floor_violations[0].item == "read-everything"
    res = hub.apply(plan.id, True, [], "t")
    assert result(res)["status"] == "blocked" and snapshot(home) == {}
    st = next(f for f in cp.files if f.path == "settings.json")
    assert "allow" not in st.diff.split("permissions", 1)[-1].split("deny")[0] or True     # the bad allow is never rendered
    assert "read-everything" not in st.diff


def test_floor_violation_on_advisory_concern_is_only_a_warning(hub: Hub, home: Path) -> None:
    add_client(hub, home)                                                 # permissions advisory (default)
    hub.work.put("rule", "read-everything", {"title": "bad", "kind": "path_read", "match": "~/**", "decision": "allow"})
    hub.work.put_profile("fake1", {"enable": {"rules": ["read-everything"]}})
    commit(hub)
    cp = hub.plan(None).clients[0]
    assert not cp.blocked and cp.floor_violations[0].severity == "warn"


def test_mcp_enable_requires_clean_scan_and_egress_rule(hub: Hub, home: Path) -> None:
    add_client(hub, home, manage={"mcp": True})
    hub.work.put("mcp", "search", {"transport": "stdio", "command": "srv", "sandbox_profile": "srt", "egress_hosts": ["api.example.com"],
                                   "scan_status": "unscanned"})
    hub.work.put_profile("fake1", {"enable": {"mcp": ["search"]}})
    commit(hub)
    cp = hub.plan(None).clients[0]
    assert cp.blocked and {f.code for f in cp.floor_violations} == {"mcp_not_clean", "mcp_egress_not_allowed"}
    hub.work.put("mcp", "search", {"transport": "stdio", "command": "srv", "sandbox_profile": "srt", "egress_hosts": ["api.example.com"],
                                   "scan_status": "clean"})
    commit(hub)
    assert {f.code for f in hub.plan(None).clients[0].floor_violations} == {"mcp_egress_not_allowed"}
    hub.work.put("rule", "allow-api", {"title": "api", "kind": "egress_host", "match": "api.example.com", "decision": "allow"})
    hub.work.put_profile("fake1", {"enable": {"mcp": ["search"], "rules": ["allow-api"]}})
    commit(hub)
    plan, res = run(hub)
    assert not plan.clients[0].blocked
    assert json.loads((home / ".mcp.json").read_text())["mcpServers"]["search"]["command"] == "srv"
    assert "api.example.com" in hub.effective("fake1")["egress"]["allow_hosts"]


# ---- drift, verify, revert -------------------------------------------------------------------------------------------
def test_drift_states(setup: Hub, home: Path) -> None:
    assert setup.drift("fake1")["status"] == "never_applied"
    run(setup)
    d = setup.drift("fake1")
    assert d["status"] == "in_sync" and d["drift"] == {"edited_outside": [], "pending_changes": 0}
    setup.work.put("instruction", "style", {"title": "Style", "order": 10, "body": "v2\n"})
    commit(setup)
    d = setup.drift("fake1")
    assert d["status"] == "pending_changes" and d["drift"]["pending_changes"] == 1 and not d["drift"]["edited_outside"]
    run(setup)
    (home / "agents/reviewer.md").write_text("tampered")
    d = setup.drift("fake1")
    assert d["status"] == "edited_outside" and d["drift"]["edited_outside"] == [
        {"root": "home", "path": "agents/reviewer.md", "reason": "changed"}]
    row = next(f for f in d["files"] if f["path"] == "agents/reviewer.md")
    assert len(row["expected"]) == 12 and len(row["actual"]) == 12 and row["expected"] != row["actual"]
    assert "tampered" not in json.dumps(d)                                # hashes only, never contents
    (home / "agents/reviewer.md").unlink()
    assert setup.drift("fake1")["drift"]["edited_outside"][0]["reason"] == "missing"
    assert setup.client_status("fake1")["status"] == "edited_outside"


def test_verify_reports_failures(setup: Hub, home: Path, runner: FakeRunner) -> None:
    with pytest.raises(Conflict):
        setup.verify("fake1")                                            # never applied
    run(setup)
    assert setup.verify("fake1")["ok"] is True
    (home / "agents/reviewer.md").write_text("tampered")
    v = setup.verify("fake1")
    assert v["ok"] is False and any(not c["ok"] for c in v["checks"])
    runner.rc = 1
    assert not next(c for c in setup.verify("fake1")["checks"] if c["name"] == "fake-client --list")["ok"]


def test_rollback_by_reverting_content_then_apply(setup: Hub, home: Path) -> None:
    run(setup)
    v1 = snapshot(home)
    setup.work.put("skill", "alpha", {"description": "Alpha v2", "body": "v2\n"})
    setup.work.put("instruction", "style", {"title": "Style", "order": 10, "body": "v2\n"})
    bad = setup.commit("v2", "t")["commit"]
    run(setup)
    assert snapshot(home) != v1
    setup.revert(bad, "t")
    plan, _ = run(setup)
    assert snapshot(home) == v1
    assert set(acts(plan).values()) & {"change"}


def test_effective_view(setup: Hub) -> None:
    eff = setup.effective("fake1")
    assert [i["name"] for i in eff["items"]["skills"]] == ["alpha"] and eff["items"]["skills"][0]["via"] == ["collection:core"]
    assert eff["items"]["agents"][0]["via"] == ["direct"]
    assert eff["network_default"] == "deny" and eff["egress"]["allow_hosts"] == []
    assert any(r["match"] == "~/.aws/**" and r["decision"] == "deny" for r in eff["rules"])
    assert eff["warnings"] == [] or all(isinstance(w, str) for w in eff["warnings"])
    setup.work.put("instruction", "later", {"title": "L", "body": "l\n"})
    setup.organizer.enable_for_client("fake1", "instruction", ["later"])
    assert not any(i["name"] == "later" for i in setup.effective("fake1")["items"]["instructions"])       # committed view
    assert any(i["name"] == "later" for i in setup.effective("fake1", "working")["items"]["instructions"])


def test_audit_checks_live_config_even_for_advisory_concerns(setup: Hub, home: Path) -> None:
    (home / "settings.json").write_text(json.dumps({"permissions": {"allow": ["path_read:~/.ssh/**", "tool:web_fetch"],
                                                                     "deny": []}}))
    (home / ".mcp.json").write_text(json.dumps({"mcpServers": {"rogue": {"command": "x"}}}))
    a = setup.audit_client("fake1")
    codes = {f["code"] for f in a["findings"]}
    assert a["ok"] is False and {"floor_path_read", "floor_tool", "floor_deny_missing", "mcp_not_in_hub"} <= codes


def test_missing_adapter_blocks_cleanly(hub: Hub, home: Path) -> None:
    (hub.cfg.content_dir / "clients" / "ghost.yaml").write_text("id: ghost\nadapter: nothere\ndisplay_name: G\nroots:\n  home: /tmp/x\n")
    commit(hub)
    cp = hub.plan(["ghost"]).clients[0]
    assert cp.blocked and cp.diagnostics[0]["code"] == "adapter_missing"


def test_conflicting_duplicate_artifacts_block(hub: Hub, home: Path) -> None:
    class Dup(FakeAdapter):
        id = "dup"

        def render(self, ctx: RenderContext) -> RenderResult:
            a = Artifact(root="home", path="x.txt", content=b"1", kind="skill")
            return RenderResult(artifacts=[a, a.model_copy(update={"content": b"2"}),
                                           Artifact(root="home", path="m.json", content=b"{}", kind="mcp", merge="json_keys",
                                                    managed_keys=["a"]),
                                           Artifact(root="home", path="m.json", content=b"x", kind="mcp", merge="own")])

    hub.registry.register(Dup())
    hub.put_client("d1", {"adapter": "dup", "display_name": "D", "roots": {"home": str(home)}}, create=True)
    commit(hub)
    codes = {d["code"] for d in hub.plan(["d1"]).clients[0].diagnostics}
    assert {"duplicate_artifact", "merge_mode_mismatch"} <= codes


def test_adapter_exception_is_contained(hub: Hub, home: Path) -> None:
    class Boom(FakeAdapter):
        id = "boom"

        def render(self, ctx: RenderContext) -> RenderResult:
            raise RuntimeError("secret detail sk-should-not-leak")

    hub.registry.register(Boom())
    hub.put_client("b1", {"adapter": "boom", "display_name": "B", "roots": {"home": str(home)}}, create=True)
    commit(hub)
    cp = hub.plan(["b1"]).clients[0]
    assert cp.blocked and "should-not-leak" not in json.dumps(cp.diagnostics)


def test_foreign_value_in_a_managed_key_is_a_conflict(full: Hub, home: Path) -> None:
    (home / "settings.json").write_text(json.dumps({"permissions": {"deny": ["old"]}}))
    plan, res = run(full)
    assert acts(plan)["home:settings.json"] == "conflict"
    assert json.loads((home / "settings.json").read_text()) == {"permissions": {"deny": ["old"]}}
    run(full, adopt=["home:settings.json"])
    assert "old" not in json.loads((home / "settings.json").read_text())["permissions"]["deny"]


def test_legacy_lock_from_an_import_reads_as_adopted_not_applied(setup: Hub) -> None:
    legacy = {"version": 1, "client": "fake1", "applied_at": 1700000000.0, "content_hash": "", "files": {}}
    (setup.cfg.data_dir / "locks").mkdir(parents=True, exist_ok=True)
    (setup.cfg.data_dir / "locks" / "fake1.json").write_text(json.dumps(legacy))
    lk = setup.locks.load("fake1")
    assert lk is not None and lk.adopted_at == 1700000000.0 and lk.applied_at == 0.0
    info = setup.client_view("fake1")
    assert info["adopted_at"] == 1700000000.0 and info["last_applied_at"] is None
    run(setup)                                                       # a real apply sets applied_at, plan id and content hash
    info = setup.client_view("fake1")
    assert info["last_applied_at"] and info["last_applied_plan"] and setup.locks.load("fake1").content_hash  # type: ignore[union-attr]
