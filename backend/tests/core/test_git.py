from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from agent_hub.domain.errors import BadRequest, Conflict, ProblemError
from agent_hub.services.gitrepo import EMPTY_TREE, GitRepo
from agent_hub.services.hub import Hub


def test_pending_changes_have_diffs(hub: Hub) -> None:
    hub.work.put("instruction", "one", {"title": "One", "body": "first\n"})
    ch = hub.changes()
    assert ch["count"] == 1 and ch["items"][0]["path"] == "instructions/one.md" and ch["items"][0]["status"] == "added"
    assert "+first" in ch["items"][0]["diff"]
    hub.commit("add one", "t")
    hub.work.put("instruction", "one", {"title": "One", "body": "second\n"})
    ch = hub.changes()
    assert ch["items"][0]["status"] == "modified" and "+second" in ch["items"][0]["diff"] and "-first" in ch["items"][0]["diff"]


def test_commit_uses_fixed_identity_not_global(hub: Hub, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    gc = tmp_path / "gitconfig"
    gc.write_text("[user]\n\tname = Somebody Else\n\temail = else@example.com\n[commit]\n\tgpgsign = true\n")
    monkeypatch.setenv("GIT_CONFIG_GLOBAL", str(gc))
    hub.work.put("instruction", "one", {"title": "One", "body": "x\n"})
    sha = hub.commit("msg", "t")["commit"]
    who = subprocess.run(["git", "log", "-1", "--format=%an <%ae> | %cn"], cwd=hub.cfg.content_dir, capture_output=True, text=True,
                         check=True).stdout.strip()
    assert who == "Agent Hub <agent-hub@localhost> | Agent Hub" and sha


def test_hooks_in_the_content_repo_are_refused(hub: Hub) -> None:
    hooks = hub.cfg.content_dir / ".git" / "hooks"
    (hooks / "pre-commit").write_text("#!/bin/sh\ntouch /tmp/hub-hook-ran\nexit 0\n")
    (hooks / "pre-commit").chmod(0o755)
    hub.work.put("instruction", "one", {"title": "One", "body": "x\n"})
    with pytest.raises(ProblemError) as ei:
        hub.commit("msg", "t")
    assert ei.value.code == "content_dir_unsafe" and "hooks" in ei.value.detail
    assert not Path("/tmp/hub-hook-ran").exists()


def test_discard_all_and_paths(hub: Hub) -> None:
    hub.work.put("instruction", "keep", {"title": "K", "body": "k\n"})
    hub.commit("keep", "t")
    hub.work.put("instruction", "keep", {"title": "K", "body": "changed\n"})
    hub.work.put("instruction", "new1", {"title": "N", "body": "n\n"})
    hub.work.put("instruction", "new2", {"title": "N", "body": "n\n"})
    hub.discard(["instructions/new1.md"], "t")
    assert "new1" not in hub.work.names("instruction") and "new2" in hub.work.names("instruction")
    hub.discard(None, "t", confirm=True)
    assert hub.repo.is_clean() and hub.work.load("instruction", "keep").body == "k\n"


def test_discard_rejects_hostile_paths(hub: Hub) -> None:
    for bad in (["../x"], ["/etc"], [".git/config"], ["--all"]):
        with pytest.raises(BadRequest):
            hub.discard(bad, "t")


def test_history_and_revert(hub: Hub) -> None:
    hub.work.put("instruction", "one", {"title": "One", "body": "v1\n"})
    c1 = hub.commit("v1", "t")["commit"]
    hub.work.put("instruction", "one", {"title": "One", "body": "v2\n"})
    c2 = hub.commit("v2", "t")["commit"]
    assert [h["message"] for h in hub.history()][:2] == ["v2", "v1"]
    with pytest.raises(Conflict):
        hub.work.put("instruction", "dirty", {"title": "D", "body": "d\n"})
        hub.revert(c2, "t")                       # dirty tree refuses
    hub.discard(None, "t", confirm=True)
    hub.revert(c2, "t")
    assert hub.work.load("instruction", "one").body == "v1\n"
    with pytest.raises(BadRequest):
        hub.revert("not-a-hash!", "t")
    with pytest.raises(BadRequest):
        hub.revert("deadbeefdeadbeef", "t")
    assert c1 != c2


def test_empty_repo_hash_and_nothing_to_commit(cfg, registry) -> None:  # type: ignore[no-untyped-def]
    repo = GitRepo(cfg.content_dir, "git", "n", "e@x")
    repo.init()
    assert repo.head_tree() == EMPTY_TREE and repo.history() == []
    with pytest.raises(BadRequest):
        repo.commit("   ")
    with pytest.raises(Conflict):
        repo.commit("empty")


def test_commit_refuses_invalid_content(hub: Hub) -> None:
    from agent_hub.domain.errors import Unprocessable
    (hub.cfg.content_dir / "agents" / "broken.md").write_text("---\nbogus: 1\n---\n")
    with pytest.raises(Unprocessable):
        hub.commit("bad", "t")


def test_snapshot_is_head_not_working_tree(hub: Hub) -> None:
    hub.work.put("instruction", "one", {"title": "One", "body": "v1\n"})
    hub.commit("v1", "t")
    hub.work.put("instruction", "one", {"title": "One", "body": "edited\n"})
    tree, store = hub.approved()
    assert store.load("instruction", "one").body == "v1\n" and tree == hub.repo.head_tree()
