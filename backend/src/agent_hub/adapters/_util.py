"""Shared helpers for the client adapters. Pure functions only (no file or process I/O except through the
injected FileSystem port), so every adapter stays testable offline.

Design rules that every adapter relies on:
  * names/ids are validated with ``fullmatch`` (never ``match``) before they become part of a path;
  * every relative path an adapter emits goes through :func:`safe_relpath` (no ``..``, absolute, backslash, NUL);
  * output is deterministic: callers sort by name/id, JSON is dumped with sorted keys, YAML with a fixed key order;
  * nothing here ever reads or prints secret values.
"""
from __future__ import annotations

import hashlib
import json
import posixpath
import re
from collections.abc import Iterable, Mapping
from typing import Any

import yaml

from agent_hub.domain import yamlsafe

from .base import CAPABILITIES, Artifact, Check, ClientConfig, Diag, FileSystem, Severity, SkillItem

NAME_RE = re.compile(r"[a-z0-9][a-z0-9._-]{0,63}")
MAX_DESC_WARN = 1024          # skill/agent descriptions longer than this are legal but usually a mistake
MAX_DESC_ERR = 8192           # ... and above this the item is refused (hostile or broken input)
MAX_FILE_BYTES = 2 * 1024 * 1024        # rendering cap for one emitted file
MAX_READ_FILE_BYTES = 512 * 1024        # discover: per-file cap for text read back from a client
MAX_READ_TOTAL_BYTES = 16 * 1024 * 1024
MAX_BODY_CHARS = 512 * 1024
MAX_DISCOVER_FILES = 400      # per skill, when reading a client tree back

MD_BEGIN = "<!-- agent-hub:begin (managed block; edits inside are overwritten) -->"
MD_END = "<!-- agent-hub:end -->"
HASH_BEGIN = "# agent-hub:begin (managed block; edits inside are overwritten)"
HASH_END = "# agent-hub:end"


class UnsafePath(ValueError):
    """A name or path that could escape its root."""


# ---- names and paths --------------------------------------------------------------------------------------------------
def valid_name(name: str) -> bool:
    return NAME_RE.fullmatch(name) is not None


def safe_relpath(*parts: str) -> str:
    """Join relative path parts with ``/``; refuse anything that could leave the root."""
    out: list[str] = []
    for part in parts:
        if not isinstance(part, str) or part == "":
            raise UnsafePath("empty path part")
        if "\x00" in part or "\\" in part or part.startswith("/") or re.match(r"^[A-Za-z]:", part):
            raise UnsafePath(f"unsafe path part {part!r}")
        for seg in part.split("/"):
            if seg in ("", ".", ".."):
                raise UnsafePath(f"unsafe path segment in {part!r}")
            if any(ord(c) < 32 for c in seg):
                raise UnsafePath("control character in path")
            out.append(seg)
    return "/".join(out)


def fs_path(cfg: ClientConfig, root: str, rel: str) -> str | None:
    """Absolute path for reading through the FileSystem port (None when the root is not configured)."""
    base = cfg.roots.get(root)
    if not base:
        return None
    return posixpath.join(base, safe_relpath(rel))


# ---- diagnostics --------------------------------------------------------------------------------------------------------
def unsupported_severity(cfg: ClientConfig) -> Severity:
    """Strict clients turn every lossy/unsupported mapping into an error; non-strict ones into a warning."""
    return "error" if cfg.strict else "warn"


def diag(sev: Severity, code: str, message: str, item: str | None = None) -> Diag:
    return Diag(severity=sev, code=code, message=message, item=item)


def oneline(text: str, item: str, out: list[Diag]) -> str:
    """Descriptions are single-line by convention in every client. Collapsing whitespace also removes the multi-line YAML scalar
    forms (whose indented ``---`` lines confuse naive frontmatter splitters)."""
    flat = " ".join(text.split())
    if flat != text:
        out.append(diag("info", "description_normalized", "description whitespace/newlines collapsed to one line", item))
    return flat


def check_description(desc: str, item: str, out: list[Diag]) -> bool:
    """False (and an error Diag) when the description is too large to accept."""
    if len(desc) > MAX_DESC_ERR:
        out.append(diag("error", "description_too_long", f"description is {len(desc)} chars (limit {MAX_DESC_ERR})", item))
        return False
    if len(desc) > MAX_DESC_WARN:
        out.append(diag("warn", "description_long", f"description is {len(desc)} chars; clients may truncate above {MAX_DESC_WARN}", item))
    return True


