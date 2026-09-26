"""`hubctl doctor` / GET /api/v1/doctor: a READ-ONLY report of how exposed the hub's secrets and policy are to other processes of
the same OS user. It never prints a secret, never writes, and all host access goes through an injectable filesystem and env."""
from __future__ import annotations

import fnmatch
import json
import re
import stat
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Protocol

from agent_hub.config import Settings
from agent_hub.services import contentguard

STATE_PATHS = ("~/.local/share/agent-hub", "~/.config/agent-hub", "~/.local/share/agent-knowledge", "~/.config/agent-knowledge")
POLICY_GLOB = "**/hub/policy/**"
# Client params the claude-code checks read. All host-specific paths come from the client's own configuration; when a param is
# absent the check is skipped with an `info: not configured` finding (never guessed).
CLAUDE_PARAMS = ("settings_template", "settings_rendered", "network_allowlist_key")
DOCS_POINTER = "docs/SECURITY.md"
MAX_READ = 1_000_000
LOOPBACK_DOMAINS = {"localhost", "127.0.0.1", "::1", "[::1]", "0.0.0.0", "host.docker.internal"}  # noqa: S104 - a denylist, not a bind


class DoctorFS(Protocol):
    def read_text(self, path: Path) -> str | None: ...
    def stat(self, path: Path) -> Any | None: ...
    def glob(self, directory: Path, pattern: str) -> list[Path]: ...


class RealDoctorFS:
    def read_text(self, path: Path) -> str | None:
        try:
            if path.stat().st_size > MAX_READ:
                return None
            return path.read_text(errors="replace")
        except OSError:
            return None

    def stat(self, path: Path) -> Any | None:
        try:
            return path.lstat()
        except OSError:
            return None

    def glob(self, directory: Path, pattern: str) -> list[Path]:
        try:
            return sorted(directory.glob(pattern))
        except OSError:
            return []


@dataclass
class DoctorContext:
    cfg: Settings
    fs: DoctorFS
    env: Mapping[str, str]
    home: Path
    app_root: Path
    uid: int
    clients: list[dict[str, Any]] = field(default_factory=list)   # {id, adapter, roots, manage}
    knowledge: dict[str, Any] | None = None                       # the /knowledge/health summary, if it was reachable
    permission_gaps: Callable[[str], list[str] | None] = lambda _cid: None
    floor_read: list[str] = field(default_factory=list)     # the floor's deny_path_read / deny_path_write globs
    floor_write: list[str] = field(default_factory=list)


def finding(fid: str, sev: str, title: str, detail: str = "", fix: str = "") -> dict[str, str]:
    return {"id": fid, "severity": sev, "title": title, "detail": detail, "fix": fix}


def _expand(p: str, home: Path) -> Path:
    return home / p[2:] if p.startswith("~/") else Path(p)


# ---- individual checks -------------------------------------------------------------------------------------------------
def check_bootstrap_key(c: DoctorContext) -> list[dict[str, str]]:
    f = c.cfg.data_dir / "bootstrap-admin.key"
    if c.fs.stat(f) is None:
        return [finding("bootstrap-key", "ok", "No bootstrap admin key file at rest")]
    return [finding("bootstrap-key", "warn", "The bootstrap admin key file still exists",
                    f"{f} is readable by every process of your OS user (including unsandboxed agents).",
                    "Store the key in a password manager, then run: hubctl retire-bootstrap-key")]


