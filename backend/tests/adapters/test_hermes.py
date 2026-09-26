from __future__ import annotations

import shutil
import stat
import subprocess
from pathlib import Path

import pytest
import yaml
from fakes import FakeFS, FakeRunner, assert_golden, sample_ctx, shuffled, skill

from agent_hub.adapters import _util as u
from agent_hub.adapters.base import Expected, PermissionSet, Rule
from agent_hub.adapters.hermes import ADAPTER, BASELINE_DISABLED, HermesAdapter

STAGE = "/w/stage"
HUB = Path(__file__).resolve().parents[3]
DELIVER = HUB / "deploy" / "hermes-deliver.sh"


@pytest.fixture
def adapter() -> HermesAdapter:
    return ADAPTER()


@pytest.fixture
def cfg(adapter: HermesAdapter):
    c = adapter.default_config()
    c.roots = {"stage": STAGE}
    c.params.update(compose_project="myproj", docker_context="myctx", exec_user="agent")
    return c


def test_default_config_is_neutral_placeholders() -> None:
    d = HermesAdapter().default_config()
    assert d.id == "hermes" and d.adapter == "hermes-agent" and d.params["compose_project"] == "CHANGEME"
    assert d.params["source_dirs"] == [] and "~/path/to" in d.roots["stage"]
    assert not any("comfy" in n for n in __import__("os").listdir(Path(__file__).parents[2] / "src/agent_hub/adapters"))


def test_golden(adapter, cfg) -> None:
    assert_golden("hermes_full", adapter.render(sample_ctx(cfg)))


def test_deterministic(adapter, cfg) -> None:
    ctx = sample_ctx(cfg)
    assert adapter.render(ctx) == adapter.render(shuffled(ctx))


def test_stage_layout_and_advisory_fragments(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    paths = {a.path: a for a in res.artifacts}
    assert "skills/example-skill/SKILL.md" in paths and "AGENTS.md" in paths
    pol = paths["config.policy.fragment.yaml"]
    assert pol.merge == "yaml_keys" and pol.managed_keys == ["agent.disabled_toolsets", "approvals.deny"]
    doc = yaml.safe_load(pol.content)
    assert set(BASELINE_DISABLED) <= set(doc["agent"]["disabled_toolsets"])
    assert "*sudo *" not in doc["approvals"]["deny"]                   # ask-rule is not a deny
    assert "***/private-keys/**" not in doc["approvals"]["deny"] and "**/private-keys/**" in doc["approvals"]["deny"]
    mcp = yaml.safe_load(paths["config.mcp.fragment.yaml"].content)["mcp_servers"]
    assert mcp["serena"] == {"command": "serena", "args": ["start-mcp-server"]} and mcp["docs"] == {"url": "http://mcpo:8000/docs"}
    assert any(d.code == "env_names_omitted" for d in res.diagnostics)
    assert not any(a.root != "stage" for a in res.artifacts)


def test_agents_and_memory_unsupported(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    assert sum(d.code == "unsupported_capability" and d.severity == "error" for d in res.diagnostics) == 2


def test_baseline_floor_is_the_expected_data() -> None:
    assert len(set(BASELINE_DISABLED)) == len(BASELINE_DISABLED) == 14
    assert {"web", "search", "browser", "computer_use", "image_gen", "cronjob", "code_execution", "desktop_ui", "tts"} <= set(BASELINE_DISABLED)


def test_weakened_floor_param_is_error_and_render_still_full(adapter, cfg) -> None:
    cfg.params["disabled_toolsets"] = ["web", "cronjob"]
    res = adapter.render(sample_ctx(cfg))
    d = [x for x in res.diagnostics if x.code == "floor_weakened"]
    assert d and d[0].severity == "error" and "browser" in d[0].message
    pol = next(a for a in res.artifacts if a.path == "config.policy.fragment.yaml")
    assert set(BASELINE_DISABLED) <= set(yaml.safe_load(pol.content)["agent"]["disabled_toolsets"])


def test_extra_disabled_toolsets_allowed_and_sorted(adapter, cfg) -> None:
    cfg.params["disabled_toolsets"] = [*BASELINE_DISABLED, "zzz", "aaa"]
    res = adapter.render(sample_ctx(cfg))
    assert not any(d.code == "floor_weakened" for d in res.diagnostics)
    ts = yaml.safe_load(next(a for a in res.artifacts if a.path == "config.policy.fragment.yaml").content)["agent"]["disabled_toolsets"]
    assert ts == sorted(ts) and "aaa" in ts and "zzz" in ts


def test_allow_rule_against_floor_toolset_is_error(adapter, cfg) -> None:
    perms = PermissionSet(rules=[Rule(kind="tool", match="web_search", decision="allow")])
    res = adapter.render(sample_ctx(cfg, permissions=perms))
    assert any(d.code == "floor_conflict" and d.severity == "error" for d in res.diagnostics)


def test_policy_fragment_rendered_even_with_no_rules(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg, permissions=PermissionSet(), skills=[], agents=[], instructions=[], mcp_servers=[], memory=[]))
    assert [a.path for a in res.artifacts] == ["config.policy.fragment.yaml"]


def test_hostile_skill_names(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg, skills=[skill("../../x"), skill("ok", extra={"a/../../b": b"x"})]))
    assert not [a for a in res.artifacts if a.kind == "skill"]


