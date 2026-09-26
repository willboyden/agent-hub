"""Path safety for everything the hub writes or reads on behalf of a client.

Rules (ARCHITECTURE section 4): relative only; no `..`, empty or `.` segments, no absolute paths, backslashes or NUL;
the resolved path must stay under the named root and no component of the relative part may be a symlink."""
from __future__ import annotations

import os
from pathlib import Path


class PathError(ValueError):
    pass


MAX_REL = 512


def check_relpath(rel: str) -> str:
    if not isinstance(rel, str) or not rel:
        raise PathError("empty path")
    if len(rel) > MAX_REL:
        raise PathError("path too long")
    if "\x00" in rel or "\\" in rel:
        raise PathError("NUL or backslash in path")
    if any(ord(c) < 32 for c in rel):
        raise PathError("control character in path")
    if rel.startswith("/") or (len(rel) > 1 and rel[1] == ":"):
        raise PathError("absolute path")
    for part in rel.split("/"):
        if part in ("", ".", ".."):
            raise PathError("empty, '.' or '..' path segment")
    return rel


def resolve_under(root: Path | str, rel: str) -> Path:
    """Return the absolute target for `rel` under `root`, refusing symlinks and escapes. The root itself may be a
    symlink (a user's ~/.claude commonly is); everything beneath it may not."""
    rel = check_relpath(rel)
    root_p = Path(root)
    if not root_p.is_absolute():
        raise PathError("root must be absolute")
    real_root = Path(os.path.realpath(root_p))
    cur = real_root
    checking = True
    for part in rel.split("/"):
        cur = cur / part
        if checking:
            if cur.is_symlink():
                raise PathError("symlink in path")
            if not cur.exists():
                checking = False       # nothing beyond a missing component can be a symlink yet
    if real_root not in cur.parents:
        raise PathError("path escapes its root")
    return cur


def expand_root(value: str) -> Path:
    """Client roots come from content: expand ~, demand an absolute path, refuse NUL/relative traversal."""
    if "\x00" in value or not value:
        raise PathError("bad root")
    p = Path(value).expanduser()
    if not p.is_absolute():
        raise PathError("root must be absolute (or start with ~)")
    if ".." in p.parts:
        raise PathError("root must not contain '..'")
    return p
