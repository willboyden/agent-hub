"""Declarative ``generic`` adapter (id ``generic``): add ANY client (Cursor, Codex CLI, Gemini CLI, ...) with a YAML spec, no code.

The spec lives in ``content/clients/<id>.yaml`` under ``spec:`` and is documented in docs/ADDING-A-CLIENT.md. Design goals:

* unknown keys are ERRORS and the message carries the precise path (``spec.skills.layoutt: unknown key``);
* templating is a tiny explicit substituter over ``{placeholder}`` tokens (regex, allow-listed names per context). There is no
  ``str.format``, no ``eval``, no shell, no attribute access, so a spec (or an MCP/skill name inside it) cannot inject anything;
* every emitted path is checked with ``_util.safe_relpath``; names are validated by ``fullmatch`` before they touch a path;
* ``validate_spec(spec)`` and ``dry_render(spec, ctx)`` are what the core's ``POST /clients/validate-spec`` calls.

Deviation from the brief, on purpose: ``instructions.mode`` is ``own`` (whole file hub-owned, ``## title`` headings), ``concat``
(whole file hub-owned, bodies concatenated with no headings) or ``block`` (only the hub marker region, headings kept). ``toml_stub``
became ``toml``: a minimal emitter for ``[table.name]`` blocks delivered as a marker block (the Merge port has no TOML merge).
"""
from __future__ import annotations

import posixpath
import re
from collections.abc import Mapping
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field, ValidationError

from . import _util as u
from .base import (
    CAPABILITIES,
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
    MemoryItem,
    PermissionSet,
    RenderContext,
    RenderResult,
    Rule,
    SkillItem,
)

_TIERS = ("fast", "standard", "deep")
_DECISIONS = ("allow", "ask", "deny")
_RULE_KINDS = ("tool", "command", "path_read", "path_write", "egress_host", "mcp_server")
_TRANSPORTS = ("stdio", "http", "sse")
_AGENT_SOURCES = ("name", "description", "mode", "body")
_MCP_PLACEHOLDERS = frozenset({"name", "command", "args", "argv", "url", "env", "env_names", "transport", "pinned_ref"})
_RULE_PLACEHOLDERS = frozenset({"match", "tool"})
_PH = re.compile(r"\{([a-z_][a-z0-9_]*)\}")
_KEYPATH = re.compile(r"[A-Za-z0-9_-]+(\.[A-Za-z0-9_-]+)*")
_DROP = object()


class _M(BaseModel):
    model_config = ConfigDict(extra="forbid")


class SkillsSpec(_M):
    root: str
    path: str
    layout: Literal["dir_per_skill", "flat"] = "dir_per_skill"
    filename: str = "SKILL.md"           # dir_per_skill: file inside <path>/<name>/;  flat: template using {name}


class AgentsSpec(_M):
    root: str
    path: str
    filename: str = "{name}.md"
    format: Literal["md_frontmatter", "json", "yaml"] = "md_frontmatter"
    fields: dict[str, str] = Field(default_factory=lambda: {"name": "name", "description": "description"})  # output key -> source
    static: dict[str, str | int | float | bool] = Field(default_factory=dict)
    tools_field: str = "tools"
    tools_format: Literal["list", "csv", "bool_map"] = "csv"
    model_field: str | None = None
    tool_map: dict[str, str | None] = Field(default_factory=dict)
    model_tiers: dict[str, str] = Field(default_factory=dict)


class InstructionsSpec(_M):
    root: str
    file: str
    mode: Literal["own", "block", "concat"] = "block"
    marker: Literal["md", "hash"] = "md"


class McpSpec(_M):
    root: str
    file: str
    format: Literal["json", "yaml", "toml"] = "json"
    key_path: str = "mcpServers"
    entry: dict[str, dict[str, Any]]      # transport -> template object
    env_style: Literal["dollar_brace", "env_ref", "names_only"] = "dollar_brace"
    wrapper: list[str] | None = None      # argv prefix for sandbox_profile servers, e.g. ["./run-sandboxed.sh", "--"]


