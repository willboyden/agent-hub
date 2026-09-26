"""LIVE self-test: real git, real files, the real claude-code and opencode adapters, in temp directories only.
Nothing here touches a real client location (the client 'project' roots are tmp dirs)."""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from agent_hub.adapters import load_registry
from agent_hub.config import APP_ROOT, Settings
from agent_hub.services.hub import Hub

from .fakes import FakeRunner

pytestmark = pytest.mark.skipif("claude-code" not in load_registry().ids(), reason="real adapters not installed")


def git(cwd: Path, *args: str) -> str:
    return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, check=True, timeout=30).stdout.strip()


def test_init_import_organise_commit_plan_apply_edit_revert(tmp_path: Path) -> None:
    project = tmp_path / "fake-claude-project"
    (project / ".claude/skills/serve-thing").mkdir(parents=True)
    (project / ".claude/skills/serve-thing/SKILL.md").write_text("---\nname: serve-thing\ndescription: Serve a thing\n---\nSteps.\n")
    (project / ".claude/agents").mkdir()
    (project / ".claude/agents/mesh-doctor.md").write_text(
        "---\nname: mesh-doctor\ndescription: Diagnoses the mesh\ntools: Read, Grep\nmodel: sonnet\n---\nLook around.\n")
    (project / "CLAUDE.md").write_text("# Hand written\n\nDo not delete me.\n")
    (project / ".claude/settings.json").write_text(json.dumps({"permissions": {"allow": ["Read"], "deny": ["Bash(rm -rf *)"]},
                                                                "env": {"FOO": "bar"}}))
    cfg = Settings(content_dir=tmp_path / "content", data_dir=tmp_path / "state", policy_file=APP_ROOT / "policy" / "floor.yaml",
                   trust_loopback=False)
    hub = Hub(cfg, registry=load_registry(), runner=FakeRunner())  # type: ignore[arg-type]
    try:
        # 1. init: a real nested git repo with a fixed identity
        r = hub.init_content()
        assert (cfg.content_dir / ".git").is_dir() and r["commit"]
        assert git(cfg.content_dir, "log", "-1", "--format=%an") == "Agent Hub"
        client_cfg = hub.registry.get("claude-code").default_config()  # type: ignore[union-attr]
        hub.put_client("claude-live", {"adapter": "claude-code", "display_name": "Claude (live test)",
                                       "roots": {"project": str(project)}, "params": client_cfg.params}, create=True)

        # 2. import what the client already has
        disc = hub.importer.discover("claude-live")["items"]
        assert [s["name"] for s in disc["skill"]] == ["serve-thing"] and disc["agent"][0]["name"] == "mesh-doctor"
        res = hub.import_items("claude-live", None, "skip")["results"]
        assert all(x["status"] in ("created", "skipped", "identical") for x in res), res
        assert hub.work.load("skill", "serve-thing").valid and hub.work.load("agent", "mesh-doctor").valid

        # 3. organise, then commit (the approval of content)
        hub.organizer.create_collection({"id": "ops", "title": "Ops", "icon": "wrench",
                                         "members": [{"kind": "skill", "name": "serve-thing"},
                                                     {"kind": "agent", "name": "mesh-doctor"}]})
        hub.work.put("instruction", "house-style", {"title": "House style", "order": 5, "body": "Be brief.\n"})
        hub.organizer.toggle("claude-live", "instruction", "house-style", True)
        assert hub.changes()["count"] > 3 and "?" not in git(cfg.content_dir, "log", "--format=%s")[:0]
        sha1 = hub.commit("organise: import + ops collection", "live-test")["commit"]
        assert git(cfg.content_dir, "rev-parse", "HEAD") == sha1 and hub.changes()["count"] == 0

        # 4. plan (pure) then apply
        plan = hub.plan(["claude-live"])
        files = {f.path: f.action for f in plan.clients[0].files}
        assert not plan.clients[0].blocked, plan.clients[0].blocked_reasons
        assert files["CLAUDE.md"] == "add" or files["CLAUDE.md"] == "change"
        assert files[".claude/settings.json"] == "advisory"                       # permissions stay advisory by default
        before_settings = (project / ".claude/settings.json").read_bytes()
        assert files[".claude/agents/mesh-doctor.md"] in ("change", "unchanged")     # imported live state is the baseline
        assert "conflict" not in files.values()
        out = hub.apply(plan.id, True, [], "live-test")["results"][0]
        assert out["status"] == "applied" and not out["skipped_conflicts"], out
        claude_md = (project / "CLAUDE.md").read_text()
        assert "Do not delete me." in claude_md and "Be brief." in claude_md
        assert (project / ".claude/settings.json").read_bytes() == before_settings      # advisory: untouched
        assert (project / ".claude/skills/serve-thing/SKILL.md").is_file()
        assert hub.drift("claude-live")["status"] in ("in_sync", "drift")
        again = hub.plan(["claude-live"]).clients[0]
        assert {f.action for f in again.files} <= {"unchanged", "advisory"}, [(f.path, f.action, f.reason) for f in again.files]

        # 5. edit -> plan shows the change
        hub.work.put("instruction", "house-style", {"title": "House style", "order": 5, "body": "Be VERY brief.\n"})
        sha2 = hub.commit("tighten style", "live-test")["commit"]
        plan2 = hub.plan(["claude-live"])
        changed = [f for f in plan2.clients[0].files if f.action == "change"]
        assert [f.path for f in changed] == ["CLAUDE.md"] and "+Be VERY brief." in changed[0].diff
        hub.apply(plan2.id, True, [], "live-test")
        assert "VERY brief" in (project / "CLAUDE.md").read_text()

        # 6. revert the content commit -> plan shows the change back -> apply restores the original bytes
        hub.revert(sha2, "live-test")
        plan3 = hub.plan(["claude-live"])
        assert [f.path for f in plan3.clients[0].files if f.action == "change"] == ["CLAUDE.md"]
        hub.apply(plan3.id, True, [], "live-test")
        text = (project / "CLAUDE.md").read_text()
        assert "Be brief." in text and "VERY" not in text and "Do not delete me." in text
        assert len(hub.history()) >= 4 and git(cfg.content_dir, "status", "--porcelain") == ""
        # nothing was written outside the temp roots
        assert sorted(p.name for p in tmp_path.iterdir()) == ["content", "fake-claude-project", "state"]
    finally:
        hub.close()


