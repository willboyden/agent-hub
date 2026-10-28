"""Golden cases for the Rust client adapters (WS-A): crates/agent-hub/tests/golden/adapters.json.

    env -u LITELLM_API_KEY -u OPENAI_API_KEY -u ANTHROPIC_API_KEY -u HF_TOKEN \
        "$PYREL/backend/.venv/bin/python" -I backend/parity/gen/gen_adapter_golden.py backend/crates/agent-hub/tests/golden/adapters.json
    (from the repo root; PYREL=$(node backend/parity/lib/python-release.mjs) is the last Python backend, see
     backend/parity/lib/python-release.mjs)

Every case runs the PYTHON adapters (backend/src/agent_hub/adapters) on an input and records what they return, byte for
byte: rendered artifacts (content, mode, kind, merge, managed keys, source ids) and diagnostics IN ORDER, discover()
results, observe() parsing, spec validation messages, caps, default configs, docs and the _util helpers. Nothing touches
a real file, process or network (a dict-backed FileSystem and a scripted runner stand in).

Value encoding (Python objects that JSON cannot carry): {"$bytes": b64}, {"$date": iso}, {"$float": "nan"|"inf"|"-inf"},
{"$int": "<digits>"} (outside i64),
{"$set": [...]}, {"$dict": [[k, v], ...]} (a dict with a non-str key or a key starting with "$"). Byte strings that are not
UTF-8 are {"$bytes": ...}; text is a plain string. A YAML-emitter-bearing artifact (its content holds a yaml.safe_dump
output recorded during the render) carries "yaml": true, so the Rust tests can tell the WS-Y engine's bytes apart.
"""
from __future__ import annotations

import base64
import copy
import datetime
import json
import math
import os
import sys
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve()
REPO = HERE.parents[3]
# the Python sources of the release being recorded (PYREL, see backend/parity/lib/python-release.mjs); its venv has them too
sys.path.insert(0, str(Path(os.environ["PYREL"]) / "backend" / "src" if os.environ.get("PYREL") else REPO / "backend" / "src"))

import yaml  # noqa: E402

from agent_hub.adapters import _util as u  # noqa: E402
from agent_hub.adapters import claude_code, generic, hermes, opencode, turnstone  # noqa: E402
from agent_hub.adapters.base import (  # noqa: E402
    AgentItem,
    ClientConfig,
    InstructionItem,
    McpItem,
    MemoryItem,
    PermissionSet,
    RenderContext,
    Rule,
    SkillItem,
)

ADAPTERS = {"claude-code": claude_code.ClaudeCodeAdapter, "opencode": opencode.OpenCodeAdapter,
            "hermes-agent": hermes.HermesAdapter, "turnstone": turnstone.TurnstoneAdapter, "generic": generic.GenericSpecAdapter}


# ---- encoding -----------------------------------------------------------------------------------------------------------
def enc(v: Any) -> Any:
    if isinstance(v, int) and not isinstance(v, bool) and not -2**63 <= v < 2**63:
        return {"$int": str(v)}
    if v is None or isinstance(v, (bool, int, str)):
        return v
    if isinstance(v, float):
        if math.isnan(v):
            return {"$float": "nan"}
        if math.isinf(v):
            return {"$float": "inf" if v > 0 else "-inf"}
        return v
    if isinstance(v, (bytes, bytearray)):
        return {"$bytes": base64.b64encode(bytes(v)).decode()}
    if isinstance(v, datetime.datetime):
        raise TypeError("datetime not supported by the encoding")
    if isinstance(v, datetime.date):
        return {"$date": v.isoformat()}
    if isinstance(v, (list, tuple)):
        return [enc(x) for x in v]
    if isinstance(v, (set, frozenset)):
        return {"$set": [enc(x) for x in v]}
    if isinstance(v, dict):
        if all(isinstance(k, str) and not k.startswith("$") for k in v):
            return {k: enc(x) for k, x in v.items()}
        return {"$dict": [[enc(k), enc(x)] for k, x in v.items()]}
    raise TypeError(type(v).__name__)


def blob(b: bytes) -> Any:
    try:
        return b.decode("utf-8")
    except UnicodeDecodeError:
        return {"$bytes": base64.b64encode(b).decode()}


def enc_cfg(c: ClientConfig) -> dict[str, Any]:
    return {"id": c.id, "adapter": c.adapter, "display_name": c.display_name, "description": c.description,
            "roots": c.roots, "params": enc(c.params), "spec": enc(c.spec), "strict": c.strict, "manage": c.manage,
            "icon": c.icon, "color": c.color}


def enc_skill(s: SkillItem) -> dict[str, Any]:
    return {"name": s.name, "description": s.description, "files": {k: blob(v) for k, v in s.files.items()},
            "groups": s.groups, "tags": s.tags, "source": s.source}


def enc_ctx(ctx: RenderContext) -> dict[str, Any]:
    return {"client": enc_cfg(ctx.client), "skills": [enc_skill(s) for s in ctx.skills],
            "agents": [a.model_dump() for a in ctx.agents], "instructions": [i.model_dump() for i in ctx.instructions],
            "mcp_servers": [m.model_dump() for m in ctx.mcp_servers], "memory": [m.model_dump() for m in ctx.memory],
            "permissions": ctx.permissions.model_dump(), "endpoints": ctx.endpoints}


RECORDED: list[str] = []
_real_safe_dump = yaml.safe_dump


def _recording_safe_dump(*a: Any, **k: Any) -> Any:
    out = _real_safe_dump(*a, **k)
    if isinstance(out, str):
        RECORDED.append(out)
    return out


yaml.safe_dump = _recording_safe_dump


def run_render(adapter: Any, ctx: RenderContext) -> dict[str, Any]:
    RECORDED.clear()
    try:
        res = adapter.render(ctx)
    except Exception as exc:  # noqa: BLE001 - the error IS the golden
        return {"error": f"{type(exc).__name__}: {str(exc)[:200]}"}
    arts = []
    for a in res.artifacts:
        text = a.content.decode("utf-8", errors="replace")
        arts.append({"root": a.root, "path": a.path, "content": blob(a.content), "mode": a.mode, "kind": a.kind,
                     "source_ids": a.source_ids, "merge": a.merge, "managed_keys": a.managed_keys,
                     "yaml": any(r and r in text for r in RECORDED)})
    return {"artifacts": arts, "diagnostics": [d.model_dump() for d in res.diagnostics]}


# ---- fakes (same as backend/tests/adapters/fakes.py) --------------------------------------------------------------------
class FakeFS:
    def __init__(self, files: dict[str, bytes] | None = None) -> None:
        self.files = dict(files or {})

    def read_bytes(self, path: str) -> bytes:
        if path not in self.files:
            raise FileNotFoundError(path)
        return self.files[path]

    def exists(self, path: str) -> bool:
        return path in self.files or self.is_dir(path)

    def is_dir(self, path: str) -> bool:
        p = path.rstrip("/") + "/"
        return any(k.startswith(p) for k in self.files)

    def listdir(self, path: str) -> list[str]:
        p = path.rstrip("/") + "/"
        return sorted({k[len(p):].split("/")[0] for k in self.files if k.startswith(p)})


def skill(name: str, desc: str = "Does a thing. Use when asked.", extra: dict[str, bytes] | None = None) -> SkillItem:
    files = {"SKILL.md": f"---\nname: {name}\ndescription: {desc}\n---\n\n# {name}\n\nSteps.\n".encode()}
    files.update(extra or {})
    return SkillItem(name=name, description=desc, files=files)


def sample_ctx(cfg: ClientConfig, **over: Any) -> RenderContext:
    base: dict[str, Any] = dict(
        skills=[skill("example-skill", extra={"scripts/run.sh": b"#!/bin/sh\necho hi\n"}), skill("example-other-skill")],
        agents=[
            AgentItem(name="reviewer", description="Reviews changes; read-only.", capabilities=["read", "shell"],
                      model_tier="standard", read_only=True, body="You review.\n"),
            AgentItem(name="builder", description="Builds things.", capabilities=["read", "write", "edit", "shell", "web_fetch"],
                      model_tier="fast", body="You build.\n"),
        ],
        instructions=[InstructionItem(id="b-second", title="Second", order=2, body="Two."),
                      InstructionItem(id="a-first", title="First", order=1, body="One.")],
        mcp_servers=[
            McpItem(name="serena", transport="stdio", command="serena", args=["start-mcp-server"], env_names=["SERENA_HOME"],
                    sandbox_profile="sandbox", scan_status="clean"),
            McpItem(name="docs", transport="http", url="http://mcpo:8000/docs"),
        ],
        memory=[MemoryItem(id="m1", type="project", title="Fact", body="Body.")],
        permissions=PermissionSet(rules=[
            Rule(kind="path_read", match="**/private-keys/**", decision="deny"),
            Rule(kind="command", match="docker compose *", decision="allow"),
            Rule(kind="command", match="sudo *", decision="ask"),
            Rule(kind="tool", match="web_fetch", decision="ask"),
            Rule(kind="egress_host", match="pypi.org", decision="allow"),
        ]),
        endpoints={"router": "http://127.0.0.1:4000/v1"},
    )
    base.update(over)
    return RenderContext(client=cfg, **base)


EMPTY = dict(skills=[], agents=[], instructions=[], mcp_servers=[], memory=[], permissions=PermissionSet())


def only(**over: Any) -> dict[str, Any]:
    d = dict(EMPTY)
    d.update(over)
    return d


# ---- client configs -----------------------------------------------------------------------------------------------------
def cc_cfg(**params: Any) -> ClientConfig:
    c = claude_code.ClaudeCodeAdapter().default_config()
    c.roots = {"project": "/w/proj"}
    c.params["sandbox_wrapper"] = ["./run-sandboxed.sh", "--"]
    c.params.update(params)
    return c


def oc_cfg(**params: Any) -> ClientConfig:
    c = opencode.OpenCodeAdapter().default_config()
    c.roots = {"project": "/w/proj"}
    c.params["sandbox_wrapper"] = ["./run-sandboxed.sh", "--"]
    c.params["model_tiers"] = {"fast": "provider/fast-model"}
    c.params.update(params)
    return c


