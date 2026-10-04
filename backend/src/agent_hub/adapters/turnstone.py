"""Turnstone adapter (id ``turnstone``), for github.com/turnstonelabs/turnstone.

Turnstone keeps skills, MCP servers and settings in its database and takes them through its console's admin API,
not from files, so the hub never writes into it. ``render`` produces a STAGE tree (root ``stage``, a host directory
the hub owns):

  skills/<name>/...      the skills tree (SKILL.md + resources); Turnstone parses the same SKILL.md format
  instructions.md        the default system instruction (the ``session.instructions`` setting)
  mcp.json               {"mcpServers": {...}}, the format of Turnstone's MCP import

Delivery is the opt-in ``deploy/turnstone-deliver.py`` (never run by the app): it pushes skills (as hub-owned
skills, never auto-approved), and only with explicit flags the instructions and MCP servers. ``verify`` checks the
stage files and then asks the running console, through that script's ``list`` command, whether every staged skill
is there as a hub-owned skill. The API token is read by the script from a 0600 file; it never appears in the hub,
in a command line or in output.

Params (all optional). They come from the client YAML, so verify refuses (a failed check, nothing run) anything outside:
  console_url    a loopback http(s) URL, no credentials, path or query (default http://127.0.0.1:8796)
  token_file     a `*.token` file directly or deeper under ~/.config/agent-hub/, not a symlink, holding a Turnstone API
                 token with read,write,approve scopes for an admin user (default ~/.config/agent-hub/turnstone.token)
The script runs under the hub's own interpreter; no parameter can choose the program.
"""
from __future__ import annotations

import ipaddress
import json
import os
import sys
import urllib.parse
from pathlib import Path
from typing import Any

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
    McpItem,
    RenderContext,
    RenderResult,
)

DEFAULT_MANAGE = {"skills": True, "agents": False, "instructions": False, "mcp": False, "permissions": False, "memory": False}
DEFAULT_CONSOLE = "http://127.0.0.1:8796"
DEFAULT_TOKEN_FILE = "~/.config/agent-hub/turnstone.token"   # noqa: S105 - the path of the token file, not a secret
DELIVER_SCRIPT = Path(__file__).resolve().parents[4] / "deploy" / "turnstone-deliver.py"
MAX_SKILL_MD = 32 * 1024                  # Turnstone refuses a larger SKILL.md / skill body
TOKEN_DIR = Path.home() / ".config" / "agent-hub"


def console_problem(url: str) -> str | None:
    """None if `url` is a loopback http(s) base URL; else why not (never sends the token off the box from verify)."""
    try:
        u_ = urllib.parse.urlsplit(url)
        host = u_.hostname or ""
        port_ok = u_.port is None or 1 <= u_.port <= 65535
    except ValueError:
        return "not a valid URL"
    if u_.scheme not in ("http", "https") or not host or not port_ok:
        return "must be an http(s) URL with a host"
    if u_.username or u_.password or u_.path not in ("", "/") or u_.query or u_.fragment:
        return "must be a bare base URL (no credentials, path, query or fragment)"
    if host != "localhost":
        try:
            if not ipaddress.ip_address(host).is_loopback:
                return "must be a loopback address"
        except ValueError:
            return "must be a loopback address"
    return None


def token_file_problem(path: str) -> str | None:
    """None if `path` is a non-symlink `*.token` file inside TOKEN_DIR; else why not. (The suffix keeps other files that
    live there, such as an env file holding the hub's own key, from being sent as a Turnstone token.)"""
    p = Path(path)
    if p.is_symlink():
        return "must not be a symlink"
    real, base = os.path.realpath(p), os.path.realpath(TOKEN_DIR)
    if os.path.commonpath([real, base]) != base or real == base:
        return f"must be inside {TOKEN_DIR}"
    if not p.name.endswith(".token"):
        return "must be a *.token file"
    return None


