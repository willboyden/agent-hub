"""Path-ingest safety: everything is resolved (symlinks followed) and must land under an allowlisted root."""
from __future__ import annotations

import re
from collections.abc import Iterator
from pathlib import Path

from agent_knowledge.errors import KnowledgeError

GLOB_RE = re.compile(r"[A-Za-z0-9_.*?/\-]{1,200}")
TEXT_SUFFIXES = {".md", ".txt", ".rst", ".py", ".ts", ".js", ".json", ".yaml", ".yml", ".toml", ".sh", ".go", ".rs",
                 ".c", ".h", ".cpp", ".java", ".html", ".css", ".csv", ".tex", ".org", ".ipynb"}
# Never ingest things conventionally treated as secrets, even when they sit inside an allowlisted root.
DENY_PARTS = {".ssh", ".aws", ".gnupg", ".git", ".config", ".kube", ".docker", "secrets", "__pycache__"}
MAX_FILE_BYTES = 2 * 1024 * 1024


def _deny(code: str, detail: str) -> KnowledgeError:
    return KnowledgeError(403 if code == "path_not_allowed" else 422, code, detail)


def resolve_base(path: str, roots: list[Path]) -> tuple[Path, Path]:
    """Return (resolved base, its root). Raises if `path` is not under an allowlisted root after resolution."""
    if not roots:
        raise _deny("path_not_allowed", "no ingest roots are configured")
    if "\x00" in path or len(path) > 1024 or not path.startswith("/"):
        raise _deny("path_not_allowed", "path must be an absolute path under an allowlisted root")
    try:
        base = Path(path).resolve(strict=True)
    except (OSError, RuntimeError) as e:
        raise _deny("path_not_found", "path does not exist") from e
    for root in roots:
        rr = root.resolve()
        if base == rr or base.is_relative_to(rr):
            return base, rr
    raise _deny("path_not_allowed", "path is outside every allowlisted root")


def check_glob(glob: str) -> str:
    if not GLOB_RE.fullmatch(glob) or glob.startswith("/") or ".." in glob.split("/"):
        raise _deny("invalid_glob", "glob may only use [A-Za-z0-9_.*?/-], must be relative and contain no '..'")
    return glob


def iter_files(base: Path, root: Path, glob: str, max_files: int) -> Iterator[Path]:
    """Yield resolved regular files under `root`, skipping symlink escapes, denied parts, non-text and huge files."""
    candidates = [base] if base.is_file() else sorted(base.glob(glob))
    n = 0
    for c in candidates:
        try:
            real = c.resolve(strict=True)
        except (OSError, RuntimeError):
            continue
        if not real.is_relative_to(root) or not real.is_file():
            continue  # symlink pointing outside the root, or a directory
        rel = real.relative_to(root)
        if any(p in DENY_PARTS or p.startswith(".env") for p in rel.parts) or real.suffix.lower() not in TEXT_SUFFIXES:
            continue
        try:
            if real.stat().st_size > MAX_FILE_BYTES:
                continue
        except OSError:
            continue
        n += 1
        if n > max_files:
            raise KnowledgeError(413, "too_many_files", f"glob matches more than {max_files} files")
        yield real