class PermissionsSpec(_M):
    root: str
    file: str
    format: Literal["json", "yaml"] = "json"
    lists: dict[str, str]                 # decision -> dotted key path
    rule_templates: dict[str, str | list[str]]


class MemorySpec(_M):
    root: str
    path: str
    filename: str = "{id}.md"
    frontmatter: bool = True


class Spec(_M):
    version: Literal[1] = 1
    notes: str = ""
    tool_map: dict[str, str | None] = Field(default_factory=dict)
    model_tiers: dict[str, str] = Field(default_factory=dict)
    skills: SkillsSpec | None = None
    agents: AgentsSpec | None = None
    instructions: InstructionsSpec | None = None
    mcp: McpSpec | None = None
    permissions: PermissionsSpec | None = None
    memory: MemorySpec | None = None


# ---- validation ---------------------------------------------------------------------------------------------------------
def _pydantic_diags(exc: ValidationError) -> list[Diag]:
    out: list[Diag] = []
    for e in exc.errors():
        path = ".".join(["spec"] + [str(p) for p in e["loc"]])
        msg = "unknown key" if e["type"] == "extra_forbidden" else str(e["msg"])
        out.append(Diag(severity="error", code="spec_invalid", message=f"{path}: {msg}", item=path))
    return out


def _err(path: str, msg: str) -> Diag:
    return Diag(severity="error", code="spec_invalid", message=f"{path}: {msg}", item=path)


def _check_template(s: str, allowed: frozenset[str], path: str, out: list[Diag]) -> None:
    for m in _PH.finditer(s):
        if m.group(1) not in allowed:
            out.append(_err(path, f"unknown placeholder {{{m.group(1)}}} (allowed: {', '.join(sorted(allowed))})"))
    if "{" in _PH.sub("", s) or "}" in _PH.sub("", s):
        out.append(_err(path, "stray brace; write placeholders as {name}"))


def _check_template_tree(obj: Any, allowed: frozenset[str], path: str, out: list[Diag]) -> None:
    if isinstance(obj, str):
        _check_template(obj, allowed, path, out)
    elif isinstance(obj, dict):
        for k, v in obj.items():
            _check_template(str(k), frozenset(), f"{path}.<key {k!r}>", out)   # keys are literal
            _check_template_tree(v, allowed, f"{path}.{k}", out)
    elif isinstance(obj, list):
        for i, v in enumerate(obj):
            _check_template_tree(v, allowed, f"{path}[{i}]", out)


def _check_relpath(s: str, path: str, out: list[Diag]) -> None:
    try:
        u.safe_relpath(s)
    except u.UnsafePath as exc:
        out.append(_err(path, f"unsafe relative path: {exc}"))


def _check_maps(tool_map: Mapping[str, Any], tiers: Mapping[str, Any], path: str, out: list[Diag]) -> None:
    for k in tool_map:
        if k not in CAPABILITIES:
            out.append(_err(f"{path}.tool_map.{k}", f"unknown capability (allowed: {', '.join(CAPABILITIES)})"))
    for k in tiers:
        if k not in _TIERS:
            out.append(_err(f"{path}.model_tiers.{k}", f"unknown tier (allowed: {', '.join(_TIERS)})"))


