"""N1: the content repo must not be a code-execution vector for whoever can write into it."""
from __future__ import annotations

import os
import stat
from pathlib import Path

import pytest

from agent_hub.cli import run
from agent_hub.config import APP_ROOT, Settings
from agent_hub.domain.errors import BadRequest, Conflict, ProblemError
from agent_hub.services import contentguard
from agent_hub.services.hub import Hub

from .conftest import add_client, bearer  # noqa: F401


def unsafe(fn) -> ProblemError:  # type: ignore[no-untyped-def]
    with pytest.raises(ProblemError) as ei:
        fn()
    assert ei.value.code == "content_dir_unsafe" and "migrate-content" in ei.value.detail
    return ei.value


def test_default_content_dir_is_private_and_outside_the_lab(tmp_path: Path) -> None:
    s = Settings(data_dir=tmp_path / "state")
    assert s.content_dir == tmp_path / "state" / "content"
    assert Settings(data_dir=tmp_path / "s", content_dir=tmp_path / "elsewhere").content_dir == tmp_path / "elsewhere"
    assert APP_ROOT not in s.content_dir.parents


def test_fresh_repo_is_0700_and_passes(hub: Hub) -> None:
    assert stat.S_IMODE(hub.cfg.content_dir.stat().st_mode) == 0o700
    assert stat.S_IMODE((hub.cfg.content_dir / ".git").stat().st_mode) == 0o700
    hub.check_content_safety()


@pytest.mark.parametrize("cfgtext", [
    '[filter "x"]\n\tclean = touch /tmp/hub-pwn\n', "[core]\n\tfsmonitor = /tmp/evil\n", "[core]\n\tsshCommand = /tmp/evil\n",
    "[alias]\n\tstatus = !touch /tmp/hub-pwn\n", '[merge "m"]\n\tdriver = /tmp/evil\n', "[include]\n\tpath = /tmp/evil\n",
    '[includeIf "gitdir:/"]\n\tpath = /tmp/evil\n', "[diff]\n\texternal = /tmp/evil\n", "[core]\n\thooksPath = /tmp/evil\n",
    '[url "x"]\n\tinsteadOf = y\n', "[core]\n\tpager = /tmp/evil\n",
])
def test_hostile_git_config_is_refused_before_any_git_runs(hub: Hub, cfgtext: str) -> None:
    cfg = hub.cfg.content_dir / ".git" / "config"
    cfg.write_text(cfg.read_text() + cfgtext)
    unsafe(hub.changes)
    unsafe(lambda: hub.commit("x", "t"))
    unsafe(lambda: hub.plan(None))
    assert not Path("/tmp/hub-pwn").exists()


def test_allowed_config_keys_pass(hub: Hub) -> None:
    cfg = hub.cfg.content_dir / ".git" / "config"
    cfg.write_text(cfg.read_text() + "[user]\n\tname = Someone\n\temail = a@b\n")
    hub.check_content_safety()


def test_gitattributes_and_info_attributes_are_refused(hub: Hub) -> None:
    ga = hub.cfg.content_dir / "skills" / ".gitattributes"
    ga.parent.mkdir(exist_ok=True)
    ga.write_text("* filter=x\n")
    unsafe(hub.check_content_safety)
    ga.write_text("")                                          # an EMPTY .gitattributes is harmless
    hub.check_content_safety()
    info = hub.cfg.content_dir / ".git" / "info" / "attributes"
    info.write_text("* diff=evil\n")
    unsafe(hub.check_content_safety)


def test_non_sample_hooks_are_refused(hub: Hub) -> None:
    h = hub.cfg.content_dir / ".git" / "hooks"
    assert any(p.name.endswith(".sample") for p in h.iterdir())
    (h / "post-checkout").write_text("#!/bin/sh\n")
    unsafe(hub.check_content_safety)


def test_group_or_world_writable_and_foreign_owner_are_refused(hub: Hub, monkeypatch: pytest.MonkeyPatch) -> None:
    for d in (hub.cfg.content_dir, hub.cfg.content_dir / ".git", hub.cfg.content_dir / ".git" / "hooks"):
        d.chmod(0o770)
        unsafe(hub.check_content_safety)
        d.chmod(0o700)
    hub.check_content_safety()
    me = os.getuid()
    monkeypatch.setattr(contentguard.os, "getuid", lambda: me + 1)
    unsafe(hub.check_content_safety)


def test_git_dir_must_be_a_real_directory(hub: Hub, tmp_path: Path) -> None:
    real = hub.cfg.content_dir / ".git"
    moved = tmp_path / "elsewhere.git"
    real.rename(moved)
    real.symlink_to(moved)
    unsafe(hub.check_content_safety)


