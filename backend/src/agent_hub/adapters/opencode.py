"""opencode adapter (id ``opencode``).

  agents        .opencode/agent/<name>.md   frontmatter: description, mode, temperature, tools{...}, permission{...}
  instructions  AGENTS.md                   merge=block (hub markers). opencode also reads it via opencode.json ``instructions``
  opencode.json merge=json_keys on ``instructions`` / ``mcp`` / ``permission``   ADVISORY by default (the file is hand-tuned:
                                            provider/model blocks and a long bash allow/deny list live there)
  skills        NONE. opencode has no skills directory that we could confirm. Param ``skills_via_claude_dir`` (default false)
                renders skills to .claude/skills for the case where opencode is found to read that path.

Layout evidence: .opencode/agent/reviewer.md (description/mode/temperature/tools{write:false,edit:false}/permission{bash:ask});
opencode.json (``instructions`` list, ``mcp.<name>{type:local,enabled,command:[...]}``, ``permission{edit, bash{glob:decision}}``).
Assumptions (they depend on your opencode version): ``environment``/``{env:VAR}`` for MCP env, ``type: remote`` for http/sse, the ``task`` and
``todowrite`` tool names, whether the bash permission map is first-match or last-match (we emit ``"*"`` first, then rules in order).
"""
from __future__ import annotations

import json
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
    "read": "read", "write": "write", "edit": "edit", "shell": "bash", "web_fetch": "webfetch",
    "web_search": None, "mcp": None, "subagent": "task", "browser": None, "notebook": None, "todo": "todowrite", "image": None,
}
# tools opencode enables by default; an agent that lacks the capability must switch the tool OFF explicitly.
_GATED = ("read", "write", "edit", "shell", "web_fetch", "subagent", "todo")
DEFAULT_MANAGE = {"skills": False, "agents": True, "instructions": True, "mcp": False, "permissions": False, "memory": False}