# ---- verify -----------------------------------------------------------------------------------------------------------
def _expected(res):
    return [Expected(root="stage", path=a.path, sha256=u.sha256_hex(a.content), kind=a.kind, source_ids=a.source_ids)
            for a in res.artifacts if a.kind == "skill"]


def test_verify_container_not_running_never_claims_ok(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    fs = FakeFS({f"{STAGE}/{a.path}": a.content for a in res.artifacts})
    run = FakeRunner([(0, "", "")])
    checks = adapter.verify(cfg, _expected(res), fs, run)
    ask = checks[-1]
    assert ask.name == "hermes skills list" and ask.ok is False and ask.detail == "container not running: unverified"
    assert all(c.ok for c in checks[:-1])                            # staged files are fine
    assert run.calls[0][:3] == ["docker", "--context", "myctx"]
    assert "label=com.docker.compose.project=myproj" in run.calls[0]


def test_verify_asks_container_and_checks_names(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    fs = FakeFS({f"{STAGE}/{a.path}": a.content for a in res.artifacts})
    run = FakeRunner([(0, "ctr1\n", ""), (0, "hub/example-skill\nhub/example-other-skill\n", "")])
    checks = adapter.verify(cfg, _expected(res), fs, run)
    assert checks[-1].ok is True
    assert run.calls[1] == ["docker", "--context", "myctx", "exec", "--user", "agent", "ctr1", "hermes", "skills", "list"]
    run2 = FakeRunner([(0, "ctr1\n", ""), (0, "hub/example-skill\n", "")])
    bad = adapter.verify(cfg, _expected(res), fs, run2)[-1]
    assert bad.ok is False and "example-other-skill" in bad.detail


def test_verify_exec_failure_and_no_docker(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    fs = FakeFS({f"{STAGE}/{a.path}": a.content for a in res.artifacts})
    bad = adapter.verify(cfg, _expected(res), fs, FakeRunner([(0, "c\n", ""), (127, "", "not found")]))[-1]
    assert bad.ok is False and "127" in bad.detail
    nodock = adapter.verify(cfg, _expected(res), fs, FakeRunner(raises=True))[-1]
    assert nodock.ok is False and "unverified" in nodock.detail


def test_verify_missing_staged_file(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    checks = adapter.verify(cfg, _expected(res), FakeFS(), FakeRunner())
    assert not any(c.ok for c in checks)


def test_discover_reads_stage(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    d = adapter.discover(cfg, FakeFS({f"{STAGE}/{a.path}": a.content for a in res.artifacts}))
    assert sorted(s.name for s in d.skills) == ["example-other-skill", "example-skill"]
    assert d.skills[0].files.keys() >= {"SKILL.md"}


# ---- deploy/hermes-deliver.sh with a fake docker on PATH ------------------------------------------------------------------
DEST = "/data/skills/hub"


def _run_deliver(tmp: Path, stage_files: dict[str, str], *, link: bool = False, docker_rc: int = 0,
                 env_over: dict[str, str | None] | None = None, compose_name: str = "docker-compose.yml"):
    stage = tmp / "stage" / "skills"
    stage.mkdir(parents=True)
    for rel, text in stage_files.items():
        p = stage / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)
    if link:
        (stage / "evil").symlink_to("/etc/hostname")
    compose = tmp / compose_name
    compose.write_text("services: {}\n")
    bindir = tmp / "bin"
    bindir.mkdir()
    shim = bindir / "docker"
    shim.write_text(f'#!/bin/sh\nprintf "%s\\0" "$@" > "{tmp}/docker.args"\ncat > "{tmp}/docker.stdin"\nexit {docker_rc}\n')
    shim.chmod(shim.stat().st_mode | stat.S_IXUSR)
    env: dict[str, str] = {"PATH": f"{bindir}:/usr/bin:/bin", "HOME": str(tmp), "HERMES_STAGE_DIR": str(stage),
                           "HERMES_COMPOSE_FILE": str(compose), "HERMES_SERVICE": "agentsvc", "HERMES_SKILLS_DEST": DEST}
    for k, v in (env_over or {}).items():
        if v is None:
            env.pop(k, None)
        else:
            env[k] = v
    return subprocess.run(["bash", str(DELIVER)], env=env, capture_output=True, text=True, timeout=30)


def test_deliver_streams_skills_via_compose_run(tmp_path: Path) -> None:
    r = _run_deliver(tmp_path, {"s1/SKILL.md": "---\nname: s1\n---\n", "s1/__pycache__/x.pyc": "junk"},
                     env_over={"HERMES_DOCKER_CONTEXT": "myctx", "HERMES_BIN": "hermes-x", "HERMES_PATH_PREFIX": "/opt/h/bin"})
    assert r.returncode == 0, r.stderr
    args = (tmp_path / "docker.args").read_text().split("\0")
    assert args[:3] == ["--context", "myctx", "compose"] and "run" in args and "--rm" in args and "-T" in args
    assert "--no-deps" in args and args[args.index("--entrypoint") + 1] == "/bin/sh" and "agentsvc" in args
    i = args.index("-c")
    assert args[i + 2:i + 6] == ["sh", DEST, "hermes-x", "/opt/h/bin"]           # destination passed as data, never spliced
    assert DEST not in args[i + 1] and "mv " in args[i + 1] and "skills list" in args[i + 1]
    listing = subprocess.run(["tar", "-tf", str(tmp_path / "docker.stdin")], capture_output=True, text=True).stdout
    assert "s1/SKILL.md" in listing and "__pycache__" not in listing


def test_deliver_without_context_omits_it(tmp_path: Path) -> None:
    assert _run_deliver(tmp_path, {"s/SKILL.md": "x"}).returncode == 0
    assert (tmp_path / "docker.args").read_text().split("\0")[:2] == ["compose", "-f"]


@pytest.mark.parametrize("var", ["HERMES_STAGE_DIR", "HERMES_COMPOSE_FILE", "HERMES_SERVICE", "HERMES_SKILLS_DEST"])
def test_deliver_refuses_when_required_env_unset(tmp_path: Path, var: str) -> None:
    r = _run_deliver(tmp_path, {"s/SKILL.md": "x"}, env_over={var: None})
    assert r.returncode == 2 and var in r.stderr and not (tmp_path / "docker.args").exists()


@pytest.mark.parametrize(("var", "val"), [("HERMES_SKILLS_DEST", "/"), ("HERMES_SKILLS_DEST", "/skills"), ("HERMES_SKILLS_DEST", "/a/../b/c"),
                                          ("HERMES_SKILLS_DEST", "rel/path"), ("HERMES_SKILLS_DEST", "/a/b; rm -rf /"),
                                          ("HERMES_SERVICE", "x; y"), ("HERMES_BIN", "h; id"), ("HERMES_PATH_PREFIX", "/a b")])
def test_deliver_refuses_unsafe_values(tmp_path: Path, var: str, val: str) -> None:
    r = _run_deliver(tmp_path, {"s/SKILL.md": "x"}, env_over={var: val})
    assert r.returncode == 2 and not (tmp_path / "docker.args").exists()


def test_deliver_only_accepts_compose_named_files(tmp_path: Path) -> None:
    r = _run_deliver(tmp_path, {"s/SKILL.md": "x"}, compose_name="passwd.yml")
    assert r.returncode == 2 and "must be named" in r.stderr and not (tmp_path / "docker.args").exists()


def test_deliver_refuses_compose_symlink(tmp_path: Path) -> None:
    (tmp_path / "real.yml").write_text("services: {}\n")
    (tmp_path / "docker-compose.yml").symlink_to(tmp_path / "real.yml")
    stage = tmp_path / "stage"
    (stage / "s").mkdir(parents=True)
    (stage / "s" / "SKILL.md").write_text("x")
    r = subprocess.run(["bash", str(DELIVER)], capture_output=True, text=True, timeout=30,
                       env={"PATH": "/usr/bin:/bin", "HOME": str(tmp_path), "HERMES_STAGE_DIR": str(stage), "HERMES_SERVICE": "svc",
                            "HERMES_COMPOSE_FILE": str(tmp_path / "docker-compose.yml"), "HERMES_SKILLS_DEST": DEST})
    assert r.returncode == 1 and "symlink" in r.stderr


def test_deliver_refuses_empty_stage_and_never_calls_docker(tmp_path: Path) -> None:
    r = _run_deliver(tmp_path, {})
    assert r.returncode == 1 and "no SKILL.md" in r.stderr
    assert not (tmp_path / "docker.args").exists()


def test_deliver_refuses_symlink_and_propagates_failure(tmp_path: Path) -> None:
    (tmp_path / "a").mkdir()
    r = _run_deliver(tmp_path / "a", {"s/SKILL.md": "x"}, link=True)
    assert r.returncode == 1 and "symlink" in r.stderr
    (tmp_path / "b").mkdir()
    r2 = _run_deliver(tmp_path / "b", {"s/SKILL.md": "x"}, docker_rc=3)
    assert r2.returncode == 1 and "FAILED" in r2.stderr


def _container_body() -> str:
    text = DELIVER.read_text()
    return text[text.index("-c '") + 4:text.index("  ' sh ")]


def test_deliver_script_hygiene() -> None:
    text = DELIVER.read_text()
    assert "set -euo pipefail" in text
    assert "'" not in _container_body()                        # an apostrophe would end the sh -c quote early
    assert "printenv" not in text and "env |" not in text
    assert DELIVER.stat().st_mode & stat.S_IXUSR
    subprocess.run(["bash", "-n", str(DELIVER)], check=True)
    sc = shutil.which("shellcheck")
    if sc:
        subprocess.run([sc, "-S", "style", str(DELIVER)], check=True)


def _run_body(tmp: Path, skills: list[str], listing: str, preexisting: dict[str, str] | None = None):
    """Run the in-container script for real (sh) against a temp dir, with a fake `hermes` that prints ``listing``."""
    dest = tmp / "data" / "skills" / "hub"
    dest.parent.mkdir(parents=True)
    for rel, text in (preexisting or {}).items():
        p = dest / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)
    bindir = tmp / "bin"
    bindir.mkdir()
    fake = bindir / "hermes"
    fake.write_text(f"#!/bin/sh\ncat <<'EOF'\n{listing}\nEOF\n")
    fake.chmod(0o755)
    src = tmp / "src"
    for n in skills:
        (src / n).mkdir(parents=True)
        (src / n / "SKILL.md").write_text(f"new-{n}")
    tar = subprocess.run(["tar", "-C", str(src), "-cf", "-", "."], capture_output=True, check=True).stdout if skills else b""
    r = subprocess.run(["sh", "-c", _container_body(), "sh", str(dest), "hermes", str(bindir)], input=tar, capture_output=True,
                       env={"PATH": "/usr/bin:/bin"}, timeout=30)
    return r, dest.parent


def test_body_exact_name_match_and_rollback(tmp_path: Path) -> None:
    r, skills = _run_body(tmp_path / "a", ["video"], "hub/video-improve-loop  ok\n", {"old/SKILL.md": "OLD"})
    assert r.returncode == 1 and b"does not list skill video" in r.stderr
    assert (skills / "hub" / "old" / "SKILL.md").read_text() == "OLD"            # rolled back, never emptied
    assert not list(skills.glob(".hub.*"))
    r2, skills2 = _run_body(tmp_path / "b", ["video", "video-improve-loop"], "hub/video-improve-loop\nhub/video\n", {"old/SKILL.md": "OLD"})
    assert r2.returncode == 0, r2.stderr
    assert (skills2 / "hub" / "video" / "SKILL.md").read_text() == "new-video" and not (skills2 / "hub" / "old").exists()
    assert not list(skills2.glob(".hub.*"))


def test_body_failed_extract_keeps_previous_tree(tmp_path: Path) -> None:
    r, skills = _run_body(tmp_path, [], "", {"old/SKILL.md": "OLD"})           # empty stdin: nothing extracted
    assert r.returncode == 1
    assert (skills / "hub" / "old" / "SKILL.md").read_text() == "OLD"


def test_registry_loads_generic_adapter_ids() -> None:
    from agent_hub.adapters import load_registry

    reg = load_registry()
    assert {"claude-code", "opencode", "hermes-agent", "generic"} <= set(reg.ids())
    assert not reg.load_errors.keys() & {"claude_code", "opencode", "hermes", "generic"}


LAB = "/src"
SKILL_FILES = {
    f"{LAB}/skills-src/assemble-video/SKILL.md": b"---\nname: assemble-video\ndescription: Assemble clips.\n---\n\nbody",
    f"{LAB}/skills-src/assemble-video/tool.py": b"print(1)",
    f"{LAB}/skills-src/assemble-video/__pycache__/tool.pyc": b"junk",
    f"{LAB}/skills-src/loose-file.md": b"x",
}
AGENT_FILES = {
    f"{LAB}/workspace-AGENTS.md": b"# Workspace\n\nrules\n",
    f"{LAB}/agent.config.yaml": (
        b"_config_version: 34\nmodel:\n  default: __MODEL__\n  api_key: brokered-placeholder\n"
        b"custom_providers:\n  - models:\n      __MODEL__:\n        context_length: __CTX__\n"
        b"agent:\n  disabled_toolsets: [web, browser]\n"
        b"approvals:\n  mode: manual\n  deny:\n    - '*hermes update*'\n    - '*secret-key*'\n"
        b"mcp_servers:\n  serena:\n    command: /usr/local/bin/serena\n    args: [start-mcp-server, --transport, stdio]\n"
        b"    env:\n      SERENA_HOME: /opt/serena-home\n      TOKEN: super-secret-value\n"
        b"  remote:\n    url: http://x:1/mcp\n"),
}


def test_discover_skills_via_source_dirs() -> None:
    a = HermesAdapter()
    cfg = a.default_config()
    cfg.params["source_root"] = LAB
    cfg.params["source_dirs"] = [{"kind": "skills", "path": "skills-src"}]
    cfg.roots = {"stage": STAGE}
    d = a.discover(cfg, FakeFS(SKILL_FILES))
    assert [s.name for s in d.skills] == ["assemble-video"]
    assert set(d.skills[0].files) == {"SKILL.md", "tool.py"} and d.skills[0].description == "Assemble clips."


def test_discover_agent_workspace_instruction_and_config_without_secrets() -> None:
    a = HermesAdapter()
    cfg = a.default_config()
    cfg.params["source_root"] = LAB
    cfg.params["source_dirs"] = [{"kind": "instruction", "path": "workspace-AGENTS.md"}, {"kind": "config", "path": "agent.config.yaml"}]
    cfg.roots = {"stage": STAGE}
    d = a.discover(cfg, FakeFS(AGENT_FILES))
    assert d.instructions[0].body == "# Workspace\n\nrules\n"
    by = {m.name: m for m in d.mcp_servers}
    assert by["serena"].env_names == ["SERENA_HOME", "TOKEN"] and by["serena"].scan_status == "unscanned"
    assert by["remote"].transport == "http"
    dump = d.model_dump_json()
    assert "super-secret-value" not in dump and "/opt/serena-home" not in dump
    assert {(r.kind, r.decision) for r in d.permissions.rules} == {("command", "deny")}  # type: ignore[union-attr]
    assert any("disabled_toolsets" in n and "not imported" in n for n in d.notes)


def test_discover_missing_and_malformed_sources_only_note() -> None:
    a = HermesAdapter()
    cfg = a.default_config()
    cfg.roots = {"stage": STAGE}
    cfg.params["source_dirs"] = [{"kind": "skills", "path": "../x"}, {"kind": "nope"}, {"kind": "config", "path": "gone.yaml"}]
    d = a.discover(cfg, FakeFS())
    assert d.skills == [] and len(d.notes) >= 3



def test_verify_uses_exact_names_not_substrings(adapter, cfg) -> None:
    from agent_hub.adapters.hermes import listed_names

    assert listed_names("hub/video-improve-loop  ok\n| video-x |") == {"hub", "video-improve-loop", "ok", "video-x"}
    res = adapter.render(sample_ctx(cfg, skills=[skill("video"), skill("video-improve-loop")]))
    fs = FakeFS({f"{STAGE}/{a.path}": a.content for a in res.artifacts})
    run = FakeRunner([(0, "c\n", ""), (0, "hub/video-improve-loop\n", "")])
    bad = adapter.verify(cfg, _expected(res), fs, run)[-1]
    assert bad.ok is False and "video" in bad.detail and "video-improve-loop" not in bad.detail.split(":")[-1]


def test_verify_unconfigured_project_never_claims_ok(adapter) -> None:
    cfg = adapter.default_config()
    cfg.roots = {"stage": STAGE}
    run = FakeRunner()
    c = adapter.verify(cfg, [], FakeFS(), run)[-1]
    assert c.ok is False and "unverified" in c.detail and run.calls == []
    cfg.params["compose_project"] = "p"
    run2 = FakeRunner([(0, "", "")])
    adapter.verify(cfg, [], FakeFS(), run2)
    assert run2.calls[0][:2] == ["docker", "ps"]                        # no --context unless configured
