"""The policy floor: loaded from the APP repo (policy/floor.yaml), enforced on every effective config.
Floor wins; content may only be stricter. Violations are structured findings, never exceptions."""
from __future__ import annotations

import fnmatch
import posixpath
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from pydantic import BaseModel, ConfigDict, ValidationError

from agent_hub.adapters.base import McpItem, PermissionSet, Rule
from agent_hub.domain import yamlsafe
from agent_hub.domain.errors import Unavailable
from agent_hub.domain.models import Finding

ORDER = {"deny": 0, "ask": 1, "allow": 2}
_HOME = str(Path.home())


class ToolPolicy(BaseModel):
    model_config = ConfigDict(extra="forbid")
    default: str = "ask"
    max: str = "ask"


class FloorPolicy(BaseModel):
    model_config = ConfigDict(extra="forbid")
    version: int = 1
    network_default: str = "deny"
    egress: dict[str, list[str]] = {}
    deny_path_read: list[str] = []
    deny_path_write: list[str] = []
    tools: dict[str, ToolPolicy] = {}
    default_tool_decision: str = "ask"
    command_allow_forbid_patterns: list[str] = []
    command_allow_probes: list[str] = []      # an allow pattern matching any probe is effectively a catch-all
    command_allow_warn_probes: list[str] = [] # general-purpose launchers: warn (an error only when the pattern is a bare wildcard)
    mcp: dict[str, bool] = {}


def load_floor(path: Path) -> FloorPolicy:
    """Fail closed: a missing or invalid floor means the app does not start (never 'no floor')."""
    try:
        return FloorPolicy.model_validate(yamlsafe.load(path.read_text(encoding="utf-8")))
    except (OSError, ValidationError, yamlsafe.YamlError) as exc:
        raise Unavailable(f"policy floor unusable ({path}): {str(exc)[:200]}", code="floor_unavailable") from exc


def norm_path(p: str) -> str:
    """Expand a leading ~ to the (fixed) home dir so `~/x/**` and `/home/u/x/**` compare as the same thing."""
    if p == "~" or p.startswith("~/"):
        p = _HOME + p[1:]
    if p.startswith("/") and ("/." in p or "//" in p):
        p = posixpath.normpath(p)               # collapse `.`/`..`/`//` so `~/a/../.aws` cannot dodge a deny
    return p


def _stem(p: str) -> str:
    for i, ch in enumerate(p):
        if ch in "*?[":
            return p[:i]
    return p


def overlaps(a: str, b: str) -> bool:
    """Could one path glob match anything the other matches? Deliberately over-approximate (fail closed)."""
    a, b = norm_path(a), norm_path(b)
    if fnmatch.fnmatchcase(a, b) or fnmatch.fnmatchcase(b, a):
        return True
    for x, y in ((a, b), (b, a)):                # a leading `**/` also matches the bare tail (`.env` vs `**/.env`)
        if y.startswith("**/") and (fnmatch.fnmatchcase(x, y[3:]) or fnmatch.fnmatchcase("/" + x.lstrip("/"), y)):
            return True
    sa, sb = _stem(a), _stem(b)
    if not sa or not sb:            # a leading-`**` pattern only overlaps what fnmatch says it does
        return False
    return sa.startswith(sb) or sb.startswith(sa)


def host_matches(pattern: str, host: str) -> bool:
    return fnmatch.fnmatchcase(host.lower(), pattern.lower())


@dataclass
class FloorResult:
    permissions: PermissionSet
    findings: list[Finding]
    allow_hosts: list[str]
    kept: list[Rule]
    kept_ids: list[str]


