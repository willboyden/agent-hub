"""L2-L5: loopback-only bind, private state dirs, admin-only reads, and git hardening."""
from __future__ import annotations

import os
import stat
import subprocess
import sys
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from agent_hub.config import check_bind
from agent_hub.domain.errors import Unprocessable
from agent_hub.services.hub import Hub

from .conftest import add_client, bearer, seed_content

API = "/api/v1"


# ---- L2 ---------------------------------------------------------------------------------------------------------------------
def test_check_bind(monkeypatch: pytest.MonkeyPatch) -> None:
    for ok in ("127.0.0.1", "127.0.0.2", "::1", "localhost"):
        check_bind(ok)
    for bad in ("0.0.0.0", "192.168.1.5", "::", "example.com"):
        with pytest.raises(ValueError, match="HUB_ALLOW_NONLOOPBACK"):
            check_bind(bad)
    monkeypatch.setenv("HUB_ALLOW_NONLOOPBACK", "1")
    check_bind("0.0.0.0")
    monkeypatch.setenv("HUB_ALLOW_NONLOOPBACK", "true")          # only the exact opt-in value counts
    with pytest.raises(ValueError):
        check_bind("0.0.0.0")


@pytest.mark.parametrize("entry", [["-m", "agent_hub.cli", "serve"], ["-c", "from agent_hub.main import run; run()"]])
def test_serve_and_main_refuse_a_public_bind(tmp_path: Path, entry: list[str]) -> None:
    env = {**os.environ, "HUB_HOST": "0.0.0.0", "HUB_CONTENT_DIR": str(tmp_path / "c"), "HUB_DATA_DIR": str(tmp_path / "s"),
           "AGENT_HUB_ENV_FILE": str(tmp_path / "none")}
    env.pop("HUB_ALLOW_NONLOOPBACK", None)
    p = subprocess.run([sys.executable, *entry], env=env, capture_output=True, text=True, timeout=30, check=False)
    assert p.returncode != 0 and "HUB_ALLOW_NONLOOPBACK" in (p.stderr + p.stdout)


# ---- L3 ---------------------------------------------------------------------------------------------------------------------
def modes(root: Path) -> set[int]:
    return {stat.S_IMODE(d.stat().st_mode) for d in [root, *[p for p in root.rglob("*") if p.is_dir() and not p.is_symlink()]]}


def test_state_directories_are_private(hub: Hub, home: Path) -> None:
    add_client(hub, home)
    seed_content(hub)
    hub.commit("c", "t")
    (home / "agents").mkdir()
    (home / "agents/reviewer.md").write_text("old")
    plan = hub.plan(None)
    hub.apply(plan.id, True, ["home:agents/reviewer.md"], "t")            # creates plans, snapshots, locks, backups
    for sub in ("plans", "snapshots", "locks", "backups"):
        assert (hub.cfg.data_dir / sub).is_dir(), sub
    assert modes(hub.cfg.data_dir) == {0o700}
    assert stat.S_IMODE((home / "skills").stat().st_mode) == 0o755          # client dirs keep normal modes


def test_existing_loose_state_dirs_are_tightened_on_startup(cfg, registry, tmp_path) -> None:  # type: ignore[no-untyped-def]
    for d in ("plans", "backups/c1/2026", "snapshots/abc/sub"):
        (cfg.data_dir / d).mkdir(parents=True)
        (cfg.data_dir / d).chmod(0o755)
    cfg.data_dir.chmod(0o755)
    Hub(cfg, registry=registry).close()
    assert modes(cfg.data_dir) == {0o700}


# ---- L4 ---------------------------------------------------------------------------------------------------------------------
def test_viewers_cannot_read_credential_listing_knowledge_paths(cfg, hub: Hub) -> None:  # type: ignore[no-untyped-def]
    import httpx

    from agent_hub.main import create_app
    from agent_hub.services.knowledge import KnowledgeProxy
    hub.knowledge = KnowledgeProxy(cfg, transport=httpx.MockTransport(lambda r: httpx.Response(200, json={})))
    with TestClient(create_app(cfg, hub), base_url="http://127.0.0.1:8792") as c:
        v = bearer(str(hub.auth.create_key("v", "viewer")["secret"]))
        a = bearer(str(hub.auth.create_key("a", "admin")["secret"]))
        for p in ("tokens", "indexes", "backends", "tokens/x"):
            assert c.get(f"{API}/knowledge/{p}", headers=v).status_code == 403, p
            assert c.get(f"{API}/knowledge/{p}", headers=a).status_code == 200, p
        assert c.get(f"{API}/knowledge/namespaces", headers=v).status_code == 200
        assert c.get(f"{API}/knowledge/jobs", headers=v).status_code == 200