def check_modes(c: DoctorContext) -> list[dict[str, str]]:
    d = c.cfg.data_dir
    dirs = [d, *(d / n for n in ("plans", "backups", "snapshots", "locks", "applies", "content")), c.cfg.content_dir]
    files = [d / "hub.db", d / "attest.key", d / "bootstrap-admin.key", c.cfg.knowledge_admin_key_file]
    bad: list[str] = []
    fixes: list[str] = []
    for p, want in [(x, 0o700) for x in dirs] + [(x, 0o600) for x in files]:
        st = c.fs.stat(p)
        if st is None:
            continue
        mode = stat.S_IMODE(st.st_mode)
        if mode & 0o077:
            bad.append(f"{p} is {oct(mode)} (expected {oct(want)})")
            fixes.append(f"chmod {oct(want)[2:]} {p}")
    if not bad:
        return [finding("modes", "ok", "State directories and key files are private (0700 / 0600)")]
    return [finding("modes", "warn", "Some hub state is readable by other users", "; ".join(bad[:8]), "; ".join(fixes[:8]))]


def check_content_dir(c: DoctorContext) -> list[dict[str, str]]:
    roots = [str(_expand(r, c.home)) for cl in c.clients for r in cl.get("roots", {}).values()]
    problems: list[str] = []
    try:
        contentguard.check_location(c.cfg.content_dir, roots, c.app_root)
    except Exception as exc:  # noqa: BLE001 - ProblemError; the message carries the reason
        problems.append(str(getattr(exc, "detail", exc)).split(". Keep the content repo")[0])
    st = c.fs.stat(c.cfg.content_dir)
    if st is not None and st.st_uid != c.uid:
        problems.append(f"{c.cfg.content_dir} is not owned by the current user")
    if not problems:
        return [finding("content-dir", "ok", "Content repo is outside every client root and the app tree, and owned by you")]
    return [finding("content-dir", "crit", "The content repository is not in a safe place", "; ".join(problems),
                    "hubctl migrate-content --to ~/.local/share/agent-hub/content --yes")]


def check_network_exposure(c: DoctorContext) -> list[dict[str, str]]:
    out: list[dict[str, str]] = []
    if c.cfg.trust_loopback:
        out.append(finding("trust-loopback", "crit", "HUB_TRUST_LOOPBACK is ON",
                           "Any local process, including an agent with a shell, is admin without a key.",
                           "Unset HUB_TRUST_LOOPBACK (or set it to false) and restart the hub."))
    else:
        out.append(finding("trust-loopback", "ok", "Every request needs a bearer key (loopback trust is off)"))
    host = c.cfg.host
    if host not in ("127.0.0.1", "::1", "localhost") and not host.startswith("127."):
        out.append(finding("bind", "crit", f"The hub is bound to {host}, not loopback",
                           "The API is reachable from other machines.", "Set HUB_HOST=127.0.0.1."))
    else:
        out.append(finding("bind", "ok", "The hub listens on loopback only"))
    return out


def claude_settings_gaps(doc: Any) -> list[str]:
    """The exact lines missing from a Claude Code settings document. Tolerates any malformed shape."""
    lines: list[str] = []
    sandbox = doc.get("sandbox") if isinstance(doc, dict) else None
    creds = sandbox.get("credentials") if isinstance(sandbox, dict) else None
    files = creds.get("files") if isinstance(creds, dict) else None
    have = {str(e.get("path")) for e in files if isinstance(e, dict)} if isinstance(files, list) else set()
    perms = doc.get("permissions") if isinstance(doc, dict) else None
    deny = perms.get("deny") if isinstance(perms, dict) else None
    denies = [e for e in deny if isinstance(e, str)] if isinstance(deny, list) else []
    for p in STATE_PATHS:
        if p not in have:
            lines.append(f'sandbox.credentials.files += {{"path": "{p}", "mode": "deny"}}')
        for tool in ("Read", "Write", "Edit"):
            if not any(d.startswith(f"{tool}(") and p in d for d in denies):
                lines.append(f'permissions.deny += "{tool}({p}/**)"')
    for tool in ("Write", "Edit"):
        if not any(d.startswith(f"{tool}(") and "hub/policy" in d for d in denies):
            lines.append(f'permissions.deny += "{tool}({POLICY_GLOB})"')
    return lines