def he_cfg(**params: Any) -> ClientConfig:
    c = hermes.HermesAdapter().default_config()
    c.roots = {"stage": "/w/stage"}
    c.params.update(compose_project="myproj", docker_context="myctx", exec_user="agent")
    c.params.update(params)
    return c


def ts_cfg(**params: Any) -> ClientConfig:
    c = turnstone.TurnstoneAdapter().default_config()
    c.roots = {"stage": "/w/stage"}
    c.params.update(console_url="http://127.0.0.1:9999", token_file="/tmp/agent-hub-golden/ts.token")
    c.params.update(params)
    return c


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


def ge_cfg(spec: Any, strict: bool = False, roots: dict[str, str] | None = None) -> ClientConfig:
    return ClientConfig(id="acme", adapter="generic", display_name="Acme", roots=roots or {"project": "/w/acme", "h": "/w/home"},
                        spec=copy.deepcopy(spec), strict=strict)


def spec_with(**sections: Any) -> dict[str, Any]:
    s = copy.deepcopy(ACME)
    for k, v in sections.items():
        if v is None:
            s.pop(k, None)
        else:
            s[k] = v
    return s


# ---- render cases -------------------------------------------------------------------------------------------------------
HOSTILE_DESCS = [
    "Use when: a thing # not a comment", "- leading dash", "@at & *star !bang", "'quoted' \"double\"", "multi\nline\n---\nx",
    "{brace}: [x]", "%pct", "  padded  ", "yes", "no", "null", "~", "true", "123", "1.5", "0x1F", "0o17", "1e3", "2024-01-01",
    "12:30", ".inf", "NaN", "on", "Off", "y", "n", "", "trailing:", "a: b", "#hash", "!tag", "&anchor", "[list]", "> folded",
    "| literal", "back\\slash", "`tick", "?q", "-", ": colon", "héllo — ünïcode ✓ 😀", "tab\there", "bell\x07char",
    "line\u2028sep", "nbsp\xa0here", "x" * 1500, "ends with space ", "= equals", ",comma", "quote ' inside",
]
INSTR_BODIES = ["  spaced body  \n\n", "Line one\r\nLine two\r\n", "ünïcode — body ✓", "", "## Already a heading\n\ntext",
                "a\n\n\n\nb\n\n", "\tTabbed\n"]


def agents_from(descs: list[str], caps: list[str] | None = None) -> list[AgentItem]:
    return [AgentItem(name=f"a{i:02}", description=d, capabilities=caps or ["read"], body=f"Body {i}.\n") for i, d in enumerate(descs)]


