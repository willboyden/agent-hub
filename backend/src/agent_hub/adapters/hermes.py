"""Hermes Agent adapter (id ``hermes-agent``). ONE class serves every Hermes instance: client ids are free and everything
instance-specific (compose project/service, docker context, binary, source dirs, stage root) is a ``ClientConfig.params`` entry.

Hermes typically runs in a container that the hub cannot reach, so the hub never writes into it. ``render`` produces a STAGE tree
(root ``stage``, a host directory the hub owns):

  skills/<name>/...              the skills tree; delivered on request by ``deploy/hermes-deliver.sh`` (opt-in, not run by the app)
  AGENTS.md                      workspace instructions (staged for review)
  config.policy.fragment.yaml    yaml_keys: approvals.deny, agent.disabled_toolsets          (advisory by default)
  config.mcp.fragment.yaml       yaml_keys: mcp_servers                                      (advisory by default)

Deliver hub skills into a category directory of their own (for example ``skills/hub``) so a sync process that rewrites another
category cannot delete them. Whether Hermes indexes that directory depends on the install, which is why verify() asks
the running container (``hermes skills list``) and matches whole skill names.

Params (all optional; defaults are neutral placeholders):
  compose_project, compose_service   label filter used to find the running container ("CHANGEME" = not configured)
  docker_context                     passed as ``docker --context <value>`` when set
  hermes_bin                         command to run inside the container (default ``hermes``)
  exec_user                          passed as ``docker exec --user <value>`` when set
  source_root, source_dirs           where discover() reads the client's real sources: a list of {kind: skills|instruction|config,
                                     path, root?}; paths are relative to ``source_root`` (or to a named entry of ``roots``)
  disabled_toolsets                  the complete toolset denylist you want; a list that drops a baseline entry is an error

Policy floor: ``BASELINE_DISABLED`` below is data in this adapter. The hub always renders the union of it and your list, so a fragment
can never weaken it, and emits an ERROR diagnostic when params.disabled_toolsets drops a member or a permission rule allows a
capability whose toolset is in the floor.
"""
from __future__ import annotations

import os
import posixpath
import re
from typing import Any

from agent_hub.domain import yamlsafe

from . import _util as u
from .base import (
    AdapterCaps,
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
    SkillItem,
)

# Toolset names as Hermes Agent spells them (keep this list in step with the toolsets your Hermes version defines): every toolset that
# reaches an external API, drives a desktop, or grants persistence stays disabled.
BASELINE_DISABLED: tuple[str, ...] = (
    "web", "search", "x_search", "browser", "computer_use", "image_gen", "bfl",
    "video_gen", "video", "tts", "cronjob", "homeassistant", "desktop_ui",
    "code_execution",
)
# capability -> the floor toolset that a rule allowing it would contradict
_CAP_TOOLSET = {"web_fetch": "web", "web_search": "search", "browser": "browser", "image": "image_gen"}
TOOL_MAP: dict[str, str | None] = {
    "read": "file", "write": "file", "edit": "file", "shell": "terminal", "web_fetch": None, "web_search": None,
    "mcp": "mcp", "subagent": None, "browser": None, "notebook": None, "todo": None, "image": None,
}
DEFAULT_MANAGE = {"skills": True, "agents": False, "instructions": False, "mcp": False, "permissions": False, "memory": False}
PLACEHOLDER = "CHANGEME"


def _approval_glob(g: str) -> str:
    g = g.strip()
    return ("*" if not g.startswith("*") else "") + g + ("*" if not g.endswith("*") else "")