def _load_json(text: str | None) -> Any:
    if text is None:
        return None
    try:
        return json.loads(text)
    except (ValueError, RecursionError):
        return "__malformed__"


def _primary_root(c: DoctorContext, cl: dict[str, Any]) -> Path | None:
    roots = cl.get("roots", {})
    r = roots.get("project") or next(iter(roots.values()), None)
    return _expand(r, c.home) if isinstance(r, str) else None


def _param_path(c: DoctorContext, cl: dict[str, Any], key: str, default_rel: str | None = None) -> Path | None:
    """A configured path param (absolute, ~, or relative to the client's primary root); the default only where one is documented."""
    v = cl.get("params", {}).get(key)
    root = _primary_root(c, cl)
    if isinstance(v, str) and v.strip():
        p = _expand(v.strip(), c.home)
        return p if p.is_absolute() else (root / p if root else None)
    return root / default_rel if (default_rel and root) else None


def _dotted(doc: Any, key: str) -> Any:
    cur = doc
    for part in key.split("."):
        if not isinstance(cur, dict) or part not in cur:
            return None
        cur = cur[part]
    return cur


def rerender_hint(cl: dict[str, Any]) -> str:
    h = cl.get("params", {}).get("rerender_hint")
    return str(h) if isinstance(h, str) and h.strip() else "then re-render the live config from the template with your own sync process"


def check_params_missing(c: DoctorContext) -> list[dict[str, str]]:
    miss = []
    for cl in c.clients:
        if cl.get("adapter") == "claude-code":
            absent = [k for k in CLAUDE_PARAMS if not (isinstance(cl.get("params", {}).get(k), str) and cl["params"][k].strip())]
            if absent:
                miss.append(f"{cl['id']}: {', '.join(absent)}")
    if not miss:
        return []
    return [finding("doctor-params", "info", "Some doctor checks are not configured",
                    "; ".join(miss), "Add these keys under `params:` in the client's yaml (see docs/SECURITY.md). Until then the "
                    "matching checks are skipped rather than guessed.")]


def check_claude_code(c: DoctorContext) -> list[dict[str, str]]:
    out: list[dict[str, str]] = []
    for cl in c.clients:
        if cl.get("adapter") != "claude-code":
            continue
        cid = cl["id"]
        tpl_path = _param_path(c, cl, "settings_template")
        doc: Any = None
        if tpl_path is None:
            out.append(finding("claude-template", "info", f"{cid}: settings_template is not configured",
                               "No template path is set, so its deny lists were not checked.",
                               "Set the client param settings_template to the file your sync process renders the live config from."))
        else:
            txt = c.fs.read_text(tpl_path)
            doc = _load_json(txt)
            if txt is None:
                out.append(finding("claude-template", "info", f"{cid}: the configured settings template is not readable", str(tpl_path)))
            elif doc == "__malformed__" or not isinstance(doc, dict):
                out.append(finding("claude-template", "warn", f"{cid}: the settings template is not valid JSON", str(tpl_path),
                                   "Fix the JSON syntax."))
                doc = None
            else:
                gaps = claude_settings_gaps(doc)
                if gaps:
                    out.append(finding("claude-template", "warn", f"{cid}: the settings template is missing hub deny entries",
                                       f"{len(gaps)} missing in {tpl_path}",
                                       f"Add to {tpl_path}:\n" + "\n".join(gaps[:14]) + "\n" + rerender_hint(cl)))
                else:
                    out.append(finding("claude-template", "ok", f"{cid}: template denies the hub/knowledge state paths and hub/policy"))
        net_key = cl.get("params", {}).get("network_allowlist_key")
        if not (isinstance(net_key, str) and net_key.strip()):
            out.append(finding("claude-network", "info", f"{cid}: network_allowlist_key is not configured",
                               "The sandbox's allowed domains were not checked.",
                               "Set the client param network_allowlist_key to the dotted key of the allowed-domains list in the template."))
        elif isinstance(doc, dict):
            domains = _dotted(doc, net_key.strip())
            if isinstance(domains, list):
                loop = sorted(str(d) for d in domains if str(d).lower() in LOOPBACK_DOMAINS)
                if loop:
                    out.append(finding("claude-network", "warn", f"{cid}: the sandbox may reach loopback ({', '.join(loop)})",
                                       "The hub API (and every other loopback service) is then reachable from sandboxed bash.",
                                       f"Remove the loopback entries from {net_key.strip()}."))
                elif not domains:
                    out.append(finding("claude-network", "info", f"{cid}: sandboxed bash has no network",
                                       "It cannot reach the hub API until a human approves a host."))
        live_path = _param_path(c, cl, "settings_rendered")
        if live_path is None:
            out.append(finding("claude-rendered", "info", f"{cid}: settings_rendered is not configured",
                               "The rendered live config was not compared with the template.",
                               "Set the client param settings_rendered to the live config file the client actually reads."))
        else:
            live = _load_json(c.fs.read_text(live_path))
            if isinstance(live, dict):
                lgaps = claude_settings_gaps(live)
                if lgaps:
                    out.append(finding("claude-rendered", "info", f"{cid}: the rendered live config lacks the current deny entries",
                                       f"{len(lgaps)} entries are not in {live_path} yet.", rerender_hint(cl).capitalize() + "."))
                else:
                    out.append(finding("claude-rendered", "ok", f"{cid}: the rendered live config has the deny entries"))
    return out