def render_cases() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []

    def add(name: str, adapter_id: str, ctx: RenderContext) -> None:
        a = ADAPTERS[adapter_id]()
        cases.append({"name": name, "adapter": adapter_id, "ctx": enc_ctx(ctx), "result": run_render(a, ctx)})

    full_agents = [
        AgentItem(name="all-caps", description="Everything.", capabilities=list(u.CAPABILITIES), model_tier="deep", body="All.\n"),
        AgentItem(name="ro-all", description="Read only, everything.", capabilities=list(u.CAPABILITIES), read_only=True, body="RO.\n"),
        AgentItem(name="primary", description="Primary agent.", capabilities=["read", "subagent", "todo"], mode="primary", body="P.\n"),
        AgentItem(name="unknowns", description="Unknown caps.", capabilities=["read", "teleport", "zz"], body="U.\n"),
        AgentItem(name="mcp-only", description="MCP.", capabilities=["mcp"], body="M.\n"),
        AgentItem(name="nothing", description="No caps.", capabilities=[], body="N.\n"),
        AgentItem(name="trailing-ws", description="Trailing body whitespace.", capabilities=["read"], body="Text.\n\n  \n\t"),
    ]
    perms_all = PermissionSet(rules=[Rule(kind=k, match=m, decision=d)
                                     for k, m in [("tool", "read"), ("tool", "shell"), ("tool", "write"), ("tool", "edit"),
                                                  ("tool", "web_fetch"), ("tool", "browser"), ("tool", "mcp"), ("tool", "web_search"),
                                                  ("command", "git *"), ("command", "ls"), ("command", " npm  *  "), ("command", "a*b"),
                                                  ("command", "x [y] *"), ("path_read", "/etc/**"), ("path_write", "~/x/**"),
                                                  ("egress_host", "*.example.org"), ("mcp_server", "serena")]
                                     for d in ("allow", "ask", "deny")], default_tool_decision="deny")
    mcp_variety = [
        McpItem(name="plain", transport="stdio", command="srv", args=[], scan_status="clean"),
        McpItem(name="with-args", transport="stdio", command="npx", args=["-y", "pkg@1.2.3", "--flag=a b"], env_names=["Z_KEY", "A_KEY"],
                scan_status="findings", pinned_ref="abc123"),
        McpItem(name="sandboxed", transport="stdio", command="uvx", args=["tool"], sandbox_profile="srt", env_names=["TOKEN"]),
        McpItem(name="remote", transport="http", url="https://mcp.example.org/x?y=1"),
        McpItem(name="events", transport="sse", url="http://127.0.0.1:9000/sse", env_names=["E"]),
        McpItem(name="nocmd", transport="stdio"),
        McpItem(name="nourl", transport="http"),
        McpItem(name="Bad Name", transport="stdio", command="x"),
        McpItem(name="unicode-arg", transport="stdio", command="ëxe", args=["ü", "😀", "a\"b", "c\\d", "tab\tx"]),
    ]
    skills_variety = [
        skill("dup"), skill("dup"), skill("Bad Name"), skill("../x"),
        skill("mismatch", extra={"SKILL.md": b"---\nname: other\ndescription: d\n---\nbody\n"}),
        skill("no-fm", extra={"SKILL.md": b"just text\n"}),
        skill("nested", extra={"a/b/c.txt": b"deep", "z.bin": b"\x00\xff\xfe", "a/x.md": b"x"}),
        skill("warn-desc", desc="w" * 1100), skill("err-desc", desc="e" * 8200),
        SkillItem(name="no-skill-md", description="d", files={"README.md": b"x"}),
        skill("bad-file", extra={"../evil": b"x"}),
        skill("abs-file", extra={"/abs": b"x"}),
    ]

    # claude-code
    add("claude-code/sample", "claude-code", sample_ctx(cc_cfg()))
    add("claude-code/sample-lenient", "claude-code", sample_ctx(cc_cfg().model_copy(update={"strict": False})))
    add("claude-code/hostile-descriptions", "claude-code", sample_ctx(cc_cfg(), **only(agents=agents_from(HOSTILE_DESCS))))
    add("claude-code/agent-variety", "claude-code", sample_ctx(cc_cfg(), **only(agents=full_agents)))
    add("claude-code/permissions-all", "claude-code", sample_ctx(cc_cfg(), **only(permissions=perms_all)))
    add("claude-code/mcp-variety", "claude-code", sample_ctx(cc_cfg(), **only(mcp_servers=mcp_variety)))
    add("claude-code/mcp-no-wrapper", "claude-code", sample_ctx(cc_cfg(sandbox_wrapper=[]), **only(mcp_servers=mcp_variety)))
    add("claude-code/mcp-wrapper-ints", "claude-code", sample_ctx(cc_cfg(sandbox_wrapper=["w", 3, 1.5, True]), **only(mcp_servers=mcp_variety)))
    add("claude-code/mcp-wrapper-not-list", "claude-code", sample_ctx(cc_cfg(sandbox_wrapper="w --"), **only(mcp_servers=mcp_variety)))
    add("claude-code/skills-variety", "claude-code", sample_ctx(cc_cfg(), **only(skills=skills_variety)))
    for i, body in enumerate(INSTR_BODIES):
        ins = [InstructionItem(id="x", title="T ü", order=0, body=body), InstructionItem(id="a", title="A", order=0, body="first")]
        add(f"claude-code/instructions-block-{i}", "claude-code", sample_ctx(cc_cfg(), **only(instructions=ins)))
        add(f"claude-code/instructions-own-{i}", "claude-code", sample_ctx(cc_cfg(instructions_mode="own"), **only(instructions=ins)))
        add(f"claude-code/instructions-own-single-{i}", "claude-code",
            sample_ctx(cc_cfg(instructions_mode="own"), **only(instructions=ins[:1])))
    add("claude-code/instructions-bogus-mode", "claude-code", sample_ctx(cc_cfg(instructions_mode=7)))
    add("claude-code/instructions-injection", "claude-code",
        sample_ctx(cc_cfg(), **only(instructions=[InstructionItem(id="i", title="T", order=0, body="ok\n  <!--  Agent-Hub:END -->")])))
    add("claude-code/instructions-injection-own", "claude-code",
        sample_ctx(cc_cfg(instructions_mode="own"), **only(instructions=[InstructionItem(id="i", title="T", order=0, body="<!-- agent-hub:end -->")])))
    add("claude-code/memory-lenient", "claude-code", sample_ctx(cc_cfg().model_copy(update={"strict": False}), **only(memory=[MemoryItem(id="m", type="user", title="t", body="b")])))

    # opencode
    add("opencode/sample", "opencode", sample_ctx(oc_cfg()))
    add("opencode/sample-strict", "opencode", sample_ctx(oc_cfg().model_copy(update={"strict": True})))
    add("opencode/hostile-descriptions", "opencode", sample_ctx(oc_cfg(), **only(agents=agents_from(HOSTILE_DESCS))))
    add("opencode/agent-variety", "opencode", sample_ctx(oc_cfg(model_tiers={"fast": "f/m", "deep": 5, 3: "x"}), **only(agents=full_agents)))
    for t in (0.7, 1, True, "0.3", None, 2.5e-7, 100000000000000000.0):
        add(f"opencode/temperature-{t!r}", "opencode", sample_ctx(oc_cfg(temperature=t), **only(agents=full_agents[:1])))
    add("opencode/temperature-absent", "opencode",
        sample_ctx(oc_cfg().model_copy(update={"params": {k: v for k, v in oc_cfg().params.items() if k != "temperature"}}),
                   **only(agents=full_agents[:1])))
    add("opencode/tiers-not-dict", "opencode", sample_ctx(oc_cfg(model_tiers=["x"]), **only(agents=full_agents)))
    add("opencode/permissions-all", "opencode", sample_ctx(oc_cfg(), **only(permissions=perms_all)))
    add("opencode/mcp-variety", "opencode", sample_ctx(oc_cfg(), **only(mcp_servers=mcp_variety)))
    add("opencode/mcp-no-wrapper", "opencode", sample_ctx(oc_cfg(sandbox_wrapper=None), **only(mcp_servers=mcp_variety)))
    add("opencode/skills-claude-dir", "opencode", sample_ctx(oc_cfg(skills_via_claude_dir=True), **only(skills=skills_variety)))
    add("opencode/skills-claude-dir-truthy-not-true", "opencode", sample_ctx(oc_cfg(skills_via_claude_dir=1)))
    add("opencode/instruction-files", "opencode", sample_ctx(oc_cfg(instruction_files=["b.md", 3, "a.md", "b.md", 1.5])))
    add("opencode/instruction-files-empty", "opencode", sample_ctx(oc_cfg(instruction_files=[])))
    add("opencode/instruction-files-no-instructions", "opencode", sample_ctx(oc_cfg(instruction_files=["a.md"]), instructions=[]))
    for i, body in enumerate(INSTR_BODIES[:3]):
        ins = [InstructionItem(id="x", title="T", order=3, body=body)]
        add(f"opencode/instructions-own-{i}", "opencode", sample_ctx(oc_cfg(instructions_mode="own"), **only(instructions=ins)))

    # hermes
    add("hermes/sample", "hermes-agent", sample_ctx(he_cfg()))
    add("hermes/sample-lenient", "hermes-agent", sample_ctx(he_cfg().model_copy(update={"strict": False})))
    add("hermes/floor-weakened", "hermes-agent", sample_ctx(he_cfg(disabled_toolsets=["web", "cronjob", 5, None])))
    add("hermes/floor-extra", "hermes-agent", sample_ctx(he_cfg(disabled_toolsets=[*hermes.BASELINE_DISABLED, "zzz", "aaa", "web"])))
    add("hermes/floor-not-list", "hermes-agent", sample_ctx(he_cfg(disabled_toolsets="web")))
    add("hermes/permissions-all", "hermes-agent", sample_ctx(he_cfg(), **only(permissions=perms_all)))
    add("hermes/no-rules", "hermes-agent", sample_ctx(he_cfg(), **only()))
    add("hermes/mcp-variety", "hermes-agent", sample_ctx(he_cfg(), **only(mcp_servers=mcp_variety)))
    add("hermes/skills-variety", "hermes-agent", sample_ctx(he_cfg(), **only(skills=skills_variety)))
    add("hermes/instructions", "hermes-agent",
        sample_ctx(he_cfg(), **only(instructions=[InstructionItem(id=f"i{i}", title=f"T{i}", order=i % 2, body=b) for i, b in enumerate(INSTR_BODIES)])))

    # turnstone
    add("turnstone/sample", "turnstone", sample_ctx(ts_cfg()))
    add("turnstone/sample-lenient", "turnstone", sample_ctx(ts_cfg().model_copy(update={"strict": False})))
    add("turnstone/mcp-variety", "turnstone", sample_ctx(ts_cfg(), **only(mcp_servers=mcp_variety)))
    add("turnstone/skills-variety", "turnstone", sample_ctx(ts_cfg(), **only(skills=skills_variety)))
    refs = {f"references/r{i:02}.md": b"r" for i in range(12)}
    odd = skill("odd", extra={"SKILL.md": b"---\nname: odd\ndescription: O. Use when asked.\nallowed-tools: Bash\n---\nSee references/r10.md and references/r11.md.\n",
                              "agents/openai.yaml": b"a", "check.py": b"print()", "assets/logo.png": b"\x89PNG\xff",
                              "assets/small.js": "é".encode() * 10,
                              "scripts": b"not a dir", **refs})
    big = skill("big-skill", extra={"SKILL.md": b"---\nname: big-skill\ndescription: Big.\n---\n" + b"x" * 40000})
    auto = skill("auto-skill", extra={"SKILL.md": b"---\nname: auto-skill\ndescription: A.\nauto-approve: yes\nallowed_tools: [a]\n---\nB.\n"})
    noauto = skill("no-auto", extra={"SKILL.md": b"---\nname: no-auto\ndescription: A.\nauto_approve: false\nallowed-tools: ''\n---\nB.\n"})
    add("turnstone/resources", "turnstone", sample_ctx(ts_cfg(), **only(skills=[odd, big, auto, noauto])))
    tagged = []
    for i, (src, tags) in enumerate([("import:video-agent", ["video", "Video Tools", "x" * 65, "ok", "video-agent"]),
                                     ("import:other", ["b", "a"]), ("", []), ("import:video-agent", [f"t{j:02d}" for j in range(25)]),
                                     ("unmapped", ["z", "z", "y"])]):
        s = skill(f"tagged-{i}")
        s.source, s.tags = src, tags
        tagged.append(s)
    bad_tagged = skill("Bad Name")
    bad_tagged.source = "import:video-agent"
    add("turnstone/categories", "turnstone",
        sample_ctx(ts_cfg(category_from_source={"import:video-agent": "video-agent", "import:other": "other"}), **only(skills=[*tagged, bad_tagged])))
    for j, param in enumerate([{"import:x": "Has Caps"}, {"import:x": "x" * 65}, {"import:x": 3}, ["import:x"], {"import:x": ""},
                               {5: "ok"}, None, {}, "", 0]):
        s = skill("one")
        s.source, s.tags = "import:x", ["t"]
        add(f"turnstone/category-param-{j}", "turnstone", sample_ctx(ts_cfg(category_from_source=param), **only(skills=[s])))

    # generic
    add("generic/acme", "generic", sample_ctx(ge_cfg(ACME)))
    add("generic/acme-strict", "generic", sample_ctx(ge_cfg(ACME, strict=True)))
    add("generic/acme-hostile-descriptions", "generic", sample_ctx(ge_cfg(ACME), **only(agents=agents_from(HOSTILE_DESCS[:20], ["read", "write"]))))
    md_agents = {"root": "project", "path": ".acme/agents", "filename": "{name}.md", "format": "md_frontmatter",
                 "fields": {"name": "name", "description": "description", "kind": "mode"},
                 "static": {"temperature": 0.2, "max": 5, "enabled": True, "label": "x: y", "big": 1e20},
                 "tools_field": "tools", "tools_format": "list", "model_field": "model", "model_tiers": {"standard": "acme-std"}}
    add("generic/md-agents-list", "generic", sample_ctx(ge_cfg(spec_with(agents=md_agents)), **only(agents=full_agents + agents_from(HOSTILE_DESCS[:12]))))
    add("generic/md-agents-csv", "generic", sample_ctx(ge_cfg(spec_with(agents={**md_agents, "tools_format": "csv"})), **only(agents=full_agents)))
    add("generic/md-agents-bool-map", "generic", sample_ctx(ge_cfg(spec_with(agents={**md_agents, "tools_format": "bool_map"})), **only(agents=full_agents)))
    yaml_agents = {**md_agents, "filename": "{name}.yaml", "format": "yaml", "fields": {"name": "name", "about": "description", "prompt": "body"}}
    add("generic/yaml-agents", "generic", sample_ctx(ge_cfg(spec_with(agents=yaml_agents)), **only(agents=full_agents + agents_from(HOSTILE_DESCS[:12]))))
    json_agents = {**yaml_agents, "filename": "agent-{name}.json", "format": "json", "tools_format": "csv"}
    add("generic/json-agents", "generic", sample_ctx(ge_cfg(spec_with(agents=json_agents)), **only(agents=full_agents + agents_from(HOSTILE_DESCS[:12]))))
    flat = {"root": "project", "path": "cmds", "layout": "flat", "filename": "{name}.md"}
    add("generic/flat-skills", "generic", sample_ctx(ge_cfg(spec_with(skills=flat)), **only(skills=skills_variety + [skill("example-skill", extra={"x": b"y"})])))
    add("generic/dir-skills-custom-filename", "generic",
        sample_ctx(ge_cfg(spec_with(skills={"root": "project", "path": "s", "filename": "README.md"})), **only(skills=skills_variety)))
    for mode in ("own", "concat", "block"):
        for marker in ("md", "hash"):
            ins = [InstructionItem(id=f"i{i}", title=f"T{i}", order=-i, body=b) for i, b in enumerate(INSTR_BODIES)]
            add(f"generic/instructions-{mode}-{marker}", "generic",
                sample_ctx(ge_cfg(spec_with(instructions={"root": "project", "file": "docs/I.md", "mode": mode, "marker": marker})),
                           **only(instructions=ins)))
    add("generic/instructions-injection-hash", "generic",
        sample_ctx(ge_cfg(spec_with(instructions={"root": "project", "file": "I.md", "mode": "block", "marker": "hash"})),
                   **only(instructions=[InstructionItem(id="i", title="t", order=0, body="  # AGENT-HUB:end")])))
    toml_mcp = {"root": "h", "file": "config.toml", "format": "toml", "key_path": "mcp_servers",
                "entry": {"stdio": {"command": "{command}", "args": "{args}", "env": "{env}", "enabled": True, "timeout": 30,
                                    "ratio": 0.5, "nested": {"z key": "{name}", "a": [1, "two", False, {"k": "v"}]},
                                    "quoted\"key": "x\"y\\z\n\t\r", "names": "{env_names}", "ref": "{pinned_ref}", "empty": [],
                                    "unicode": "ü😀", "none": None, "fmt": "{transport}:{name}:{url}"},
                          "http": {"url": "{url}", "headers": {"X-Name": "{name}"}}, "sse": {"url": "{url}", "type": "sse"}},
                "env_style": "dollar_brace", "wrapper": ["w", "--"]}
    add("generic/toml-mcp", "generic", sample_ctx(ge_cfg(spec_with(mcp=toml_mcp)), **only(mcp_servers=mcp_variety)))
    add("generic/toml-mcp-env-ref", "generic", sample_ctx(ge_cfg(spec_with(mcp={**toml_mcp, "env_style": "env_ref"})), **only(mcp_servers=mcp_variety)))
    add("generic/toml-mcp-names-only-no-wrapper", "generic",
        sample_ctx(ge_cfg(spec_with(mcp={**{k: v for k, v in toml_mcp.items() if k != "wrapper"}, "env_style": "names_only"})), **only(mcp_servers=mcp_variety)))
    add("generic/toml-mcp-no-servers", "generic",
        sample_ctx(ge_cfg(spec_with(mcp=toml_mcp)), **only(mcp_servers=[McpItem(name="Bad Name", transport="stdio", command="x")])))
    add("generic/toml-mcp-quoted-key-path", "generic",
        sample_ctx(ge_cfg(spec_with(mcp={**toml_mcp, "key_path": "a.b-c.d_e"})), **only(mcp_servers=mcp_variety[:4])))
    json_mcp = {**toml_mcp, "format": "json", "file": "m/mcp.json", "key_path": "a.b"}
    add("generic/json-mcp", "generic", sample_ctx(ge_cfg(spec_with(mcp=json_mcp)), **only(mcp_servers=mcp_variety)))
    yaml_mcp = {**toml_mcp, "format": "yaml", "file": "m/mcp.yaml", "key_path": "servers"}
    add("generic/yaml-mcp", "generic", sample_ctx(ge_cfg(spec_with(mcp=yaml_mcp)), **only(mcp_servers=mcp_variety)))
    add("generic/mcp-missing-transport", "generic",
        sample_ctx(ge_cfg(spec_with(mcp={**json_mcp, "entry": {"http": {"u": "{url}"}}})), **only(mcp_servers=mcp_variety)))
    perm_yaml = {"root": "project", "file": "p.yaml", "format": "yaml", "lists": {"deny": "a.deny", "ask": "a.ask", "allow": "z.allow"},
                 "rule_templates": {"command": "Shell({match})", "tool": ["T:{tool}", "M:{match}"], "path_read": "R({match})",
                                    "path_write": ["W({match})"], "egress_host": "net:{match}", "mcp_server": "mcp.{match}"}}
    add("generic/permissions-yaml-all", "generic", sample_ctx(ge_cfg(spec_with(permissions=perm_yaml)), **only(permissions=perms_all)))
    add("generic/permissions-json-sample", "generic", sample_ctx(ge_cfg(spec_with(permissions={**perm_yaml, "format": "json", "file": "p.json"}))))
    mem = {"root": "project", "path": "mem", "filename": "{id}.txt", "frontmatter": False}
    memories = [MemoryItem(id="m1", type="user", title="T: one", body="Body one.\n\n"), MemoryItem(id="Bad", type="project", title="x", body="y"),
                MemoryItem(id="m0", type="reference", title="'quoted'", body="  b  ")]
    add("generic/memory-plain", "generic", sample_ctx(ge_cfg(spec_with(memory=mem)), **only(memory=memories)))
    add("generic/memory-frontmatter", "generic", sample_ctx(ge_cfg(spec_with(memory={**mem, "frontmatter": True})), **only(memory=memories)))
    add("generic/root-missing", "generic", sample_ctx(ge_cfg(ACME, roots={"other": "/x", "zz": "/y"})))
    add("generic/sections-missing-strict", "generic", sample_ctx(ge_cfg({"version": 1}, strict=True)))
    add("generic/sections-missing-lenient", "generic", sample_ctx(ge_cfg({"version": 1})))
    add("generic/invalid-spec", "generic", sample_ctx(ge_cfg({"nope": 1, "skills": "x"})))
    add("generic/no-spec", "generic", sample_ctx(ge_cfg(None)))
    add("generic/spec-not-dict", "generic", sample_ctx(ge_cfg(None).model_copy(update={"spec": [1]})))
    add("generic/error-flat-unsafe-filename", "generic",
        sample_ctx(ge_cfg(spec_with(skills={"root": "project", "path": "c", "layout": "flat", "filename": "../{name}.md"}))))
    add("generic/error-partial-list-placeholder", "generic",
        sample_ctx(ge_cfg(spec_with(mcp={**json_mcp, "entry": {"stdio": {"cmd": "run {args}"}}}))))
    add("generic/error-memory-unsafe-filename", "generic",
        sample_ctx(ge_cfg(spec_with(memory={**mem, "filename": "../{id}.md"}))))
    add("generic/tool-rule-unmapped", "generic",
        sample_ctx(ge_cfg(spec_with(permissions={**perm_yaml, "lists": {"allow": "x"}})),
                   **only(permissions=PermissionSet(rules=[Rule(kind="tool", match="browser", decision="allow"),
                                                           Rule(kind="tool", match="read", decision="allow"),
                                                           Rule(kind="tool", match="web_fetch", decision="allow")]))))
    for ex in ("cursor", "codex-cli", "gemini-cli"):
        data = yaml.safe_load((REPO / "examples" / "clients" / f"{ex}.yaml").read_text())
        cfg = ClientConfig(**{k: v for k, v in data.items() if k in ClientConfig.model_fields})
        add(f"generic/example-{ex}", "generic", sample_ctx(cfg))
    return cases