class TurnstoneAdapter(ClientAdapter):
    id = "turnstone"
    display_name = "Turnstone"

    def caps(self) -> AdapterCaps:
        return AdapterCaps(
            skills=True, agents=False, instructions=True, mcp=True, permissions=False, egress=False, memory=False,
            tool_map={}, model_tiers={},
            notes="Renders to a hub-owned stage dir; delivery into Turnstone's database is the opt-in "
                  "deploy/turnstone-deliver.py (admin API). Personas, governance policies and model selection stay in "
                  "Turnstone.",
        )

    def default_config(self) -> ClientConfig:
        return ClientConfig(
            id="turnstone", adapter=self.id, display_name="Turnstone", strict=True,
            description="Turnstone. Renders a stage tree; deploy/turnstone-deliver.py pushes it through the admin API.",
            roots={"stage": "~/path/to/hub/out/turnstone"},
            params={"manage": dict(DEFAULT_MANAGE), "console_url": DEFAULT_CONSOLE, "token_file": DEFAULT_TOKEN_FILE},
            icon="box",
        )

    # ------------------------------------------------------------------------------------------------ render (pure)
    def render(self, ctx: RenderContext) -> RenderResult:
        cfg = ctx.client
        diags: list[Diag] = []
        arts: list[Artifact] = u.skill_artifacts(ctx.skills, "stage", "skills", diags)
        for s in ctx.skills:
            size = len(s.files.get("SKILL.md", b""))
            if size > MAX_SKILL_MD:
                diags.append(u.diag("error", "skill_too_large",
                                    f"SKILL.md is {size} bytes; Turnstone accepts at most {MAX_SKILL_MD}", s.name))
            fm, _ = u.parse_frontmatter(s.files.get("SKILL.md", b"").decode("utf-8", errors="replace"))
            if fm.get("allowed-tools") or fm.get("allowed_tools"):
                diags.append(u.diag("warn", "allowed_tools_dropped",
                                    "allowed-tools is not delivered to Turnstone (with auto_approve it would let the listed "
                                    "tools run unasked); Turnstone's approval gates apply", s.name))
            if fm.get("auto_approve") or fm.get("auto-approve"):
                diags.append(u.diag("info", "auto_approve_ignored",
                                    "the delivery script always creates skills with auto_approve off", s.name))
        if ctx.agents:
            diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability",
                                "Turnstone has no per-agent definitions the hub can map (personas stay in Turnstone); "
                                "agents are not rendered"))
        if ctx.memory:
            diags.append(u.diag(u.unsupported_severity(cfg), "unsupported_capability", "memory is not rendered for Turnstone"))
        if ctx.permissions.rules:
            diags.append(u.diag("info", "permissions_not_rendered",
                                "Turnstone's own approval gates and governance policies apply; hub rules are not rendered"))
        if ctx.instructions:
            arts.append(Artifact(root="stage", path="instructions.md",
                                 content=u.to_bytes(u.render_instructions(ctx.instructions) + "\n"), kind="instruction",
                                 merge="own", source_ids=[i.id for i in sorted(ctx.instructions, key=lambda i: (i.order, i.id))]))
        if ctx.mcp_servers:
            servers = self._mcp(ctx.mcp_servers, diags)
            arts.append(Artifact(root="stage", path="mcp.json", content=u.dump_json({"mcpServers": servers}), kind="mcp",
                                 merge="own", source_ids=sorted(servers)))
        arts.sort(key=lambda a: (a.root, a.path))
        return RenderResult(artifacts=arts, diagnostics=diags)

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
                                    "env var names not rendered: set credentials in Turnstone, never in the hub", m.name))
            if m.sandbox_profile:
                diags.append(u.diag("info", "sandbox_profile_ignored",
                                    "Turnstone runs MCP servers itself; sandbox wrapping is not applied", m.name))
            if m.transport == "stdio":
                if not m.command:
                    diags.append(u.diag("error", "mcp_no_command", "stdio server without a command", m.name))
                    continue
                diags.append(u.diag("warn", "mcp_stdio_in_node",
                                    "a stdio server runs inside the Turnstone node; anything it fetches at start (npx, uvx) "
                                    "needs network the node may not have. Prefer an http server run in its own sandbox", m.name))
                out[m.name] = {"command": m.command, "args": list(m.args)}
            else:
                if not m.url:
                    diags.append(u.diag("error", "mcp_no_url", f"{m.transport} server without a url", m.name))
                    continue
                out[m.name] = {"type": "streamable-http" if m.transport == "http" else "sse", "url": m.url}
        return out

    # ------------------------------------------------------------------------------------------------ discover
    def discover(self, cfg: ClientConfig, fs: FileSystem) -> DiscoveredContent:
        """Turnstone's real content lives in its database; import reads only the hub's own stage."""
        res = DiscoveredContent(notes=["Turnstone keeps skills in its database; import reads only the hub stage dir"])
        res.skills = u.discover_skills(fs, cfg, "stage", "skills", res.notes)
        return res

    # ------------------------------------------------------------------------------------------------ verify
    def verify(self, cfg: ClientConfig, expected: list[Expected], fs: FileSystem, run: CommandRunner) -> list[Check]:
        checks = u.verify_expected(cfg, expected, fs)
        skill_names = sorted({e.source_ids[0] for e in expected if e.kind == "skill" and e.source_ids})
        checks.append(self._ask_console(cfg, skill_names, run))
        return checks

    def _ask_console(self, cfg: ClientConfig, skills: list[str], run: CommandRunner) -> Check:
        name = "turnstone skills (console API)"
        console = str(cfg.params.get("console_url") or DEFAULT_CONSOLE)
        token_file = os.path.expanduser(str(cfg.params.get("token_file") or DEFAULT_TOKEN_FILE))
        if prob := console_problem(console):
            return Check(name=name, ok=False, detail=f"params.console_url {prob}: not checked")
        if prob := token_file_problem(token_file):
            return Check(name=name, ok=False, detail=f"params.token_file {prob}: not checked")
        if not DELIVER_SCRIPT.is_file():
            return Check(name=name, ok=False, detail="the delivery script is not installed next to the hub: not checked")
        try:
            rc, out, err = run.run([sys.executable, str(DELIVER_SCRIPT), "list", "--console", console,
                                    "--token-file", token_file], timeout=30)
        except OSError:
            return Check(name=name, ok=False, detail="could not run the delivery script: unverified")
        if rc != 0:
            return Check(name=name, ok=False, detail=f"listing skills failed: {err.strip()[:200]}")
        try:
            listed = json.loads(out)
            hub = set(listed.get("hub_skills", [])) if isinstance(listed, dict) else set()
            unsafe = set(listed.get("unsafe", [])) if isinstance(listed, dict) else set()
        except ValueError:
            return Check(name=name, ok=False, detail="the delivery script printed no JSON: unverified")
        flipped = [s for s in skills if s in unsafe]
        if flipped:
            return Check(name=name, ok=False, detail=f"auto_approve or is_default was switched on in Turnstone for: "
                                                     f"{', '.join(flipped[:10])} (the next push resets it)")
        missing = [s for s in skills if s not in hub]
        if missing:
            return Check(name=name, ok=False, detail=f"Turnstone has no hub-delivered skill for {len(missing)} staged skill(s): "
                                                     f"{', '.join(missing[:10])} (run deploy/turnstone-deliver.py push)")
        return Check(name=name, ok=True, detail=f"Turnstone lists all {len(skills)} staged skill(s) as hub-delivered")


ADAPTER = TurnstoneAdapter

__all__ = ["ADAPTER", "DELIVER_SCRIPT", "TurnstoneAdapter"]