def test_content_inside_a_client_root_or_the_app_tree_is_refused(hub: Hub, home: Path, tmp_path: Path) -> None:
    add_client(hub, home)
    hub.cfg.content_dir  # noqa: B018
    unsafe(lambda: contentguard.check_location(home / "content", [str(home)], APP_ROOT))
    unsafe(lambda: contentguard.check_location(tmp_path, [str(home)], APP_ROOT))           # a root INSIDE the content dir
    unsafe(lambda: contentguard.check_location(APP_ROOT / "content", [], APP_ROOT))
    unsafe(lambda: contentguard.check_location(APP_ROOT, [], APP_ROOT))
    contentguard.check_location(tmp_path / "private", [str(home)], APP_ROOT)              # a sibling is fine


def test_client_root_containing_the_content_dir_stops_planning(cfg: Settings, registry, tmp_path: Path) -> None:  # type: ignore[no-untyped-def]
    cfg.content_dir = tmp_path / "wide" / "content"
    h = Hub(cfg, registry=registry)
    try:
        h.init_content()
        (h.cfg.content_dir / "clients" / "wide.yaml").write_text(
            f"id: wide\nadapter: fake\ndisplay_name: W\nroots:\n  home: '{tmp_path / 'wide'}'\n")
        unsafe(lambda: h.plan(None))
        unsafe(h.changes)
    finally:
        h.close()