class Floor:
    def __init__(self, policy: FloorPolicy) -> None:
        self.p = policy

    # -- rules ---------------------------------------------------------------------------------
    def floor_rules(self) -> list[Rule]:
        rules = [Rule(kind="path_read", match=g, decision="deny", reason="floor: secrets and credentials")
                 for g in self.p.deny_path_read]
        rules += [Rule(kind="path_write", match=g, decision="deny", reason="floor: secrets and credentials")
                  for g in self.p.deny_path_write]
        return rules

    def check_rule(self, r: Rule, concern: str = "permissions", rid: str | None = None) -> Finding | None:
        """None when the rule is acceptable, else the violation. Used for content rules and for live-config audits."""
        def bad(code: str, msg: str) -> Finding:
            return Finding(code=code, severity="error", message=msg, item=rid, concern=concern,
                           rule=f"{r.kind}:{r.match}")
        if r.decision == "allow":
            if r.kind == "path_read":
                hit = next((g for g in self.p.deny_path_read if overlaps(r.match, g)), None)
                if hit:
                    return bad("floor_path_read", f"allowing reads of {r.match!r} overlaps the floor-denied {hit!r}")
            if r.kind == "path_write":
                hit = next((g for g in self.p.deny_path_write if overlaps(r.match, g)), None)
                if hit:
                    return bad("floor_path_write", f"allowing writes to {r.match!r} overlaps the floor-denied {hit!r}")
            if r.kind == "egress_host" and (r.match in self.p.egress.get("forbid_allow_patterns", [])
                                            or any(host_matches(r.match, pr) for pr in self.p.egress.get("forbid_allow_probes", []))):
                return bad("floor_egress_wildcard", f"a catch-all egress allow ({r.match!r}) defeats deny-by-default")
            if r.kind == "command" and (r.match.strip() in self.p.command_allow_forbid_patterns or not r.match.strip()
                                        or any(fnmatch.fnmatchcase(pr, r.match.strip()) for pr in self.p.command_allow_probes)):
                return bad("floor_command_pattern", "allow on a command rule needs a specific pattern, not a catch-all")
            if r.kind == "command" and any(fnmatch.fnmatchcase(pr, r.match.strip()) for pr in self.p.command_allow_warn_probes):
                return Finding(code="floor_command_pattern", severity="warn", concern=concern, item=rid, rule=f"{r.kind}:{r.match}",
                               message=f"allow {r.match!r} lets the agent run arbitrary programs through a general-purpose launcher; "
                                       "narrow it to specific arguments")
        if r.kind == "tool":
            pol = self.p.tools.get(r.match)
            if pol is not None and ORDER.get(r.decision, 2) > ORDER.get(pol.max, 2):
                return bad("floor_tool", f"tool {r.match!r} may be at most {pol.max!r}, rule says {r.decision!r}")
        return None

    def check_permission_set(self, ps: PermissionSet, concern: str = "permissions") -> list[Finding]:
        out = [f for r in ps.rules if (f := self.check_rule(r, concern)) is not None]
        if ORDER.get(ps.default_tool_decision, 2) > ORDER.get(self.p.default_tool_decision, 1):
            out.append(Finding(code="floor_default_tool", severity="error", concern=concern,
                               message=f"default tool decision {ps.default_tool_decision!r} is weaker than the floor's "
                                       f"{self.p.default_tool_decision!r}"))
        if ps.network_default != self.p.network_default:
            out.append(Finding(code="floor_network_default", severity="error", concern=concern,
                               message=f"network default must be {self.p.network_default!r}"))
        return out

    def enforce(self, content_rules: list[tuple[str, Rule]], mcp_clean: dict[str, bool] | None = None) -> FloorResult:
        """content_rules: [(rule id, Rule)]. Violating rules are dropped (the output stays safe) and reported."""
        findings: list[Finding] = []
        kept: list[Rule] = []
        kept_ids: list[str] = []
        for rid, r in content_rules:
            f = self.check_rule(r, "permissions", rid) or self._mcp_rule(r, rid, mcp_clean)
            if f is not None:
                findings.append(f)
            if f is None or f.severity != "error":               # warnings keep the rule; errors drop it
                kept.append(r)
                kept_ids.append(rid)
        named_tools = {r.match for r in kept if r.kind == "tool"}
        defaults = [Rule(kind="tool", match=t, decision=p.default, reason="floor default")
                    for t, p in self.p.tools.items() if t not in named_tools and p.default == "deny"]
        ps = PermissionSet(rules=[*self.floor_rules(), *defaults, *kept],
                           default_tool_decision=self.p.default_tool_decision,
                           network_default=self.p.network_default)
        allow_hosts = sorted({r.match for r in kept if r.kind == "egress_host" and r.decision == "allow"})
        return FloorResult(ps, findings, allow_hosts, kept, kept_ids)

    def _mcp_rule(self, r: Rule, rid: str | None, mcp_clean: dict[str, bool] | None) -> Finding | None:
        """MCP allow rules. A WILDCARD allow (`tool:mcp`, `mcp__*`, a globbed server name) is always an error. A NAMED allow
        (`mcp__<server>[__tool]`, kind mcp_server) is valid only for a server that is enabled AND clean for this client."""
        if r.decision != "allow" or mcp_clean is None:
            return None
        server: str | None
        if r.kind == "mcp_server":
            server = r.match
        elif r.kind == "tool" and (r.match == "mcp" or r.match.startswith("mcp__")):
            server = None if r.match == "mcp" else r.match[5:].split("__", 1)[0]
        else:
            return None

        def err(msg: str) -> Finding:
            return Finding(code="floor_mcp_rule", severity="error", item=rid, concern="permissions", rule=f"{r.kind}:{r.match}",
                           message=msg)
        if not server or any(c in server for c in "*?["):
            return err(f"allow {r.kind}:{r.match!r} is a wildcard over MCP servers; name the server, and it must be enabled and clean")
        if server not in mcp_clean:
            return err(f"allow {r.kind}:{r.match!r} names MCP server {server!r}, which is not enabled for this client")
        if not mcp_clean[server]:
            return err(f"allow {r.kind}:{r.match!r} would enable MCP server {server!r}, which is not scan_status=clean")
        return None

    # -- skills ----------------------------------------------------------------------------------
    @staticmethod
    def _split_tools(raw: Any) -> list[str]:
        """allowed-tools entries: split on commas AND whitespace, but not inside parentheses (`Bash(git status:*)`)."""
        if isinstance(raw, list):
            return [str(e).strip() for e in raw if str(e).strip()]
        if not isinstance(raw, str):
            return []
        out, cur, depth = [], "", 0
        for ch in raw:
            if ch == "(":
                depth += 1
            elif ch == ")":
                depth = max(0, depth - 1)
            if depth == 0 and (ch == "," or ch.isspace()):
                if cur:
                    out.append(cur)
                cur = ""
            else:
                cur += ch
        if cur:
            out.append(cur)
        return out

    def check_skill(self, name: str, extra: dict[str, Any], clean_servers: set[str] | None = None) -> list[Finding]:
        """SKILL.md is client-owned and delivered byte-for-byte, so the floor can only judge it, never rewrite it."""
        out: list[Finding] = []
        if "hooks" in extra:
            out.append(Finding(code="skill_declares_hooks", severity="error", concern="skills", item=name,
                               message=f"skill {name!r} declares hooks in its frontmatter: hooks run code and are not allowed via skills"))
        entries = self._split_tools(extra.get("allowed-tools"))
        wide = [e for e in entries if e in ("Bash", "*", "mcp__*") or re.fullmatch(r"Bash\(\s*[*:]*\s*\)", e)]
        if wide:
            out.append(Finding(code="skill_unrestricted_tools", severity="error", concern="skills", item=name,
                               message=f"skill {name!r} pre-approves unrestricted tools {wide[:3]} via allowed-tools"))
        mcp_named = [e for e in entries if e.startswith("mcp__") and e not in wide]
        if clean_servers is not None:
            bad = [e for e in mcp_named if (e[5:].split("__", 1)[0] or "*") not in clean_servers]
            if bad:
                out.append(Finding(code="skill_unscanned_mcp_tool", severity="error", concern="skills", item=name,
                                   message=f"skill {name!r} pre-approves MCP tools {bad[:3]} of servers that are not enabled and clean here"))
        if entries and not wide and not any(f.code == "skill_unscanned_mcp_tool" for f in out):
            out.append(Finding(code="skill_preapproves_tools", severity="warn", concern="skills", item=name,
                               message=f"skill {name!r} pre-approves tools via allowed-tools: {entries[:5]}"))
        return out

    # -- mcp -----------------------------------------------------------------------------------
    def check_mcp(self, m: McpItem, rules: list[Rule], concern: str = "mcp") -> list[Finding]:
        out: list[Finding] = []

        def err(code: str, msg: str) -> None:
            out.append(Finding(code=code, severity="error", message=msg, item=m.name, concern=concern))
        if self.p.mcp.get("require_scan_clean", True) and m.scan_status != "clean":
            err("mcp_not_clean", f"MCP server {m.name!r} has scan_status={m.scan_status!r}; it must be 'clean' "
                                 "(run your MCP scanner yourself, then `hubctl attest-mcp NAME`; edits to command/args/url/pinned_ref/transport, "
                                 "or a missing/foreign attestation, reset it)")
        allowed = [r.match for r in rules if r.kind == "egress_host" and r.decision == "allow"]
        denied = [r.match for r in rules if r.kind == "egress_host" and r.decision == "deny"]
        if self.p.mcp.get("require_explicit_egress", True):
            for h in m.egress_hosts:
                if any(host_matches(d, h) for d in denied) or not any(host_matches(a, h) for a in allowed):
                    err("mcp_egress_not_allowed", f"egress host {h!r} of MCP server {m.name!r} has no explicit "
                                                  "egress_host allow rule")
        if m.transport == "stdio" and self.p.mcp.get("require_sandbox_stdio", True) and not m.sandbox_profile:
            err("mcp_no_sandbox", f"stdio MCP server {m.name!r} has no sandbox_profile (each MCP server runs in its own sandbox)")
        if m.transport != "stdio" and m.url:
            host = urlsplit(m.url).hostname or ""
            loop = self.p.egress.get("loopback_hosts", [])
            if host not in loop:
                if not m.egress_hosts:
                    err("mcp_no_egress_declared", f"{m.transport} MCP server {m.name!r} must declare its egress_hosts")
                elif not any(host_matches(h, host) for h in m.egress_hosts):
                    err("mcp_url_host_undeclared", f"URL host {host!r} of MCP server {m.name!r} is not in its egress_hosts")
        return out

    # -- audit of a live config (advisory concerns included) -----------------------------------------
    def missing_denies(self, ps: PermissionSet) -> list[Finding]:
        out = []
        for kind, globs in (("path_read", self.p.deny_path_read), ("path_write", self.p.deny_path_write)):
            for g in globs:
                covered = any(r.kind == kind and r.decision == "deny"
                              and fnmatch.fnmatchcase(norm_path(g), norm_path(r.match)) for r in ps.rules)
                if not covered:
                    out.append(Finding(code="floor_deny_missing", severity="warn", concern="permissions",
                                       message=f"live config has no {kind} deny covering {g!r}", rule=f"{kind}:{g}"))
        return out

    def describe(self) -> dict[str, Any]:
        return self.p.model_dump(mode="json")