class OpenCodeAdapter(ClientAdapter):
    id = "opencode"
    display_name = "opencode"

    def caps(self) -> AdapterCaps:
        return AdapterCaps(
            skills=False, agents=True, instructions=True, mcp=True, permissions=True, egress=False, memory=False,
            tool_map=dict(TOOL_MAP), model_tiers={},
            notes="No skills directory is known for opencode (param skills_via_claude_dir=true renders to .claude/skills; "
                  "whether opencode reads it depends on your opencode version). Model tiers are unmapped by default: set params.model_tiers {fast/standard/deep: provider/model}.",
        )

    def default_config(self) -> ClientConfig:
        return ClientConfig(
            id="opencode", adapter=self.id, display_name="opencode", strict=False,
            description="opencode: agents + AGENTS.md managed; opencode.json advisory; skills off.",
            roots={"project": "~/path/to/project"},
            params={"manage": dict(DEFAULT_MANAGE), "skills_via_claude_dir": False, "temperature": 0.1,
                    "model_tiers": {}, "sandbox_wrapper": []},
            icon="code",
        )

    def _tiers(self, cfg: ClientConfig) -> dict[str, str]:
        t = cfg.params.get("model_tiers")
        return {str(k): str(v) for k, v in t.items()} if isinstance(t, dict) else {}

    # ------------------------------------------------------------------------------------------------ render (pure)
    def render(self, ctx: RenderContext) -> RenderResult:
        cfg = ctx.client
        diags: list[Diag] = []
        arts: list[Artifact] = []
        root = "project"
        if ctx.skills:
            if cfg.params.get("skills_via_claude_dir") is True:
                diags.append(u.diag("info", "skills_via_claude_dir",
                                    "rendering skills to .claude/skills; whether opencode reads .claude/skills depends on your opencode version"))
                arts += u.skill_artifacts(ctx.skills, root, ".claude/skills", diags)
            else:
                diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability",
                                    "opencode has no known skills directory (set params.skills_via_claude_dir to try .claude/skills)"))
        dup = u.dedupe([a.name for a in ctx.agents], "agent", diags)
        for ag in sorted(ctx.agents, key=lambda a: a.name):
            if ag.name in dup:
                continue
            art = self._agent(ag, cfg, root, diags)
            if art is not None:
                arts.append(art)
        if ctx.instructions:
            ins = u.instruction_artifact(ctx.instructions, root, "AGENTS.md", cfg.params, diags)
            if ins is not None:
                arts.append(ins)
        # One artifact per concern on the SAME file: the core merges them into one document but applies each concern's
        # manage flag (mcp/permissions stay advisory while a managed concern could still be written).
        files = cfg.params.get("instruction_files")
        if ctx.instructions and isinstance(files, list) and files:
            # opt-in: the live opencode.json is hand-tuned and its `instructions` list holds more than AGENTS.md
            arts.append(self._json_art("instruction", {"instructions": sorted({str(f) for f in files})}, ["instructions"],
                                       [i.id for i in sorted(ctx.instructions, key=lambda i: (i.order, i.id))]))
        if ctx.mcp_servers:
            servers = self._mcp(ctx, diags)
            arts.append(self._json_art("mcp", {"mcp": servers}, ["mcp"], sorted(servers)))
        if ctx.permissions.rules:
            arts.append(self._json_art("rule", {"permission": self._permission(ctx.permissions, diags)}, ["permission"],
                                       sorted({f"{r.kind}:{r.match}" for r in ctx.permissions.rules})))
        if ctx.memory:
            diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability", "memory is not rendered for opencode"))
        arts.sort(key=lambda a: (a.root, a.path))
        return RenderResult(artifacts=arts, diagnostics=diags)

    @staticmethod
    def _json_art(kind: Any, doc: dict[str, Any], keys: list[str], ids: list[str]) -> Artifact:
        return Artifact(root="project", path="opencode.json", content=u.dump_json(doc), kind=kind, source_ids=ids,
                        merge="json_keys", managed_keys=keys)

    def _agent(self, ag: AgentItem, cfg: ClientConfig, root: str, diags: list[Diag]) -> Artifact | None:
        if not u.valid_name(ag.name):
            diags.append(u.diag("error", "invalid_name", f"agent name {ag.name!r} is not a valid identifier", ag.name))
            return None
        if not u.check_description(ag.description, ag.name, diags):
            return None
        if len(ag.body) > u.MAX_BODY_CHARS:
            diags.append(u.diag("error", "body_too_large", "agent body exceeds the size cap", ag.name))
            return None
        for c in u.unknown_caps(ag.capabilities):
            diags.append(u.diag("error", "unknown_capability", f"unknown capability {c!r}", ag.name))
        caps = u.effective_caps([c for c in ag.capabilities if c in TOOL_MAP], ag.read_only)
        for c in caps:
            if c == "mcp":
                diags.append(u.diag("info", "mcp_not_gated", "opencode does not gate MCP per agent; access follows opencode.json mcp", ag.name))
            elif TOOL_MAP[c] is None:
                diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability",
                                    f"capability {c!r} has no opencode tool", ag.name))
        tools: dict[str, bool] = {}
        for c in _GATED:
            native = TOOL_MAP[c]
            if native and c not in caps:
                tools[native] = False
        perm: dict[str, str] = {}
        if ag.read_only:
            perm["edit"] = "deny"
        if "shell" in caps:
            perm["bash"] = "ask"
        if "web_fetch" in caps:
            perm["webfetch"] = "ask"
        temp = cfg.params.get("temperature", 0.1)
        fm: dict[str, Any] = {"description": u.oneline(ag.description, ag.name, diags), "mode": ag.mode,
                              "temperature": float(temp) if isinstance(temp, (int, float)) else 0.1}
        if ag.model_tier is not None:
            model = self._tiers(cfg).get(ag.model_tier)
            if model:
                fm["model"] = model
            else:
                diags.append(u.diag("info", "no_model_mapping",
                                    f"no model mapped for tier {ag.model_tier!r}; agent inherits the default model", ag.name))
        if tools:
            fm["tools"] = dict(sorted(tools.items()))
        if perm:
            fm["permission"] = dict(sorted(perm.items()))
        return Artifact(root=root, path=u.safe_relpath(".opencode/agent", f"{ag.name}.md"),
                        content=u.to_bytes(u.render_frontmatter(fm, ag.body)), kind="agent", source_ids=[ag.name])

    def _mcp(self, ctx: RenderContext, diags: list[Diag]) -> dict[str, Any]:
        wrapper = u.sandbox_prefix(ctx.client.params)
        out: dict[str, Any] = {}
        for m in sorted(ctx.mcp_servers, key=lambda x: x.name):
            if not u.valid_name(m.name):
                diags.append(u.diag("error", "invalid_name", f"mcp name {m.name!r} is not valid", m.name))
                continue
            if m.scan_status != "clean":
                diags.append(u.diag("info", "mcp_not_clean", f"scan_status={m.scan_status}; the core blocks enabling unscanned servers", m.name))
            if m.transport == "stdio":
                if not m.command:
                    diags.append(u.diag("error", "mcp_no_command", "stdio server without a command", m.name))
                    continue
                argv = [m.command] + list(m.args)
                if m.sandbox_profile:
                    if not wrapper:
                        u.sandbox_missing(m.name, diags)
                        continue
                    argv = wrapper + argv
                e: dict[str, Any] = {"type": "local", "enabled": True, "command": argv}
                if m.env_names:
                    e["environment"] = {n: "{env:" + n + "}" for n in sorted(m.env_names)}
            else:
                if not m.url:
                    diags.append(u.diag("error", "mcp_no_url", f"{m.transport} server without a url", m.name))
                    continue
                e = {"type": "remote", "enabled": True, "url": m.url}
            out[m.name] = e
        return out

    def _permission(self, perms: PermissionSet, diags: list[Diag]) -> dict[str, Any]:
        bash: dict[str, str] = {}
        top: dict[str, Any] = {}
        tool_names = {"write": "edit", "edit": "edit", "web_fetch": "webfetch"}
        for r in perms.rules:
            if r.kind == "command":
                bash[r.match] = r.decision
            elif r.kind == "tool" and r.match == "shell":
                bash["*"] = r.decision
            elif r.kind == "tool" and r.match in tool_names:
                top[tool_names[r.match]] = r.decision
            elif r.kind in ("path_read", "path_write") and r.decision == "deny":
                bash[f"* {r.match}"] = "deny"
                diags.append(u.diag("info", "approximated", f"{r.kind} deny {r.match!r} rendered as a bash glob (opencode has no path ACL)"))
            else:
                diags.append(u.diag("warn", "unsupported_rule", f"rule {r.kind}:{r.match}:{r.decision} has no opencode equivalent"))
        if bash:
            bash.setdefault("*", perms.default_tool_decision)
            ordered = {"*": bash["*"]}
            ordered.update({k: v for k, v in bash.items() if k != "*"})
            top["bash"] = ordered
        return dict(sorted(top.items()))

    # ------------------------------------------------------------------------------------------------ discover
    def discover(self, cfg: ClientConfig, fs: FileSystem) -> DiscoveredContent:
        res = DiscoveredContent()
        if cfg.params.get("skills_via_claude_dir") is True:
            res.skills = u.discover_skills(fs, cfg, "project", ".claude/skills", res.notes)
        base = u.fs_path(cfg, "project", ".opencode/agent")
        if base and fs.exists(base) and fs.is_dir(base):
            for fname in sorted(fs.listdir(base)):
                if not fname.endswith(".md"):
                    continue
                name = fname[:-3]
                if not u.valid_name(name):
                    res.notes.append(f"skipped agent file {fname!r}: not a valid hub name")
                    continue
                fm, body = u.parse_frontmatter(u.read_text(fs, f"{base}/{fname}"))
                tools: dict[str, Any] = fm["tools"] if isinstance(fm.get("tools"), dict) else {}
                caps = [c for c in _GATED if not (c_native := TOOL_MAP[c]) or tools.get(c_native) is not False]
                perm: dict[str, Any] = fm["permission"] if isinstance(fm.get("permission"), dict) else {}
                ro = "write" not in caps or "edit" not in caps or perm.get("edit") == "deny"
                mode = "primary" if fm.get("mode") == "primary" else "subagent"
                res.agents.append(AgentItem(name=name, description=u.cap_str(fm.get("description")), capabilities=caps,
                                            mode=mode, read_only=ro, body=body))
                if fm.get("model"):
                    res.notes.append(f"agent {name}: model {fm['model']!r} not mapped to a tier")
        p = u.fs_path(cfg, "project", "AGENTS.md")
        if p and fs.exists(p) and u.extract_block(u.read_text(fs, p)) is None:
            res.suggested_params = {"instructions_mode": "own"}
            res.instructions.append(InstructionItem(id="agents-md", title="AGENTS.md", order=0,
                                                    body=u.read_text(fs, p)))
        cj = u.fs_path(cfg, "project", "opencode.json")
        if cj and fs.exists(cj):
            try:
                doc = json.loads(u.read_text(fs, cj) or "null")
            except (ValueError, OSError):
                res.notes.append("opencode.json is not valid JSON (may contain comments)")
                doc = {}
            if isinstance(doc, dict):
                res.mcp_servers = _discover_mcp(doc.get("mcp"), res.notes)
                gperm = doc.get("permission")
                if isinstance(gperm, dict):
                    res.permissions = _discover_permission(gperm)
        return res

    def verify(self, cfg: ClientConfig, expected: list[Expected], fs: FileSystem, run: CommandRunner) -> list[Check]:
        # no offline "list what you loaded" command exists for opencode; files + hashes only.
        return u.verify_expected(cfg, expected, fs)


