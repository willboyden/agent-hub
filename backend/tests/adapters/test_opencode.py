from __future__ import annotations

import json

import pytest
from fakes import FakeFS, FakeRunner, assert_golden, sample_ctx, shuffled

from agent_hub.adapters import _util as u
from agent_hub.adapters.base import AgentItem, Expected, InstructionItem, PermissionSet, Rule
from agent_hub.adapters.opencode import ADAPTER, OpenCodeAdapter

ROOT = "/w/proj"


@pytest.fixture
def adapter() -> OpenCodeAdapter:
    return ADAPTER()


@pytest.fixture
def cfg(adapter: OpenCodeAdapter):
    c = adapter.default_config()
    c.roots = {"project": ROOT}
    c.params["sandbox_wrapper"] = ["./run-sandboxed.sh", "--"]
    c.params["model_tiers"] = {"fast": "provider/fast-model"}      # 'standard' deliberately unmapped
    return c


def test_golden(adapter, cfg) -> None:
    assert_golden("opencode_full", adapter.render(sample_ctx(cfg)))


def test_deterministic(adapter, cfg) -> None:
    ctx = sample_ctx(cfg)
    assert adapter.render(ctx) == adapter.render(shuffled(ctx))


def test_skills_off_by_default_and_diag(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    assert not any("skills" in a.path for a in res.artifacts)
    d = [x for x in res.diagnostics if x.code == "unsupported_capability" and "skills" in x.message]
    assert d and d[0].severity == "warn"                        # default config is non-strict
    strict = adapter.render(sample_ctx(cfg.model_copy(update={"strict": True})))
    assert any(x.severity == "error" and "skills" in x.message for x in strict.diagnostics)


def test_skills_via_claude_dir_param(adapter, cfg) -> None:
    cfg.params["skills_via_claude_dir"] = True
    res = adapter.render(sample_ctx(cfg))
    assert ".claude/skills/example-skill/SKILL.md" in [a.path for a in res.artifacts]
    assert any(d.code == "skills_via_claude_dir" for d in res.diagnostics)


def test_agent_frontmatter_tools_permission(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    by = {a.path: a.content.decode() for a in res.artifacts}
    fm, body = u.parse_frontmatter(by[".opencode/agent/reviewer.md"])
    assert fm["mode"] == "subagent" and fm["temperature"] == 0.1 and body.strip() == "You review."
    assert fm["tools"]["write"] is False and fm["tools"]["edit"] is False and "bash" not in fm["tools"]
    assert fm["permission"] == {"bash": "ask", "edit": "deny"}
    assert "model" not in fm                                      # standard tier unmapped
    fm2, _ = u.parse_frontmatter(by[".opencode/agent/builder.md"])
    assert fm2["model"] == "provider/fast-model"
    assert fm2["permission"]["webfetch"] == "ask" and "write" not in fm2.get("tools", {})
    assert any(d.code == "no_model_mapping" and d.item == "reviewer" and d.severity == "info" for d in res.diagnostics)


def test_strict_vs_lenient_unsupported(adapter, cfg) -> None:
    ag = AgentItem(name="x", description="d", capabilities=["read", "browser", "web_search"], body="b")
    lenient = adapter.render(sample_ctx(cfg, agents=[ag]))
    strict = adapter.render(sample_ctx(cfg.model_copy(update={"strict": True}), agents=[ag]))
    f = lambda r: sorted(d.severity for d in r.diagnostics if d.code == "unsupported_capability" and d.item == "x")  # noqa: E731
    assert f(lenient) == ["warn", "warn"] and f(strict) == ["error", "error"]


def test_opencode_json_keys(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    arts = [a for a in res.artifacts if a.path == "opencode.json"]
    assert [(a.kind, a.managed_keys) for a in arts] == [("mcp", ["mcp"]), ("rule", ["permission"])]      # instructions opt-in
    assert all(a.merge == "json_keys" for a in arts)
    doc = {**json.loads(arts[0].content), **json.loads(arts[1].content)}
    assert doc["mcp"]["serena"] == {"type": "local", "enabled": True,
                                    "command": ["./run-sandboxed.sh", "--", "serena", "start-mcp-server"],
                                    "environment": {"SERENA_HOME": "{env:SERENA_HOME}"}}
    assert doc["mcp"]["docs"]["type"] == "remote"
    bash = doc["permission"]["bash"]
    assert list(bash)[0] == "*" and bash["docker compose *"] == "allow" and bash["sudo *"] == "ask"
    assert bash["* **/private-keys/**"] == "deny"
    assert doc["permission"]["webfetch"] == "ask"
    live = {"provider": {"litellm": {}}, "model": "m", "mcp": {"old": {}}}
    merged = u.merge_keys(live, doc, ["mcp", "permission"])
    assert merged["provider"] == {"litellm": {}} and "old" not in merged["mcp"]


def test_instruction_files_param_is_opt_in(adapter, cfg) -> None:
    cfg.params["instruction_files"] = ["docs/b.md", "AGENTS.md"]
    res = adapter.render(sample_ctx(cfg))
    art = next(a for a in res.artifacts if a.kind == "instruction" and a.path == "opencode.json")
    assert json.loads(art.content) == {"instructions": ["AGENTS.md", "docs/b.md"]} and art.managed_keys == ["instructions"]


def test_agents_md_block(adapter, cfg) -> None:
    art = next(a for a in adapter.render(sample_ctx(cfg)).artifacts if a.path == "AGENTS.md")
    assert art.merge == "block" and art.content.decode().startswith(u.MD_BEGIN)


def test_hostile_and_huge(adapter, cfg) -> None:
    ags = [AgentItem(name="../up", description="d", capabilities=["read"], body="b"),
           AgentItem(name="ok", description="d" * 9000, capabilities=["read"], body="b")]
    res = adapter.render(sample_ctx(cfg, agents=ags, instructions=[], mcp_servers=[], memory=[], skills=[],
                                    permissions=PermissionSet()))
    assert res.artifacts == []
    assert {"invalid_name", "description_too_long"} <= {d.code for d in res.diagnostics}


def test_discover_real_layout_and_round_trip(adapter, cfg) -> None:
    fs = FakeFS({
        f"{ROOT}/.opencode/agent/reviewer.md": b"---\ndescription: Reviews.\nmode: subagent\ntemperature: 0.1\ntools:\n  write: false\n  edit: false\npermission:\n  bash: ask\n---\n\nYou review.\n",
        f"{ROOT}/opencode.json": json.dumps({
            "instructions": ["AGENTS.md"],
            "mcp": {"example": {"type": "local", "enabled": False, "command": ["./run-sandboxed.sh", "--", "npx"]}},
            "permission": {"edit": "allow", "bash": {"*": "ask", "ls *": "allow", "sudo *": "deny"}}}).encode(),
        f"{ROOT}/AGENTS.md": b"# Repo\n",
    })
    d = adapter.discover(cfg, fs)
    ag = d.agents[0]
    assert ag.name == "reviewer" and "write" not in ag.capabilities and "edit" not in ag.capabilities and ag.read_only
    assert "shell" in ag.capabilities and "read" in ag.capabilities
    assert d.mcp_servers[0].command == "./run-sandboxed.sh"
    kinds = {(r.kind, r.match, r.decision) for r in d.permissions.rules}  # type: ignore[union-attr]
    assert ("command", "ls *", "allow") in kinds and ("tool", "shell", "ask") in kinds and ("tool", "edit", "allow") in kinds
    assert d.instructions[0].id == "agents-md"
    res = adapter.render(sample_ctx(cfg, agents=d.agents, skills=[], instructions=[], mcp_servers=[], memory=[],
                                    permissions=PermissionSet()))
    fm, _ = u.parse_frontmatter(res.artifacts[0].content.decode())
    assert fm["tools"] == {"edit": False, "write": False} or fm["tools"]["write"] is False
    assert fm["permission"]["bash"] == "ask" and fm["description"] == "Reviews."


def test_verify(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg, instructions=[InstructionItem(id="a", title="A", order=0, body="b")]))
    agent = next(a for a in res.artifacts if a.kind == "agent")
    fs = FakeFS({f"{ROOT}/{agent.path}": agent.content})
    exp = [Expected(root="project", path=agent.path, sha256=u.sha256_hex(agent.content), kind="agent")]
    assert adapter.verify(cfg, exp, fs, FakeRunner())[0].ok
    fs.files[f"{ROOT}/{agent.path}"] = b"other"
    assert not adapter.verify(cfg, exp, fs, FakeRunner())[0].ok


def test_unsupported_permission_rules_warn(adapter, cfg) -> None:
    perms = PermissionSet(rules=[Rule(kind="egress_host", match="x.org", decision="allow"),
                                 Rule(kind="path_write", match="/etc/**", decision="allow")])
    res = adapter.render(sample_ctx(cfg, permissions=perms, skills=[], agents=[], instructions=[], mcp_servers=[], memory=[]))
    assert sum(d.code == "unsupported_rule" for d in res.diagnostics) == 2


def test_own_mode_agents_md_round_trip_and_suggestion(adapter, cfg) -> None:
    whole = b"# Canonical\n\nLots of text: with colons.\n"
    d = adapter.discover(cfg, FakeFS({f"{ROOT}/AGENTS.md": whole}))
    assert d.suggested_params == {"instructions_mode": "own"}
    cfg.params.update(d.suggested_params)
    res = adapter.render(sample_ctx(cfg, skills=[], agents=[], mcp_servers=[], memory=[], permissions=PermissionSet(), instructions=d.instructions))
    assert res.artifacts[0].merge == "own" and res.artifacts[0].content == whole
    cfg.params["instructions_mode"] = "block"
    assert adapter.render(sample_ctx(cfg, skills=[], agents=[], mcp_servers=[], memory=[], permissions=PermissionSet(), instructions=d.instructions)).artifacts[0].merge == "block"