def test_git_child_environment_is_scrubbed(hub: Hub, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    for k, v in {"GIT_DIR": str(tmp_path / "evil"), "GIT_WORK_TREE": str(tmp_path), "GIT_EXEC_PATH": str(tmp_path),
                 "GIT_ASKPASS": "/tmp/x", "GIT_EXTERNAL_DIFF": "/tmp/x", "PAGER": "/tmp/x", "EDITOR": "/tmp/x",
                 "GIT_SSH_COMMAND": "/tmp/x"}.items():
        monkeypatch.setenv(k, v)
    env = hub.repo._env()
    assert not {"GIT_DIR", "GIT_WORK_TREE", "GIT_EXEC_PATH", "GIT_ASKPASS", "GIT_EXTERNAL_DIFF", "PAGER", "EDITOR",
                "GIT_SSH_COMMAND"} & set(env)
    assert env["GIT_CONFIG_GLOBAL"] == "/dev/null" and env["GIT_CONFIG_NOSYSTEM"] == "1" and env["GIT_ATTR_NOSYSTEM"] == "1"
    hub.work.put("instruction", "a", {"title": "A", "body": "x\n"})
    assert hub.commit("still works", "t")["commit"]


# ---- migrate-content ---------------------------------------------------------------------------------------------------------
def test_migrate_content_moves_the_repo_safely(hub: Hub, tmp_path: Path) -> None:
    hub.work.put("instruction", "a", {"title": "A", "body": "x\n"})
    head = hub.commit("one", "t")["commit"]
    (hub.cfg.content_dir / "instructions" / "a.md").chmod(0o640)
    src = hub.cfg.content_dir
    target = tmp_path / "new" / "place" / "content"
    dry = hub.migrate_content(str(target), yes=False)
    assert dry["dry_run"] is True and src.exists() and not target.exists()
    out = hub.migrate_content(str(target), yes=True)
    assert out["head"] == head and not src.exists() and target.is_dir()
    assert stat.S_IMODE(target.stat().st_mode) == 0o700 and stat.S_IMODE((target / ".git").stat().st_mode) == 0o700
    assert stat.S_IMODE((target / "instructions" / "a.md").stat().st_mode) == 0o640           # modes preserved
    assert not [p for p in target.parent.iterdir() if p.name.startswith(".migrate-")]
    hub.cfg.content_dir = target
    hub.work.root = target
    hub.repo.path = target
    assert hub.changes()["count"] == 0 and hub.history()[0]["message"] == "one"


def test_migrate_content_refusals_and_cleanup(hub: Hub, home: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    add_client(hub, home)
    existing = tmp_path / "exists"
    existing.mkdir()
    with pytest.raises(Conflict):
        hub.migrate_content(str(existing), yes=True)
    for bad in (str(home / "content"), str(APP_ROOT / "content"), str(hub.cfg.content_dir / "inner")):
        with pytest.raises((ProblemError, BadRequest)):
            hub.migrate_content(bad, yes=True)
    # a hostile source is reported by the dry run (never executed); --yes waives and strips it from the copy
    (hub.cfg.content_dir / ".git" / "hooks" / "pre-commit").write_text("#!/bin/sh\ntouch /tmp/hub-migrate-pwn\n")
    dry = hub.migrate_content(str(tmp_path / "t1"), yes=False)
    assert dry["dry_run"] and "hooks" in dry["findings"][0]
    (hub.cfg.content_dir / ".git" / "hooks" / "pre-commit").unlink()
    # a verification failure leaves nothing behind and keeps the source
    from agent_hub.services.gitrepo import GitRepo
    real = GitRepo.run

    def failing(self, *args, **kw):  # type: ignore[no-untyped-def]
        if args and args[0] == "fsck":
            raise Conflict("fsck failed", code="git_error")
        return real(self, *args, **kw)

    monkeypatch.setattr(GitRepo, "run", failing)
    with pytest.raises(Conflict):
        hub.migrate_content(str(tmp_path / "t2" / "content"), yes=True)
    assert hub.cfg.content_dir.is_dir() and not (tmp_path / "t2" / "content").exists()
    assert not [p for p in (tmp_path / "t2").iterdir()] if (tmp_path / "t2").exists() else True


def test_cli_migrate_content(hub: Hub, tmp_path: Path, capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("HUB_API_KEY", str(hub.auth.create_key("k", "admin")["secret"]))
    target = tmp_path / "moved"
    assert run(["migrate-content", "--to", str(target)], hub=hub) == 1                  # dry run needs --yes
    assert "re-run with --yes" in capsys.readouterr().out and not target.exists()
    assert run(["migrate-content", "--to", str(target), "--yes"], hub=hub) == 0
    out = capsys.readouterr().out
    assert str(target) in out and "HUB_API_KEY" not in out and target.is_dir()


def test_migrate_rescues_a_hostile_group_writable_repo_inside_a_client_root(hub: Hub, home: Path, tmp_path: Path) -> None:
    import shutil
    import subprocess
    add_client(hub, home)
    hub.work.put("instruction", "a", {"title": "A", "body": "x\n"})
    head = hub.commit("one", "t")["commit"]
    src = home / "content"                                                     # inside the client root, i.e. exactly what the gate refuses
    shutil.copytree(hub.cfg.content_dir, src)
    src.chmod(0o775)
    (src / ".git").chmod(0o775)
    pwn = Path("/tmp/hub-migrate-pwn2")
    pwn.unlink(missing_ok=True)
    cfg = src / ".git" / "config"
    cfg.write_text(cfg.read_text() + f'[core]\n\tfsmonitor = touch {pwn}\n\tsshCommand = touch {pwn}\n[alias]\n\tfsck = !touch {pwn}\n')
    (src / ".git" / "hooks" / "post-checkout").write_text(f"#!/bin/sh\ntouch {pwn}\n")
    (src / ".gitattributes").write_text("* filter=evil\n")
    hub.cfg.content_dir = src
    with pytest.raises(ProblemError):
        hub.check_content_safety()                                             # the normal gate still refuses it
    target = tmp_path / "safe" / "content"
    dry = hub.migrate_content(str(target), yes=False)
    assert dry["dry_run"] and dry["findings"] and not target.exists() and not pwn.exists()
    out = hub.migrate_content(str(target), yes=True)
    assert not pwn.exists()                                                    # nothing planted was ever executed
    assert out["head"] == head and not src.exists()
    assert stat.S_IMODE(target.stat().st_mode) == 0o700 and stat.S_IMODE((target / ".git").stat().st_mode) == 0o700
    assert any("fsmonitor" in r or "sshcommand" in r.lower() for r in out["sanitized"]) and any("post-checkout" in r for r in out["sanitized"])
    contentguard.verify_all(target, [str(home)], APP_ROOT)                     # the result passes the full gate
    log = subprocess.run(["git", "-C", str(target), "-c", "core.hooksPath=/dev/null", "log", "--format=%s"], capture_output=True,
                         text=True, check=True, timeout=30).stdout
    assert "one" in log


def test_migrate_refuses_an_unsafe_target_and_foreign_source(hub: Hub, home: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    add_client(hub, home)
    with pytest.raises(ProblemError):
        hub.migrate_content(str(home / "new-content"), yes=True)               # target inside a client root
    assert hub.cfg.content_dir.is_dir()
    me = os.getuid()
    monkeypatch.setattr(os, "getuid", lambda: me + 1)
    with pytest.raises(BadRequest):
        hub.migrate_content(str(tmp_path / "elsewhere"), yes=True)
