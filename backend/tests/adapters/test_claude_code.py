from __future__ import annotations

import json

import pytest
from fakes import FakeFS, FakeRunner, assert_golden, sample_ctx, shuffled, skill, tree

from agent_hub.adapters import _util as u
from agent_hub.adapters.base import AgentItem, Expected, InstructionItem, McpItem, PermissionSet, Rule
from agent_hub.adapters.claude_code import ADAPTER, ClaudeCodeAdapter

ROOT = "/w/proj"


@pytest.fixture
def adapter() -> ClaudeCodeAdapter:
    return ADAPTER()


@pytest.fixture
def cfg(adapter: ClaudeCodeAdapter):
    c = adapter.default_config()
    c.roots = {"project": ROOT}
    c.params["sandbox_wrapper"] = ["./run-sandboxed.sh", "--"]
    return c


def test_golden(adapter, cfg) -> None:
    assert_golden("claude_code_full", adapter.render(sample_ctx(cfg)))


def test_deterministic_regardless_of_input_order(adapter, cfg) -> None:
    ctx = sample_ctx(cfg)
    assert adapter.render(ctx) == adapter.render(shuffled(ctx))


def test_paths_relative_and_no_absolute_paths_in_artifacts(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    for a in res.artifacts:
        assert u.safe_relpath(a.path) == a.path
        assert ROOT.encode() not in a.content and b"/home/" not in a.content


def test_default_manage_makes_mcp_and_permissions_advisory(adapter) -> None:
    m = adapter.default_config().params["manage"]
    assert m["skills"] and m["agents"] and m["instructions"]
    assert m["mcp"] is False and m["permissions"] is False


def test_only_writes_project_level_files(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    assert {a.root for a in res.artifacts} == {"project"}
    assert not any(a.path.startswith("~") or "/." in a.path[:2] for a in res.artifacts)


def test_agent_tools_model_and_read_only(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    doc = {a.path: a.content.decode() for a in res.artifacts}
    ro = doc[".claude/agents/reviewer.md"]
    assert "tools: Read, Grep, Glob, Bash" in ro and "model: sonnet" in ro
    full = doc[".claude/agents/builder.md"]
    assert "Write" in full and "Edit" in full and "WebFetch" in full and "model: haiku" in full
    fm, body = u.parse_frontmatter(ro)
    assert fm["name"] == "reviewer" and body.strip() == "You review."


def test_read_only_removes_write_tools_with_info(adapter, cfg) -> None:
    ag = AgentItem(name="r", description="d", capabilities=["read", "write", "edit"], read_only=True, body="b")
    res = adapter.render(sample_ctx(cfg, agents=[ag], skills=[], instructions=[], mcp_servers=[], memory=[],
                                    permissions=PermissionSet()))
    text = res.artifacts[0].content.decode()
    assert "Write" not in text and "Edit" not in text
    assert any(d.code == "read_only_applied" and d.severity == "info" for d in res.diagnostics)


def test_empty_toolset_is_an_error_not_an_inherit_all(adapter, cfg) -> None:
    ag = AgentItem(name="x", description="d", capabilities=["browser"], body="b")
    res = adapter.render(sample_ctx(cfg, agents=[ag]))
    assert not [a for a in res.artifacts if a.kind == "agent"]
    codes = {d.code for d in res.diagnostics}
    assert {"empty_toolset", "unsupported_capability"} <= codes


def test_unsupported_capability_strict_vs_lenient(adapter, cfg) -> None:
    ag = AgentItem(name="x", description="d", capabilities=["read", "browser"], body="b")
    strict = adapter.render(sample_ctx(cfg, agents=[ag]))
    cfg2 = cfg.model_copy(update={"strict": False})
    lenient = adapter.render(sample_ctx(cfg2, agents=[ag]))
    sev = lambda r: {d.severity for d in r.diagnostics if d.code == "unsupported_capability" and d.item == "x"}  # noqa: E731
    assert sev(strict) == {"error"} and sev(lenient) == {"warn"}


def test_no_model_tier_omits_model_line(adapter, cfg) -> None:
    ag = AgentItem(name="x", description="d", capabilities=["read"], body="b")
    res = adapter.render(sample_ctx(cfg, agents=[ag]))
    assert "model:" not in [a for a in res.artifacts if a.kind == "agent"][0].content.decode()


def test_instructions_block_preserves_handwritten_text(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    art = next(a for a in res.artifacts if a.path == "CLAUDE.md")
    assert art.merge == "block"
    text = art.content.decode()
    assert text.index("## First") < text.index("## Second")          # ordered by (order, id)
    hand = "# My notes\n\nkeep me\n"
    merged = u.replace_block(hand, text)
    assert "keep me" in merged and "## First" in merged
    again = u.replace_block(merged, u.wrap_block("## Only\n\nnew"))
    assert "keep me" in again and "## First" not in again and again.count("agent-hub:begin") == 1


def test_mcp_json_shape_and_merge_keeps_other_servers(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    art = next(a for a in res.artifacts if a.path == ".mcp.json")
    assert art.merge == "json_keys" and art.managed_keys == ["mcpServers"]
    doc = json.loads(art.content)
    s = doc["mcpServers"]["serena"]
    assert s["command"] == "./run-sandboxed.sh" and s["args"][:2] == ["--", "serena"]
    assert s["env"] == {"SERENA_HOME": "${SERENA_HOME}"}          # names only
    assert doc["mcpServers"]["docs"] == {"type": "http", "url": "http://mcpo:8000/docs"}
    assert any(d.code == "mcp_not_clean" and d.item == "docs" for d in res.diagnostics)
    merged = u.merge_keys({"mcpServers": {"old": {}}, "other": 1}, doc, art.managed_keys)
    assert merged["other"] == 1 and "old" not in merged["mcpServers"]


def test_permissions_render_and_merge_keeps_hooks(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    art = next(a for a in res.artifacts if a.path == ".claude/settings.json")
    assert art.managed_keys == ["permissions.allow", "permissions.ask", "permissions.deny"]
    p = json.loads(art.content)["permissions"]
    assert p["allow"] == ["Bash(docker compose:*)", "WebFetch(domain:pypi.org)"]
    assert p["ask"] == ["Bash(sudo:*)", "WebFetch"]
    assert p["deny"] == ["Read(**/private-keys/**)"]
    live = {"hooks": {"PreToolUse": []}, "permissions": {"allow": ["Bash(ls)"], "deny": ["x"], "other": 1}}
    merged = u.merge_keys(live, json.loads(art.content), art.managed_keys)
    assert merged["hooks"] == {"PreToolUse": []} and merged["permissions"]["other"] == 1
    assert merged["permissions"]["allow"] == p["allow"]
    assert any(d.code == "egress_advisory" for d in res.diagnostics)


def test_hostile_names_and_paths_are_refused(adapter, cfg) -> None:
    bad_skill = skill("ok-name", extra={"../../evil": b"x"})
    hostile = [skill("../../etc/x"), skill("A B"), bad_skill]
    agents = [AgentItem(name="../../x", description="d", capabilities=["read"], body="b"),
              AgentItem(name="Fine", description="d", capabilities=["read"], body="b")]
    res = adapter.render(sample_ctx(cfg, skills=hostile, agents=agents, instructions=[], mcp_servers=[], memory=[],
                                    permissions=PermissionSet()))
    assert res.artifacts == []
    assert sum(d.code == "invalid_name" for d in res.diagnostics) >= 3
    assert any(d.code == "unsafe_path" for d in res.diagnostics)


def test_huge_description_and_body(adapter, cfg) -> None:
    s = skill("big", desc="x" * 9000)
    a = AgentItem(name="warned", description="y" * 2000, capabilities=["read"], body="b")
    b = AgentItem(name="huge-body", description="d", capabilities=["read"], body="z" * (u.MAX_BODY_CHARS + 1))
    res = adapter.render(sample_ctx(cfg, skills=[s], agents=[a, b], instructions=[], mcp_servers=[], memory=[],
                                    permissions=PermissionSet()))
    paths = [x.path for x in res.artifacts]
    assert ".claude/agents/warned.md" in paths and ".claude/agents/huge-body.md" not in paths
    assert not any("skills/big" in p for p in paths)
    codes = {d.code for d in res.diagnostics}
    assert {"description_too_long", "description_long", "body_too_large"} <= codes


def test_frontmatter_injection_in_description_is_quoted(adapter, cfg) -> None:
    ag = AgentItem(name="inj", description="ok\n---\ntools: Bash, Write\nmodel: opus", capabilities=["read"], body="b")
    res = adapter.render(sample_ctx(cfg, agents=[ag]))
    fm, _ = u.parse_frontmatter(next(a for a in res.artifacts if a.kind == "agent").content.decode())
    assert fm["tools"] == "Read, Grep, Glob" and "model" not in fm


def test_duplicate_names_are_errors(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg, skills=[skill("dup"), skill("dup")]))
    assert any(d.code == "duplicate_name" for d in res.diagnostics)
    assert not any("skills/dup" in a.path for a in res.artifacts)


def test_memory_unsupported_diag(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg))
    assert any(d.code == "unsupported_capability" and "memory" in d.message for d in res.diagnostics)


# ---- discover / round trip / verify ----------------------------------------------------------------------------------
REAL_LAYOUT = {
    f"{ROOT}/.claude/settings.json": json.dumps({
        "permissions": {"deny": ["Read(./**/secrets/**)"], "ask": ["Bash(curl:*)", "WebFetch"], "allow": ["Bash(docker compose:*)"]},
        "hooks": {}}).encode(),
    f"{ROOT}/.claude/agents/reviewer.md": b"---\nname: reviewer\ndescription: Use when broken.\ntools: Read, Grep, Glob, Bash\nmodel: sonnet\n---\n\nYou review.\n",
    f"{ROOT}/.claude/skills/example-skill/SKILL.md": b"---\nname: example-skill\ndescription: Bring up a model.\n---\n\n# Serve\n",
    f"{ROOT}/.claude/skills/example-skill/ref/notes.md": b"notes",
    f"{ROOT}/.claude/skills/Bad Name/SKILL.md": b"x",
    f"{ROOT}/CLAUDE.md": b"# Hand written\n",
    f"{ROOT}/.mcp.json": json.dumps({"mcpServers": {"serena": {"command": "serena", "args": ["a"], "env": {"K": "secret-value"}}}}).encode(),
}


def test_discover_real_layout(adapter, cfg) -> None:
    d = adapter.discover(cfg, FakeFS(REAL_LAYOUT))
    assert [s.name for s in d.skills] == ["example-skill"] and "ref/notes.md" in d.skills[0].files
    assert d.skills[0].description == "Bring up a model."
    ag = d.agents[0]
    assert ag.name == "reviewer" and ag.capabilities == ["read", "shell"] and ag.model_tier == "standard" and ag.read_only
    assert d.instructions[0].id == "claude-md"
    kinds = {(r.kind, r.decision) for r in d.permissions.rules}  # type: ignore[union-attr]
    assert ("command", "allow") in kinds and ("path_read", "deny") in kinds and ("tool", "ask") in kinds
    assert d.mcp_servers[0].env_names == ["K"]
    assert "secret-value" not in d.model_dump_json()
    assert any("Bad Name" in n for n in d.notes)


def test_round_trip_discover_then_render_reproduces_files(adapter, cfg) -> None:
    d = adapter.discover(cfg, FakeFS(REAL_LAYOUT))
    res = adapter.render(sample_ctx(cfg, skills=d.skills, agents=d.agents, instructions=[], mcp_servers=[], memory=[],
                                    permissions=PermissionSet()))
    got = {a.path: a.content for a in res.artifacts}
    assert got[".claude/agents/reviewer.md"] == REAL_LAYOUT[f"{ROOT}/.claude/agents/reviewer.md"].replace(b"You review.\n", b"You review.\n")
    assert got[".claude/skills/example-skill/SKILL.md"] == REAL_LAYOUT[f"{ROOT}/.claude/skills/example-skill/SKILL.md"]
    assert got[".claude/skills/example-skill/ref/notes.md"] == b"notes"


def test_round_trip_permissions(adapter, cfg) -> None:
    d = adapter.discover(cfg, FakeFS(REAL_LAYOUT))
    res = adapter.render(sample_ctx(cfg, skills=[], agents=[], instructions=[], mcp_servers=[], memory=[], permissions=d.permissions))
    p = json.loads(res.artifacts[0].content)["permissions"]
    assert p == {"allow": ["Bash(docker compose:*)"], "ask": ["Bash(curl:*)", "WebFetch"], "deny": ["Read(./**/secrets/**)"]}


def test_discover_empty_and_missing_roots(adapter, cfg) -> None:
    d = adapter.discover(cfg, FakeFS())
    assert d.skills == [] and d.agents == [] and d.permissions is None
    cfg2 = cfg.model_copy(update={"roots": {}})
    assert adapter.discover(cfg2, FakeFS(REAL_LAYOUT)).skills == []


def test_verify_hash_missing_and_tampered(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg, agents=[], instructions=[], mcp_servers=[], memory=[], permissions=PermissionSet()))
    fs = FakeFS(tree(res, ROOT))
    exp = [Expected(root="project", path=a.path, sha256=u.sha256_hex(a.content), kind=a.kind) for a in res.artifacts]
    assert all(c.ok for c in adapter.verify(cfg, exp, fs, FakeRunner()))
    fs.files[f"{ROOT}/.claude/skills/example-other-skill/SKILL.md"] = b"tampered"
    del fs.files[f"{ROOT}/.claude/skills/example-skill/SKILL.md"]
    checks = {c.name: c for c in adapter.verify(cfg, exp, fs, FakeRunner())}
    assert not checks["project:.claude/skills/example-other-skill/SKILL.md"].ok
    assert checks["project:.claude/skills/example-skill/SKILL.md"].detail == "file missing"


def test_verify_block_artifact_by_marker_region(adapter, cfg) -> None:
    res = adapter.render(sample_ctx(cfg, skills=[], agents=[], mcp_servers=[], memory=[], permissions=PermissionSet()))
    art = res.artifacts[0]
    fs = FakeFS({f"{ROOT}/CLAUDE.md": b"hand\n\n" + art.content + b"\ntrailer\n"})
    exp = [Expected(root="project", path="CLAUDE.md", sha256=u.sha256_hex(art.content), kind="instruction", merge="block")]
    assert adapter.verify(cfg, exp, fs, FakeRunner())[0].ok


def test_caps_shape(adapter) -> None:
    c = adapter.caps()
    assert c.tool_map["read"] == "Read" and c.tool_map["browser"] is None and c.tool_map["subagent"] == "Task"
    assert c.model_tiers == {"fast": "haiku", "standard": "sonnet", "deep": "opus"}
    assert not c.memory


def test_unknown_rule_and_mcp_errors(adapter, cfg) -> None:
    bad = [McpItem(name="nocmd", transport="stdio"), McpItem(name="nourl", transport="sse")]
    res = adapter.render(sample_ctx(cfg, mcp_servers=bad, permissions=PermissionSet(rules=[Rule(kind="tool", match="browser", decision="allow")])))
    codes = {d.code for d in res.diagnostics}
    assert {"mcp_no_command", "mcp_no_url", "unsupported_rule"} <= codes


def test_instruction_only_ctx(adapter, cfg) -> None:
    ctx = sample_ctx(cfg, skills=[], agents=[], mcp_servers=[], memory=[], permissions=PermissionSet(),
                     instructions=[InstructionItem(id="x", title="T", order=0, body="B")])
    assert [a.path for a in adapter.render(ctx).artifacts] == ["CLAUDE.md"]


# ---- instructions_mode / suggested_params / yaml safety ----------------------------------------------------------------
WHOLE = b"# Repo rules\n\nBe careful.\n\n## Section\n\ntext without trailing newline"


def test_own_mode_whole_file_round_trips_byte_identical(adapter, cfg) -> None:
    fs = FakeFS({f"{ROOT}/CLAUDE.md": WHOLE})
    d = adapter.discover(cfg, fs)
    assert d.suggested_params == {"instructions_mode": "own"}
    cfg.params.update(d.suggested_params)
    res = adapter.render(sample_ctx(cfg, skills=[], agents=[], mcp_servers=[], memory=[], permissions=PermissionSet(), instructions=d.instructions))
    art = res.artifacts[0]
    assert art.merge == "own" and art.content == WHOLE


def test_own_mode_several_and_default_block(adapter, cfg) -> None:
    assert adapter.render(sample_ctx(cfg)).artifacts[-1].merge == "block"
    cfg.params["instructions_mode"] = "own"
    art = next(a for a in adapter.render(sample_ctx(cfg)).artifacts if a.path == "CLAUDE.md")
    assert art.merge == "own" and art.content.decode() == "## First\n\nOne.\n\n## Second\n\nTwo.\n"
    cfg.params["instructions_mode"] = "bogus"
    assert any(d.code == "invalid_param" for d in adapter.render(sample_ctx(cfg)).diagnostics)


def test_marker_block_import_suggests_nothing(adapter, cfg) -> None:
    fs = FakeFS({f"{ROOT}/CLAUDE.md": u.wrap_block("x").encode()})
    assert adapter.discover(cfg, fs).suggested_params == {}


def test_every_emitted_frontmatter_is_valid_yaml_and_round_trips(adapter, cfg) -> None:
    import yaml

    from agent_hub.adapters.opencode import OpenCodeAdapter
    descs = ["Use when: a thing # not a comment", "- leading dash", "@at & *star !bang", "'quoted' \"double\"", "multi\nline\n---\nx", "{brace}: [x]", "%pct", "  padded  "]
    ags = [AgentItem(name=f"a{i}", description=d, capabilities=["read"], body="b") for i, d in enumerate(descs)]
    res = adapter.render(sample_ctx(cfg, agents=ags, skills=[], instructions=[], mcp_servers=[], memory=[], permissions=PermissionSet()))
    oc = OpenCodeAdapter()
    res2 = oc.render(sample_ctx(oc.default_config(), agents=ags, skills=[], instructions=[], mcp_servers=[], memory=[], permissions=PermissionSet()))
    for r in (res, res2):
        agents = [a for a in r.artifacts if a.kind == "agent"]
        assert len(agents) == len(descs)
        for art, d in zip(agents, descs, strict=True):
            head = art.content.decode().split("\n---\n", 1)[0].removeprefix("---\n")
            assert yaml.safe_load(head)["description"] == " ".join(d.split())


def test_sandbox_profile_without_wrapper_fails_closed(adapter, cfg) -> None:
    cfg.params["sandbox_wrapper"] = []
    res = adapter.render(sample_ctx(cfg))
    art = next(a for a in res.artifacts if a.path == ".mcp.json")
    assert "serena" not in json.loads(art.content)["mcpServers"]
    assert any(d.code == "sandbox_wrapper_missing" and d.item == "serena" and d.severity == "error" for d in res.diagnostics)
