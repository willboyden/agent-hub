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
  observe_services                   extra compose services whose running container observe() may read (list, default []); the
                                     service named by compose_service is tried first. Each is fullmatch-validated
  observe_python                     interpreter run inside the container by observe() (default ``python3``; fullmatch
                                     ``[A-Za-z0-9_./-]{1,200}``)
  observe_root                       absolute skills root inside the container (default ``/opt/data/skills``; fullmatch, no ``..``)
  disabled_toolsets                  the complete toolset denylist you want; a list that drops a baseline entry is an error

Policy floor: ``BASELINE_DISABLED`` below is data in this adapter. The hub always renders the union of it and your list, so a fragment
can never weaken it, and emits an ERROR diagnostic when params.disabled_toolsets drops a member or a permission rule allows a
capability whose toolset is in the floor.
"""
from __future__ import annotations

import binascii
import json
import os
import posixpath
import re
import unicodedata
from typing import Any

from agent_hub.domain import yamlsafe
from agent_hub.domain.pathsafe import PathError, check_relpath

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
    Observation,
    ObservedSkill,
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


    # ------------------------------------------------------------------------------------------------ observe (Addendum F2)
    def observe(self, cfg: ClientConfig, fs: FileSystem, run: CommandRunner) -> Observation:
        """Read the skills the RUNNING container really has (read-only; never starts a container).

        SECURITY: the interpreter that runs inside the container, and the filesystem it reads, are controlled by the (untrusted)
        agent. The constant program is therefore NOT a defence by itself; the parse-side caps and checks in _parse_observation
        are. Every byte that comes back is untrusted: size-capped, JSON object only, strict base64, check_relpath on every path."""
        p = cfg.params
        project = str(p.get("compose_project", PLACEHOLDER))
        service = str(p.get("compose_service", "hermes"))
        extra = p.get("observe_services", [])
        python = str(p.get("observe_python", "python3"))
        root = str(p.get("observe_root", "/opt/data/skills"))
        ctx_name = str(p.get("docker_context", ""))
        user = str(p.get("exec_user", ""))
        if not project or project == PLACEHOLDER:
            return _fail("compose_project is not configured: nothing observed")
        services = [service] + [x for x in (extra if isinstance(extra, list) else []) if isinstance(x, str)]
        if not all(_SERVICE_RE.fullmatch(x) for x in services) or not _SERVICE_RE.fullmatch(project):
            return _fail("compose project/service name is not a plain name: nothing observed")
        if not _PY_RE.fullmatch(python):
            return _fail("observe_python is not a plain path: nothing observed")
        if not _ROOT_RE.fullmatch(root) or ".." in root.split("/"):
            return _fail("observe_root is not a plain absolute path: nothing observed")
        if (ctx_name and not _CTX_RE.fullmatch(ctx_name)) or (user and not _USER_RE.fullmatch(user)):
            return _fail("docker_context/exec_user is not a plain name: nothing observed")
        dk = ["docker", *(["--context", ctx_name] if ctx_name else [])]
        ctr = ""
        for svc in services:
            try:
                rc, out, _err = run.run([*dk, "ps", "--format", "{{.Names}}",
                                         "--filter", f"label=com.docker.compose.project={project}",
                                         "--filter", f"label=com.docker.compose.service={svc}"], timeout=15)
            except OSError:
                return _fail("docker is not available: nothing observed")
            names = [ln.strip() for ln in out.splitlines() if _CTR_RE.fullmatch(ln.strip())]
            if rc == 0 and names:
                ctr = names[0]
                break
        if not ctr:
            return _fail("no running container for the configured compose project/service: nothing observed (none is started)")
        try:
            rc, out, err = run.run([*dk, "exec", *(["--user", user] if user else []), ctr, python, "-c", _OBSERVE_PROGRAM, root],
                                   timeout=120)
        except OSError:
            return _fail("docker exec failed to start: nothing observed")
        if rc != 0:
            return _fail(f"in-container read exited {rc}: {_scrub(err)}")
        return _parse_observation(out, ctr, root, str(p.get("skills_dest", "")))


_SERVICE_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}")
_CTR_RE = _SERVICE_RE
_CTX_RE = _SERVICE_RE
_USER_RE = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_.:-]{0,63}")
_PY_RE = re.compile(r"[A-Za-z0-9_./-]{1,200}")
_ROOT_RE = re.compile(r"/[A-Za-z0-9_./-]{0,200}")
MAX_OBSERVE_OUT = 48 * 1024 * 1024
MAX_FILE = 1024 * 1024
MAX_TOTAL = 32 * 1024 * 1024
MAX_FILES = 5000
MAX_SKILL_NAME = 200
MAX_SKILL_DESC = 2000

# A CONSTANT: nothing (params, content) is ever interpolated into it. The root arrives as sys.argv[1].
_OBSERVE_PROGRAM = r"""
import base64, json, os, stat, sys
root = sys.argv[1]
MAXF, MAXT, MAXN = 1048576, 33554432, 5000
files, total, trunc, manifest = {}, 0, False, []
mp = os.path.join(root, ".bundled_manifest")
try:
    st = os.lstat(mp)
    if stat.S_ISREG(st.st_mode) and st.st_size <= MAXF:
        with open(mp, "rb") as fh:
            manifest = [l.strip() for l in fh.read().decode("utf-8", "replace").splitlines() if l.strip()][:20000]
except OSError:
    pass