# ---- discover cases -------------------------------------------------------------------------------------------------------
P = "/w/proj"
ALIAS_BOMB = "a: &a [1,2,3,4,5,6,7,8,9]\nb: &b [*a,*a,*a,*a,*a,*a,*a,*a,*a]\nc: &c [*b,*b,*b,*b,*b,*b,*b,*b,*b]\nd: [*c,*c,*c,*c,*c,*c,*c,*c,*c]\n"


def enc_discovered(d: Any) -> dict[str, Any]:
    return {"skills": [enc_skill(s) for s in d.skills], "agents": [a.model_dump() for a in d.agents],
            "instructions": [i.model_dump() for i in d.instructions], "mcp_servers": [m.model_dump() for m in d.mcp_servers],
            "permissions": d.permissions.model_dump() if d.permissions is not None else None, "notes": d.notes,
            "suggested_params": enc(d.suggested_params)}


def discover_cases() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []

    def add(name: str, adapter_id: str, cfg: ClientConfig, files: dict[str, bytes]) -> None:
        a = ADAPTERS[adapter_id]()
        try:
            out: Any = enc_discovered(a.discover(cfg, FakeFS(files)))
        except Exception as exc:  # noqa: BLE001
            out = {"error": f"{type(exc).__name__}: {str(exc)[:200]}"}
        cases.append({"name": name, "adapter": adapter_id, "cfg": enc_cfg(cfg), "fs": {k: blob(v) for k, v in files.items()},
                      "result": out})

    real = {
        f"{P}/.claude/settings.json": json.dumps({
            "permissions": {"deny": ["Read(./**/secrets/**)", 5, None], "ask": ["Bash(curl:*)", "WebFetch", "Unknown", "Bash"],
                            "allow": ["Bash(docker compose:*)", " Edit(src/**) ", "Write(x)", "WebFetch(domain:x.org)", "WebFetch(x)",
                                      "mcp__serena", "mcp__s(x)", "Task", "Bad Name(", "Bash()", "Read(a(b))", "Glob", "MultiEdit",
                                      "NotebookEdit", "TodoWrite", "Grep(x)", "Ünïcode", "mcp__"]}, "hooks": {}}).encode(),
        f"{P}/.claude/agents/reviewer.md": b"---\nname: reviewer\ndescription: Use when broken.\ntools: Read, Grep, Glob, Bash\nmodel: sonnet\n---\n\nYou review.\n",
        f"{P}/.claude/agents/writer.md": b"---\ndescription: [1, 2]\ntools: Write, Edit, MultiEdit, Teleport, , WebSearch\nmodel: opus\n---\nW.\n",
        f"{P}/.claude/agents/nofm.md": b"no frontmatter at all\n",
        f"{P}/.claude/agents/inherit.md": b"---\ndescription: d\nmodel: 5\n---\n\n\n\nbody\n",
        f"{P}/.claude/agents/Bad Name.md": b"x",
        f"{P}/.claude/agents/notes.txt": b"x",
        f"{P}/.claude/agents/bomb.md": f"---\n{ALIAS_BOMB}---\nbody".encode(),
        f"{P}/.claude/skills/example-skill/SKILL.md": b"---\nname: example-skill\ndescription: Bring up a model.\n---\n\n# Serve\n",
        f"{P}/.claude/skills/example-skill/ref/notes.md": b"notes",
        f"{P}/.claude/skills/example-skill/__pycache__/x.pyc": b"junk",
        f"{P}/.claude/skills/example-skill/node_modules/m/index.js": b"junk",
        f"{P}/.claude/skills/no-skill-md/README.md": b"x",
        f"{P}/.claude/skills/desc-list/SKILL.md": b"---\ndescription: [a]\n---\n",
        f"{P}/.claude/skills/desc-int/SKILL.md": b"---\ndescription: 12\nname: other\n---\n",
        f"{P}/.claude/skills/Bad Name/SKILL.md": b"x",
        f"{P}/.claude/skills/loose.md": b"x",
        f"{P}/CLAUDE.md": b"# Hand written\n",
        f"{P}/.mcp.json": json.dumps({"mcpServers": {
            "serena": {"command": "serena", "args": ["a", 1, None, True, 1.5], "env": {"K": "secret-value", "A": "x"}},
            "remote": {"url": "http://x/mcp", "type": "sse", "env": ["no"]}, "web": {"url": "http://y", "type": "http"},
            "listurl": {"url": [1]}, "bare": {}, "Bad Name": {"command": "x"}, "notdict": 5,
            "strargs": {"command": 7, "args": "abc"}, "dictargs": {"command": "c", "args": {"k": 1, "j": 2}}}}).encode(),
    }
    add("claude-code/real-layout", "claude-code", cc_cfg(), real)
    add("claude-code/empty", "claude-code", cc_cfg(), {})
    add("claude-code/no-roots", "claude-code", cc_cfg().model_copy(update={"roots": {}}), real)
    add("claude-code/claude-md-block", "claude-code", cc_cfg(), {f"{P}/CLAUDE.md": ("hand\n" + u.wrap_block("x")).encode()})
    add("claude-code/claude-md-non-utf8", "claude-code", cc_cfg(), {f"{P}/CLAUDE.md": b"caf\xe9 \xff\xfe ok"})
    for i, settings in enumerate([b"{not json", b"[1, 2]", b'{"permissions": [1]}', b'{"permissions": {"allow": "Bash"}}', b"", b"null",
                                  b'{"permissions": {"allow": ["Bash(x)"], "deny": {"a": 1}}}', b'\xef\xbb\xbf{"permissions": {}}',
                                  b'{"permissions": {"ask": [NaN, 1e400, 12345678901234567890123]}}']):
        add(f"claude-code/settings-{i}", "claude-code", cc_cfg(), {f"{P}/.claude/settings.json": settings})
    for i, mcp in enumerate([b"{bad", b"[]", b'{"mcpServers": [1]}', b'{"mcpServers": "x"}', b'{"mcpServers": 5}', b'{"mcpServers": true}',
                             b'{"mcpServers": null}', b'{"mcpServers": {}}', b'{"mcpServers": {"a": {"command": "c", "args": 5}}}',
                             b'{"mcpServers": {"a": {"command": "c", "args": null}}}', b'{"mcpServers": {"b": {"url": "u", "args": 5}}}',
                             b'{"mcpServers": {"z": {"command": "z"}, "a": {"command": "a"}, "m": {"url": "m"}}}',
                             b'{"mcpServers": {"a": {"url": "", "command": ""}}}', b'{"mcpServers": {"a": {"command": "x"}, "a": {"url": "y"}}}']):
        add(f"claude-code/mcp-{i}", "claude-code", cc_cfg(), {f"{P}/.mcp.json": mcp})

    oc_real = {
        f"{P}/.opencode/agent/reviewer.md": b"---\ndescription: Reviews.\nmode: subagent\ntemperature: 0.1\ntools:\n  write: false\n  edit: false\npermission:\n  bash: ask\n---\n\nYou review.\n",
        f"{P}/.opencode/agent/primary.md": b"---\ndescription: P\nmode: primary\nmodel: provider/x\ntools:\n  bash: 0\n  read: false\n  task: null\npermission:\n  edit: deny\n---\nB\n",
        f"{P}/.opencode/agent/plain.md": b"no frontmatter",
        f"{P}/.opencode/agent/listtools.md": b"---\ntools: [read]\npermission: deny\nmodel: ''\n---\n",
        f"{P}/.opencode/agent/Bad.md": b"x",
        f"{P}/.opencode/agent/x.txt": b"x",
        f"{P}/opencode.json": json.dumps({
            "instructions": ["AGENTS.md"],
            "mcp": {"example": {"type": "local", "enabled": False, "command": ["./run-sandboxed.sh", "--", "npx", 5]},
                    "remote": {"type": "remote", "url": "http://r"}, "remote-nourl": {"type": "remote"},
                    "envs": {"command": ["c"], "environment": {"B": "1", "A": "2"}}, "emptycmd": {"command": []},
                    "strcmd": {"command": "c"}, "Bad Name": {"command": ["x"]}, "notdict": [1]},
            "permission": {"edit": "allow", "webfetch": "deny", "bash": {"*": "ask", "ls *": "allow", "sudo *": "deny", "x": "maybe", "y": 1},
                           "read": "allow", "zzz": "deny", "list": ["x"]}}).encode(),
        f"{P}/AGENTS.md": b"# Repo\n",
    }
    add("opencode/real-layout", "opencode", oc_cfg(), oc_real)
    add("opencode/real-layout-skills", "opencode", oc_cfg(skills_via_claude_dir=True),
        {**oc_real, f"{P}/.claude/skills/s1/SKILL.md": b"---\nname: s1\ndescription: S\n---\n"})
    add("opencode/agents-md-block", "opencode", oc_cfg(), {f"{P}/AGENTS.md": u.wrap_block("x").encode()})
    for i, cj in enumerate([b"// comment\n{}", b"[]", b'{"mcp": [1], "permission": [1]}', b'{"permission": {"bash": "ask", "edit": 5}}',
                            b"", b'{"mcp": {"a": {"type": "remote", "url": ["x"]}}}']):
        add(f"opencode/config-{i}", "opencode", oc_cfg(), {f"{P}/opencode.json": cj})

    gspec = spec_with(agents={"root": "project", "path": ".acme/agents", "filename": "agent.{name}.md", "format": "md_frontmatter",
                              "fields": {"name": "name", "about": "description"}, "tools_field": "allowed", "tools_format": "list",
                              "tool_map": {"edit": "fs.write", "write": "fs.write2"}},
                      instructions={"root": "project", "file": "docs/ACME.md", "mode": "own"})
    g_files = {
        "/w/acme/.acme/skills/s1/SKILL.md": b"---\nname: s1\ndescription: One\n---\n",
        "/w/acme/.acme/skills/s1/x/y.txt": b"y",
        "/w/acme/.acme/agents/agent.a1.md": b"---\nabout: First\nallowed: fs.read, sh, nope\n---\nA1\n",
        "/w/acme/.acme/agents/agent.a2.md": b"---\nabout: Second\nallowed: [fs.read, fs.write, 5]\n---\nA2\n",
        "/w/acme/.acme/agents/agent.a3.md": b"---\nallowed: {fs.read: true, sh: false, fs.write2: 1}\ndescription: ignored\n---\nA3\n",
        "/w/acme/.acme/agents/agent.a4.md": b"---\nallowed: 5\n---\n",
        "/w/acme/.acme/agents/agent.Bad.md": b"x",
        "/w/acme/.acme/agents/agentXa5.md": b"x",
        "/w/acme/.acme/agents/agent.a6.md.bak": b"x",
        "/w/acme/docs/ACME.md": b"Whole file\n",
    }
    add("generic/discover", "generic", ge_cfg(gspec), g_files)
    add("generic/discover-flat-and-json", "generic", ge_cfg(spec_with(skills={"root": "project", "path": "c", "layout": "flat", "filename": "{name}.md"})), g_files)
    add("generic/discover-invalid-spec", "generic", ge_cfg({"bogus": 1, "skills": 5}), g_files)
    add("generic/discover-no-spec", "generic", ge_cfg(None), g_files)
    add("generic/discover-minimal", "generic", ge_cfg({"version": 1, "skills": {"root": "project", "path": ".acme/skills"}}), g_files)

    lab = "/src"
    h_files = {
        f"{lab}/skills-src/assemble-video/SKILL.md": b"---\nname: assemble-video\ndescription: Assemble clips.\n---\n\nbody",
        f"{lab}/skills-src/assemble-video/tool.py": b"print(1)",
        f"{lab}/skills-src/assemble-video/__pycache__/tool.pyc": b"junk",
        f"{lab}/skills-src/assemble-video/lib/x.pyc": b"junk",
        f"{lab}/skills-src/Bad Name/SKILL.md": b"x",
        f"{lab}/skills-src/no-md/x.txt": b"x",
        f"{lab}/skills-src/loose-file.md": b"x",
        f"{lab}/workspace-AGENTS.md": b"# Workspace\n\nrules\n",
        f"{lab}/Bad.Name.MD": b"other",
        f"{lab}/agent.config.yaml": (
            b"_config_version: 34\nagent:\n  disabled_toolsets: [web, browser]\n"
            b"approvals:\n  mode: manual\n  deny:\n    - '*hermes update*'\n    - 5\n    - '*secret-key*'\n"
            b"mcp_servers:\n  serena:\n    command: /usr/local/bin/serena\n    args: [start-mcp-server, 5, true, 1.5, null, x]\n"
            b"    env:\n      SERENA_HOME: /opt/serena-home\n      TOKEN: super-secret-value\n      7: x\n"
            b"  remote:\n    url: http://x:1/mcp\n  zbad:\n    nothing: 1\n  Bad Name:\n    command: x\n  notdict: [1]\n"
            b"  strargs:\n    command: c\n    args: abc\n  listurl:\n    url: [1]\n    command: c\n"),
        f"{lab}/deny-str.yaml": b"approvals:\n  deny: abc\nagent: []\n",
        f"{lab}/deny-dict.yaml": b"approvals:\n  deny: {x: 1, 2: y}\nagent:\n  disabled_toolsets: none\n",
        f"{lab}/list.yaml": b"- a\n- b\n",
        f"{lab}/bomb.yaml": ALIAS_BOMB.encode(),
        f"{lab}/dup.yaml": b"a: 1\na: 2\n",
        f"{lab}/bad-args.yaml": b"mcp_servers:\n  a:\n    command: c\n    args: 5\n",
        f"{lab}/mixed-keys.yaml": b"mcp_servers:\n  b:\n    command: b\n  a:\n    url: http://a\n",
        f"{lab}/notes.md": b"Notes\n",
        f"{lab}/int-keys.yaml": b"mcp_servers:\n  1: {command: x}\n  a: {url: y}\n",
        "/w/stage/skills/staged/SKILL.md": b"---\nname: staged\ndescription: From the stage.\n---\n",
        "/w/stage/skills/assemble-video/SKILL.md": b"---\nname: assemble-video\ndescription: Stage copy.\n---\n",
        "/elsewhere/abs.md": b"absolute\n",
    }

    def hcfg(dirs: Any, **extra: Any) -> ClientConfig:
        c = he_cfg(source_root=lab, source_dirs=dirs)
        c.roots = {"stage": "/w/stage", "other": "/src/skills-src"}
        c.params.update(extra)
        return c

    add("hermes/discover-all", "hermes-agent", hcfg([
        {"kind": "skills", "path": "skills-src"}, {"kind": "instruction", "path": "workspace-AGENTS.md"},
        {"kind": "instruction", "path": "Bad.Name.MD"}, {"kind": "instruction", "path": "/elsewhere/abs.md"},
        {"kind": "config", "path": "agent.config.yaml"}, {"kind": "config", "path": "deny-str.yaml"},
        {"kind": "config", "path": "deny-dict.yaml"}, {"kind": "config", "path": "list.yaml"}, {"kind": "config", "path": "bomb.yaml"},
        {"kind": "config", "path": "dup.yaml"}, {"kind": "config", "path": "mixed-keys.yaml"},
        {"kind": "skills", "path": ".", "root": "other"}, {"kind": "skills", "path": "x", "root": "nope"},
        {"kind": "skills", "path": "../x"}, {"kind": "nope"}, "str", 5, {"kind": "config", "path": 5}, {"kind": "config", "path": "gone.yaml"},
        {"kind": "instruction", "path": "notes.md", "root": ""}, {"kind": "skills", "path": "skills-src", "root": None},
    ]), h_files)
    add("hermes/discover-bad-args", "hermes-agent", hcfg([{"kind": "config", "path": "bad-args.yaml"}]), h_files)
    add("hermes/discover-mixed-keys", "hermes-agent", hcfg([{"kind": "config", "path": "int-keys.yaml"}]), h_files)
    add("hermes/discover-not-list", "hermes-agent", hcfg({"kind": "skills"}), h_files)
    add("hermes/discover-no-source-root", "hermes-agent",
        he_cfg(source_dirs=[{"kind": "instruction", "path": "x.md"}]).model_copy(update={"roots": {"stage": "/w/stage"}}), h_files)
    c_none = he_cfg(source_dirs=[{"kind": "instruction", "path": "x.md"}])
    c_none.params["source_root"] = None
    add("hermes/discover-source-root-none", "hermes-agent", c_none, h_files)
    add("hermes/discover-stage-only", "hermes-agent", he_cfg(), h_files)

    add("turnstone/discover", "turnstone", ts_cfg(), {"/w/stage/skills/a/SKILL.md": b"---\ndescription: A\n---\n", "/w/stage/x.json": b"{}"})
    return cases


