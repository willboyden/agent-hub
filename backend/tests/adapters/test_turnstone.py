from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest
from fakes import FakeFS, FakeRunner, assert_golden, sample_ctx, shuffled, skill, tree

from agent_hub.adapters import turnstone as ts_mod
from agent_hub.adapters.base import Expected, PermissionSet
from agent_hub.adapters.turnstone import ADAPTER, DELIVER_SCRIPT, TurnstoneAdapter

STAGE = "/w/stage"


@pytest.fixture
def adapter() -> TurnstoneAdapter:
    return ADAPTER()


@pytest.fixture
def token_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    d = tmp_path / "agent-hub"
    d.mkdir()
    monkeypatch.setattr(ts_mod, "TOKEN_DIR", d)
    return d


@pytest.fixture
def cfg(adapter: TurnstoneAdapter, token_dir: Path):
    c = adapter.default_config()
    c.roots = {"stage": STAGE}
    c.params.update(console_url="http://127.0.0.1:9999", token_file=str(token_dir / "ts.token"))
    return c


def test_default_config_is_neutral_and_the_delivery_script_ships() -> None:
    d = TurnstoneAdapter().default_config()
    assert d.id == "turnstone" and d.adapter == "turnstone" and "~/path/to" in d.roots["stage"]
    assert d.params["manage"]["skills"] is True and d.params["manage"]["instructions"] is False
    assert DELIVER_SCRIPT.is_file() and DELIVER_SCRIPT.parent.name == "deploy"


def test_golden(adapter, cfg) -> None:
    assert_golden("turnstone_full", adapter.render(sample_ctx(cfg)))


def test_deterministic(adapter, cfg) -> None:
    ctx = sample_ctx(cfg)
    assert adapter.render(ctx) == adapter.render(shuffled(ctx))


def test_stage_layout(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    paths = {a.path: a for a in res.artifacts}
    assert set(paths) == {"skills/example-skill/SKILL.md", "skills/example-skill/scripts/run.sh",
                          "skills/example-other-skill/SKILL.md", "instructions.md", "mcp.json"}
    assert all(a.root == "stage" and a.merge == "own" for a in res.artifacts)
    assert paths["instructions.md"].content.decode().index("One.") < paths["instructions.md"].content.decode().index("Two.")
    mcp = json.loads(paths["mcp.json"].content)["mcpServers"]
    assert mcp["docs"] == {"type": "streamable-http", "url": "http://mcpo:8000/docs"}
    assert mcp["serena"] == {"command": "serena", "args": ["start-mcp-server"]}
    assert "SERENA_HOME" not in paths["mcp.json"].content.decode()          # env names never rendered


def test_diagnostics(adapter, cfg) -> None:
    codes = {(d.severity, d.code, d.item) for d in adapter.render(sample_ctx(cfg)).diagnostics}
    assert ("error", "unsupported_capability", None) in codes               # agents and memory, strict client
    assert ("warn", "mcp_stdio_in_node", "serena") in codes
    assert ("info", "permissions_not_rendered", None) in codes
    cfg.strict = False
    assert any(d.severity == "warn" and d.code == "unsupported_capability" for d in adapter.render(sample_ctx(cfg)).diagnostics)


def test_nothing_but_skills_renders_only_skills(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg, agents=[], memory=[], instructions=[], mcp_servers=[], permissions=PermissionSet()))
    assert {a.path.split("/")[0] for a in res.artifacts} == {"skills"} and res.diagnostics == []


def test_oversized_skill_and_auto_approve_frontmatter_are_flagged(adapter, cfg) -> None:
    big = skill("big-skill", extra={"SKILL.md": b"---\nname: big-skill\ndescription: Big. Use when asked.\n---\n" + b"x" * 40000})
    auto = skill("auto-skill", extra={"SKILL.md": b"---\nname: auto-skill\ndescription: A. Use when asked.\nauto_approve: true\n---\nBody.\n"})
    diags = adapter.render(sample_ctx(cfg, skills=[big, auto])).diagnostics
    assert any(d.code == "skill_too_large" and d.severity == "error" and d.item == "big-skill" for d in diags)
    assert any(d.code == "auto_approve_ignored" and d.item == "auto-skill" for d in diags)
    body_only = skill("talks-about-it", extra={"SKILL.md": b"---\nname: talks-about-it\ndescription: T. Use when asked.\n---\nauto_approve: is a field.\n"})
    assert not any(d.code == "auto_approve_ignored" for d in adapter.render(sample_ctx(cfg, skills=[body_only])).diagnostics)


def _expected(adapter, cfg):
    res = adapter.render(sample_ctx(cfg))
    fs = FakeFS(tree(res, STAGE))
    exp = [Expected(root=a.root, path=a.path, sha256=__import__("hashlib").sha256(a.content).hexdigest(), kind=a.kind,
                    source_ids=a.source_ids) for a in res.artifacts]
    return fs, exp


