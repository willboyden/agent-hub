"""Observed inventory (Addendum F3): the skills a client REALLY has, as read by `ClientAdapter.observe`, stored under
<state>/observed/<client>/ so they can be listed, compared with the hub's own skills, evaluated and (gated) imported.

Everything stored here is untrusted text from the client: it is never executed, files are 0600 and directories 0700, and every
path is checked (`check_relpath`, UTF-8 encodable, no control/format characters) before it is written. A new observation REPLACES
the previous one: it is built in a sibling temporary directory and renamed into place, so a failed build leaves the old one
untouched. An observation that is incomplete AND empty (the client could not be reached) is a failed observe: it is refused
and the previous observation is kept."""
from __future__ import annotations

import contextlib
import hashlib
import json
import os
import secrets
import shutil
import threading
import time
import unicodedata
from collections import Counter
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from agent_hub.adapters.base import OBSERVED_ORIGINS, Observation
from agent_hub.domain.errors import BadRequest, NotFound, Unavailable
from agent_hub.domain.ids import slugify, valid_id
from agent_hub.domain.pathsafe import PathError, check_relpath
from agent_hub.services.common import fsync_dir, scrub
from agent_hub.services.skilleval import (
    INDEX_CAP,
    KEY_RE,
    _mkdir_private,
    _read_regular,
    _TooBig,
    content_hash,
    is_bytecode,
)

MAX_SKILLS = 5000
MAX_BYTES = 64 * 1024 * 1024
MAX_NOTES = 50
HUB_STATUSES = ("in_hub_same", "in_hub_differs", "not_in_hub")
INDEX = "index.json"


@dataclass
class HubView:
    """What the hub has, for comparison: content hash -> skill names, name -> hash, and the names enabled for the client."""
    by_hash: dict[str, list[str]] = field(default_factory=dict)
    hash_of: dict[str, str | None] = field(default_factory=dict)
    enabled: set[str] = field(default_factory=set)


def make_key(path: str) -> str:
    """`<slug(last path segment)>-<first 8 hex of sha256(path)>`; always fullmatches KEY_RE."""
    slug = slugify(path.rsplit("/", 1)[-1])[:72] or "skill"
    key = f"{slug}-{hashlib.sha256(path.encode('utf-8')).hexdigest()[:8]}"
    if not KEY_RE.fullmatch(key):                         # slugify only emits [a-z0-9._-] starting alphanumeric; belt and braces
        key = f"skill-{hashlib.sha256(path.encode('utf-8')).hexdigest()[:8]}"
    return key


def _bad_text(text: str) -> bool:
    """Not UTF-8 encodable (a lone surrogate), or contains control/format/line-separator characters (U+202E, U+2028, DEL, ...)."""
    try:
        text.encode("utf-8")
    except UnicodeEncodeError:
        return True
    return any(unicodedata.category(c) in ("Cc", "Cf", "Zl", "Zp") for c in text)


def _display(text: str, limit: int) -> str:
    """Untrusted display text: unencodable characters dropped, control/format characters turned into spaces, secrets scrubbed."""
    clean = "".join(" " if unicodedata.category(c) in ("Cc", "Cf", "Zl", "Zp") else c
                    for c in text.encode("utf-8", "ignore").decode("utf-8"))
    return scrub(clean)[:limit]


def _hub_name(name: str) -> str:
    return name if valid_id(name) else slugify(name)


