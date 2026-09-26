"""Claude Code adapter (id ``claude-code``): any Claude Code install, sandboxed or not.

Where things go (all under the configured ``project`` root, never under the user config dir):
  skills        .claude/skills/<name>/...           (dir-per-skill; SKILL.md standard passed through)
  agents        .claude/agents/<name>.md            (frontmatter name/description/tools/model)
  instructions  CLAUDE.md                           (params.instructions_mode: block = hub markers, own = whole file)
  mcp           .mcp.json                           (merge=json_keys on ``mcpServers``)            ADVISORY by default
  permissions   .claude/settings.json               (merge=json_keys on permissions.allow/ask/deny)  ADVISORY by default

If your user-level settings file is rendered by another process (a hardened template, a sync script) and denied to the model, keep the
hub away from it: this adapter only writes project-level files, and mcp/permissions are advisory until you opt in via ``manage``.

Layout evidence: Claude Code's documented project layout (.claude/settings.json permissions arrays, .claude/agents/*.md frontmatter,
.claude/skills/<name>/SKILL.md). Assumption: ``mcp__*`` is accepted as a wildcard in an agent ``tools`` list (we emit it).

Params: ``sandbox_wrapper`` (list of argv, e.g. ``["./run-sandboxed.sh", "--"]``) is prefixed to the command of every MCP server that
declares a ``sandbox_profile``; without it such a server is an ERROR (fail closed, never launched unsandboxed).
"""

from __future__ import annotations

import json
import re
from typing import Any

from . import _util as u
from .base import (
    AdapterCaps,
    AgentItem,
    Artifact,
    Check,
    ClientAdapter,
    ClientConfig,
    CommandRunner,
    Diag,
    DiscoveredContent,
    Expected,
    FileSystem,
    InstructionItem,
    McpItem,
    PermissionSet,
    RenderContext,
    RenderResult,
    Rule,
)

TOOL_MAP: dict[str, str | None] = {
    "read": "Read", "write": "Write", "edit": "Edit", "shell": "Bash", "web_fetch": "WebFetch",
    "web_search": "WebSearch", "mcp": "mcp__*", "subagent": "Task", "browser": None, "notebook": "NotebookEdit",
    "todo": "TodoWrite", "image": None,
}
# "read" also grants the search tools: an agent that can Read but not Grep/Glob cannot navigate a repo.
EXPANSION: dict[str, list[str]] = {"read": ["Read", "Grep", "Glob"]}
MODEL_TIERS = {"fast": "haiku", "standard": "sonnet", "deep": "opus"}
_REV_TOOLS = {"Read": "read", "Grep": "read", "Glob": "read", "Write": "write", "Edit": "edit", "MultiEdit": "edit",
              "Bash": "shell", "WebFetch": "web_fetch", "WebSearch": "web_search", "Task": "subagent",
              "NotebookEdit": "notebook", "TodoWrite": "todo"}
_REV_MODELS = {v: k for k, v in MODEL_TIERS.items()}
DEFAULT_MANAGE = {"skills": True, "agents": True, "instructions": True, "mcp": False, "permissions": False, "memory": False}


def _native_tools(caps: list[str]) -> list[str]:
    out: list[str] = []
    for c in caps:
        native = TOOL_MAP.get(c)
        for t in EXPANSION.get(c, [native] if native else []):
            if t not in out:
                out.append(t)
    return out


def _cmd_pattern(glob: str) -> str:
    """Hub command glob -> Claude ``Bash(...)`` matcher. ``docker compose *`` -> ``Bash(docker compose:*)``."""
    g = glob.strip()
    m = re.fullmatch(r"([^*?\[\]]+?)\s*\*", g)
    if m:
        return f"Bash({m.group(1).strip()}:*)"
    return f"Bash({g})"


def _rule_strings(rule: Rule) -> list[str] | None:
    if rule.kind == "tool":
        t = TOOL_MAP.get(rule.match)
        if t is None:
            return None
        return [t] if t != "mcp__*" else ["mcp__*"]
    if rule.kind == "command":
        return [_cmd_pattern(rule.match)]
    if rule.kind == "path_read":
        return [f"Read({rule.match})"]
    if rule.kind == "path_write":
        return [f"Edit({rule.match})", f"Write({rule.match})"]
    if rule.kind == "mcp_server":
        return [f"mcp__{rule.match}"]
    if rule.kind == "egress_host":
        return [f"WebFetch(domain:{rule.match})"]
    return None