class HermesAdapter(ClientAdapter):
    id = "hermes-agent"
    display_name = "Hermes Agent"

    def caps(self) -> AdapterCaps:
        return AdapterCaps(
            skills=True, agents=False, instructions=True, mcp=True, permissions=True, egress=False, memory=False,
            tool_map=dict(TOOL_MAP), model_tiers={},
            notes="Renders to a hub-owned stage dir; delivery into the container volume is the opt-in deploy/hermes-deliver.sh. "
                  "Hermes has no per-agent definitions and model selection is owned by the deployment.",
        )

    def default_config(self) -> ClientConfig:
        return ClientConfig(
            id="hermes", adapter=self.id, display_name="Hermes Agent", strict=True,
            description="Hermes Agent. Renders a stage tree; skills are delivered by deploy/hermes-deliver.sh.",
            roots={"stage": "~/path/to/hub/out/hermes"},
            params={"manage": dict(DEFAULT_MANAGE), "hermes_bin": "hermes", "compose_project": PLACEHOLDER,
                    "compose_service": "hermes", "source_root": "~/path/to/project", "source_dirs": []},
            icon="bot",
        )

    # ------------------------------------------------------------------------------------------------ render (pure)
    def render(self, ctx: RenderContext) -> RenderResult:
        cfg = ctx.client
        diags: list[Diag] = []
        arts: list[Artifact] = u.skill_artifacts(ctx.skills, "stage", "skills", diags)
        if ctx.agents:
            diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability",
                                "Hermes has no per-agent definitions; agents are not rendered"))
        if ctx.memory:
            diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability", "memory is not rendered for Hermes"))
        if ctx.instructions:
            arts.append(Artifact(root="stage", path="AGENTS.md", content=u.to_bytes(u.render_instructions(ctx.instructions) + "\n"),
                                 kind="instruction", merge="own",
                                 source_ids=[i.id for i in sorted(ctx.instructions, key=lambda i: (i.order, i.id))]))
        if ctx.mcp_servers:
            servers = self._mcp(ctx.mcp_servers, diags)
            arts.append(Artifact(root="stage", path="config.mcp.fragment.yaml", content=u.dump_yaml({"mcp_servers": servers}),
                                 kind="mcp", source_ids=sorted(servers), merge="yaml_keys", managed_keys=["mcp_servers"]))
        # the policy fragment is ALWAYS rendered: the floor must be visible in the diff even when no rules are enabled
        deny = self._deny(ctx.permissions)
        disabled = self._disabled(cfg, ctx.permissions, diags)
        arts.append(Artifact(root="stage", path="config.policy.fragment.yaml",
                             content=u.dump_yaml({"approvals": {"deny": deny}, "agent": {"disabled_toolsets": disabled}}),
                             kind="rule", source_ids=sorted({f"{r.kind}:{r.match}" for r in ctx.permissions.rules}) or ["floor"],
                             merge="yaml_keys", managed_keys=["agent.disabled_toolsets", "approvals.deny"]))
        arts.sort(key=lambda a: (a.root, a.path))
        return RenderResult(artifacts=arts, diagnostics=diags)

    def _disabled(self, cfg: ClientConfig, perms: PermissionSet, diags: list[Diag]) -> list[str]:
        wanted = cfg.params.get("disabled_toolsets")
        chosen = [str(x) for x in wanted] if isinstance(wanted, list) else list(BASELINE_DISABLED)
        missing = [t for t in BASELINE_DISABLED if t not in chosen]
        if missing:
            diags.append(u.diag("error", "floor_weakened",
                                f"params.disabled_toolsets omits baseline toolsets {missing}; rendering the floor regardless"))
        for r in perms.rules:
            ts = _CAP_TOOLSET.get(r.match) if r.kind == "tool" else None
            if ts and r.decision == "allow":
                diags.append(u.diag("error", "floor_conflict",
                                    f"rule allows {r.match!r} but toolset {ts!r} is in the Hermes baseline-disabled floor"))
        return sorted(set(chosen) | set(BASELINE_DISABLED))

    def _deny(self, perms: PermissionSet) -> list[str]:
        out: set[str] = set()
        for r in perms.rules:
            if r.decision != "deny":
                continue
            if r.kind in ("command", "path_read", "path_write"):
                out.add(_approval_glob(r.match))
        return sorted(out)

    def _mcp(self, items: list[McpItem], diags: list[Diag]) -> dict[str, Any]:
        out: dict[str, Any] = {}
        for m in sorted(items, key=lambda x: x.name):
            if not u.valid_name(m.name):
                diags.append(u.diag("error", "invalid_name", f"mcp name {m.name!r} is not valid", m.name))
                continue
            if m.scan_status != "clean":
                diags.append(u.diag("info", "mcp_not_clean", f"scan_status={m.scan_status}; the core blocks enabling unscanned servers", m.name))
            if m.env_names:
                diags.append(u.diag("info", "env_names_omitted",
                                    "env var names not rendered: the Hermes agent never holds real keys (credential broker)", m.name))
            if m.sandbox_profile:
                diags.append(u.diag("info", "sandbox_profile_ignored", "the container is the sandbox; sandbox wrapping is not applied", m.name))
            if m.transport == "stdio":
                if not m.command:
                    diags.append(u.diag("error", "mcp_no_command", "stdio server without a command", m.name))
                    continue
                out[m.name] = {"command": m.command, "args": list(m.args)}
            else:
                if not m.url:
                    diags.append(u.diag("error", "mcp_no_url", f"{m.transport} server without a url", m.name))
                    continue
                out[m.name] = {"url": m.url}
        return out

    # ------------------------------------------------------------------------------------------------ discover
    def discover(self, cfg: ClientConfig, fs: FileSystem) -> DiscoveredContent:
        """Reads the client's REAL sources (params.source_dirs, relative to params.lab_dir) plus the hub stage dir. The container
        volume is not visible to the host. Read-only; env VALUES are never read into the result."""
        res = DiscoveredContent()
        lab = os.path.expanduser(str(cfg.params.get("source_root", "")))
        sources = cfg.params.get("source_dirs")
        for src in sources if isinstance(sources, list) else []:
            if not isinstance(src, dict) or src.get("kind") not in ("skills", "instruction", "config") or not isinstance(src.get("path"), str):
                res.notes.append(f"ignored malformed source_dirs entry {src!r}")
                continue
            root = src.get("root")
            base = cfg.roots.get(str(root)) if root else lab
            if not base:
                res.notes.append(f"source root {root!r} is not configured")
                continue
            path = src["path"] if posixpath.isabs(src["path"]) else posixpath.join(base, src["path"])
            if ".." in path.split("/"):
                res.notes.append(f"ignored source path with '..': {src['path']!r}")
                continue
            if not fs.exists(path):
                res.notes.append(f"source not found: {src['path']}")
                continue
            kind = src["kind"]
            if kind == "skills":
                res.skills += _skills_from_dir(fs, path, res.notes)
            elif kind == "instruction":
                raw = u.read_capped(fs, path, res.notes)
                if raw is None:
                    continue
                text = raw.decode("utf-8", errors="replace")
                res.instructions.append(InstructionItem(id=u.valid_name(_stem(path)) and _stem(path) or "workspace", title=posixpath.basename(path),
                                                        order=len(res.instructions), body=text))
            else:
                _read_config(fs, path, res)
        res.skills += [s for s in u.discover_skills(fs, cfg, "stage", "skills", res.notes) if s.name not in {x.name for x in res.skills}]
        return res

    # ------------------------------------------------------------------------------------------------ verify
    def verify(self, cfg: ClientConfig, expected: list[Expected], fs: FileSystem, run: CommandRunner) -> list[Check]:
        checks = u.verify_expected(cfg, expected, fs)
        skill_names = sorted({e.source_ids[0] for e in expected if e.kind == "skill" and e.source_ids})
        checks.append(self._ask_container(cfg, skill_names, run))
        return checks

    def _ask_container(self, cfg: ClientConfig, skills: list[str], run: CommandRunner) -> Check:
        name = "hermes skills list"
        project = str(cfg.params.get("compose_project", PLACEHOLDER))
        service = str(cfg.params.get("compose_service", "hermes"))
        hbin = str(cfg.params.get("hermes_bin", "hermes"))
        ctx_name = str(cfg.params.get("docker_context", ""))
        user = str(cfg.params.get("exec_user", ""))
        dk = ["docker", *(["--context", ctx_name] if ctx_name else [])]
        if not project or project == PLACEHOLDER or not hbin:
            return Check(name=name, ok=False, detail="compose_project/hermes_bin not configured: unverified")
        try:
            rc, out, _err = run.run([*dk, "ps", "--format", "{{.Names}}",
                                     "--filter", f"label=com.docker.compose.project={project}",
                                     "--filter", f"label=com.docker.compose.service={service}"], timeout=15)
        except OSError:
            return Check(name=name, ok=False, detail="docker not available: unverified")
        names = [ln.strip() for ln in out.splitlines() if ln.strip()]
        if rc != 0 or not names:
            return Check(name=name, ok=False, detail="container not running: unverified")
        ctr = names[0]
        try:
            rc, out, err = run.run([*dk, "exec", *(["--user", user] if user else []), ctr, hbin, "skills", "list"], timeout=60)
        except OSError:
            return Check(name=name, ok=False, detail="docker exec failed: unverified")
        if rc != 0:
            return Check(name=name, ok=False, detail=f"`hermes skills list` exited {rc}: {err.strip()[:200]}")
        listed = listed_names(out)
        missing = [s for s in skills if s not in listed]
        if missing:
            return Check(name=name, ok=False, detail=f"container does not list {len(missing)} skill(s): {', '.join(missing[:10])}")
        return Check(name=name, ok=True, detail=f"container lists all {len(skills)} staged skill(s)")