class ObserveService:
    def __init__(self, root: Path, hub_view: Callable[[str], HubView], *, clock: Callable[[], float] = time.time,
                 max_skills: int = MAX_SKILLS, max_bytes: int = MAX_BYTES,
                 eval_root: Path | None = None) -> None:
        self.root = root                                   # <state>/observed
        self.eval_root = eval_root                         # <state>/skilleval (stored runs of observed skills live under @observed)
        self.hub_view = hub_view
        self.clock = clock
        self.max_skills, self.max_bytes = max_skills, max_bytes
        self._mu = threading.Lock()

    # -- writing ---------------------------------------------------------------------------------------------------------------
    def store(self, client: str, obs: Observation) -> dict[str, Any]:
        """Replace <root>/<client>/ with this observation; returns the index document."""
        if not valid_id(client):
            raise BadRequest("invalid client id", code="invalid_id")
        notes = [_display(n, 300) for n in obs.notes[:MAX_NOTES]]
        complete = obs.complete
        if not obs.skills and not complete:
            raise Unavailable("the client could not be observed completely and nothing was found; the previous observation is "
                              "kept" + (": " + "; ".join(notes[:3]) if notes else ""), code="observe_failed")
        entries: list[dict[str, Any]] = []
        payload: dict[str, dict[str, bytes]] = {}
        seen: set[str] = set()
        total = 0
        bytecode = 0
        for s in obs.skills:
            if len(entries) >= self.max_skills:
                complete = False
                notes.append(f"stopped at {self.max_skills} skills; the rest were not stored")
                break
            try:
                if _bad_text(s.path):
                    raise PathError("unprintable path")
                path = check_relpath(s.path)
            except PathError:
                notes.append("skipped a skill whose path is unsafe")
                continue
            files: dict[str, bytes] = {}
            for rel, data in s.files.items():
                if is_bytecode(rel):                       # generated, unscannable, never delivered (skilleval.is_bytecode)
                    bytecode += 1
                    continue
                try:
                    if _bad_text(rel):
                        raise PathError("unprintable path")
                    files[check_relpath(rel)] = bytes(data)
                except PathError:
                    notes.append(f"skipped an unsafe file path inside {path}")
            for rel in [r for r in files if any(o.startswith(r + "/") for o in files)]:
                del files[rel]                             # a path cannot be both a file and a directory
                notes.append(f"skipped {path}/{rel}: it is also used as a directory")
            if "SKILL.md" not in files:
                notes.append(f"skipped {path}: no SKILL.md")
                continue
            size = sum(len(b) for b in files.values())
            if total + size > self.max_bytes:
                complete = False
                notes.append(f"skipped {path}: the {self.max_bytes // (1024 * 1024)} MiB limit for one observation is reached")
                continue
            key = make_key(path)
            if key in seen:
                notes.append(f"skipped a second skill at the same path {path}")
                continue
            seen.add(key)
            total += size
            payload[key] = files
            entries.append({"key": key, "path": path, "name": _display(s.name, 200), "description": _display(s.description, 500),
                            "origin": s.origin if s.origin in OBSERVED_ORIGINS else "client", "content_hash": content_hash(files),
                            "files": len(files), "bytes": size})
        if bytecode:
            notes.append(f"left out {bytecode} compiled Python bytecode file(s) (__pycache__, .pyc): never evaluated or imported")
        index = {"client": client, "observed_at": self.clock(), "source": _display(obs.source, 300), "complete": complete,
                 "notes": notes[:MAX_NOTES + 5], "skills": entries}
        with self._mu:
            self._replace(client, index, payload)
        return index

    def _replace(self, client: str, index: dict[str, Any], payload: dict[str, dict[str, bytes]]) -> None:
        _mkdir_private(self.root)
        for stale in self.root.glob(f".*-{client}-*"):           # leftovers of a crashed observation (we hold the lock)
            if stale.name.startswith((".tmp-", ".old-")) and stale.is_dir() and not stale.is_symlink():
                shutil.rmtree(stale, ignore_errors=True)
        token = secrets.token_hex(4)
        tmp, old, dest = self.root / f".tmp-{client}-{token}", self.root / f".old-{client}-{token}", self.root / client
        try:
            tmp.mkdir(mode=0o700)
            for key, files in payload.items():
                for rel, data in files.items():
                    target = tmp / "skills" / key / rel
                    _mkdir_private(target.parent)
                    fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
                    with os.fdopen(fd, "wb") as fh:
                        fh.write(data)
            (tmp / "skills").mkdir(mode=0o700, exist_ok=True)
            fd = os.open(tmp / INDEX, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
            with os.fdopen(fd, "w") as fh:
                json.dump(index, fh, indent=1, sort_keys=True)
                fh.flush()
                os.fsync(fh.fileno())
            had_old = dest.exists() or dest.is_symlink()
            if had_old:
                os.rename(dest, old)
            try:
                os.rename(tmp, dest)
            except OSError:
                if had_old:
                    os.rename(old, dest)
                raise
            fsync_dir(self.root)
            if had_old:
                shutil.rmtree(old, ignore_errors=True)
        except BaseException:
            shutil.rmtree(tmp, ignore_errors=True)
            raise

    # -- reading ----------------------------------------------------------------------------------------------------------------
    def index(self, client: str) -> dict[str, Any] | None:
        if not valid_id(client):
            return None
        try:
            got = _read_regular(self.root / client / INDEX, INDEX_CAP)
            doc = json.loads(got[0]) if got else None
        except (_TooBig, OSError, ValueError, RecursionError):
            return None
        return doc if isinstance(doc, dict) and isinstance(doc.get("skills"), list) else None

    def entry(self, client: str, key: str) -> dict[str, Any] | None:
        if not KEY_RE.fullmatch(key):
            return None
        idx = self.index(client)
        for s in (idx or {}).get("skills", []):
            if isinstance(s, dict) and s.get("key") == key:
                return s
        return None

    def files(self, client: str, key: str) -> dict[str, bytes]:
        """The stored files of one observed skill (regular files only, never through a symlink; no compiled bytecode, the
        same rule as evaluation, so an import delivers exactly what was evaluated)."""
        if self.entry(client, key) is None:
            raise NotFound(f"no such observed skill: {client}:{key}")
        base = self.root / client / "skills" / key
        out: dict[str, bytes] = {}
        total = 0
        for dirpath, dirnames, filenames in os.walk(base, followlinks=False):
            dirnames[:] = sorted(d for d in dirnames if not os.path.islink(os.path.join(dirpath, d)))
            for n in sorted(filenames):
                full = Path(dirpath) / n
                rel = full.relative_to(base).as_posix()
                if is_bytecode(rel):
                    continue
                try:
                    check_relpath(rel)
                    got = _read_regular(full, self.max_bytes - total)
                except (PathError, _TooBig, OSError):
                    continue
                if got is not None:
                    total += len(got[0])
                    out[rel] = got[0]
        if "SKILL.md" not in out:
            raise NotFound(f"observed skill {client}:{key} has no SKILL.md")
        return out

    def annotated(self, client: str) -> dict[str, Any] | None:
        """The stored index with, per skill, the comparison with the hub computed NOW: hub_status, hub_name, enabled_for_client."""
        idx = self.index(client)
        if idx is None:
            return None
        view = self.hub_view(client)
        items: list[dict[str, Any]] = []
        for s in idx["skills"]:
            if not isinstance(s, dict):
                continue
            hn = _hub_name(str(s.get("name") or ""))
            names = view.by_hash.get(str(s.get("content_hash")), [])
            if names:
                status, hub_name = "in_hub_same", (hn if hn in names else names[0])
            elif hn and hn in view.hash_of:
                status, hub_name = "in_hub_differs", hn
            else:
                status, hub_name = "not_in_hub", None
            items.append({**s, "hub_status": status, "hub_name": hub_name,
                          "enabled_for_client": bool(hub_name and hub_name in view.enabled)})
        return {**{k: v for k, v in idx.items() if k != "skills"}, "items": items}

    @staticmethod
    def count(items: list[dict[str, Any]]) -> dict[str, Any]:
        return {"by_origin": dict(Counter(str(i.get("origin")) for i in items)),
                "by_hub_status": {k: sum(1 for i in items if i.get("hub_status") == k) for k in HUB_STATUSES}}

    def summary(self, client: str) -> dict[str, Any] | None:
        """For drift and the client views; None before the first observation."""
        doc = self.annotated(client)
        if doc is None:
            return None
        c = self.count(doc["items"])
        return {"observed_at": doc.get("observed_at"), "total": len(doc["items"]), "complete": bool(doc.get("complete")),
                "not_in_hub": c["by_hub_status"]["not_in_hub"], "in_hub_differs": c["by_hub_status"]["in_hub_differs"],
                "in_hub_same": c["by_hub_status"]["in_hub_same"], "by_origin": c["by_origin"]}

    def forget(self, client: str) -> None:
        """Drop a client's observation (the client was deleted)."""
        if valid_id(client):
            with self._mu:
                for d in (self.root / client, *([self.eval_root / "@observed" / client] if self.eval_root else [])):
                    with contextlib.suppress(OSError):
                        shutil.rmtree(d)


__all__ = ["HUB_STATUSES", "HubView", "ObserveService", "make_key"]