# ---- spec validation ------------------------------------------------------------------------------------------------------
def spec_cases() -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    specs: list[tuple[str, Any]] = [("acme", ACME), ("not-a-dict", [1]), ("str", "x"), ("none", None), ("empty", {}),
                                    ("default-config", generic.GenericSpecAdapter().default_config().spec)]
    for vname, v in [("true", True), ("float", 1.0), ("str", "1"), ("two", 2), ("none", None), ("list", [1])]:
        specs.append((f"version-{vname}", {"version": v}))
    bad_values = [("none", None), ("int", 5), ("float", 1.5), ("bool", True), ("str", "s"), ("list", ["a", 1]), ("dict", {"k": 1}),
                  ("date", datetime.date(2024, 1, 2)), ("bytes", b"x")]
    fields = {
        "notes": [], "tool_map": [], "model_tiers": [],
        "skills": ["root", "path", "layout", "filename"],
        "agents": ["root", "path", "filename", "format", "fields", "static", "tools_field", "tools_format", "model_field", "tool_map", "model_tiers"],
        "instructions": ["root", "file", "mode", "marker"],
        "mcp": ["root", "file", "format", "key_path", "entry", "env_style", "wrapper"],
        "permissions": ["root", "file", "format", "lists", "rule_templates"],
        "memory": ["root", "path", "filename", "frontmatter"],
    }
    for section, keys in fields.items():
        for bname, bv in bad_values:
            s = copy.deepcopy(ACME)
            s[section] = bv
            specs.append((f"{section}={bname}", s))
            for k in keys:
                s2 = copy.deepcopy(ACME)
                s2[section][k] = bv
                specs.append((f"{section}.{k}={bname}", s2))
        if keys:
            s3 = copy.deepcopy(ACME)
            s3[section]["zzz_extra"] = 1
            s3[section][5] = 2
            specs.append((f"{section}.extras", s3))
            s4 = copy.deepcopy(ACME)
            for k in keys[:2]:
                s4[section].pop(k, None)
            specs.append((f"{section}.missing", s4))
    for k, v in [("tool_map", {"read": 5, 3: "x", "teleport": "y", "browser": None, "write": ["l"]}),
                 ("model_tiers", {"fast": None, "huge": "x", 1: "y"})]:
        s = copy.deepcopy(ACME)
        s[k] = v
        specs.append((f"{k}-values", s))
        s = copy.deepcopy(ACME)
        s["agents"][k] = v
        specs.append((f"agents.{k}-values", s))
    agent_mut = [("static-values", {"static": {"a": [1], "b": None, "c": b"x", "d": 1.5, "e": {"x": 1}, "f": datetime.date(2024, 1, 1), 5: "k"}}),
                 ("fields-values", {"fields": {"a": "secrets", "b": 5, "c": "body", 1: "name"}}),
                 ("fields-md-body", {"format": "md_frontmatter", "fields": {"p": "body"}}),
                 ("fields-yaml-nobody", {"format": "yaml", "fields": {"n": "name"}}),
                 ("filename-noname", {"filename": "agent.md"}), ("filename-bad-ph", {"filename": "{id}-{name}.md"}),
                 ("filename-stray", {"filename": "{name}}.md"}), ("path-unsafe", {"path": "/abs"}), ("path-dots", {"path": "a/../b"}),
                 ("model-field-no-tiers", {"model_field": "m", "model_tiers": {}}), ("tools-format", {"tools_format": "xml"})]
    agent_mut.append(("static-odd-keys", {"static": {True: "a", False: "b", 1.5: "c", None: "d", datetime.date(2024, 1, 2): "e",
                                                     -3: "f", 10**20: "g", b"ok": "h"}}))
    for name, upd in agent_mut:
        s = copy.deepcopy(ACME)
        s["agents"].update(upd)
        if name == "model-field-no-tiers":
            s["model_tiers"] = {}
        specs.append((f"agents.{name}", s))
    mcp_mut = [("entry-bad-ph", {"entry": {"stdio": {"exec": "{shell}", "k": ["{x}", {"y": "{z}{name}"}], "{name}": "v", "a{b": "c}"}}}),
               ("entry-empty", {"entry": {}}), ("entry-grpc", {"entry": {"grpc": {}, "stdio": {"a": "{argv}"}}}),
               ("key-path-bad", {"key_path": "a b"}), ("key-path-dots", {"key_path": "a..b"}), ("key-path-ok", {"key_path": "a.b-c_d.E9"}),
               ("file-unsafe", {"file": "C:\\x"}), ("format-toml", {"format": "toml"}), ("env-style", {"env_style": "other"}),
               ("wrapper-ints", {"wrapper": ["a", 1]}), ("entry-nested-keys", {"entry": {"stdio": {"a": {"b{": 1, 2: "{name}"}}}}),
               ("entry-evil", {"entry": {"stdio": {"e": e} for e in ["{0.__class__}"]}}),
               ("entry-dollar", {"entry": {"stdio": {"e": "${name}"}}}), ("entry-double-brace", {"entry": {"stdio": {"e": "{{name}}"}}}),
               ("entry-format-spec", {"entry": {"stdio": {"e": "{name:>9}", "f": "{name!r}", "g": "{NAME}", "h": "{}"}}})]
    for name, upd in mcp_mut:
        s = copy.deepcopy(ACME)
        s["mcp"].update(upd)
        specs.append((f"mcp.{name}", s))
    perm_mut = [("lists-bad", {"lists": {"maybe": "x", "allow": "a b", "deny": "ok.path"}}), ("lists-empty", {"lists": {}}),
                ("templates-bad", {"rule_templates": {"command": "{cmd}", "teleport": "x", "tool": ["{tool}", "{match}{x}", "}"]}}),
                ("templates-wrong-types", {"rule_templates": {"command": [1, "x"], "tool": 5, "path_read": None}}),
                ("file-unsafe", {"file": "a/./b"})]
    for name, upd in perm_mut:
        s = copy.deepcopy(ACME)
        s["permissions"].update(upd)
        specs.append((f"permissions.{name}", s))
    other = [("skills-flat-noname", {"skills": {"root": "p", "path": "s", "layout": "flat", "filename": "x.md"}}),
             ("skills-flat-bad-ph", {"skills": {"root": "p", "path": "s", "layout": "flat", "filename": "{id}{name}.md"}}),
             ("skills-dir-ph", {"skills": {"root": "p", "path": "s", "filename": "{name}.md"}}),
             ("skills-dir-unsafe-filename", {"skills": {"root": "p", "path": "s", "filename": "../SKILL.md"}}),
             ("skills-path-ctrl", {"skills": {"root": "p", "path": "a\x07b"}}),
             ("skills-path-backslash", {"skills": {"root": "p", "path": "a\\b"}}),
             ("skills-path-empty", {"skills": {"root": "p", "path": ""}}),
             ("instructions-file-dot", {"instructions": {"root": "p", "file": "."}}),
             ("memory-noid", {"memory": {"root": "p", "path": "m", "filename": "x.md"}}),
             ("memory-bad-ph", {"memory": {"root": "p", "path": "m", "filename": "{name}-{id}.md"}}),
             ("memory-unsafe-path", {"memory": {"root": "p", "path": "../m"}}),
             ("top-extras", {"wat": 1, "zz": None, 7: "x"}),
             ("top-odd-keys", {True: 1, 2.5: 1, None: 1, datetime.date(2024, 1, 2): 1, "tool_map": {False: "x", 1.5: None}})]
    for name, upd in other:
        s = copy.deepcopy(ACME)
        s.update(upd)
        specs.append((name, s))
    for ex in ("cursor", "codex-cli", "gemini-cli"):
        data = yaml.safe_load((REPO / "examples" / "clients" / f"{ex}.yaml").read_text())
        specs.append((f"example-{ex}", data["spec"]))
    for name, spec in specs:
        diags = generic.validate_spec(spec)  # type: ignore[arg-type]
        caps = generic.caps_for_spec(spec)  # type: ignore[arg-type]
        # caps_for_spec of a spec with any diagnostic is AdapterCaps(notes="invalid spec"): recorded as null
        assert bool(diags) == (caps.notes == "invalid spec" and not caps.skills and not caps.tool_map)
        out.append({"name": name, "spec": enc(spec), "diagnostics": [d.model_dump() for d in diags],
                    "caps": None if diags else caps.model_dump()})
    return out