def dedupe(names: Iterable[str], kind: str, out: list[Diag]) -> set[str]:
    """Names that occur more than once (each reported once as an error)."""
    seen: set[str] = set()
    dup: set[str] = set()
    for n in names:
        if n in seen and n not in dup:
            dup.add(n)
            out.append(diag("error", "duplicate_name", f"duplicate {kind} name {n!r}", n))
        seen.add(n)
    return dup


# ---- hashing / bytes ----------------------------------------------------------------------------------------------------
def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def to_bytes(text: str) -> bytes:
    return text.encode("utf-8")


def cap_str(value: Any, limit: int = MAX_DESC_ERR) -> str:
    """str() of a parsed value with a length cap (never stringify an unbounded parsed structure)."""
    if value is None:
        return ""
    if isinstance(value, (list, dict)):
        return ""
    return str(value)[:limit]


# ---- frontmatter --------------------------------------------------------------------------------------------------------
def render_frontmatter(fields: Mapping[str, Any], body: str) -> str:
    """``---`` YAML ``---`` body. Key order is the caller's (fixed) order; scalars are quoted by the safe dumper, so a
    description containing ``---`` or newlines cannot break out of the block."""
    head = yaml.safe_dump(dict(fields), sort_keys=False, allow_unicode=True, default_flow_style=False, width=100000)
    return f"---\n{head}---\n\n{body.rstrip()}\n"


def parse_frontmatter(text: str) -> tuple[dict[str, Any], str]:
    """Tolerant inverse of :func:`render_frontmatter`. No frontmatter or a broken one gives ``({}, text)``."""
    if not text.startswith("---"):
        return {}, text
    lines = text.split("\n")
    if lines[0].strip() != "---":
        return {}, text
    for i in range(1, len(lines)):
        if lines[i].rstrip() == "---":
            try:
                data = yamlsafe.load("\n".join(lines[1:i]))     # hardened: no aliases/anchors, size/depth/node caps
            except yamlsafe.YamlError:
                return {}, text
            if data is None:
                data = {}
            if not isinstance(data, dict):
                return {}, text
            return {cap_str(k, 200): v for k, v in data.items()}, "\n".join(lines[i + 1:]).lstrip("\n")
    return {}, text


# ---- hub blocks ---------------------------------------------------------------------------------------------------------
class MarkerInjection(ValueError):
    """A body line looks like a hub block marker; embedding it would end (or forge) the managed region early."""


_MARKER_LIKE = re.compile(r"(<!--\s*agent-hub:)|(^\s*#\s*agent-hub:)", re.IGNORECASE)


def looks_like_marker(line: str) -> bool:
    """Deliberately broader than the merge engine's exact rule: case-insensitive, whitespace-tolerant, ``<!-- agent-hub:``
    anywhere on the line."""
    return _MARKER_LIKE.search(line) is not None


def wrap_block(body: str, style: str = "md") -> str:
    """Body between hub markers (the markers are part of the artifact content; the applier replaces only that region).
    Raises MarkerInjection when any body line looks like a hub marker: callers turn that into an error Diag, no artifact."""
    for ln in body.splitlines():
        if looks_like_marker(ln):
            raise MarkerInjection("content contains a line that looks like an agent-hub block marker")
    begin, end = (MD_BEGIN, MD_END) if style == "md" else (HASH_BEGIN, HASH_END)
    return f"{begin}\n{body.strip()}\n{end}\n"


# Same rule as services/merge.py find_region (tests/adapters/test_markers.py feeds both the same fixtures so they cannot drift).
_BEGIN_PREFIX = {"md": "<!-- agent-hub:begin", "hash": "# agent-hub:begin"}
_END_LINE = {"md": MD_END, "hash": HASH_END}


def find_region(text: str, style: str) -> tuple[int, int] | None | str:
    begin, end = _BEGIN_PREFIX[style], _END_LINE[style]
    pos = 0
    start: int | None = None
    for line in text.splitlines(keepends=True):
        if start is None and line.startswith(begin):
            start = pos
        elif start is not None and line.rstrip() == end:
            return start, pos + len(line)
        pos += len(line)
    return "broken" if start is not None else None


def extract_block(text: str, style: str = "md") -> str | None:
    span = find_region(text, style)
    if not isinstance(span, tuple):
        return None
    region = text[span[0]:span[1]]
    return region if region.endswith("\n") else region + "\n"


def replace_block(existing: str, block: str, style: str = "md") -> str:
    """What the applier does for merge='block' (used by tests to prove non-managed text survives)."""
    old = extract_block(existing, style)
    if old is None:
        return existing.rstrip("\n") + ("\n\n" if existing.strip() else "") + block
    return existing.replace(old.rstrip("\n"), block.rstrip("\n"), 1)