def render_permissions(perms: PermissionSet) -> tuple[dict[str, list[str]], list[Diag]]:
    lists: dict[str, set[str]] = {"allow": set(), "ask": set(), "deny": set()}
    diags: list[Diag] = []
    for r in perms.rules:
        strs = _rule_strings(r)
        if strs is None:
            diags.append(u.diag("warn", "unsupported_rule", f"rule {r.kind}:{r.match} has no Claude Code equivalent"))
            continue
        lists[r.decision].update(strs)
    if any(r.kind == "egress_host" for r in perms.rules):
        diags.append(u.diag("info", "egress_advisory",
                            "egress_host rules render as WebFetch(domain:...) only; Bash network egress is governed by the "
                            "sandbox settings, which the hub never writes"))
    return {k: sorted(v) for k, v in lists.items()}, diags


def _mcp_entry(m: McpItem, wrapper: list[str]) -> dict[str, Any]:
    if m.transport == "stdio":
        argv = [m.command or ""] + list(m.args)
        if m.sandbox_profile:
            argv = wrapper + argv
        entry: dict[str, Any] = {"command": argv[0], "args": argv[1:]}
        if m.env_names:
            # names only: Claude Code expands ${VAR} from the launching environment; the value never enters the hub
            entry["env"] = {n: "${" + n + "}" for n in sorted(m.env_names)}
        return entry
    return {"type": m.transport, "url": m.url or ""}