# ---- _util helpers ---------------------------------------------------------------------------------------------------------
def util_cases() -> dict[str, Any]:
    fm_docs: list[Any] = [{"name": "n", "description": "a: b # c '\" --- {x}", "tools": "Read, Bash"}]
    fm_docs += [{"description": d} for d in HOSTILE_DESCS]
    fm_docs += [{"id": "m1", "type": "project", "title": "Fact"}, {"temperature": 0.1, "n": 5, "b": False, "none": None, "l": ["a", 1], "d": {"k": "v"}},
                {"tools": {"edit": False, "write": False}, "permission": {"bash": "ask"}}, {}, {"empty_list": [], "empty_dict": {}}]
    render_fm = []
    for f in fm_docs:
        for body in ("Body.\n", "  \n", "x\n\n---\nnot frontmatter\n\n"):
            render_fm.append({"fields": enc(f), "body": body, "text": u.render_frontmatter(f, body)})
    parse_texts = ["no fm", "---\n: : [\n---\nx", "---\n- a\n---\nx", "---\nunterminated: 1\n", "---\nname: ok\n---\nb",
                   "---  \nname: ok\n---   \n\n\nbody", "---\r\nname: ok\r\n---\r\nbody", "---x\nname: 1\n---\n", "--\n", "",
                   "---\n---\nbody", "---\n\n---\n\nbody\n", "---\n1: a\nTrue: b\nnull: c\n1.5: d\n2024-01-01: e\n---\n",
                   "---\nk: 1\nk: 2\n---\n", "---\nx: " + "[" * 200 + "]" * 200 + "\n---\n", f"---\n{ALIAS_BOMB}---\nbody\n",
"---\nn: [1, {a: b}]\nd: 2024-05-06\nf: 1.0\ns: 'yes'\nb: yes\n---\n\n\n x\n",
                   "---\n" + "k" * 300 + ": v\n---\n", "  ---\nname: x\n---\n", "---\nname: x\n --- \nbody"]
    parse_fm = []
    for t in parse_texts:
        fm, body = u.parse_frontmatter(t)
        parse_fm.append({"text": t, "fm": enc(fm), "body": body})
    yaml_docs = [{"b": 1, "a": 2}, {"mcp_servers": {"serena": {"command": "serena", "args": ["start-mcp-server"]}, "docs": {"url": "http://x"}}},
                 {"approvals": {"deny": ["*sudo *", "**/x/**"]}, "agent": {"disabled_toolsets": list(hermes.BASELINE_DISABLED)}},
                 {"approvals": {"deny": []}, "agent": {"disabled_toolsets": []}}, {"s": [d for d in HOSTILE_DESCS]}]
    dump_yaml = [{"doc": enc(d), "text": u.dump_yaml(d).decode()} for d in yaml_docs]
    json_docs = [{"b": 1, "a": [2, 1]}, {"x": {"é": "ü😀", "n": None, "f": 0.1, "t": True, "e": {}, "l": []}}, {"k": "a\"\\\n\x01\x7f\u2028"}]
    dump_json = [{"doc": d, "text": u.dump_json(d).decode()} for d in json_docs]
    regions = []
    texts = ["", "plain text\n", u.wrap_block("body"), "before\n" + u.wrap_block("body") + "after\n", u.MD_BEGIN + "\nno end\n",
             "  " + u.MD_BEGIN + "\nx\n" + u.MD_END + "\n", u.MD_BEGIN + "\nx\n" + u.MD_END + "  \n", u.MD_BEGIN + "\nx\n" + u.MD_END.upper() + "\n",
             u.MD_BEGIN + "\na\n" + u.MD_END + "\nb\n" + u.MD_END + "\n", u.wrap_block("k = 1", "hash"), "x\n" + u.wrap_block("k = 1", "hash") + "y",
             u.HASH_BEGIN + "\nk\n", u.MD_BEGIN + "\r\nx\r\n" + u.MD_END + "\r\n", "é\u2028" + u.wrap_block("ü😀"), "a\rb\r" + u.MD_BEGIN + "\rx\r" + u.MD_END,
             "x\x0c" + u.MD_BEGIN + "\x0bx\n" + u.MD_END + "\x85y", u.MD_BEGIN + "x\n" + u.MD_END, "<!-- agent-hub:begin\n" + u.MD_END + "\u2029tail",
             u.MD_BEGIN + "\n" + u.MD_END]
    for t in texts:
        for style in ("md", "hash"):
            r = u.find_region(t, style)
            regions.append({"text": t, "style": style, "region": list(r) if isinstance(r, tuple) else r,
                            "extract": u.extract_block(t, style), "replace": u.replace_block(t, u.wrap_block("NEW", style), style)})
    wraps = []
    for body in ["hello", "  spaced\n\n", "a # comment\n<!-- normal comment -->\nagent-hub is a tool", "x\u2028<!-- agent-hub:end -->",
                 "ok\n  <!--   AGENT-HUB:END -->\nmore", "ok\n#  Agent-Hub:end", "x\x1c#agent-hub:x", "\u3000#agent-hub:", "x\r<!--\tagent-hub:",
                 "x <!--agent-hub", "#agent_hub:", "a\x0b# agent-hub:b", "ünï ✓", ""]:
        for style in ("md", "hash"):
            try:
                w: Any = u.wrap_block(body, style)
            except u.MarkerInjection as exc:
                w = {"error": str(exc)}
            wraps.append({"body": body, "style": style, "out": w})
    relpaths = []
    for parts in [["../x"], ["a/../b"], ["/abs"], ["a\\b"], ["a\x00b"], [""], ["."], ["a//b"], ["C:\\x"], ["a/\x07"], ["c:x"], ["1:x"],
                  [".claude/skills", "x", "sub/f.md"], ["a", "b/"], ["ü/✓"], ["a", "."], ["x\x1f"], ["x\x7f"], ["a/b", "c:d"]]:
        try:
            r2: Any = u.safe_relpath(*parts)
        except u.UnsafePath as exc:
            r2 = {"error": str(exc)}
        relpaths.append({"parts": parts, "out": r2})
    slices = []
    for raw, merge, keys in [
        (b'{"mcpServers": {"a": {"command": "x"}}, "other": 1}', "json_keys", ["mcpServers"]),
        (b'{"b": {"z": [1, 2.5, "\xc3\xa9", null, true]}, "a": {"y": 1}}', "json_keys", ["b.z", "a", "a.y", "missing"]),
        (b'{"a": NaN, "b": Infinity, "c": 12345678901234567890123, "d": -0, "e": 1e5, "f": 1.0}', "json_keys", ["a", "b", "c", "d", "e", "f"]),
        (b'{"a": {"x": 1}, "a": {"y": 2}}', "json_keys", ["a"]), (b"not json", "json_keys", ["a"]), (b"[1]", "json_keys", ["a"]),
        (b'{"a": null}', "json_keys", ["a"]), (b"{}", "json_keys", []),
        (b"agent:\n  disabled_toolsets: [a]\nz: 1\n", "yaml_keys", ["agent.disabled_toolsets"]),
        (b"a:\n  d: 2024-01-02\n  b: !!binary aGk=\n  n: .nan\n  i: 1\n  f: 1.0\n  s: !!set {x: null}\n", "yaml_keys", ["a.d", "a.b", "a.i", "a.f", "a.n"]),
        (b"a: {1: x, 2: y}\n", "yaml_keys", ["a"]), (b"a: [&x 1, *x]\n", "yaml_keys", ["a"]), (b"- 1\n", "yaml_keys", ["a"]),
        (b"hand\n\n" + u.wrap_block("x").encode() + b"\ntrailer\n", "block", []), (b"x\n" + u.wrap_block("k=1", "hash").encode(), "block", []),
        (b"no block", "block", []), (b"caf\xe9", "own", []), (b"\xff" + u.wrap_block("x").encode(), "block", []),
    ]:
        try:
            d: Any = u.slice_digest(raw, merge, keys)
        except Exception as exc:  # noqa: BLE001
            d = {"error": f"{type(exc).__name__}: {exc}"}
        slices.append({"raw": blob(raw), "merge": merge, "managed_keys": keys, "digest": d})
    onelines = []
    for t in ["a\n---\nb", "plain", "  x  ", "a\u2028b\u00a0c\x1cd", "\t", ""]:
        o: list[Any] = []
        onelines.append({"text": t, "out": u.oneline(t, "i", o), "diags": [x.model_dump() for x in o]})
    substs = []
    vals = {"name": "N", "args": ["1", "2"], "env": {"A": "${A}"}, "empty": {}, "elist": [], "none": None, "num": "7"}
    for obj in ["x{name}y", "{args}", "{env}", "{empty}", "{elist}", "{none}", "{missing}", "a{none}b{missing}c", ["{args}", "{missing}", "{none}", "k"],
                {"a": "{none}", "b": {"c": "{empty}"}, 5: "{num}", "d": [1, None, True]}, 5, None, 1.5, "{0}", "{{name}}", "pre-{args}", "{name}{env}"]:
        try:
            r = generic.subst(obj, vals)
            r3: Any = {"drop": True} if r is generic._DROP else {"value": enc(r)}
        except ValueError as exc:
            r3 = {"error": str(exc)}
        substs.append({"obj": enc(obj), "out": r3})
    tomls = []
    for prefix, servers in [("mcp_servers", {"b": {"command": "x", "args": ["1", 2, 1.5, True, None]}, "a b": {"k": {"z": 1, "a": [{"q": "w"}]}}}),
                            ("x.y", {}), ("p", {"s": {"quoted\"k": "a\\b\"c\nd\re\tf", "u": "ü", "big": 10**20, "f": 1e20, "neg": -0.0}})]:
        tomls.append({"prefix": prefix, "servers": enc(servers), "text": generic.toml_tables(prefix, servers)})
    return {"render_frontmatter": render_fm, "parse_frontmatter": parse_fm, "dump_yaml": dump_yaml, "dump_json": dump_json,
            "regions": regions, "wrap_block": wraps, "safe_relpath": relpaths, "slice_digest": slices, "oneline": onelines,
            "subst": substs, "toml_tables": tomls}