def _semantic(spec: Spec) -> list[Diag]:
    out: list[Diag] = []
    _check_maps(spec.tool_map, spec.model_tiers, "spec", out)
    if spec.skills:
        _check_relpath(spec.skills.path, "spec.skills.path", out)
        if spec.skills.layout == "flat":
            _check_template(spec.skills.filename, frozenset({"name"}), "spec.skills.filename", out)
            if "{name}" not in spec.skills.filename:
                out.append(_err("spec.skills.filename", "flat layout needs {name} in the filename"))
        else:
            _check_relpath(spec.skills.filename, "spec.skills.filename", out)
            _check_template(spec.skills.filename, frozenset(), "spec.skills.filename", out)
    if spec.agents:
        a = spec.agents
        _check_relpath(a.path, "spec.agents.path", out)
        _check_template(a.filename, frozenset({"name"}), "spec.agents.filename", out)
        if "{name}" not in a.filename:
            out.append(_err("spec.agents.filename", "needs {name}"))
        _check_maps(a.tool_map, a.model_tiers, "spec.agents", out)
        for k, src in a.fields.items():
            if src not in _AGENT_SOURCES:
                out.append(_err(f"spec.agents.fields.{k}", f"unknown source {src!r} (allowed: {', '.join(_AGENT_SOURCES)})"))
        if a.format == "md_frontmatter" and "body" in a.fields.values():
            out.append(_err("spec.agents.fields", "md_frontmatter puts the body after the frontmatter; do not map a field to 'body'"))
        if a.format != "md_frontmatter" and "body" not in a.fields.values():
            out.append(_err("spec.agents.fields", f"{a.format} agents need one field mapped to 'body' or the instructions are lost"))
        if a.model_field is not None and not (a.model_tiers or spec.model_tiers):
            out.append(_err("spec.agents.model_field", "set but no model_tiers are defined"))
    if spec.instructions:
        _check_relpath(spec.instructions.file, "spec.instructions.file", out)
    if spec.mcp:
        m = spec.mcp
        _check_relpath(m.file, "spec.mcp.file", out)
        if not _KEYPATH.fullmatch(m.key_path):
            out.append(_err("spec.mcp.key_path", "must be dotted identifiers, e.g. mcpServers or mcp.servers"))
        for tr, tmpl in m.entry.items():
            if tr not in _TRANSPORTS:
                out.append(_err(f"spec.mcp.entry.{tr}", f"unknown transport (allowed: {', '.join(_TRANSPORTS)})"))
            _check_template_tree(tmpl, _MCP_PLACEHOLDERS, f"spec.mcp.entry.{tr}", out)
        if not m.entry:
            out.append(_err("spec.mcp.entry", "at least one transport template is required"))
    if spec.permissions:
        p = spec.permissions
        _check_relpath(p.file, "spec.permissions.file", out)
        for d, kp in p.lists.items():
            if d not in _DECISIONS:
                out.append(_err(f"spec.permissions.lists.{d}", f"unknown decision (allowed: {', '.join(_DECISIONS)})"))
            if not _KEYPATH.fullmatch(kp):
                out.append(_err(f"spec.permissions.lists.{d}", "must be a dotted key path"))
        if not p.lists:
            out.append(_err("spec.permissions.lists", "at least one decision list is required"))
        for k, rt in p.rule_templates.items():
            if k not in _RULE_KINDS:
                out.append(_err(f"spec.permissions.rule_templates.{k}", f"unknown rule kind (allowed: {', '.join(_RULE_KINDS)})"))
            for i, rtmpl in enumerate([rt] if isinstance(rt, str) else rt):
                _check_template(rtmpl, _RULE_PLACEHOLDERS, f"spec.permissions.rule_templates.{k}[{i}]", out)
    if spec.memory:
        _check_relpath(spec.memory.path, "spec.memory.path", out)
        _check_template(spec.memory.filename, frozenset({"id"}), "spec.memory.filename", out)
        if "{id}" not in spec.memory.filename:
            out.append(_err("spec.memory.filename", "needs {id}"))
    return out


def parse_spec(spec: Any) -> tuple[Spec | None, list[Diag]]:
    if not isinstance(spec, dict):
        return None, [_err("spec", "must be a mapping")]
    try:
        parsed = Spec.model_validate(spec)
    except ValidationError as exc:
        return None, _pydantic_diags(exc)
    diags = _semantic(parsed)
    return (None if diags else parsed), diags


def validate_spec(spec: dict[str, Any]) -> list[Diag]:
    """All problems with a spec (empty list = valid). Used by POST /clients/validate-spec."""
    return parse_spec(spec)[1]


