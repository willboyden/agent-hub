"""Small shared helpers: clock, ids, hashing, redaction, atomic file writes."""
from __future__ import annotations

import contextlib
import hashlib
import os
import re
import secrets
import time
from pathlib import Path
from typing import Any


def now() -> float:
    return time.time()


def new_id(prefix: str = "") -> str:
    return prefix + secrets.token_hex(6)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


_SENSITIVE_KEY = re.compile(r"(token|secret|password|passwd|api[_-]?key|authorization|cookie|credential)", re.I)
_SECRET_VALUE = re.compile(
    r"(\b(hf_[A-Za-z0-9]{8,}|ec_[A-Za-z0-9_\-]{16,}|hc_[A-Za-z0-9_\-]{16,}|sk-[A-Za-z0-9_\-]{16,}|"
    r"ghp_[A-Za-z0-9]{16,}|xox[bp]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16})\b|Bearer\s+[A-Za-z0-9._\-]{8,}|"
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----)")
# `KEY=value` / `"api_key": "value"` inside free text (plan diffs): mask the value, keep the name
_ASSIGN = re.compile(r"""(?i)((?:token|secret|password|passwd|api[_-]?key|credential)[\w-]*["']?\s*[:=]\s*["']?)([^\s"',}]{6,})""")


def scrub(text: str) -> str:
    """Mask secret-looking tokens in free text (diffs, logs). No truncation."""
    return _ASSIGN.sub(lambda m: m.group(1) + "[redacted]", _SECRET_VALUE.sub("[redacted]", text))


def redact(obj: Any, _depth: int = 0) -> Any:
    """Deep copy with sensitive keys masked, secret-looking values scrubbed, long strings truncated."""
    if _depth > 8:
        return "[truncated]"
    if isinstance(obj, dict):
        return {k: ("[redacted]" if isinstance(k, str) and _SENSITIVE_KEY.search(k) else redact(v, _depth + 1))
                for k, v in obj.items()}
    if isinstance(obj, list):
        return [redact(v, _depth + 1) for v in obj[:100]]
    if isinstance(obj, str):
        s = scrub(obj)
        return s if len(s) <= 500 else s[:500] + f"...[{len(s) - 500} more chars]"
    return obj


def atomic_write(path: Path, data: bytes, mode: int = 0o644, dir_mode: int = 0o755) -> None:
    """temp file in the SAME directory (so rename is atomic), fsync, fchmod, rename, fsync the directory."""
    _mkdirs(path.parent, dir_mode)
    tmp = path.parent / f".hub-tmp-{secrets.token_hex(6)}"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, "wb") as fh:
            fh.write(data)
            fh.flush()
            os.fsync(fh.fileno())
            os.fchmod(fh.fileno(), mode)
        os.replace(tmp, path)
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(tmp)
        raise
    fsync_dir(path.parent)


def _mkdirs(d: Path, mode: int) -> None:
    """mkdir -p where every directory THIS call creates gets `mode` (Path.mkdir applies mode only to the leaf)."""
    missing: list[Path] = []
    p = d
    while not p.exists() and p != p.parent:
        missing.append(p)
        p = p.parent
    for m in reversed(missing):
        with contextlib.suppress(FileExistsError):
            m.mkdir(mode=mode)
        m.chmod(mode)


def fsync_dir(d: Path) -> None:
    with contextlib.suppress(OSError):
        fd = os.open(d, os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)


def harden_tree(root: Path) -> None:
    """chmod every directory under `root` (and root) to 0700; symlinks are never followed."""
    if not root.is_dir() or root.is_symlink():
        return
    with contextlib.suppress(OSError):
        root.chmod(0o700)
    for dirpath, dirnames, _ in os.walk(root, followlinks=False):
        for d in dirnames:
            p = Path(dirpath) / d
            if not p.is_symlink():
                with contextlib.suppress(OSError):
                    p.chmod(0o700)


def write_secret_file(path: Path, text: str) -> None:
    """0600 from the first byte; fchmod because O_CREAT's mode is ignored for a pre-existing, looser file."""
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    os.fchmod(fd, 0o600)
    with os.fdopen(fd, "w") as fh:
        fh.write(text)
