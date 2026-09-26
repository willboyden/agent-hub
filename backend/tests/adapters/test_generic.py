from __future__ import annotations

import copy
import json
from pathlib import Path
from typing import Any

import pytest
import yaml
from fakes import FakeFS, FakeRunner, assert_golden, sample_ctx, shuffled, skill

from agent_hub.adapters import _util as u
from agent_hub.adapters.base import AgentItem, ClientConfig, Expected, McpItem, PermissionSet, Rule
from agent_hub.adapters.generic import (
    ADAPTER,
    GenericSpecAdapter,
    caps_for_spec,
    dry_render,
    subst,
    validate_spec,
)

ROOT = "/w/acme"
EXAMPLES = Path(__file__).resolve().parents[3] / "examples" / "clients"

# A brand-new client that exists ONLY as data: no Python module, no registry change.
ACME: dict[str, Any] = {
    "version": 1,
    "tool_map": {"read": "fs.read", "write": "fs.write", "shell": "sh", "web_fetch": None},
    "model_tiers": {"fast": "acme-small", "deep": "acme-large"},
    "skills": {"root": "project", "path": ".acme/skills", "layout": "dir_per_skill", "filename": "SKILL.md"},
    "agents": {"root": "project", "path": ".acme/agents", "filename": "{name}.json", "format": "json",
               "fields": {"id": "name", "about": "description", "prompt": "body"}, "static": {"kind": "helper"},
               "tools_field": "allowedTools", "tools_format": "bool_map", "model_field": "model"},
    "instructions": {"root": "project", "file": "ACME.md", "mode": "block"},
    "mcp": {"root": "project", "file": ".acme/mcp.yaml", "format": "yaml", "key_path": "tools.mcp", "wrapper": ["./run-sandboxed.sh", "--"],
            "entry": {"stdio": {"exec": "{argv}", "environment": "{env}", "label": "srv-{name}"},
                      "http": {"endpoint": "{url}"}}},
    "permissions": {"root": "project", "file": ".acme/policy.json", "format": "json",
                    "lists": {"allow": "rules.allow", "deny": "rules.deny"},
                    "rule_templates": {"command": "shell:{match}", "tool": "tool:{tool}", "path_read": ["read:{match}", "stat:{match}"]}},
    "memory": {"root": "project", "path": ".acme/memory", "filename": "{id}.md"},
}


@pytest.fixture
def acme_cfg() -> ClientConfig:
    return ClientConfig(id="acme", adapter="generic", display_name="Acme", roots={"project": ROOT}, spec=copy.deepcopy(ACME), strict=False)


def test_brand_new_client_purely_via_spec_end_to_end(acme_cfg) -> None:
    assert validate_spec(ACME) == []
    res = ADAPTER().render(sample_ctx(acme_cfg))
    assert_golden("generic_acme", res)
    by = {a.path: a for a in res.artifacts}
    assert set(by) == {".acme/agents/builder.json", ".acme/agents/reviewer.json", ".acme/mcp.yaml", ".acme/memory/m1.md",
                       ".acme/policy.json", ".acme/skills/example-skill/SKILL.md", ".acme/skills/example-skill/scripts/run.sh",
                       ".acme/skills/example-other-skill/SKILL.md", "ACME.md"}
    agent = json.loads(by[".acme/agents/reviewer.json"].content)
    assert agent == {"id": "reviewer", "about": "Reviews changes; read-only.", "prompt": "You review.\n", "kind": "helper",
                     "allowedTools": {"fs.read": True, "fs.write": False, "sh": True}, "model": "acme-large"} or agent["allowedTools"]["fs.write"] is False
    assert agent["allowedTools"] == {"fs.read": True, "fs.write": False, "sh": True}
    assert "model" not in agent or agent["model"] in ("acme-small", "acme-large")
    mcp = yaml.safe_load(by[".acme/mcp.yaml"].content)["tools"]["mcp"]
    assert mcp["serena"] == {"exec": ["./run-sandboxed.sh", "--", "serena", "start-mcp-server"], "environment": {"SERENA_HOME": "${SERENA_HOME}"}, "label": "srv-serena"}
    assert by[".acme/mcp.yaml"].merge == "yaml_keys" and by[".acme/mcp.yaml"].managed_keys == ["tools.mcp"]
    pol = json.loads(by[".acme/policy.json"].content)["rules"]
    assert pol["allow"] == ["shell:docker compose *"] and pol["deny"] == ["read:**/private-keys/**", "stat:**/private-keys/**"]
    assert by["ACME.md"].merge == "block"
    assert any(d.code == "unsupported_capability" and d.item == "builder" for d in res.diagnostics)      # web_fetch -> None
    assert any(d.code == "unsupported_rule" for d in res.diagnostics)                                       # ask/egress not templated / no list