def dry_render(spec: dict[str, Any], ctx: RenderContext) -> RenderResult:
    """Render sample content with ``spec`` regardless of ``ctx.client.spec`` (the UI's live preview)."""
    client = ctx.client.model_copy(update={"adapter": "generic", "spec": spec})
    return GenericSpecAdapter().render(ctx.model_copy(update={"client": client}))


def caps_for_spec(spec: dict[str, Any]) -> AdapterCaps:
    """Capabilities implied by a spec (``caps()`` cannot know them: it takes no config)."""
    parsed, _ = parse_spec(spec)
    if parsed is None:
        return AdapterCaps(notes="invalid spec")
    tm = dict(parsed.tool_map)
    if parsed.agents:
        tm.update(parsed.agents.tool_map)
    tiers = dict(parsed.model_tiers)
    if parsed.agents:
        tiers.update(parsed.agents.model_tiers)
    return AdapterCaps(skills=parsed.skills is not None, agents=parsed.agents is not None,
                       instructions=parsed.instructions is not None, mcp=parsed.mcp is not None,
                       permissions=parsed.permissions is not None, memory=parsed.memory is not None,
                       tool_map={c: tm.get(c) for c in CAPABILITIES}, model_tiers=tiers, notes=parsed.notes)


# ---- the tiny substituter ---------------------------------------------------------------------------------------------
def _subst_str(s: str, vals: Mapping[str, Any]) -> Any:
    m = _PH.fullmatch(s)
    if m:
        v = vals.get(m.group(1))
        return _DROP if v is None or v == {} else v          # a whole-value placeholder keeps its type (list/dict/str)

    def rep(mm: re.Match[str]) -> str:
        v = vals.get(mm.group(1))
        if v is None:
            return ""
        if isinstance(v, (list, dict)):
            raise ValueError(f"placeholder {{{mm.group(1)}}} is a list/dict and must be the whole value")
        return str(v)

    return _PH.sub(rep, s)


def subst(obj: Any, vals: Mapping[str, Any]) -> Any:
    """Recursive placeholder substitution. Values that resolve to nothing are dropped from their dict/list."""
    if isinstance(obj, str):
        return _subst_str(obj, vals)
    if isinstance(obj, dict):
        out: dict[str, Any] = {}
        for k, v in obj.items():
            r = subst(v, vals)
            if r is not _DROP:
                out[str(k)] = r
        return out
    if isinstance(obj, list):
        return [r for r in (subst(v, vals) for v in obj) if r is not _DROP]
    return obj


def _fmt(template: str, **vals: str) -> str:
    r = _subst_str(template, vals)
    return "" if r is _DROP else str(r)


# ---- TOML stub --------------------------------------------------------------------------------------------------------
def _toml_key(k: str) -> str:
    return k if re.fullmatch(r"[A-Za-z0-9_-]+", k) else '"' + k.replace("\\", "\\\\").replace('"', '\\"') + '"'


def _toml_val(v: Any) -> str:
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return str(v)
    if isinstance(v, list):
        return "[" + ", ".join(_toml_val(x) for x in v) + "]"
    if isinstance(v, dict):
        return "{ " + ", ".join(f"{_toml_key(str(k))} = {_toml_val(x)}" for k, x in sorted(v.items())) + " }"
    s = str(v).replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\r", "\\r").replace("\t", "\\t")
    return f'"{s}"'


def toml_tables(prefix: str, servers: Mapping[str, Mapping[str, Any]]) -> str:
    lines: list[str] = []
    for name in sorted(servers):
        lines.append(f"[{prefix}.{_toml_key(name)}]")
        for k, v in servers[name].items():
            lines.append(f"{_toml_key(k)} = {_toml_val(v)}")
        lines.append("")
    return "\n".join(lines).rstrip() + "\n"