def render_instructions(items: list[Any], headings: bool = True) -> str:
    """Instructions sorted by (order, id); deterministic."""
    parts = []
    for it in sorted(items, key=lambda i: (i.order, i.id)):
        parts.append((f"## {it.title}\n\n" if headings else "") + it.body.strip())
    return "\n\n".join(parts)


# ---- json / yaml key helpers --------------------------------------------------------------------------------------------
def dump_json(doc: Mapping[str, Any]) -> bytes:
    return (json.dumps(doc, indent=2, sort_keys=True, ensure_ascii=False) + "\n").encode("utf-8")


def dump_yaml(doc: Mapping[str, Any]) -> bytes:
    return yaml.safe_dump(dict(doc), sort_keys=True, allow_unicode=True, default_flow_style=False, width=100000).encode("utf-8")


def set_dotted(doc: dict[str, Any], path: str, value: Any) -> None:
    cur = doc
    parts = path.split(".")
    for p in parts[:-1]:
        nxt = cur.get(p)
        if not isinstance(nxt, dict):
            nxt = {}
            cur[p] = nxt
        cur = nxt
    cur[parts[-1]] = value


def get_dotted(doc: Mapping[str, Any], path: str) -> Any:
    cur: Any = doc
    for p in path.split("."):
        if not isinstance(cur, Mapping) or p not in cur:
            return None
        cur = cur[p]
    return cur


def del_dotted(doc: dict[str, Any], path: str) -> None:
    cur: Any = doc
    parts = path.split(".")
    for p in parts[:-1]:
        cur = cur.get(p) if isinstance(cur, dict) else None
        if not isinstance(cur, dict):
            return
    cur.pop(parts[-1], None)


def merge_keys(existing: Mapping[str, Any], managed_doc: Mapping[str, Any], managed_keys: list[str]) -> dict[str, Any]:
    """What the applier does for json_keys/yaml_keys: replace ONLY managed keys, keep everything else (tests use it to
    prove other settings survive)."""
    out: dict[str, Any] = json.loads(json.dumps(existing))
    for key in managed_keys:
        val = get_dotted(managed_doc, key)
        if val is None:
            del_dotted(out, key)
        else:
            set_dotted(out, key, val)
    return out


# ---- capability mapping -------------------------------------------------------------------------------------------------
def ordered_caps(caps: Iterable[str]) -> list[str]:
    """Canonical capabilities in canonical order (deterministic, unknown names dropped by the caller first)."""
    have = set(caps)
    return [c for c in CAPABILITIES if c in have]


def unknown_caps(caps: Iterable[str]) -> list[str]:
    return sorted({c for c in caps if c not in CAPABILITIES})


READ_ONLY_DROPS = ("write", "edit", "notebook")


def effective_caps(caps: list[str], read_only: bool) -> list[str]:
    have = [c for c in ordered_caps(caps)]
    if read_only:
        have = [c for c in have if c not in READ_ONLY_DROPS]
    return have


# ---- verify -----------------------------------------------------------------------------------------------------------
def _canon_digest(obj: Any) -> str:
    """Same digest as services/merge.py ``_canon`` (kept in step by tests/adapters/test_markers.py)."""
    return sha256_hex(json.dumps(obj, sort_keys=True, separators=(",", ":"), default=str).encode())


def slice_digest(raw: bytes, merge: str, managed_keys: list[str]) -> str | None:
    """Digest of the hub-owned slice of a live file, identical to the merge engine's ``slice_live``:
    own -> whole file; block -> the marker region; json_keys/yaml_keys -> canonical JSON of the managed keys present."""
    if merge == "own":
        return sha256_hex(raw)
    text = raw.decode("utf-8", errors="replace")
    if merge == "block":
        blk = extract_block(text, "md") or extract_block(text, "hash")
        return sha256_hex(blk.encode("utf-8")) if blk is not None else None
    try:
        doc = json.loads(text) if merge == "json_keys" else yamlsafe.load(text, max_bytes=8 * 1024 * 1024)
    except (ValueError, yamlsafe.YamlError):
        return None
    if not isinstance(doc, dict):
        return None
    found: dict[str, Any] = {}
    for k in sorted(managed_keys):
        v = get_dotted(doc, k)
        if v is not None:
            found[k] = v
    return _canon_digest(found) if found else None