def test_deterministic_and_no_absolute_paths(acme_cfg) -> None:
    ctx = sample_ctx(acme_cfg)
    a = ADAPTER().render(ctx)
    assert a == ADAPTER().render(shuffled(ctx))
    for art in a.artifacts:
        assert u.safe_relpath(art.path) == art.path and ROOT.encode() not in art.content


def test_dry_render_ignores_client_spec_and_validates(acme_cfg) -> None:
    ctx = sample_ctx(acme_cfg.model_copy(update={"spec": None}))
    res = dry_render(ACME, ctx)
    assert res.artifacts and dry_render({"bogus": 1}, ctx).artifacts == []


def test_round_trip_skills_agents_via_discover(acme_cfg) -> None:
    spec = copy.deepcopy(ACME)
    spec["agents"] = {"root": "project", "path": ".acme/agents", "filename": "{name}.md", "format": "md_frontmatter",
                      "fields": {"name": "name", "description": "description"}, "tools_field": "tools", "tools_format": "list"}
    cfg = acme_cfg.model_copy(update={"spec": spec})
    res = ADAPTER().render(sample_ctx(cfg, memory=[], instructions=[], mcp_servers=[], permissions=PermissionSet()))
    fs = FakeFS({f"{ROOT}/{a.path}": a.content for a in res.artifacts})
    d = ADAPTER().discover(cfg, fs)
    assert sorted(s.name for s in d.skills) == ["example-other-skill", "example-skill"]
    assert {a.name: a.capabilities for a in d.agents} == {"builder": ["read", "write", "shell"], "reviewer": ["read", "shell"]}
    res2 = ADAPTER().render(sample_ctx(cfg, skills=d.skills, agents=d.agents, memory=[], instructions=[], mcp_servers=[],
                                       permissions=PermissionSet()))
    assert {a.path: a.content for a in res2.artifacts if a.kind == "skill"} == {a.path: a.content for a in res.artifacts if a.kind == "skill"}


# ---- validation: unknown keys and precise paths -------------------------------------------------------------------------
def _bad(mutate) -> list[str]:
    s = copy.deepcopy(ACME)
    mutate(s)
    return [d.message for d in validate_spec(s)]


def test_unknown_keys_are_errors_with_paths() -> None:
    assert any(m == "spec.skills.layoutt: unknown key" for m in _bad(lambda s: s["skills"].update(layoutt="x")))
    assert any(m == "spec.wat: unknown key" for m in _bad(lambda s: s.update(wat=1)))
    assert any(m.startswith("spec.agents.format:") for m in _bad(lambda s: s["agents"].update(format="xml")))
    assert validate_spec({"skills": "nope"})[0].message.startswith("spec.skills:")
    assert validate_spec([])[0].message == "spec: must be a mapping"  # type: ignore[arg-type]


def test_semantic_errors_have_precise_paths() -> None:
    assert any("spec.tool_map.telepathy" in m for m in _bad(lambda s: s["tool_map"].update(telepathy="x")))
    assert any("spec.model_tiers.huge" in m for m in _bad(lambda s: s["model_tiers"].update(huge="x")))
    assert any(m.startswith("spec.skills.path: unsafe relative path") for m in _bad(lambda s: s["skills"].update(path="../x")))
    assert any(m.startswith("spec.instructions.file: unsafe") for m in _bad(lambda s: s["instructions"].update(file="/etc/passwd")))
    assert any(m.startswith("spec.agents.fields.x: unknown source") for m in _bad(lambda s: s["agents"]["fields"].update(x="secrets")))
    assert any("need one field mapped to 'body'" in m for m in _bad(lambda s: s["agents"]["fields"].pop("prompt")))
    assert any(m.startswith("spec.mcp.entry.stdio.exec: unknown placeholder {shell}") for m in _bad(lambda s: s["mcp"]["entry"]["stdio"].update(exec="{shell}")))
    assert any("spec.mcp.entry.grpc" in m for m in _bad(lambda s: s["mcp"]["entry"].update(grpc={})))
    assert any("spec.mcp.key_path" in m for m in _bad(lambda s: s["mcp"].update(key_path="a b")))
    assert any("spec.permissions.lists.maybe" in m for m in _bad(lambda s: s["permissions"]["lists"].update(maybe="x")))
    assert any("spec.permissions.rule_templates.command" in m and "{cmd}" in m for m in _bad(lambda s: s["permissions"]["rule_templates"].update(command="{cmd}")))
    assert any("spec.memory.filename" in m for m in _bad(lambda s: s["memory"].update(filename="fixed.md")))


