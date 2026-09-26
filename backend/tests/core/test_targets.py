"""H1: delivery targets are constrained by the floor and the hub's own paths."""
from __future__ import annotations

from pathlib import Path

import pytest

from agent_hub.adapters.base import Artifact, RenderContext, RenderResult
from agent_hub.config import APP_ROOT
from agent_hub.domain.errors import Unprocessable
from agent_hub.services.hub import Hub

from .conftest import add_client
from .fakes import FakeAdapter


@pytest.mark.parametrize("root", ["~", "/", "/home", "~/.ssh", "~/.aws/x", "~/.config/gcloud", "~/.gnupg", "~/.config/agent-hub",
                                  "~/.local/share/agent-knowledge", str(APP_ROOT / "policy"), str(APP_ROOT / "backend/src"),
                                  "/tmp/x/.git", "/tmp/x/.git/hooks"])
def test_hostile_roots_are_refused_at_creation_and_in_plans(hub: Hub, root: str) -> None:
    with pytest.raises(Unprocessable) as ei:
        hub.put_client("evil", {"adapter": "fake", "display_name": "E", "roots": {"home": root}}, create=True)
    assert "invalid roots" in ei.value.detail
    # a client file that bypassed the API is still refused by the planner
    (hub.cfg.content_dir / "clients" / "evil.yaml").write_text(f"id: evil\nadapter: fake\ndisplay_name: E\nroots:\n  home: '{root}'\n")
    hub.commit("c", "t")
    cp = hub.plan(["evil"]).clients[0]
    assert cp.blocked and cp.diagnostics[0]["code"] == "bad_root"


def test_roots_that_contain_or_are_the_hub_state_are_refused(hub: Hub) -> None:
    for root in (hub.cfg.data_dir, hub.cfg.data_dir / "backups", hub.cfg.data_dir.parent, hub.cfg.content_dir):
        with pytest.raises(Unprocessable):
            hub.put_client("evil", {"adapter": "fake", "display_name": "E", "roots": {"home": str(root)}}, create=True)


def test_project_root_containing_the_app_is_allowed_but_targets_inside_protected_dirs_are_not(hub: Hub, tmp_path: Path) -> None:
    class Sneaky(FakeAdapter):
        id = "sneaky"

        def render(self, ctx: RenderContext) -> RenderResult:
            return RenderResult(artifacts=[Artifact(root="home", path=p, content=b"x", kind="skill")
                                           for p in ("hub/policy/floor.yaml", "hub/backend/src/x.py", "a/.git/config", "ok.txt")])

    hub.registry.register(Sneaky())
    proj = tmp_path / "proj"
    (proj / "hub/policy").mkdir(parents=True)
    hub.put_client("sn", {"adapter": "sneaky", "display_name": "S", "roots": {"home": str(proj)}}, create=True)   # root itself is fine
    hub.guard.inside_only += [proj / "hub/policy", proj / "hub/backend"]
    hub.commit("c", "t")
    cp = hub.plan(["sn"]).clients[0]
    msgs = [d["message"] for d in cp.diagnostics if d["code"] == "unsafe_path"]
    assert len(msgs) == 3 and cp.blocked
    assert not (proj / "hub/policy/floor.yaml").exists()


@pytest.mark.parametrize("f", ["hooks/pre-commit", ".git/hooks/pre-commit", "sub/hooks/x"])
def test_generic_spec_cannot_write_hooks_or_git(hub: Hub, tmp_path: Path, f: str) -> None:
    from agent_hub.adapters.generic import GenericSpecAdapter
    hub.registry.register(GenericSpecAdapter())
    proj = tmp_path / "proj"
    proj.mkdir()
    spec = {"version": 1, "instructions": {"root": "project", "file": f, "mode": "own"}}
    hub.put_client("gen", {"adapter": "generic", "display_name": "G", "roots": {"project": str(proj)}, "spec": spec}, create=True)
    hub.work.put("instruction", "i1", {"title": "I", "body": "x\n"})
    hub.work.put_profile("gen", {"enable": {"instructions": ["i1"]}})
    hub.commit("g", "t")
    cp = hub.plan(["gen"]).clients[0]
    assert cp.blocked and any(d["code"] == "unsafe_path" for d in cp.diagnostics)
    assert not list(proj.rglob("pre-commit")) and not list(proj.rglob("x"))


def test_normal_roots_still_work(hub: Hub, home: Path) -> None:
    add_client(hub, home)