def verify_expected(cfg: ClientConfig, expected: list[Any], fs: FileSystem) -> list[Check]:
    """Files present with the delivered content. Whole-file hash first (right after an apply the file equals what was
    written); otherwise, for json_keys/yaml_keys/block artifacts, compare ONLY the hub-owned slice so unmanaged edits
    elsewhere in the file do not fail verification. A mismatch is never reported ok."""
    checks: list[Check] = []
    for e in sorted(expected, key=lambda x: (x.root, x.path)):
        name = f"{e.root}:{e.path}"
        try:
            p = fs_path(cfg, e.root, e.path)
        except UnsafePath as exc:
            checks.append(Check(name=name, ok=False, detail=f"unsafe path: {exc}"))
            continue
        if p is None:
            checks.append(Check(name=name, ok=False, detail=f"root {e.root!r} not configured"))
            continue
        if not fs.exists(p):
            checks.append(Check(name=name, ok=False, detail="file missing"))
            continue
        try:
            raw = fs.read_bytes(p)
        except OSError as exc:
            checks.append(Check(name=name, ok=False, detail=f"unreadable: {exc.__class__.__name__}"))
            continue
        if sha256_hex(raw) == e.sha256:
            checks.append(Check(name=name, ok=True, detail="hash matches"))
            continue
        merge = getattr(e, "merge", "own")
        if merge != "own" and slice_digest(raw, merge, list(getattr(e, "managed_keys", []))) == e.sha256:
            checks.append(Check(name=name, ok=True, detail=f"managed {merge} slice matches"))
        elif merge != "own":
            checks.append(Check(name=name, ok=False, detail=f"managed {merge} slice differs from what the hub delivered"))
        else:
            checks.append(Check(name=name, ok=False, detail="hash differs from the delivered content"))
    return checks


# ---- discover helpers ---------------------------------------------------------------------------------------------------
def size_of(fs: FileSystem, path: str) -> int | None:
    """File size BEFORE reading, when the port offers it (optional ``size(path)``; RealFS should add one). None = unknown."""
    fn = getattr(fs, "size", None)
    if callable(fn):
        try:
            n = fn(path)
        except OSError:
            return None
        return int(n) if isinstance(n, int) else None
    return None


def read_capped(fs: FileSystem, path: str, notes: list[str], limit: int = MAX_READ_FILE_BYTES) -> bytes | None:
    """Read one file, skipping (with a note) anything over ``limit``. Stats first when the port can; otherwise the read itself
    is the only option and the result is dropped if oversize."""
    n = size_of(fs, path)
    if n is not None and n > limit:
        notes.append(f"skipped {posixpath.basename(path)}: {n} bytes exceeds the {limit} byte read cap")
        return None
    try:
        data = fs.read_bytes(path)
    except OSError:
        return None
    if len(data) > limit:
        notes.append(f"skipped {posixpath.basename(path)}: {len(data)} bytes exceeds the {limit} byte read cap")
        return None
    return data


def read_text(fs: FileSystem, path: str, notes: list[str] | None = None) -> str:
    """Capped text read ('' when unreadable or over the cap)."""
    data = read_capped(fs, path, notes if notes is not None else [])
    return "" if data is None else data.decode("utf-8", errors="replace")


def read_tree(fs: FileSystem, base: str, limit: int = MAX_DISCOVER_FILES, notes: list[str] | None = None) -> dict[str, bytes]:
    """Read a directory tree (relpath -> bytes) through the port: at most ``limit`` files, 512 KB per file, 16 MB in total;
    over-limit files are skipped with a note."""
    out: dict[str, bytes] = {}
    sink: list[str] = notes if notes is not None else []
    total = 0

    def walk(rel: str) -> None:
        nonlocal total
        cur = base if not rel else posixpath.join(base, rel)
        for name in sorted(fs.listdir(cur)):
            if len(out) >= limit:
                if not any("file limit" in n for n in sink):
                    sink.append(f"stopped reading {base}: {limit} file limit reached")
                return
            r = f"{rel}/{name}" if rel else name
            p = posixpath.join(base, r)
            if fs.is_dir(p):
                if name in ("__pycache__", ".git", "node_modules"):
                    continue
                walk(r)
                continue
            data = read_capped(fs, p, sink)
            if data is None:
                continue
            if total + len(data) > MAX_READ_TOTAL_BYTES:
                sink.append(f"skipped {r}: total read cap of {MAX_READ_TOTAL_BYTES} bytes reached")
                continue
            total += len(data)
            out[r] = data

    if fs.exists(base) and fs.is_dir(base):
        walk("")
    return out