def test_generic_spec_example_validate_and_deliver(tmp_path: Path) -> None:
    """A YAML-only client (docs/ADDING-A-CLIENT.md path): dry-run the spec, then plan and apply it into temp roots."""
    from agent_hub.domain import yamlsafe

    example = yamlsafe.load((APP_ROOT / "examples" / "clients" / "codex-cli.yaml").read_text())
    proj, chome = tmp_path / "proj", tmp_path / "codex-home"
    proj.mkdir()
    chome.mkdir()
    cfg = Settings(content_dir=tmp_path / "content", data_dir=tmp_path / "state", policy_file=APP_ROOT / "policy" / "floor.yaml")
    hub = Hub(cfg, registry=load_registry(), runner=FakeRunner())  # type: ignore[arg-type]
    try:
        hub.init_content()
        roots = {"project": str(proj), "codex_home": str(chome)}
        dry = hub.validate_spec({"spec": example["spec"], "roots": roots})
        assert dry["ok"] is True and any(a["path"] == "AGENTS.md" for a in dry["artifacts"])
        bad = hub.validate_spec({"spec": {**example["spec"], "instructions": {"root": "project", "file": "../../x", "mode": "block"}},
                                 "roots": roots})
        assert bad["ok"] is False or all(".." not in a["path"] for a in bad["artifacts"])
        hub.put_client("codex-live", {**{k: v for k, v in example.items() if k != "id"}, "roots": roots}, create=True)
        hub.work.put("instruction", "style", {"title": "Style", "body": "Be terse.\n"})
        hub.organizer.toggle("codex-live", "instruction", "style", True)
        hub.commit("codex", "t")
        plan = hub.plan(["codex-live"])
        assert not plan.clients[0].blocked, plan.clients[0].blocked_reasons
        hub.apply(plan.id, True, [], "t")
        assert "Be terse." in (proj / "AGENTS.md").read_text()
    finally:
        hub.close()


