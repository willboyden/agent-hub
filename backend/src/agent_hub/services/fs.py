"""Real I/O ports. `RealFS` (adapter FileSystem port) refuses anything outside the allowed roots; `RealRunner`
(CommandRunner port) never uses a shell, always has a timeout, and passes a minimal environment."""
from __future__ import annotations

import fnmatch
import os
import subprocess
from pathlib import Path

from agent_hub.domain.pathsafe import PathError, resolve_under

MAX_READ = 8 * 1024 * 1024


SECRET_NAMES = (".env", ".env.*", "*.pem", "*.key", "id_rsa*", "credentials*", "*.p12")
SECRET_DIRS = {".ssh", ".aws", ".gnupg"}


def is_secret_path(p: Path) -> bool:
    """Defence in depth for discovery: never read credential-looking files, even inside an allowed dir."""
    if any(part in SECRET_DIRS for part in p.parts):
        return True
    return any(fnmatch.fnmatchcase(p.name, pat) for pat in SECRET_NAMES)


class RealFS:
    def __init__(self, roots: list[str | Path], deny_secrets: bool = False) -> None:
        self.roots = [Path(os.path.realpath(r)) for r in roots]
        self.deny_secrets = deny_secrets

    def _ok(self, path: str) -> Path:
        if "\x00" in path:
            raise PermissionError("bad path")
        real = Path(os.path.realpath(path))
        if not any(real == r or r in real.parents for r in self.roots):
            raise PermissionError("path is outside the configured roots")
        if self.deny_secrets and is_secret_path(real):
            raise FileNotFoundError(path)                      # invisible, not "forbidden": nothing to probe
        return real

    def read_bytes(self, path: str) -> bytes:
        p = self._ok(path)
        if p.stat().st_size > MAX_READ:
            raise OSError("file too large")
        return p.read_bytes()

    def exists(self, path: str) -> bool:
        try:
            return self._ok(path).exists()
        except (PermissionError, FileNotFoundError):
            return False

    def listdir(self, path: str) -> list[str]:
        base = self._ok(path)
        return sorted(n for n in os.listdir(base) if not (self.deny_secrets and is_secret_path(base / n)))

    def size(self, path: str) -> int:
        """stat only (same allowed-path checks), so callers can cap BEFORE reading."""
        return self._ok(path).stat().st_size

    def is_dir(self, path: str) -> bool:
        try:
            return self._ok(path).is_dir()
        except (PermissionError, FileNotFoundError):
            return False


class RealRunner:
    def __init__(self, max_timeout: float = 60.0) -> None:
        self.max_timeout = max_timeout

    def run(self, argv: list[str], *, timeout: float) -> tuple[int, str, str]:
        if not argv or not all(isinstance(a, str) and "\x00" not in a for a in argv):
            return 127, "", "invalid argv"
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": os.environ.get("HOME", "/"), "LC_ALL": "C"}
        try:
            p = subprocess.run(argv, capture_output=True, timeout=min(timeout, self.max_timeout), env=env, check=False)
        except FileNotFoundError:
            return 127, "", "command not found"
        except subprocess.TimeoutExpired:
            return 124, "", "timed out"
        return p.returncode, p.stdout.decode("utf-8", "replace")[:100_000], p.stderr.decode("utf-8", "replace")[:20_000]


class LiveFiles:
    """Read/locate files under a client's named roots with the path-safety rules of ARCHITECTURE section 4."""

    def __init__(self, roots: dict[str, str], max_bytes: int = MAX_READ) -> None:
        self.roots = roots
        self.max_bytes = max_bytes

    def target(self, root: str, rel: str) -> Path:
        base = self.roots.get(root)
        if base is None:
            raise PathError(f"root {root!r} is not configured")
        return resolve_under(base, rel)

    def read(self, root: str, rel: str) -> bytes | None:
        """None when the file does not exist. Raises PathError/OSError for unsafe or non-regular targets."""
        p = self.target(root, rel)
        if not p.exists():
            return None
        if not p.is_file():
            raise OSError("not a regular file")
        if p.stat().st_size > self.max_bytes:
            raise OSError("file too large")
        return p.read_bytes()


def discovery_fs(roots: dict[str, str], params: dict[str, object], extra: list[str]) -> RealFS:
    """READ-ONLY discovery view: exactly the client's roots, the adapter's declared `params.source_dirs` (relative to
    `params.source_root`, no `..`, no symlink escape) and operator-configured extras. Secret-looking files are invisible."""
    allowed: list[str | Path] = [*roots.values(), *extra]
    base = next((params[k] for k in ("source_root", "base_dir", "lab_dir") if isinstance(params.get(k), str)), None)   # lab_dir: legacy name
    srcs = params.get("source_dirs")
    if isinstance(base, str) and isinstance(srcs, list) and base and "\x00" not in base:
        base_real = Path(os.path.realpath(Path(base).expanduser()))
        for entry in srcs:
            rel = entry.get("path") if isinstance(entry, dict) else entry      # adapters declare {kind, path} or a bare path
            if not isinstance(rel, str) or not rel or "\x00" in rel or ".." in Path(rel).parts:
                continue
            cand = Path(rel).expanduser()
            cand = cand if cand.is_absolute() else base_real / cand
            real = Path(os.path.realpath(cand))
            if real == base_real or base_real in real.parents:       # a link that leaves the base dir is refused
                allowed.append(real)
    return RealFS(allowed, deny_secrets=True)