# ---- shared renderers -----------------------------------------------------------------------------------------------
def skill_artifacts(skills: list[Any], root: str, base: str, out: list[Diag], skill_filename: str = "SKILL.md") -> list[Artifact]:
    """Skills as ``<base>/<name>/<relpath>`` files (the shared SKILL.md standard is passed through untouched; only the
    frontmatter name is cross-checked so a mislabelled skill is reported, not silently renamed)."""
    arts: list[Artifact] = []
    dup = dedupe([s.name for s in skills], "skill", out)
    for sk in sorted(skills, key=lambda x: x.name):
        if sk.name in dup:
            continue
        if not valid_name(sk.name):
            out.append(diag("error", "invalid_name", f"skill name {sk.name!r} is not a valid identifier", sk.name))
            continue
        if not check_description(sk.description, sk.name, out):
            continue
        if "SKILL.md" not in sk.files:
            out.append(diag("error", "missing_skill_md", "skill has no SKILL.md", sk.name))
            continue
        fm, _ = parse_frontmatter(sk.files["SKILL.md"].decode("utf-8", errors="replace"))
        if fm.get("name") != sk.name:
            out.append(diag("warn", "skill_name_mismatch",
                            f"SKILL.md frontmatter name {fm.get('name')!r} differs from the skill name", sk.name))
        bad = False
        staged: list[Artifact] = []
        for rel in sorted(sk.files):
            data = sk.files[rel]
            try:
                path = safe_relpath(base, sk.name, skill_filename if rel == "SKILL.md" else rel)
            except UnsafePath as exc:
                out.append(diag("error", "unsafe_path", f"skill file {rel!r}: {exc}", sk.name))
                bad = True
                break
            if len(data) > MAX_FILE_BYTES:
                out.append(diag("error", "file_too_large", f"skill file {rel!r} is {len(data)} bytes", sk.name))
                bad = True
                break
            staged.append(Artifact(root=root, path=path, content=data, kind="skill", source_ids=[sk.name]))
        if not bad:
            arts.extend(staged)
    return arts


def discover_skills(fs: FileSystem, cfg: ClientConfig, root: str, base: str, notes: list[str]) -> list[SkillItem]:
    """Skills found at ``<root>/<base>/<name>/SKILL.md`` (dir-per-skill layout)."""
    top = fs_path(cfg, root, base)
    out: list[SkillItem] = []
    if top is None or not fs.exists(top) or not fs.is_dir(top):
        return out
    for name in sorted(fs.listdir(top)):
        d = posixpath.join(top, name)
        if not fs.is_dir(d):
            continue
        if not valid_name(name):
            notes.append(f"skipped skill dir {name!r}: not a valid hub name")
            continue
        files = read_tree(fs, d, notes=notes)
        if "SKILL.md" not in files:
            notes.append(f"skipped skill dir {name!r}: no SKILL.md")
            continue
        fm, _ = parse_frontmatter(files["SKILL.md"].decode("utf-8", errors="replace"))
        out.append(SkillItem(name=name, description=cap_str(fm.get("description")), files=files))
    return out


def instruction_artifact(items: list[Any], root: str, path: str, params: Mapping[str, Any], diags: list[Diag]) -> Artifact | None:
    """CLAUDE.md / AGENTS.md. ``instructions_mode`` = ``block`` (default; hub markers, hand-written text survives) or ``own``
    (whole file hub-owned; one instruction is written VERBATIM so an unchanged import is byte-identical)."""
    ordered = sorted(items, key=lambda i: (i.order, i.id))
    ids = [i.id for i in ordered]
    mode = params.get("instructions_mode", "block")
    if mode not in ("own", "block"):
        diags.append(diag("error", "invalid_param", f"instructions_mode must be own or block, got {mode!r}"))
        mode = "block"
    if mode == "own":
        if len(ordered) == 1:
            text = ordered[0].body
        else:
            text = "\n\n".join(f"## {i.title}\n\n{i.body.strip()}" for i in ordered) + "\n"
        return Artifact(root=root, path=path, content=to_bytes(text), kind="instruction", source_ids=ids, merge="own")
    try:
        block = wrap_block(render_instructions(items))
    except MarkerInjection as exc:
        diags.append(diag("error", "marker_injection", f"{path}: {exc}; instructions not rendered"))
        return None
    return Artifact(root=root, path=path, content=to_bytes(block), kind="instruction", source_ids=ids, merge="block")


def sandbox_prefix(params: Mapping[str, Any]) -> list[str]:
    """argv prefix (params.sandbox_wrapper) for MCP servers that declare a sandbox_profile."""
    w = params.get("sandbox_wrapper")
    return [str(x) for x in w] if isinstance(w, list) else []


def sandbox_missing(name: str, out: list[Diag]) -> None:
    out.append(diag("error", "sandbox_wrapper_missing",
                    "server declares a sandbox_profile but params.sandbox_wrapper is not set; refusing to render it unsandboxed", name))