def walk(d, rel):
    global total, trunc
    try:
        names = sorted(os.listdir(d))
    except OSError:
        return
    for n in names:
        if trunc:
            return
        if n == ".curator_backups" or (n.startswith(".") and not (n == ".archive" and rel == "")):
            continue
        p = os.path.join(d, n)
        r = n if not rel else rel + "/" + n
        try:
            st = os.lstat(p)
        except OSError:
            continue
        if stat.S_ISDIR(st.st_mode):
            walk(p, r)
        elif stat.S_ISREG(st.st_mode):
            if st.st_size > MAXF or total + st.st_size > MAXT or len(files) >= MAXN:
                trunc = True
                continue
            try:
                fd = os.open(p, os.O_RDONLY | os.O_NOFOLLOW)
                with os.fdopen(fd, "rb") as fh:
                    data = fh.read(MAXF + 1)
            except OSError:
                continue
            if len(data) > MAXF:
                trunc = True
                continue
            total += len(data)
            files[r] = base64.b64encode(data).decode("ascii")
if not os.path.isdir(root):
    sys.stderr.write("root is not a directory")
    sys.exit(3)
walk(root, "")
sys.stdout.write(json.dumps({"manifest": manifest, "files": files, "truncated": trunc}))
"""


def _fail(note: str) -> Observation:
    return Observation(complete=False, notes=[note])


def _scrub(text: str) -> str:
    """Untrusted stderr -> a short single-line note without control characters."""
    return "".join(c if c.isprintable() else " " for c in text.strip())[:160] or "no detail"


def _parse_observation(out: str, ctr: str, root: str, skills_dest: str) -> Observation:
    source = f"container {ctr}: {root}"
    notes: list[str] = []
    if len(out) > MAX_OBSERVE_OUT:
        return Observation(source=source, complete=False, notes=["observation output exceeded the size cap: nothing read"])
    try:
        doc = json.loads(out)
    except (ValueError, RecursionError):
        return Observation(source=source, complete=False, notes=["observation output was not valid JSON: nothing read"])
    if not isinstance(doc, dict) or not isinstance(doc.get("files"), dict):
        return Observation(source=source, complete=False, notes=["observation output has the wrong shape: nothing read"])
    complete = doc.get("truncated") is not True
    if not complete:
        notes.append("the in-container read hit a size or count cap: the inventory is partial")
    manifest: set[str] = set()
    raw_manifest = doc.get("manifest")
    for line in (raw_manifest if isinstance(raw_manifest, list) else [])[:20000]:
        if isinstance(line, str) and line.split(":", 1)[0].strip():
            manifest.add(line.split(":", 1)[0].strip())
    files: dict[str, bytes] = {}
    total = 0
    bad = 0
    for rel, b64 in doc["files"].items():
        if len(files) >= MAX_FILES:
            complete = False
            notes.append("more files than the cap: the rest were ignored")
            break
        try:
            check_relpath(rel)
            rel.encode("utf-8")                         # lone surrogates raise UnicodeEncodeError
            if any(unicodedata.category(c) in ("Cc", "Cf") for c in rel):
                raise ValueError("control or format character in path")
            if not isinstance(b64, str):
                raise ValueError("not text")
            data = binascii.a2b_base64(b64.encode("ascii"), strict_mode=True)
        except (PathError, ValueError, binascii.Error, UnicodeEncodeError):
            bad += 1
            continue
        if len(data) > MAX_FILE or total + len(data) > MAX_TOTAL:
            bad += 1
            continue
        total += len(data)
        files[rel] = data
    if bad:
        complete = False
        notes.append(f"{bad} file(s) were rejected (bad path, bad encoding or over a cap)")
    # a path that is a file in one entry and a directory in another cannot be materialised: drop both sides
    prefixes = {"/".join(r.split("/")[:i]) for r in files for i in range(1, len(r.split("/")))}
    clash = [r for r in files if r in prefixes]
    if clash:
        complete = False
        notes.append(f"{len(clash)} file(s) collide with a directory of the same path and were skipped")
        for r in clash:
            del files[r]
    skill_dirs = sorted((rel[: -len("/SKILL.md")] if "/" in rel else "") for rel in files if rel == "SKILL.md" or rel.endswith("/SKILL.md"))
    owned: dict[str, dict[str, bytes]] = {d: {} for d in skill_dirs if d}
    if "" in skill_dirs:
        notes.append("a SKILL.md at the skills root itself was ignored")
    for rel, data in files.items():
        parts = rel.split("/")
        for i in range(len(parts) - 1, 0, -1):          # deepest ancestor first
            d = "/".join(parts[:i])
            if d in owned:
                owned[d]["/".join(parts[i:])] = data
                break
    hub_cat = skills_dest.rstrip("/").rsplit("/", 1)[-1] if skills_dest.strip("/") else ""
    skills: list[ObservedSkill] = []
    for d in sorted(owned):
        sf = owned[d]
        leaf = d.rsplit("/", 1)[-1]
        name, desc = leaf, ""
        try:
            fm, _ = yamlsafe.split_frontmatter(sf["SKILL.md"].decode("utf-8", errors="replace"))
        except (yamlsafe.YamlError, ValueError, RecursionError):
            fm = {}
        if isinstance(fm.get("name"), str) and fm["name"].strip():
            name = fm["name"].strip()[:MAX_SKILL_NAME]
        desc = u.cap_str(fm.get("description"), MAX_SKILL_DESC)
        parts = d.split("/")
        if ".archive" in parts:
            origin = "archived"
        elif parts[0] == "lab":
            origin = "lab_sync"
        elif hub_cat and parts[0] == hub_cat:
            origin = "hub"
        elif leaf in manifest:
            origin = "shipped"
        else:
            origin = "agent"
        skills.append(ObservedSkill(path=d, name=name, description=desc, origin=origin, files=sf))
    return Observation(skills=skills, source=source, notes=notes, complete=complete)


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