# ---- the adapter ----------------------------------------------------------------------------------------------------
class GenericSpecAdapter(ClientAdapter):
    id = "generic"
    display_name = "Generic (declarative spec)"

    def caps(self) -> AdapterCaps:
        return AdapterCaps(skills=True, agents=True, instructions=True, mcp=True, permissions=True, memory=True,
                           notes="Capabilities depend on the client's spec; use caps_for_spec(spec).")

    def default_config(self) -> ClientConfig:
        return ClientConfig(
            id="my-client", adapter="generic", display_name="My client", strict=False,
            description="Starter spec: skills + an instructions block. See docs/ADDING-A-CLIENT.md.",
            roots={"project": "~/path/to/project"},
            spec={"version": 1,
                  "skills": {"root": "project", "path": ".myclient/skills", "layout": "dir_per_skill", "filename": "SKILL.md"},
                  "instructions": {"root": "project", "file": "MYCLIENT.md", "mode": "block"}},
        )

    # -------------------------------------------------------------------------------------------- render
    def render(self, ctx: RenderContext) -> RenderResult:
        cfg = ctx.client
        spec, sdiags = parse_spec(cfg.spec)
        if spec is None:
            return RenderResult(artifacts=[], diagnostics=sdiags or [_err("spec", "generic client has no spec")])
        diags: list[Diag] = []
        arts: list[Artifact] = []

        def root_ok(section: str, root: str) -> bool:
            if root in cfg.roots:
                return True
            diags.append(u.diag("error", "root_missing", f"spec.{section}.root {root!r} is not one of the client's roots {sorted(cfg.roots)}"))
            return False

        if ctx.skills:
            if spec.skills is None:
                diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability", "this client's spec has no skills section"))
            elif root_ok("skills", spec.skills.root):
                arts += self._skills(spec.skills, ctx.skills, diags)
        if ctx.agents:
            if spec.agents is None:
                diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability", "this client's spec has no agents section"))
            elif root_ok("agents", spec.agents.root):
                arts += self._agents(spec, spec.agents, cfg, ctx.agents, diags)
        if ctx.instructions:
            if spec.instructions is None:
                diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability", "this client's spec has no instructions section"))
            elif root_ok("instructions", spec.instructions.root):
                ins = self._instructions(spec.instructions, ctx, diags)
                if ins is not None:
                    arts.append(ins)
        if ctx.mcp_servers:
            if spec.mcp is None:
                diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability", "this client's spec has no mcp section"))
            elif root_ok("mcp", spec.mcp.root):
                mc = self._mcp(spec, spec.mcp, ctx.mcp_servers, diags)
                if mc is not None:
                    arts.append(mc)
        if ctx.permissions.rules:
            if spec.permissions is None:
                diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability", "this client's spec has no permissions section"))
            elif root_ok("permissions", spec.permissions.root):
                arts.append(self._permissions(spec, spec.permissions, cfg, ctx.permissions, diags))
        if ctx.memory:
            if spec.memory is None:
                diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability", "this client's spec has no memory section"))
            elif root_ok("memory", spec.memory.root):
                arts += self._memory(spec.memory, ctx.memory, diags)
        arts.sort(key=lambda a: (a.root, a.path))
        return RenderResult(artifacts=arts, diagnostics=diags)

    def _skills(self, s: SkillsSpec, skills: list[SkillItem], diags: list[Diag]) -> list[Artifact]:
        if s.layout == "dir_per_skill":
            return u.skill_artifacts(skills, s.root, s.path, diags, skill_filename=s.filename)
        out: list[Artifact] = []
        for sk in sorted(skills, key=lambda x: x.name):
            if not u.valid_name(sk.name) or "SKILL.md" not in sk.files:
                diags.append(u.diag("error", "invalid_skill", "invalid name or missing SKILL.md", sk.name))
                continue
            if len(sk.files) > 1:
                diags.append(u.diag("warn", "extra_files_dropped", "flat layout cannot carry supporting files; only SKILL.md is written", sk.name))
            out.append(Artifact(root=s.root, path=u.safe_relpath(s.path, _fmt(s.filename, name=sk.name)),
                                content=sk.files["SKILL.md"], kind="skill", source_ids=[sk.name]))
        return out

    def _tools(self, spec: Spec, a: AgentsSpec, ag: AgentItem, cfg: ClientConfig, diags: list[Diag]) -> list[str] | None:
        tmap = {**spec.tool_map, **a.tool_map}
        caps = u.effective_caps([c for c in ag.capabilities if c in CAPABILITIES], ag.read_only)
        natives: list[str] = []
        for c in caps:
            n = tmap.get(c)
            if n is None:
                diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability", f"capability {c!r} has no native tool in this spec", ag.name))
            elif n not in natives:
                natives.append(n)
        return natives

    def _agents(self, spec: Spec, a: AgentsSpec, cfg: ClientConfig, agents: list[AgentItem], diags: list[Diag]) -> list[Artifact]:
        out: list[Artifact] = []
        tmap = {**spec.tool_map, **a.tool_map}
        tiers = {**spec.model_tiers, **a.model_tiers}
        dup = u.dedupe([g.name for g in agents], "agent", diags)
        for ag in sorted(agents, key=lambda g: g.name):
            if ag.name in dup:
                continue
            if not u.valid_name(ag.name):
                diags.append(u.diag("error", "invalid_name", f"agent name {ag.name!r} is not a valid identifier", ag.name))
                continue
            if not u.check_description(ag.description, ag.name, diags):
                continue
            if len(ag.body) > u.MAX_BODY_CHARS:
                diags.append(u.diag("error", "body_too_large", "agent body exceeds the size cap", ag.name))
                continue
            natives = self._tools(spec, a, ag, cfg, diags)
            src = {"name": ag.name, "description": u.oneline(ag.description, ag.name, diags), "mode": ag.mode, "body": ag.body}
            doc: dict[str, Any] = {k: src[v] for k, v in a.fields.items()}
            doc.update(a.static)
            if a.tools_format == "csv":
                doc[a.tools_field] = ", ".join(natives or [])
            elif a.tools_format == "list":
                doc[a.tools_field] = list(natives or [])
            else:
                allowed = set(natives or [])
                doc[a.tools_field] = {n: (n in allowed) for n in sorted({v for v in tmap.values() if v})}
            if not natives:
                diags.append(u.diag("warn", "empty_toolset", "agent has no native tools in this client; the client may default to ALL tools", ag.name))
            if a.model_field and ag.model_tier is not None:
                model = tiers.get(ag.model_tier)
                if model:
                    doc[a.model_field] = model
                else:
                    diags.append(u.diag("info", "no_model_mapping", f"no model mapped for tier {ag.model_tier!r}", ag.name))
            path = u.safe_relpath(a.path, _fmt(a.filename, name=ag.name))
            if a.format == "md_frontmatter":
                content = u.render_frontmatter(doc, ag.body)
            elif a.format == "json":
                content = u.dump_json(doc).decode()
            else:
                content = u.dump_yaml(doc).decode()
            out.append(Artifact(root=a.root, path=path, content=u.to_bytes(content), kind="agent", source_ids=[ag.name]))
        return out

    def _instructions(self, i: InstructionsSpec, ctx: RenderContext, diags: list[Diag]) -> Artifact | None:
        ids = [x.id for x in sorted(ctx.instructions, key=lambda x: (x.order, x.id))]
        path = u.safe_relpath(i.file)
        if i.mode == "block":
            try:
                block = u.wrap_block(u.render_instructions(ctx.instructions), i.marker)
            except u.MarkerInjection as exc:
                diags.append(u.diag("error", "marker_injection", f"{path}: {exc}; instructions not rendered"))
                return None
            return Artifact(root=i.root, path=path, content=u.to_bytes(block), kind="instruction", source_ids=ids, merge="block")
        text = u.render_instructions(ctx.instructions, headings=(i.mode == "own")) + "\n"
        return Artifact(root=i.root, path=path, content=u.to_bytes(text), kind="instruction", source_ids=ids)

    def _mcp(self, spec: Spec, m: McpSpec, items: list[McpItem], diags: list[Diag]) -> Artifact | None:
        servers: dict[str, Any] = {}
        for it in sorted(items, key=lambda x: x.name):
            if not u.valid_name(it.name):
                diags.append(u.diag("error", "invalid_name", f"mcp name {it.name!r} is not valid", it.name))
                continue
            tmpl = m.entry.get(it.transport)
            if tmpl is None:
                diags.append(u.diag("error", "unsupported_transport", f"spec.mcp.entry has no template for {it.transport!r}", it.name))
                continue
            if it.scan_status != "clean":
                diags.append(u.diag("info", "mcp_not_clean", f"scan_status={it.scan_status}; the core blocks enabling unscanned servers", it.name))
            argv = ([it.command] if it.command else []) + list(it.args)
            if it.sandbox_profile and not m.wrapper:
                u.sandbox_missing(it.name, diags)
                continue
            if it.sandbox_profile and m.wrapper:
                argv = list(m.wrapper) + argv
            env: dict[str, str] = {}
            if m.env_style == "dollar_brace":
                env = {n: "${" + n + "}" for n in sorted(it.env_names)}
            elif m.env_style == "env_ref":
                env = {n: "{env:" + n + "}" for n in sorted(it.env_names)}
            vals: dict[str, Any] = {"name": it.name, "command": it.command, "args": list(it.args) if it.command else None,
                                    "argv": argv or None, "url": it.url, "env": env or None, "env_names": sorted(it.env_names) or None,
                                    "transport": it.transport, "pinned_ref": it.pinned_ref}
            if it.command and it.sandbox_profile and m.wrapper:
                vals["command"], vals["args"] = argv[0], argv[1:]
            servers[it.name] = subst(tmpl, vals)
        ids = sorted(servers)
        if m.format == "toml":
            body = toml_tables(m.key_path, servers)
            try:
                block = u.wrap_block(body, "hash")
            except u.MarkerInjection as exc:
                diags.append(u.diag("error", "marker_injection", f"{m.file}: {exc}; mcp block not rendered"))
                return None
            return Artifact(root=m.root, path=u.safe_relpath(m.file), content=u.to_bytes(block), kind="mcp",
                            source_ids=ids, merge="block")
        doc: dict[str, Any] = {}
        u.set_dotted(doc, m.key_path, servers)
        content = u.dump_json(doc) if m.format == "json" else u.dump_yaml(doc)
        return Artifact(root=m.root, path=u.safe_relpath(m.file), content=content, kind="mcp", source_ids=ids,
                        merge="json_keys" if m.format == "json" else "yaml_keys", managed_keys=[m.key_path])

    def _permissions(self, spec: Spec, p: PermissionsSpec, cfg: ClientConfig, perms: PermissionSet, diags: list[Diag]) -> Artifact:
        lists: dict[str, set[str]] = {d: set() for d in p.lists}
        for r in perms.rules:
            t = p.rule_templates.get(r.kind)
            if t is None or (r.kind == "tool" and spec.tool_map.get(r.match) is None):
                diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_rule", f"rule {r.kind}:{r.match} has no mapping in spec.permissions"))
                continue
            if r.decision not in lists:
                diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_decision", f"client has no {r.decision!r} list; rule {r.kind}:{r.match} dropped"))
                continue
            for tmpl in [t] if isinstance(t, str) else t:
                lists[r.decision].add(_fmt(tmpl, match=r.match, tool=spec.tool_map.get(r.match) or ""))
        doc: dict[str, Any] = {}
        for d, kp in p.lists.items():
            u.set_dotted(doc, kp, sorted(lists[d]))
        content = u.dump_json(doc) if p.format == "json" else u.dump_yaml(doc)
        return Artifact(root=p.root, path=u.safe_relpath(p.file), content=content, kind="rule",
                        source_ids=sorted({f"{r.kind}:{r.match}" for r in perms.rules}),
                        merge="json_keys" if p.format == "json" else "yaml_keys", managed_keys=sorted(p.lists.values()))

    def _memory(self, mem: MemorySpec, items: list[MemoryItem], diags: list[Diag]) -> list[Artifact]:
        out: list[Artifact] = []
        for it in sorted(items, key=lambda x: x.id):
            if not u.valid_name(it.id):
                diags.append(u.diag("error", "invalid_name", f"memory id {it.id!r} is not valid", it.id))
                continue
            text = u.render_frontmatter({"id": it.id, "type": it.type, "title": it.title}, it.body) if mem.frontmatter else it.body.rstrip() + "\n"
            out.append(Artifact(root=mem.root, path=u.safe_relpath(mem.path, _fmt(mem.filename, id=it.id)),
                                content=u.to_bytes(text), kind="memory", source_ids=[it.id]))
        return out

    # -------------------------------------------------------------------------------------------- discover
    def discover(self, cfg: ClientConfig, fs: FileSystem) -> DiscoveredContent:
        """Best effort: dir-per-skill skills, md_frontmatter agents and a single-file instruction. Others are noted, not guessed."""
        res = DiscoveredContent()
        spec, diags = parse_spec(cfg.spec)
        if spec is None:
            res.notes += [d.message for d in diags]
            return res
        if spec.skills and spec.skills.layout == "dir_per_skill" and spec.skills.filename == "SKILL.md":
            res.skills = u.discover_skills(fs, cfg, spec.skills.root, spec.skills.path, res.notes)
        elif spec.skills:
            res.notes.append("skills discovery supports dir_per_skill with filename SKILL.md only")
        if spec.agents and spec.agents.format == "md_frontmatter":
            res.agents = self._discover_agents(spec, spec.agents, cfg, fs, res.notes)
        elif spec.agents:
            res.notes.append("agent discovery supports md_frontmatter only")
        if spec.instructions and spec.instructions.mode != "block":
            p = u.fs_path(cfg, spec.instructions.root, spec.instructions.file)
            if p and fs.exists(p):
                res.instructions.append(InstructionItem(id="imported", title=posixpath.basename(p), order=0,
                                                        body=u.read_text(fs, p)))
        if spec.mcp or spec.permissions or spec.memory:
            res.notes.append("mcp/permissions/memory discovery is not implemented for generic specs")
        return res

    def _discover_agents(self, spec: Spec, a: AgentsSpec, cfg: ClientConfig, fs: FileSystem, notes: list[str]) -> list[AgentItem]:
        base = u.fs_path(cfg, a.root, a.path)
        if base is None or not fs.exists(base) or not fs.is_dir(base):
            return []
        pat = re.compile("^" + re.escape(a.filename).replace(re.escape("{name}"), "(?P<name>[a-z0-9][a-z0-9._-]{0,63})") + "$")
        tmap = {**spec.tool_map, **a.tool_map}
        rev: dict[str, str] = {}
        for c in reversed(CAPABILITIES):
            if tmap.get(c):
                rev[str(tmap[c])] = c
        desc_key = next((k for k, v in a.fields.items() if v == "description"), "description")
        out: list[AgentItem] = []
        for fname in sorted(fs.listdir(base)):
            mt = pat.match(fname)
            if not mt:
                continue
            fm, body = u.parse_frontmatter(u.read_text(fs, posixpath.join(base, fname)))
            raw = fm.get(a.tools_field)
            names: list[str]
            if isinstance(raw, str):
                names = [t.strip() for t in raw.split(",") if t.strip()]
            elif isinstance(raw, list):
                names = [str(t) for t in raw]
            elif isinstance(raw, dict):
                names = [str(k) for k, v in raw.items() if v]
            else:
                names = []
            caps = u.ordered_caps({rev[t] for t in names if t in rev})
            out.append(AgentItem(name=mt.group("name"), description=u.cap_str(fm.get(desc_key)), capabilities=caps,
                                 read_only="write" not in caps and "edit" not in caps, body=body))
        return out

    def verify(self, cfg: ClientConfig, expected: list[Expected], fs: FileSystem, run: CommandRunner) -> list[Check]:
        return u.verify_expected(cfg, expected, fs)


ADAPTER = GenericSpecAdapter

__all__ = ["ADAPTER", "GenericSpecAdapter", "Rule", "caps_for_spec", "dry_render", "parse_spec", "subst", "validate_spec"]