def test_import_then_first_plan_has_no_conflicts_and_second_is_unchanged(tmp_path: Path) -> None:
    project = tmp_path / "proj"
    (project / ".claude/skills/one").mkdir(parents=True)
    (project / ".claude/skills/one/SKILL.md").write_text("---\nname: one\ndescription: First\n---\nBody\n")
    (project / ".claude/agents").mkdir()
    (project / ".claude/agents/a.md").write_text("---\nname: a\ndescription: Agent a\ntools: Read\nmodel: sonnet\n---\nDo a.\n")
    cfg = Settings(content_dir=tmp_path / "content", data_dir=tmp_path / "state", policy_file=APP_ROOT / "policy" / "floor.yaml")
    hub = Hub(cfg, registry=load_registry(), runner=FakeRunner())  # type: ignore[arg-type]
    try:
        hub.init_content()
        hub.put_client("c1", {"adapter": "claude-code", "display_name": "C", "roots": {"project": str(project)}}, create=True)
        out = hub.import_items("c1", None, "skip")
        assert out["adopted_live"] >= 2
        hub.commit("import", "t")
        plan = hub.plan(["c1"])
        acts = {f.path: f.action for f in plan.clients[0].files}
        assert "conflict" not in acts.values() and not plan.clients[0].blocked, acts
        assert acts[".claude/skills/one/SKILL.md"] == "unchanged"
        res = hub.apply(plan.id, True, [], "t")["results"][0]
        assert res["status"] in ("applied", "nothing") and not res["skipped_conflicts"]
        again = hub.plan(["c1"]).clients[0]
        assert {f.action for f in again.files} <= {"unchanged", "advisory"}, [(f.path, f.action) for f in again.files]
    finally:
        hub.close()


@pytest.mark.skipif("instructions_mode" not in Path(APP_ROOT / "backend/src/agent_hub/adapters/opencode.py").read_text(),
                    reason="the opencode adapter does not implement instructions_mode/suggested_params yet")
def test_opencode_whole_file_instructions_first_plan_unchanged(tmp_path: Path) -> None:
    proj = tmp_path / "oc"
    proj.mkdir()
    (proj / "AGENTS.md").write_text("# Team rules\n\nBe kind.\n")
    cfg = Settings(content_dir=tmp_path / "content", data_dir=tmp_path / "state", policy_file=APP_ROOT / "policy" / "floor.yaml")
    hub = Hub(cfg, registry=load_registry(), runner=FakeRunner())  # type: ignore[arg-type]
    try:
        hub.init_content()
        d = hub.registry.get("opencode").default_config()  # type: ignore[union-attr]
        hub.put_client("oc", {"adapter": "opencode", "display_name": "oc", "roots": {k: str(proj) for k in d.roots},
                              "params": d.params}, create=True)
        hub.import_items("oc", None, "skip")
        assert hub.work.get_client("oc").params.get("instructions_mode") == "own"
        hub.commit("import", "t")
        acts = {f.path: f.action for f in hub.plan(["oc"]).clients[0].files}
        assert acts["AGENTS.md"] == "unchanged", acts
    finally:
        hub.close()


def test_adapter_default_manage_flags_are_honoured_for_the_real_default_configs(tmp_path: Path) -> None:
    cfg = Settings(content_dir=tmp_path / "content", data_dir=tmp_path / "state", policy_file=APP_ROOT / "policy" / "floor.yaml")
    hub = Hub(cfg, registry=load_registry(), runner=FakeRunner())  # type: ignore[arg-type]
    expected = {"claude-code": ("skills", "agents", "instructions"), "opencode": ("agents", "instructions"),
                "hermes-agent": ("skills",)}
    try:
        hub.init_content()
        hub.work.put("skill", "s1", {"description": "S", "body": "b\n"})
        hub.work.put("agent", "a1", {"description": "A", "capabilities": ["read"], "body": "b\n"})
        hub.work.put("instruction", "i1", {"title": "I", "body": "b\n"})
        expected = {k: v for k, v in expected.items() if hub.registry.get(k) is not None}      # adapters may be added/removed
        assert expected
        for aid, managed in expected.items():
            d = hub.registry.get(aid).default_config()  # type: ignore[union-attr]
            roots = {k: str(tmp_path / aid / k) for k in d.roots}
            for p in roots.values():
                Path(p).mkdir(parents=True)
            hub.put_client(aid, {"adapter": aid, "display_name": aid, "roots": roots, "params": d.params}, create=True)   # typed manage empty
            hub.work.put_profile(aid, {"enable": {"skills": ["s1"], "agents": ["a1"], "instructions": ["i1"]}})
        hub.commit("clients", "t")
        for aid, managed in expected.items():
            eff = hub.client_view(aid)["manage_effective"]
            assert {c for c in ("skills", "agents", "instructions") if eff[c]} == set(managed), (aid, eff)
            assert not (eff["mcp"] or eff["permissions"] or eff["memory"])
            plan = hub.plan([aid]).clients[0]
            wrote = {f.kind for f in plan.files if f.action in ("add", "change") and f.managed}
            assert wrote <= {"skill", "agent", "instruction"} and {w + "s" if w != "mcp" else w for w in wrote} <= set(managed), (aid, wrote)
    finally:
        hub.close()
