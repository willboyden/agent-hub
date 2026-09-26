"""The content repo is the approval boundary, so it must not be writable by the agents it governs. A planted `.git/config`
(filter / merge driver / fsmonitor / sshCommand), `.gitattributes` or hook would run attacker commands as the hub user the next
time git is invoked. This module refuses to operate on a content dir that is inside a client root or the app tree, not owned
by the current user, group/world-writable, or whose git metadata contains anything outside a small allowlist."""
from __future__ import annotations

import os
import shutil
from pathlib import Path

from agent_hub.domain.errors import ProblemError

CODE = "content_dir_unsafe"
FIX = ("Keep the content repo in a private directory outside every client root and the app tree "
       "(default ~/.local/share/agent-hub/content, mode 0700) and move it with `hubctl migrate-content --to PATH`.")
ALLOWED_CONFIG_KEYS = {"core.repositoryformatversion", "core.filemode", "core.bare", "core.logallrefupdates", "core.ignorecase",
                       "core.precomposeunicode", "user.name", "user.email"}


def _unsafe(msg: str) -> ProblemError:
    return ProblemError(f"content directory is not safe: {msg}. {FIX}", code=CODE, status=503)


def _real(p: str | Path) -> Path:
    return Path(os.path.realpath(p))


def check_location(content_dir: Path, client_roots: list[str], app_root: Path) -> None:
    cd = _real(content_dir)
    app = _real(app_root)
    if cd == app or app in cd.parents:
        raise _unsafe(f"{cd} is inside the app tree {app}, which agents can write")
    for r in client_roots:
        rr = _real(r)
        if cd == rr or rr in cd.parents:
            raise _unsafe(f"{cd} is inside the client root {rr}")
        if cd in rr.parents:
            raise _unsafe(f"the client root {rr} is inside the content directory")


def _check_owner_mode(p: Path) -> None:
    try:
        st = p.lstat()
    except FileNotFoundError:
        return
    if st.st_uid != os.getuid():
        raise _unsafe(f"{p} is not owned by the current user")
    if st.st_mode & 0o022:
        raise _unsafe(f"{p} is group- or world-writable (mode {oct(st.st_mode & 0o777)})")


def check_ownership(content_dir: Path) -> None:
    _check_owner_mode(content_dir)
    git = content_dir / ".git"
    if git.exists() or git.is_symlink():
        if git.is_symlink() or not git.is_dir():
            raise _unsafe(".git must be a real directory (not a symlink or gitdir file)")
        _check_owner_mode(git)
        for sub in ("hooks", "info", "objects", "refs"):
            _check_owner_mode(git / sub)


def parse_config_keys(text: str) -> set[str]:
    keys: set[str] = set()
    section = ""
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line[0] in "#;":
            continue
        if line.startswith("["):
            end = line.find("]")
            head = line[1:end] if end > 0 else line[1:]
            section = head.split(None, 1)[0].lower() if head.strip() else ""
            if " " in head.strip() or '"' in head:
                keys.add(f"{head.strip().lower()}.*")       # any [section "subsection"] is outside the allowlist
            rest = line[end + 1:].strip() if end > 0 else ""
            if rest and "=" in rest:
                keys.add(f"{section}.{rest.split('=', 1)[0].strip().lower()}")
            continue
        key = line.split("=", 1)[0].strip().lower()
        keys.add(f"{section}.{key}")
    return keys


def verify_git_metadata(content_dir: Path) -> None:
    git = content_dir / ".git"
    if not git.is_dir():
        return
    cfg = git / "config"
    if cfg.exists():
        bad = sorted(k for k in parse_config_keys(cfg.read_text(errors="replace")) if k not in ALLOWED_CONFIG_KEYS)
        if bad:
            raise _unsafe(f".git/config contains keys outside the allowlist: {bad[:6]}")
    attrs = git / "info" / "attributes"
    if attrs.exists() and attrs.stat().st_size > 0:
        raise _unsafe(".git/info/attributes is not empty")
    hooks = git / "hooks"
    if hooks.is_dir():
        live = sorted(h.name for h in hooks.iterdir() if not h.name.endswith(".sample"))
        if live:
            raise _unsafe(f".git/hooks contains non-sample files: {live[:5]}")
    for dirpath, dirnames, filenames in os.walk(content_dir):
        if ".git" in dirnames:
            dirnames.remove(".git")
        if ".gitattributes" in filenames and os.path.getsize(os.path.join(dirpath, ".gitattributes")) > 0:
            raise _unsafe(f"{os.path.join(dirpath, '.gitattributes')} is not empty")