def _discover_mcp(mcp: Any, notes: list[str]) -> list[McpItem]:
    out: list[McpItem] = []
    if not isinstance(mcp, dict):
        return out
    for name, e in sorted(mcp.items()):
        if not isinstance(e, dict) or not u.valid_name(str(name)):
            notes.append(f"skipped mcp server {name!r}")
            continue
        if e.get("type") == "remote" and e.get("url"):
            out.append(McpItem(name=name, transport="http", url=u.cap_str(e["url"], 2000)))
        elif isinstance(e.get("command"), list) and e["command"]:
            cmd = [str(c) for c in e["command"]]
            env = sorted(e["environment"]) if isinstance(e.get("environment"), dict) else []
            out.append(McpItem(name=name, transport="stdio", command=cmd[0], args=cmd[1:], env_names=env))
    return out


def _discover_permission(perm: dict[str, Any]) -> PermissionSet:
    rules: list[Rule] = []
    dec_ok = ("allow", "ask", "deny")
    for key, val in sorted(perm.items()):
        if key == "bash" and isinstance(val, dict):
            for glob, d in val.items():
                if d in dec_ok:
                    if glob == "*":
                        rules.append(Rule(kind="tool", match="shell", decision=d))
                    else:
                        rules.append(Rule(kind="command", match=str(glob), decision=d))
        elif isinstance(val, str) and val in dec_ok:
            cap = {"edit": "edit", "webfetch": "web_fetch"}.get(key)
            if cap:
                rules.append(Rule(kind="tool", match=cap, decision=val))
    return PermissionSet(rules=rules)


ADAPTER = OpenCodeAdapter