def check_opencode(c: DoctorContext) -> list[dict[str, str]]:
    if not any(cl.get("adapter") == "opencode" for cl in c.clients):
        return []
    return [finding("opencode-host", "info", "opencode runs as an unsandboxed process of your OS user",
                    "It can read everything the hub protects (key files, state, content, policy). The hub cannot constrain it.",
                    f"Run it as a separate OS user; see {DOCS_POINTER} (host-level, needs your approval).")]


def check_ptrace(c: DoctorContext) -> list[dict[str, str]]:
    raw = c.fs.read_text(Path("/proc/sys/kernel/yama/ptrace_scope"))
    v = (raw or "").strip()
    if not v.isdigit():
        return [finding("ptrace", "info", "ptrace_scope is unknown", "Yama is not exposed on this host.")]
    if int(v) == 0:
        return [finding("ptrace", "info", "kernel.yama.ptrace_scope = 0",
                        "A same-user process can attach to and read the memory of your other processes (including the hub).",
                        "sudo sysctl kernel.yama.ptrace_scope=1 (host-level; needs your approval)")]
    return [finding("ptrace", "info", f"kernel.yama.ptrace_scope = {v}", "Same-user ptrace is restricted to descendants.")]


def check_units(c: DoctorContext) -> list[dict[str, str]]:
    udir = c.home / ".config" / "systemd" / "user"
    units = [*c.fs.glob(udir, "agent-hub*.service"), *c.fs.glob(udir, "agent-knowledge*.service")]
    if not units:
        return []
    parent = c.app_root.parent
    out: list[dict[str, str]] = []
    for u in units:
        text = c.fs.read_text(u) or ""
        bad: list[str] = []
        for m in re.finditer(r"^\s*ReadWritePaths\s*=\s*(.+)$", text, re.M):
            for tok in m.group(1).split():
                tok = tok.lstrip("-+").replace("%h", str(c.home)).replace("~", str(c.home), 1)
                p = Path(tok)
                pol = c.app_root / "policy"
                if p in (parent, pol) or p in parent.parents or pol in p.parents:
                    bad.append(tok)
        if bad:
            out.append(finding(f"unit-{u.name}", "warn", f"{u.name}: ReadWritePaths exposes the hub's parent tree or hub/policy",
                               ", ".join(bad), "Restrict ReadWritePaths to the hub/knowledge state directories only."))
        else:
            out.append(finding(f"unit-{u.name}", "ok", f"{u.name}: ReadWritePaths does not include the hub's parent tree or hub/policy"))
    return out