# ---- L5 ---------------------------------------------------------------------------------------------------------------------
def test_discard_all_needs_confirm_and_supports_dry_run(hub: Hub) -> None:
    hub.work.put("instruction", "a", {"title": "A", "body": "a\n"})
    hub.work.put("instruction", "b", {"title": "B", "body": "b\n"})
    with pytest.raises(Unprocessable) as ei:
        hub.discard(None, "t")
    assert ei.value.code == "confirm_required" and set(ei.value.extra["would_discard"]) == {"instructions/a.md", "instructions/b.md"}
    assert hub.changes()["count"] == 2
    assert set(hub.discard(None, "t", dry_run=True)["would_discard"]) == {"instructions/a.md", "instructions/b.md"}
    assert hub.changes()["count"] == 2                                    # dry run deleted nothing
    assert hub.discard(["instructions/a.md"], "t")["discarded"] == ["instructions/a.md"]      # explicit paths need no confirm
    hub.discard(None, "t", confirm=True)
    assert hub.changes()["count"] == 0


def test_discard_over_http(api: TestClient, hub: Hub, admin_key: str) -> None:
    h = bearer(admin_key)
    hub.work.put("instruction", "a", {"title": "A", "body": "a\n"})
    assert api.post(f"{API}/changes/discard", json={}, headers=h).status_code == 422
    assert api.post(f"{API}/changes/discard", json={"dry_run": True}, headers=h).json()["would_discard"] == ["instructions/a.md"]
    assert api.post(f"{API}/changes/discard", json={"confirm": True}, headers=h).status_code == 200


def test_commit_stages_only_known_content_paths_and_reports_strays(hub: Hub) -> None:
    hub.work.put("instruction", "a", {"title": "A", "body": "a\n"})
    (hub.cfg.content_dir / "stray.txt").write_text("not content")
    (hub.cfg.content_dir / "notes").mkdir()
    (hub.cfg.content_dir / "notes/todo.md").write_text("x")
    (hub.cfg.content_dir / "inbox").mkdir(exist_ok=True)
    (hub.cfg.content_dir / "inbox/c1").mkdir(exist_ok=True)
    (hub.cfg.content_dir / "inbox/c1/x.md").write_text("proposal")
    out = hub.commit("only content", "t")
    assert set(out["strays"]) == {"stray.txt", "notes/todo.md", "inbox/c1/x.md"}
    tracked = set(hub.repo.text("ls-files").split())
    assert "instructions/a.md" in tracked and not tracked & {"stray.txt", "notes/todo.md", "inbox/c1/x.md"}
    from agent_hub.domain.errors import Conflict
    with pytest.raises(Conflict) as ei:
        hub.commit("only strays left", "t")                              # nothing STAGEABLE, strays alone never commit
    assert ei.value.code == "nothing_to_commit"


def test_deletions_under_known_paths_are_committed(hub: Hub) -> None:
    hub.work.put("instruction", "a", {"title": "A", "body": "a\n"})
    hub.commit("add", "t")
    hub.work.delete("instruction", "a")
    hub.commit("remove", "t")
    assert "instructions/a.md" not in hub.repo.text("ls-files")


def test_pathspecs_are_literal(hub: Hub) -> None:
    hub.work.put("instruction", "a", {"title": "A", "body": "a\n"})
    hub.commit("add", "t")
    assert hub.repo.run("ls-files", "--", "instructions/*.md").out == b""      # a glob is not expanded
    assert b"instructions/a.md" in hub.repo.run("ls-files", "--", "instructions/a.md").out


def test_snapshot_reads_objects_not_attributes(hub: Hub) -> None:
    hub.work.put("instruction", "a", {"title": "A", "body": "hello\n"})
    hub.commit("add", "t")
    _, store = hub.approved()
    assert store.load("instruction", "a").body == "hello\n"
    hub.work.put("instruction", "a", {"title": "A", "body": "changed\n"})
    assert "+changed" in hub.changes()["items"][0]["diff"]