# ---- hermes observe parsing ---------------------------------------------------------------------------------------------------
def b64(s: str | bytes) -> str:
    return base64.b64encode(s.encode() if isinstance(s, str) else s).decode()


def md(name: str, desc: str = "d") -> str:
    return f"---\nname: {name}\ndescription: {desc}\n---\nbody\n"


def observe_cases() -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []

    def doc(files: dict[str, Any], manifest: Any = None, truncated: Any = False) -> str:
        return json.dumps({"manifest": manifest if manifest is not None else [], "files": files, "truncated": truncated})

    files = {
        "devops/foo/SKILL.md": b64(md("foo-real", "Foo")), "devops/foo/scripts/x.sh": b64("x"),
        "devops/foo/refs/inner/SKILL.md": b64(md("inner")), "devops/foo/refs/inner/y.md": b64("y"),
        "mine/SKILL.md": b64("no frontmatter"), "lab/l/SKILL.md": b64(md("l")), "hub/h/SKILL.md": b64(md("h")),
        ".archive/old/SKILL.md": b64(md("old")), ".archive/bundledold/SKILL.md": b64(md("bundledold")), "stray.txt": b64("ignored"),
        "SKILL.md": b64(md("root")), "named/SKILL.md": b64(md("  spaced name  ", "x" * 3000)), "listname/SKILL.md": b64("---\nname: [a]\ndescription: [b]\n---\n"),
        "badyaml/SKILL.md": b64("---\nname: &x [*x\n---\n"), "long/SKILL.md": b64(md("n" * 5000, "d" * 9000)),
        "intname/SKILL.md": b64("---\nname: 5\ndescription: 6\n---\n"), "blank/SKILL.md": b64("---\nname: '   '\n---\n"),
        "bin/SKILL.md": b64(b"---\nname: b\xff\n---\n"), "crlf/SKILL.md": b64("---\r\nname: crlf-name\r\n---\r\n"),
    }
    raw_cases: list[tuple[str, str, str]] = [
        ("grouping", doc(files, ["foo:abc123", "bundledold:ff", "  : x", 5, "mine", "inner :z"]), "/opt/data/skills/hub"),
        ("grouping-no-dest", doc(files, ["foo"]), ""), ("grouping-dest-slashes", doc(files), "///"), ("dest-trailing", doc(files), "/a/lab/"),
        ("truncated", doc({"a/SKILL.md": b64(md("a"))}, truncated=True), ""), ("truncated-1", doc({"a/SKILL.md": b64(md("a"))}, truncated=1), ""),
        ("truncated-str", doc({"a/SKILL.md": b64(md("a"))}, truncated="true"), ""),
        ("list", "[]", ""), ("null", "null", ""), ("not-json", "not json", ""), ("files-list", '{"files": []}', ""), ("empty", "", ""),
        ("cut", '{"files": {}}'[:-3], ""), ("no-files", '{"manifest": []}', ""), ("trailing-ws", ' {"files": {}} \n\t', ""),
        ("extra-data", '{"files": {}} x', ""), ("nan", '{"files": {}, "x": NaN, "y": -Infinity}', ""), ("bigint", '{"files": {}, "x": 123456789012345678901234567890}', ""),
        ("bad-paths", doc({"ok/SKILL.md": b64(md("ok")), "../evil/SKILL.md": b64("x"), "/abs/SKILL.md": b64("x"), "a//b/SKILL.md": b64("x"),
                           "bad/SKILL.md": "not base64!!", "pad/SKILL.md": "QUJD\nREVG", "num/SKILL.md": 5, "nul/SKILL\u0000.md": b64("x"),
                           "bs/a\\b/SKILL.md": b64("x"), "c:/SKILL.md": b64("x"), "x/./SKILL.md": b64("x"), "t/\tSKILL.md": b64("x"),
                           "bidi/\u202e/SKILL.md": b64("x"), "zw/\u200b/SKILL.md": b64("x"), "nbsp/\u00a0/SKILL.md": b64(md("nbsp")),
                           "ls/\u2028/SKILL.md": b64(md("ls")), "u/ü/SKILL.md": b64(md("u"))}), ""),
        ("surrogates", json.dumps({"manifest": ["\ud800x"], "files": {"ok/SKILL.md": b64(md("ok")), "a/\ud800.txt": b64("x"),
                                                                      "s/\ud83d\ude00/SKILL.md": b64(md("emoji")), "v/SKILL.md": "QQ\ud800=="},
                                   "truncated": False}, ensure_ascii=True), ""),
        ("base64-strict", doc({f"b{i}/SKILL.md": v for i, v in enumerate(
            ["", "QQ==", "QQ=", "QQ", "Q", "QR==", "QUI=", "QUJD", "=QUJD", "QQ==QQ==", "QQ=A", "Q===", "QUJD====", "QU JD", "QUJD\n", "QUJ+/w==",
             "QUJ-_w==", "QQ===", "QUI==", "ab=c", "QUJD=", "QUJDRA=="])}), ""),
        ("collision", doc({"a/SKILL.md": b64(md("a")), "a/x": b64("f"), "a/x/y": b64("g"), "b/SKILL.md": b64(md("b")), "b/SKILL.md/z": b64("z")}), ""),
        ("deep-nesting-ok", '{"files": {}, "x": ' + "[" * 9000 + "]" * 9000 + "}", ""),
        ("deep-nesting-too-deep", '{"files": {}, "x": ' + "[" * 9998 + "]" * 9998 + "}", ""),
        ("dup-keys", '{"files": {"a/SKILL.md": "eA==", "a/SKILL.md": "' + b64(md("dup")) + '"}, "files2": 1}', ""),
        ("control-in-json", '{"files": {"a\tb/SKILL.md": "eA=="}}', ""),
        ("escapes", '{"files": {"e\\u0301/SKILL.md": "' + b64(md("e")) + '", "q\\"/SKILL.md": "eA==", "s\\/l/SKILL.md": "eA=="}}', ""),
        ("many-files", doc({f"s/f{i:05}": b64("x") for i in range(5002)} | {"s/SKILL.md": b64(md("s"))}), ""),
    ]
    for name, raw, dest in raw_cases:
        obs = hermes._parse_observation(raw, "ctr-1", "/opt/data/skills", dest)
        out.append({"name": name, "out": raw, "skills_dest": dest,
                    "result": {"skills": [{"path": s.path, "name": s.name, "description": s.description, "origin": s.origin,
                                           "files": {k: blob(v) for k, v in s.files.items()}} for s in obs.skills],
                               "source": obs.source, "notes": obs.notes, "complete": obs.complete}})
    big = " " * (hermes.MAX_OBSERVE_OUT + 1)
    obs = hermes._parse_observation(big, "c", "/r", "")
    out.append({"name": "oversize", "out_spaces": hermes.MAX_OBSERVE_OUT + 1, "skills_dest": "",
                "result": {"skills": [], "source": obs.source, "notes": obs.notes, "complete": obs.complete}})
    return out