def check_knowledge(c: DoctorContext) -> list[dict[str, str]]:
    k = c.knowledge
    if k is None or not k.get("reachable"):
        return [finding("knowledge", "info", "Knowledge service not reachable", "Its key and embedding-key checks were skipped.",
                        "Start it: cd hub/knowledge && uv run agent-knowledge")]
    if not k.get("authenticated"):
        return [finding("knowledge", "warn", "The hub cannot authenticate to the knowledge service",
                        f"No valid knowledge admin key (source: {k.get('key_source', 'none')}).",
                        "Set HUB_KNOWLEDGE_ADMIN_KEY or make ~/.local/share/agent-knowledge/admin.key readable (0600).")]
    return [finding("knowledge", "ok", "Knowledge service reachable and authenticated", f"key source: {k.get('key_source')}")]


def _norm(p: str, home: Path) -> str:
    """Canonical form for coverage comparison: ~ / $HOME / the real home dir are one spelling, trailing `/**`, `/*`, `/` dropped."""
    p = p.replace("${HOME}", "~").replace("$HOME", "~")
    if p == str(home) or p.startswith(str(home) + "/"):
        p = "~" + p[len(str(home)):]
    while p.endswith(("/**", "/*")):
        p = p[:-3] if p.endswith("/**") else p[:-2]
    return p.rstrip("/") or "/"


def _covers(pattern: str, target: str) -> bool:
    """Does a deny for `pattern` cover the floor entry `target` (both normalised)? Glob-aware containment."""
    return pattern == target or target.startswith(pattern + "/") or fnmatch.fnmatchcase(target, pattern)


def _claude_coverage(doc: Any, home: Path) -> dict[str, set[str]]:
    """{read, write, edit} -> normalised paths denied by one claude settings document (credentials.files deny both)."""
    cov: dict[str, set[str]] = {"read": set(), "write": set(), "edit": set()}
    if not isinstance(doc, dict):
        return cov
    perms = doc.get("permissions")
    deny = perms.get("deny") if isinstance(perms, dict) else None
    for e in deny if isinstance(deny, list) else []:
        m = re.fullmatch(r"(Read|Write|Edit)\((.*)\)", e) if isinstance(e, str) else None
        if m:
            cov[m.group(1).lower()].add(_norm(m.group(2), home))
    sandbox = doc.get("sandbox")
    creds = sandbox.get("credentials") if isinstance(sandbox, dict) else None
    files = creds.get("files") if isinstance(creds, dict) else None
    for e in files if isinstance(files, list) else []:
        if isinstance(e, dict) and e.get("mode") == "deny" and isinstance(e.get("path"), str):
            for k in cov:
                cov[k].add(_norm(e["path"], home))
    return cov


def claude_effective_gaps(c: DoctorContext, cl: dict[str, Any]) -> tuple[list[str], list[str]]:
    """(uncovered floor entries, sources examined) for a claude-code client: the UNION of the template, the rendered live config
    and the project settings files. Lines are in claude's own syntax."""
    docs: list[tuple[str, Any]] = []
    project = _param_path(c, cl, "settings_project", ".claude/settings.json")
    candidates = [_param_path(c, cl, "settings_template"), _param_path(c, cl, "settings_rendered"), project,
                  project.with_name("settings.local.json") if project else None]
    for p in candidates:
        if p is not None:
            txt = c.fs.read_text(p)
            if txt is not None:
                docs.append((str(p), _load_json(txt)))
    cov: dict[str, set[str]] = {"read": set(), "write": set(), "edit": set()}
    for _, d in docs:
        for k, v in _claude_coverage(d, c.home).items():
            cov[k] |= v
    lines: list[str] = []
    wants = [("read", g) for g in c.floor_read] + [(k, g) for g in c.floor_write for k in ("write", "edit")]
    for kind, glob in wants:
        target = _norm(glob, c.home)
        if not any(_covers(p, target) for p in cov[kind]):
            tool = kind.capitalize()
            line = f'permissions.deny += "{tool}({glob})"'
            if line not in lines:
                lines.append(line)
            stem = _norm(glob, c.home)
            if not stem.startswith("**") and glob.endswith("/**"):
                creds = f'sandbox.credentials.files += {{"path": "{stem}", "mode": "deny"}}'
                if creds not in lines:
                    lines.append(creds)
    return lines, [n for n, _ in docs]