class ClaudeCodeAdapter(ClientAdapter):
    id = "claude-code"
    display_name = "Claude Code"

    def caps(self) -> AdapterCaps:
        return AdapterCaps(
            skills=True, agents=True, instructions=True, mcp=True, permissions=True, egress=False, memory=False,
            tool_map=dict(TOOL_MAP), model_tiers=dict(MODEL_TIERS),
            notes="mcp and permissions are advisory by default (a user-level settings file may be owned by another process; the hub "
                  "only writes project files). Memory is not rendered.",
        )

    def default_config(self) -> ClientConfig:
        return ClientConfig(
            id="claude-code", adapter=self.id, display_name="Claude Code", strict=True,
            description="Claude Code. Skills/agents/CLAUDE.md managed; mcp/permissions advisory.",
            roots={"project": "~/path/to/project"},
            params={"manage": dict(DEFAULT_MANAGE), "sandbox_wrapper": []},
            icon="terminal",
        )

    # ------------------------------------------------------------------------------------------------ render (pure)
    def render(self, ctx: RenderContext) -> RenderResult:
        cfg = ctx.client
        diags: list[Diag] = []
        arts: list[Artifact] = []
        root = "project"
        arts += u.skill_artifacts(ctx.skills, root, ".claude/skills", diags)
        arts += self._agents(ctx, root, diags)
        if ctx.instructions:
            ins = u.instruction_artifact(ctx.instructions, root, "CLAUDE.md", cfg.params, diags)
            if ins is not None:
                arts.append(ins)
        if ctx.mcp_servers:
            arts.append(self._mcp(ctx, root, diags))
        if ctx.permissions.rules:
            lists, pd = render_permissions(ctx.permissions)
            diags += pd
            doc = {"permissions": lists}
            arts.append(Artifact(root=root, path=".claude/settings.json", content=u.dump_json(doc), kind="rule",
                                 source_ids=sorted({f"{r.kind}:{r.match}" for r in ctx.permissions.rules}), merge="json_keys",
                                 managed_keys=["permissions.allow", "permissions.ask", "permissions.deny"]))
        if ctx.memory:
            diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability",
                                "memory is not rendered for Claude Code (use the curated CLAUDE.md instructions instead)"))
        arts.sort(key=lambda a: (a.root, a.path))
        return RenderResult(artifacts=arts, diagnostics=diags)

    def _agents(self, ctx: RenderContext, root: str, diags: list[Diag]) -> list[Artifact]:
        cfg = ctx.client
        out: list[Artifact] = []
        dup = u.dedupe([a.name for a in ctx.agents], "agent", diags)
        for ag in sorted(ctx.agents, key=lambda a: a.name):
            if ag.name in dup:
                continue
            art = self._agent(ag, cfg, root, diags)
            if art is not None:
                out.append(art)
        return out

    def _agent(self, ag: AgentItem, cfg: ClientConfig, root: str, diags: list[Diag]) -> Artifact | None:
        if not u.valid_name(ag.name):
            diags.append(u.diag("error", "invalid_name", f"agent name {ag.name!r} is not a valid identifier", ag.name))
            return None
        if not u.check_description(ag.description, ag.name, diags) or len(ag.body) > u.MAX_BODY_CHARS:
            if len(ag.body) > u.MAX_BODY_CHARS:
                diags.append(u.diag("error", "body_too_large", "agent body exceeds the size cap", ag.name))
            return None
        for c in u.unknown_caps(ag.capabilities):
            diags.append(u.diag("error", "unknown_capability", f"unknown capability {c!r}", ag.name))
        caps = u.effective_caps([c for c in ag.capabilities if c in TOOL_MAP], ag.read_only)
        for c in caps:
            if TOOL_MAP[c] is None:
                diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability",
                                    f"capability {c!r} has no Claude Code tool", ag.name))
        if ag.read_only and set(ag.capabilities) & set(u.READ_ONLY_DROPS):
            diags.append(u.diag("info", "read_only_applied", "read_only: write/edit/notebook removed", ag.name))
        tools = _native_tools(caps)
        if not tools:
            # omitting `tools:` would make Claude Code inherit EVERY tool: fail closed instead
            diags.append(u.diag("error", "empty_toolset", "agent would have no tools (Claude Code would inherit all)", ag.name))
            return None
        fm: dict[str, Any] = {"name": ag.name, "description": u.oneline(ag.description, ag.name, diags), "tools": ", ".join(tools)}
        if ag.model_tier is not None:
            fm["model"] = MODEL_TIERS[ag.model_tier]
        if ag.mode == "primary":
            diags.append(u.diag("info", "mode_ignored", "Claude Code agents are subagents; mode=primary has no effect", ag.name))
        text = u.render_frontmatter(fm, ag.body)
        return Artifact(root=root, path=u.safe_relpath(".claude/agents", f"{ag.name}.md"), content=u.to_bytes(text),
                        kind="agent", source_ids=[ag.name])

    def _mcp(self, ctx: RenderContext, root: str, diags: list[Diag]) -> Artifact:
        wrapper = u.sandbox_prefix(ctx.client.params)
        servers: dict[str, Any] = {}
        for m in sorted(ctx.mcp_servers, key=lambda x: x.name):
            if not u.valid_name(m.name):
                diags.append(u.diag("error", "invalid_name", f"mcp name {m.name!r} is not valid", m.name))
                continue
            if m.scan_status != "clean":
                diags.append(u.diag("info", "mcp_not_clean", f"scan_status={m.scan_status}; the core blocks enabling unscanned servers", m.name))
            if m.transport == "stdio" and not m.command:
                diags.append(u.diag("error", "mcp_no_command", "stdio server without a command", m.name))
                continue
            if m.transport != "stdio" and not m.url:
                diags.append(u.diag("error", "mcp_no_url", f"{m.transport} server without a url", m.name))
                continue
            if m.sandbox_profile and not wrapper:
                u.sandbox_missing(m.name, diags)
                continue
            servers[m.name] = _mcp_entry(m, wrapper)
        return Artifact(root=root, path=".mcp.json", content=u.dump_json({"mcpServers": servers}), kind="mcp",
                        source_ids=sorted(servers), merge="json_keys", managed_keys=["mcpServers"])

    # ------------------------------------------------------------------------------------------------ discover
    def discover(self, cfg: ClientConfig, fs: FileSystem) -> DiscoveredContent:
        res = DiscoveredContent()
        root = "project"
        res.skills = u.discover_skills(fs, cfg, root, ".claude/skills", res.notes)
        res.agents = self._discover_agents(cfg, fs, res.notes)
        res.instructions = self._discover_instructions(cfg, fs)
        if res.instructions:
            res.suggested_params = {"instructions_mode": "own"}
        res.permissions = self._discover_permissions(cfg, fs, res.notes)
        res.mcp_servers = self._discover_mcp(cfg, fs, res.notes)
        return res

    def _discover_agents(self, cfg: ClientConfig, fs: FileSystem, notes: list[str]) -> list[AgentItem]:
        base = u.fs_path(cfg, "project", ".claude/agents")
        out: list[AgentItem] = []
        if base is None or not fs.exists(base) or not fs.is_dir(base):
            return out
        for fname in sorted(fs.listdir(base)):
            if not fname.endswith(".md"):
                continue
            name = fname[:-3]
            if not u.valid_name(name):
                notes.append(f"skipped agent file {fname!r}: not a valid hub name")
                continue
            fm, body = u.parse_frontmatter(u.read_text(fs, f"{base}/{fname}"))
            raw_tools = u.cap_str(fm.get("tools"), 2000)
            names = [t.strip() for t in raw_tools.split(",") if t.strip()]
            caps = u.ordered_caps({_REV_TOOLS[t] for t in names if t in _REV_TOOLS})
            for t in names:
                if t not in _REV_TOOLS:
                    notes.append(f"agent {name}: tool {t!r} has no canonical capability (dropped)")
            if not names:
                notes.append(f"agent {name}: no tools: line, Claude inherits all tools; imported with no capabilities")
            tier = _REV_MODELS.get(u.cap_str(fm.get("model"), 100))
            out.append(AgentItem(name=name, description=u.cap_str(fm.get("description")), capabilities=caps, model_tier=tier,
                                 read_only="write" not in caps and "edit" not in caps, body=body))
        return out

    def _discover_instructions(self, cfg: ClientConfig, fs: FileSystem) -> list[InstructionItem]:
        p = u.fs_path(cfg, "project", "CLAUDE.md")
        if p is None or not fs.exists(p):
            return []
        text = u.read_text(fs, p)
        blk = u.extract_block(text, "md")
        if blk is not None:
            return []      # the hub's own block: nothing new to import
        return [InstructionItem(id="claude-md", title="CLAUDE.md", order=0, body=text)]

    def _discover_permissions(self, cfg: ClientConfig, fs: FileSystem, notes: list[str]) -> PermissionSet | None:
        p = u.fs_path(cfg, "project", ".claude/settings.json")
        if p is None or not fs.exists(p):
            return None
        try:
            doc = json.loads(u.read_text(fs, p) or "null")
        except (ValueError, OSError):
            notes.append(".claude/settings.json is not valid JSON")
            return None
        perms = doc.get("permissions") if isinstance(doc, dict) else None
        if not isinstance(perms, dict):
            return None
        rules: list[Rule] = []
        for dec in ("deny", "ask", "allow"):
            for s in perms.get(dec, []) if isinstance(perms.get(dec), list) else []:
                r = _parse_rule(str(s), dec)
                if r is None:
                    notes.append(f"permission {s!r} not representable as an abstract rule")
                else:
                    rules.append(r)
        return PermissionSet(rules=rules)

    def _discover_mcp(self, cfg: ClientConfig, fs: FileSystem, notes: list[str]) -> list[McpItem]:
        p = u.fs_path(cfg, "project", ".mcp.json")
        if p is None or not fs.exists(p):
            return []
        try:
            doc = json.loads(u.read_text(fs, p) or "null")
        except (ValueError, OSError):
            notes.append(".mcp.json is not valid JSON")
            return []
        servers = doc.get("mcpServers") if isinstance(doc, dict) else None
        out: list[McpItem] = []
        for name, e in sorted((servers or {}).items()):
            if not isinstance(e, dict) or not u.valid_name(str(name)):
                notes.append(f"skipped mcp server {name!r}")
                continue
            env_names = sorted(e["env"]) if isinstance(e.get("env"), dict) else []   # names only, values dropped
            if e.get("url"):
                t = "sse" if e.get("type") == "sse" else "http"
                out.append(McpItem(name=name, transport=t, url=u.cap_str(e["url"], 2000), env_names=env_names))
            else:
                out.append(McpItem(name=name, transport="stdio", command=str(e.get("command", "")),
                                   args=[str(a) for a in e.get("args", [])], env_names=env_names))
        return out

    # ------------------------------------------------------------------------------------------------ verify
    def verify(self, cfg: ClientConfig, expected: list[Expected], fs: FileSystem, run: CommandRunner) -> list[Check]:
        # Claude Code has no non-interactive `skills list`; files + hashes is the strongest offline proof available.
        return u.verify_expected(cfg, expected, fs)


def _parse_rule(s: str, decision: str) -> Rule | None:
    m = re.fullmatch(r"(\w+)(?:\((.*)\))?", s.strip())
    if not m:
        return None
    tool, arg = m.group(1), m.group(2)
    dec: Any = decision
    if tool == "Bash" and arg is not None:
        a = arg[:-2] + " *" if arg.endswith(":*") else arg
        return Rule(kind="command", match=a, decision=dec)
    if tool == "Read" and arg is not None:
        return Rule(kind="path_read", match=arg, decision=dec)
    if tool in ("Edit", "Write") and arg is not None:
        return Rule(kind="path_write", match=arg, decision=dec)
    if tool == "WebFetch" and arg and arg.startswith("domain:"):
        return Rule(kind="egress_host", match=arg[7:], decision=dec)
    if tool.startswith("mcp__") and arg is None:
        return Rule(kind="mcp_server", match=tool[5:], decision=dec)
    if arg is None and tool in _REV_TOOLS:
        return Rule(kind="tool", match=_REV_TOOLS[tool], decision=dec)
    return None


ADAPTER = ClaudeCodeAdapter