# ---- turnstone helpers ---------------------------------------------------------------------------------------------------------
def turnstone_cases() -> dict[str, Any]:
    urls = ["http://127.0.0.1:8796", "http://127.0.0.1:8796/", "https://localhost", "http://LOCALHOST:1", "http://[::1]:8796", "http://[::1%lo]:1",
            "http://[::ffff:127.0.0.1]", "http://127.1.2.3", "http://127.0.0.1:0", "http://127.0.0.1:65536", "http://127.0.0.1:x",
            "http://127.0.0.1:", "http://evil.example:8796", "https://10.0.0.5", "http://user:pw@127.0.0.1:8796", "http://:@127.0.0.1",
            "http://@127.0.0.1", "http://127.0.0.1:8796/v1/api?x=1", "http://127.0.0.1?x", "http://127.0.0.1#f", "http://127.0.0.1/#",
            "ftp://127.0.0.1", "127.0.0.1:8796", "", "http://", "http://[::1", "http://::1]", "http://[127.0.0.1]", "http://[vx.y]",
            "http://[v1.x]", "http://a[::1]", "http://[::1]x", "HTTP://127.0.0.1", " http://127.0.0.1", "http://127.0.0.1 ",
            "http://127.0.0.01", "http://0x7f.0.0.1", "http://2130706433", "http://localhost.", "http://127.0.0.1:８０",
            "http://127.0.0.1:+80", "http://127.0.0.1:080", "http://loc\talhost", "\x00http://127.0.0.1", "http://[0:0:0:0:0:0:0:1]",
            "http://[::2]", "http://ｌocalhost", "http://127.0.0.1\\x", "http://127.0.0.1;x", "file:///x", "http:127.0.0.1", "http:/127.0.0.1",
            "http://127.0.0.1:8796%2F", "http://%31%32%37.0.0.1", "http://[::1]:80:80", "http://127.0.0.1:1:2"]
    problems = []
    for url in urls:
        problems.append({"url": url, "problem": turnstone.console_problem(url)})
    resources = []
    for files in [
        {"SKILL.md": b"See scripts/b.sh", **{f"scripts/{c}.sh": b"x" for c in "abcdefghijkl"}},
        {"SKILL.md": b"x", "refs/x": b"x", "references": b"x", "assets/\xff": b"x", "scripts/bad.bin": b"\xc3(", "ScRiPtS/x": b"x"},
        {"x.md": b"no SKILL.md"}, {},
    ]:
        resources.append({"files": {k: blob(v) for k, v in files.items()}, "out": [list(p) for p in turnstone.undelivered_resources(files)]})
    return {"console_problem": problems, "undelivered_resources": resources}


def meta() -> list[dict[str, Any]]:
    out = []
    for aid, cls in ADAPTERS.items():
        a = cls()
        out.append({"id": a.id, "module": cls.__module__.rsplit(".", 1)[-1], "display_name": a.display_name,
                    "docs": (type(a).__doc__ or "").strip()[:4000], "caps": a.caps().model_dump(), "default_config": a.default_config().model_dump(),
                    "registry_id": aid})
    return out


def main() -> int:
    data = {"meta": meta(), "render": render_cases(), "discover": discover_cases(), "spec": spec_cases(), "util": util_cases(),
            "observe": observe_cases(), "turnstone": turnstone_cases(),
            "constants": {"baseline_disabled": list(hermes.BASELINE_DISABLED), "observe_program": hermes._OBSERVE_PROGRAM,
                          "resource_dirs": list(turnstone.RESOURCE_DIRS), "max_resource_chars": turnstone.MAX_RESOURCE_CHARS,
                          "max_resources_per_skill": turnstone.MAX_RESOURCES_PER_SKILL}}
    text = json.dumps(data, indent=1, ensure_ascii=False, sort_keys=False) + "\n"
    Path(sys.argv[1]).write_text(text, encoding="utf-8")
    print(f"wrote {sys.argv[1]}: {len(data['render'])} render, {len(data['discover'])} discover, {len(data['spec'])} spec, "
          f"{len(data['observe'])} observe cases")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