def test_templating_is_not_format_string_or_eval() -> None:
    for evil in ("{0.__class__}", "{name.__class__}", "{name!r}", "{name:>99}", "{{name}}", "${name}", "{__import__}"):
        msgs = _bad(lambda s, e=evil: s["mcp"]["entry"]["stdio"].update(exec=e))
        if evil == "${name}":
            assert msgs == [] or any("stray" in m or "unknown placeholder" in m for m in msgs)      # ${name} is literal $ + {name}
        else:
            assert msgs, evil
    # substituter only touches allow-listed names and never evaluates
    assert subst({"a": "x{name}y", "b": ["{args}", "{missing}"], "c": "{env}"}, {"name": "N", "args": ["1"], "env": {}}) == {"a": "xNy", "b": [["1"]]}
    with pytest.raises(ValueError):
        subst("pre-{args}", {"args": ["1"]})


def test_hostile_names_through_templates(acme_cfg) -> None:
    ctx = sample_ctx(acme_cfg, skills=[skill("../x")], agents=[AgentItem(name="a/../../b", description="d", capabilities=["read"], body="b")],
                     mcp_servers=[McpItem(name="{name}", transport="stdio", command="c")], instructions=[], memory=[], permissions=PermissionSet())
    res = ADAPTER().render(ctx)
    assert res.artifacts == [] or all(u.safe_relpath(a.path) == a.path for a in res.artifacts)
    assert sum(d.code == "invalid_name" for d in res.diagnostics) >= 3


def test_env_command_values_cannot_inject_structure(acme_cfg) -> None:
    m = McpItem(name="s", transport="stdio", command='c"; rm -rf /; "', args=["{name}", "${X}"], env_names=["A"])
    res = ADAPTER().render(sample_ctx(acme_cfg, mcp_servers=[m], skills=[], agents=[], instructions=[], memory=[], permissions=PermissionSet()))
    doc = yaml.safe_load(res.artifacts[0].content)["tools"]["mcp"]["s"]
    assert doc["exec"] == ['c"; rm -rf /; "', "{name}", "${X}"]           # data stays data, never re-substituted


def test_missing_root_and_missing_section_diags(acme_cfg) -> None:
    cfg = acme_cfg.model_copy(update={"roots": {"other": "/x"}})
    res = ADAPTER().render(sample_ctx(cfg))
    assert {d.code for d in res.diagnostics} >= {"root_missing"} and res.artifacts == []
    spec = {"version": 1, "instructions": ACME["instructions"]}
    cfg2 = acme_cfg.model_copy(update={"spec": spec, "strict": True})
    r2 = ADAPTER().render(sample_ctx(cfg2))
    errs = [d for d in r2.diagnostics if d.code == "unsupported_capability"]
    assert len(errs) >= 5 and all(d.severity == "error" for d in errs)
    r3 = ADAPTER().render(sample_ctx(cfg2.model_copy(update={"strict": False})))
    assert all(d.severity == "warn" for d in r3.diagnostics if d.code == "unsupported_capability")


def test_invalid_spec_render_returns_only_errors() -> None:
    res = ADAPTER().render(sample_ctx(ClientConfig(id="x", adapter="generic", display_name="x", roots={"project": ROOT}, spec={"nope": 1})))
    assert res.artifacts == [] and res.diagnostics[0].severity == "error"
    res2 = ADAPTER().render(sample_ctx(ClientConfig(id="x", adapter="generic", display_name="x", roots={}, spec=None)))
    assert res2.diagnostics[0].message.startswith("spec:")