def check_advisory_gap(c: DoctorContext) -> list[dict[str, str]]:
    out: list[dict[str, str]] = []
    for cl in c.clients:
        if cl.get("manage", {}).get("permissions"):
            continue
        cid, adapter = cl["id"], str(cl.get("adapter", ""))
        fid = f"advisory-gap-{cid}"
        if adapter == "claude-code":
            lines, sources = claude_effective_gaps(c, cl)
            if not sources:
                out.append(finding(fid, "info", f"{cid}: no settings files found to check the floor's deny paths against",
                                   "Neither a configured template, a rendered config nor the project settings could be read.",
                                   "Set the client params settings_template / settings_rendered / settings_project."))
            elif lines:
                out.append(finding(fid, "warn", f"{cid}: {len(lines)} floor deny entries are not covered by its effective config",
                                   "Checked the union of: " + ", ".join(sources),
                                   "Add to the template (or the settings file you maintain):\n" + "\n".join(lines[:16])
                                   + ("\n..." if len(lines) > 16 else "") + "\n" + rerender_hint(cl)))
            else:
                out.append(finding(fid, "ok", f"{cid}: every floor deny path is covered by its effective config",
                                   "Checked the union of: " + ", ".join(sources)))
        elif adapter == "opencode":
            gaps = c.permission_gaps(cid)
            if gaps:
                out.append(finding(fid, "info", f"{cid}: {len(gaps)} floor paths have no bash-glob deny (cannot be made complete)",
                                   "opencode is unsandboxed and its bash-glob permissions only approximate path denies.",
                                   "Add what you can to opencode.json, and rely on running it as a separate OS user "
                                   f"({DOCS_POINTER})."))
        elif adapter.startswith("hermes"):
            out.append(finding(fid, "ok", f"{cid}: runs in a container with no host mounts of the hub's state",
                               "Verify the compose volumes do not mount ~/.local/share/agent-hub or the hub's parent tree read-write."))
        else:
            gaps = c.permission_gaps(cid)
            if gaps:
                out.append(finding(fid, "warn", f"{cid}: permissions are advisory and the floor's deny paths are missing from its config",
                                   f"{len(gaps)} missing", "Add to the client's config:\n" + "\n".join(gaps[:8])
                                   + ("\n..." if len(gaps) > 8 else "")))
    return out


CHECKS: tuple[Callable[[DoctorContext], list[dict[str, str]]], ...] = (
    check_bootstrap_key, check_modes, check_content_dir, check_network_exposure, check_claude_code, check_params_missing, check_opencode, check_ptrace,
    check_units, check_knowledge, check_advisory_gap)


def run_doctor(c: DoctorContext) -> dict[str, Any]:
    findings: list[dict[str, str]] = []
    for chk in CHECKS:
        try:
            findings += chk(c)
        except Exception as exc:  # noqa: BLE001 - a broken check must not hide the others
            findings.append(finding(f"check-{chk.__name__}", "info", f"{chk.__name__} could not run", type(exc).__name__))
    counts = {s: sum(1 for f in findings if f["severity"] == s) for s in ("crit", "warn", "info", "ok")}
    status = "action_needed" if counts["crit"] else "attention" if counts["warn"] else "ok"
    return {"status": status, "counts": counts, "findings": findings}