_NAME_TOKEN = re.compile(r"[A-Za-z0-9._-]+")


def listed_names(output: str) -> set[str]:
    """Whole-name tokens of ``hermes skills list`` output (a skill ``video`` must not match ``video-improve-loop``)."""
    return set(_NAME_TOKEN.findall(output))


def _stem(path: str) -> str:
    return posixpath.basename(path).rsplit(".", 1)[0].lower()


def _skills_from_dir(fs: FileSystem, top: str, notes: list[str]) -> list[SkillItem]:
    out: list[SkillItem] = []
    if not fs.is_dir(top):
        return out
    for name in sorted(fs.listdir(top)):
        d = posixpath.join(top, name)
        if not fs.is_dir(d) or not u.valid_name(name):
            continue
        files = u.read_tree(fs, d, notes=notes)
        files = {k: v for k, v in files.items() if not k.endswith(".pyc")}
        if "SKILL.md" not in files:
            notes.append(f"skipped {name!r}: no SKILL.md")
            continue
        fm, _ = u.parse_frontmatter(files["SKILL.md"].decode("utf-8", errors="replace"))
        out.append(SkillItem(name=name, description=u.cap_str(fm.get("description")), files=files))
    return out


def _read_config(fs: FileSystem, path: str, res: DiscoveredContent) -> None:
    try:
        raw = u.read_capped(fs, path, res.notes)
        if raw is None:
            return
        doc = yamlsafe.load(raw.decode("utf-8", errors="replace"), max_bytes=u.MAX_READ_FILE_BYTES)
    except yamlsafe.YamlError as exc:
        res.notes.append(f"{posixpath.basename(path)} rejected by the safe YAML loader: {exc}"[:200])
        return
    if not isinstance(doc, dict):
        return
    servers = doc.get("mcp_servers")
    for name, e in sorted((servers if isinstance(servers, dict) else {}).items()):
        if not isinstance(e, dict) or not u.valid_name(str(name)):
            res.notes.append(f"skipped mcp server {name!r}")
            continue
        env = sorted(str(k) for k in e["env"]) if isinstance(e.get("env"), dict) else []      # names only, values dropped
        if e.get("url"):
            res.mcp_servers.append(McpItem(name=str(name), transport="http", url=u.cap_str(e["url"], 2000), env_names=env))
        elif e.get("command"):
            res.mcp_servers.append(McpItem(name=str(name), transport="stdio", command=u.cap_str(e["command"], 2000),
                                           args=[str(a) for a in e.get("args", []) if isinstance(a, (str, int))], env_names=env))
    deny = (doc.get("approvals") or {}).get("deny") if isinstance(doc.get("approvals"), dict) else None
    rules = [Rule(kind="command", match=str(g), decision="deny") for g in deny or [] if isinstance(g, str)]
    if rules:
        res.permissions = PermissionSet(rules=(res.permissions.rules if res.permissions else []) + rules)
    dis = (doc.get("agent") or {}).get("disabled_toolsets") if isinstance(doc.get("agent"), dict) else None
    if isinstance(dis, list):
        res.notes.append(f"agent.disabled_toolsets floor ({len(dis)} toolsets) noted, not imported: it is enforced by the adapter, never an allow")


ADAPTER = HermesAdapter

__all__ = ["ADAPTER", "BASELINE_DISABLED", "HermesAdapter", "listed_names"]