def test_flat_skills_and_modes_and_toml() -> None:
    spec = {"version": 1,
            "skills": {"root": "p", "path": "cmds", "layout": "flat", "filename": "{name}.md"},
            "instructions": {"root": "p", "file": "I.md", "mode": "concat"},
            "mcp": {"root": "h", "file": "config.toml", "format": "toml", "key_path": "mcp_servers",
                    "entry": {"stdio": {"command": "{command}", "args": "{args}", "env": "{env}"}, "http": {"url": "{url}"}}, "env_style": "names_only"}}
    assert validate_spec(spec) == []
    cfg = ClientConfig(id="t", adapter="generic", display_name="t", roots={"p": "/p", "h": "/h"}, spec=spec, strict=False)
    plain = [McpItem(name="serena", transport="stdio", command="serena", args=["start-mcp-server"], env_names=["SERENA_HOME"]),
             McpItem(name="docs", transport="http", url="http://mcpo:8000/docs")]
    res = ADAPTER().render(sample_ctx(cfg, memory=[], agents=[], permissions=PermissionSet(), mcp_servers=plain))
    by = {a.path: a for a in res.artifacts}
    assert "cmds/example-skill.md" in by and "cmds/scripts" not in "".join(by)
    assert any(d.code == "extra_files_dropped" for d in res.diagnostics)
    assert by["I.md"].merge == "own" and "## " not in by["I.md"].content.decode()          # concat: no headings
    toml = by["config.toml"]
    assert toml.merge == "block"
    text = toml.content.decode()
    assert text.startswith(u.HASH_BEGIN) and '[mcp_servers.serena]\ncommand = "serena"\nargs = ["start-mcp-server"]' in text
    assert "[mcp_servers.docs]\nurl = " in text and "SERENA_HOME" not in text


def test_caps_for_spec_reflects_sections() -> None:
    c = caps_for_spec(ACME)
    assert c.skills and c.agents and c.memory and c.mcp and c.permissions
    assert c.tool_map["read"] == "fs.read" and c.tool_map["browser"] is None and c.model_tiers["deep"] == "acme-large"
    assert not caps_for_spec({"version": 1, "instructions": ACME["instructions"]}).skills
    assert caps_for_spec({"bad": 1}).notes == "invalid spec"
    assert GenericSpecAdapter().caps().skills


def test_default_config_spec_is_valid() -> None:
    assert validate_spec(GenericSpecAdapter().default_config().spec or {}) == []


def test_verify_uses_hash(acme_cfg) -> None:
    res = ADAPTER().render(sample_ctx(acme_cfg, agents=[], instructions=[], mcp_servers=[], memory=[], permissions=PermissionSet()))
    fs = FakeFS({f"{ROOT}/{a.path}": a.content for a in res.artifacts})
    exp = [Expected(root="project", path=a.path, sha256=u.sha256_hex(a.content), kind=a.kind) for a in res.artifacts]
    assert all(c.ok for c in ADAPTER().verify(acme_cfg, exp, fs, FakeRunner()))


def test_permission_decision_without_list_is_reported(acme_cfg) -> None:
    perms = PermissionSet(rules=[Rule(kind="command", match="ls *", decision="ask")])
    res = ADAPTER().render(sample_ctx(acme_cfg, permissions=perms, skills=[], agents=[], instructions=[], mcp_servers=[], memory=[]))
    assert any(d.code == "unsupported_rule" for d in res.diagnostics) or any(d.code == "unsupported_decision" for d in res.diagnostics)


# ---- shipped examples -----------------------------------------------------------------------------------------------------
@pytest.mark.parametrize("name", ["cursor", "codex-cli", "gemini-cli"])
def test_example_specs_are_valid_labelled_and_render(name: str) -> None:
    path = EXAMPLES / f"{name}.yaml"
    raw = path.read_text()
    assert "Example spec" in raw.splitlines()[0]
    data = yaml.safe_load(raw)
    cfg = ClientConfig(**{k: v for k, v in data.items() if k in ClientConfig.model_fields})
    assert cfg.adapter == "generic" and cfg.spec and validate_spec(cfg.spec) == []
    res = ADAPTER().render(sample_ctx(cfg))
    assert res.artifacts, name
    assert not [d for d in res.diagnostics if d.code in ("spec_invalid", "root_missing")]
    assert all(u.safe_relpath(a.path) == a.path for a in res.artifacts)


def test_sandbox_profile_without_spec_wrapper_fails_closed(acme_cfg) -> None:
    spec = copy.deepcopy(ACME)
    del spec["mcp"]["wrapper"]
    cfg = acme_cfg.model_copy(update={"spec": spec})
    res = ADAPTER().render(sample_ctx(cfg, skills=[], agents=[], instructions=[], memory=[], permissions=PermissionSet()))
    servers = yaml.safe_load(res.artifacts[0].content)["tools"]["mcp"]
    assert "serena" not in servers and any(d.code == "sandbox_wrapper_missing" for d in res.diagnostics)