def hostile_findings(content_dir: Path) -> str | None:
    """The reason verify_git_metadata would refuse this repo, or None. Plain file reads only: git is never run."""
    try:
        verify_git_metadata(content_dir)
    except ProblemError as exc:
        return exc.detail.split(". Keep the content repo")[0]
    return None


def sanitize_git(root: Path) -> list[str]:
    """Strip everything the safety gate refuses from a COPY (never the source): config keys outside the allowlist, non-sample
    hooks, info/attributes and non-empty .gitattributes. Plain file operations only. Returns what was removed."""
    removed: list[str] = []
    git = root / ".git"
    cfg = git / "config"
    if cfg.exists():
        keep: dict[str, list[str]] = {}
        section = ""
        for raw in cfg.read_text(errors="replace").splitlines():
            line = raw.strip()
            if not line or line[0] in "#;":
                continue
            if line.startswith("["):
                end = line.find("]")
                head = line[1:end] if end > 0 else line[1:]
                section = head.strip().lower() if " " not in head.strip() and '"' not in head else ""
                continue
            key = line.split("=", 1)[0].strip().lower()
            if section and f"{section}.{key}" in ALLOWED_CONFIG_KEYS:
                keep.setdefault(section, []).append(line)
            else:
                removed.append(f"config {section or '?'}.{key}")
        cfg.write_text("".join(f"[{s}]\n" + "".join(f"\t{ln}\n" for ln in lines) for s, lines in keep.items()))
    hooks = git / "hooks"
    if hooks.is_dir():
        for h in hooks.iterdir():
            if not h.name.endswith(".sample"):
                removed.append(f"hook {h.name}")
                h.unlink() if h.is_file() or h.is_symlink() else shutil.rmtree(h)
    attrs = git / "info" / "attributes"
    if attrs.exists() and attrs.stat().st_size > 0:
        removed.append("info/attributes")
        attrs.write_text("")
    for dirpath, dirnames, filenames in os.walk(root):
        if ".git" in dirnames:
            dirnames.remove(".git")
        if ".gitattributes" in filenames:
            p = os.path.join(dirpath, ".gitattributes")
            if os.path.getsize(p) > 0:
                removed.append(os.path.relpath(p, root))
                open(p, "w").close()
    return removed


def plain_head(root: Path) -> str | None:
    """HEAD commit id read from the ref files (no git process)."""
    git = root / ".git"
    try:
        head = (git / "HEAD").read_text().strip()
    except OSError:
        return None
    if not head.startswith("ref:"):
        return head or None
    ref = head[4:].strip()
    try:
        return (git / ref).read_text().strip()
    except OSError:
        pass
    try:
        for line in (git / "packed-refs").read_text().splitlines():
            if line.endswith(" " + ref):
                return line.split()[0]
    except OSError:
        return None
    return None


def tighten(root: Path) -> None:
    """Directories 0700; files lose group/world write (other bits kept, so executables stay executable)."""
    os.chmod(root, 0o700)
    for dirpath, dirnames, filenames in os.walk(root):
        for d in dirnames:
            p = os.path.join(dirpath, d)
            if not os.path.islink(p):
                os.chmod(p, 0o700)
        for f in filenames:
            p = os.path.join(dirpath, f)
            if not os.path.islink(p):
                os.chmod(p, os.stat(p).st_mode & 0o7755 & ~0o022)


def verify_all(content_dir: Path, client_roots: list[str], app_root: Path) -> None:
    check_location(content_dir, client_roots, app_root)
    check_ownership(content_dir)
    verify_git_metadata(content_dir)