def test_verify_asks_the_console_through_the_script_without_the_token_on_argv(adapter, cfg) -> None:
    fs, exp = _expected(adapter, cfg)
    run = FakeRunner([(0, json.dumps({"skills": ["example-skill", "example-other-skill", "theirs"],
                                      "hub_skills": ["example-skill", "example-other-skill"]}), "")])
    checks = adapter.verify(cfg, exp, fs, run)
    console = checks[-1]
    assert console.ok and "2 staged skill" in console.detail and all(c.ok for c in checks)
    argv = run.calls[0]
    assert argv[0] == sys.executable and argv[1] == str(DELIVER_SCRIPT) and argv[2] == "list"
    assert argv[argv.index("--console") + 1] == "http://127.0.0.1:9999"
    assert argv[argv.index("--token-file") + 1] == cfg.params["token_file"]


@pytest.mark.parametrize(("params", "fragment"), [
    ({"python_bin": "/bin/sh"}, None),                                   # ignored: the interpreter is never a param
    ({"console_url": "http://evil.example:8796"}, "loopback"),
    ({"console_url": "https://10.0.0.5"}, "loopback"),
    ({"console_url": "http://user:pw@127.0.0.1:8796"}, "bare base URL"),
    ({"console_url": "http://127.0.0.1:8796/v1/api?x=1"}, "bare base URL"),
    ({"console_url": "ftp://127.0.0.1"}, "http(s)"),
    ({"token_file": "/etc/passwd"}, "must be inside"),
    ({"token_file": "~/.config/other-app/x.token"}, "must be inside"),
])
def test_hostile_params_never_reach_the_command_line(adapter, cfg, token_dir, params, fragment) -> None:
    fs, exp = _expected(adapter, cfg)
    cfg.params.update(params)
    run = FakeRunner([(0, json.dumps({"skills": [], "hub_skills": ["example-skill", "example-other-skill"]}), "")])
    console = adapter.verify(cfg, exp, fs, run)[-1]
    if fragment is None:
        assert run.calls and run.calls[0][0] == sys.executable and "/bin/sh" not in run.calls[0]
    else:
        assert not console.ok and fragment in console.detail and "not checked" in console.detail and run.calls == []


def test_a_non_token_file_in_the_token_dir_is_refused(adapter, cfg, token_dir) -> None:
    fs, exp = _expected(adapter, cfg)
    (token_dir / "env").write_text("HUB_API_KEY=hc_not_a_turnstone_token\n")      # the hub's own env file lives here
    cfg.params["token_file"] = str(token_dir / "env")
    run = FakeRunner()
    console = adapter.verify(cfg, exp, fs, run)[-1]
    assert not console.ok and "*.token file" in console.detail and run.calls == []


def test_a_symlinked_token_file_is_refused(adapter, cfg, token_dir, tmp_path) -> None:
    fs, exp = _expected(adapter, cfg)
    (tmp_path / "elsewhere").write_text("x")
    (token_dir / "ts.token").symlink_to(tmp_path / "elsewhere")
    run = FakeRunner()
    console = adapter.verify(cfg, exp, fs, run)[-1]
    assert not console.ok and "symlink" in console.detail and run.calls == []


def test_verify_fails_when_a_hub_skill_was_switched_to_auto_approve(adapter, cfg) -> None:
    fs, exp = _expected(adapter, cfg)
    run = FakeRunner([(0, json.dumps({"skills": ["example-skill", "example-other-skill"],
                                      "hub_skills": ["example-skill", "example-other-skill"],
                                      "unsafe": ["example-skill"]}), "")])
    console = adapter.verify(cfg, exp, fs, run)[-1]
    assert not console.ok and "example-skill" in console.detail and "auto_approve" in console.detail


def test_allowed_tools_in_frontmatter_is_warned_as_not_delivered(adapter, cfg) -> None:
    tooled = skill("tooled", extra={"SKILL.md": b"---\nname: tooled\ndescription: T. Use when asked.\nallowed-tools: Bash\n---\nBody.\n"})
    assert any(d.code == "allowed_tools_dropped" and d.item == "tooled" and d.severity == "warn"
               for d in adapter.render(sample_ctx(cfg, skills=[tooled])).diagnostics)


@pytest.mark.parametrize(("response", "raises", "fragment"), [
    ((0, json.dumps({"skills": ["example-skill"], "hub_skills": ["example-skill"]}), ""), False, "example-other-skill"),
    ((0, json.dumps({"skills": ["example-skill", "example-other-skill"], "hub_skills": ["example-skill"]}), ""), False, "example-other-skill"),
    ((1, "", "turnstone-deliver: token file /secrets/ts.token not found"), False, "token file"),
    ((0, "not json", ""), False, "no JSON"),
    (None, True, "unverified"),
])
def test_verify_fails_closed(adapter, cfg, response, raises, fragment) -> None:
    fs, exp = _expected(adapter, cfg)
    run = FakeRunner([response] if response else [], raises=raises)
    console = adapter.verify(cfg, exp, fs, run)[-1]
    assert not console.ok and fragment in console.detail


def test_discover_reads_only_the_stage(adapter, cfg) -> None:
    fs, _ = _expected(adapter, cfg)
    res = adapter.discover(cfg, fs)
    assert sorted(s.name for s in res.skills) == ["example-other-skill", "example-skill"]
    assert any("database" in n for n in res.notes)


def test_registry_loads_it() -> None:
    from agent_hub.adapters import load_registry
    reg = load_registry()
    assert "turnstone" in reg.ids() and "turnstone" not in reg.load_errors
    assert Path(reg.get("turnstone").__class__.__module__.replace(".", "/")).name == "turnstone"  # type: ignore[union-attr]
