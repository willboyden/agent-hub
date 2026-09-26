"""The content repo, via the `git` CLI: argv lists only, every call has a timeout, no shell, a fixed identity, and the
operator's global/system git config is ignored (hooks, signing, aliases and includes cannot influence the hub)."""
from __future__ import annotations

import difflib
import os
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

from agent_hub.domain.errors import BadRequest, Conflict, ProblemError, Unavailable
from agent_hub.domain.ids import COMMIT_RE
from agent_hub.domain.pathsafe import PathError, check_relpath
from agent_hub.services import contentguard

EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbf2c1ff"     # git's well-known empty tree object id
MAX_ARCHIVE = 64 * 1024 * 1024


@dataclass
class GitResult:
    rc: int
    out: bytes
    err: str


@dataclass
class Change:
    path: str
    status: str          # added | modified | deleted | renamed
    diff: str


class GitRepo:
    def __init__(self, path: Path, git_bin: str, name: str, email: str, timeout: float = 30.0) -> None:
        self.path = path
        self.git_bin = git_bin
        self.name, self.email, self.timeout = name, email, timeout
        self._verified = 0.0

    def verify_safe(self, force: bool = False) -> None:
        """Refuse to run git against a repo whose metadata could execute code (see contentguard). Re-checked at most every 2 s
        between hub operations, and always at the start of one (force)."""
        if not (self.path / ".git").exists() and not (self.path / ".git").is_symlink():
            return
        now = time.monotonic()
        if force or now - self._verified > 2.0:
            contentguard.check_ownership(self.path)
            contentguard.verify_git_metadata(self.path)
            self._verified = now

    # -- plumbing ------------------------------------------------------------------------------
    def _env(self) -> dict[str, str]:
        return {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(self.path),                 # a HOME with no gitconfig
            "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_SYSTEM": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_ATTR_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0", "GIT_LITERAL_PATHSPECS": "1", "GIT_OPTIONAL_LOCKS": "0", "LC_ALL": "C",
            "GIT_AUTHOR_NAME": self.name, "GIT_AUTHOR_EMAIL": self.email,
            "GIT_COMMITTER_NAME": self.name, "GIT_COMMITTER_EMAIL": self.email,
        }

    def run(self, *args: str, check: bool = True, timeout: float | None = None) -> GitResult:
        if args and args[0] != "init":
            self.verify_safe()
        argv = [self.git_bin, "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false",
                "-c", "core.sshCommand=/bin/false", "-c", "protocol.allow=never", "-c", "diff.external=",
                "-c", "core.quotepath=off", "-c", "core.fsmonitor=false", "-c", "core.autocrlf=false", "-c", "core.attributesFile=/dev/null",
                "-c", "safe.directory=" + str(self.path), *args]
        try:
            p = subprocess.run(argv, cwd=self.path, env=self._env(), capture_output=True, umask=0o077,   # git's files stay private
                               timeout=timeout or self.timeout, check=False)
        except FileNotFoundError as exc:
            raise Unavailable("git is not installed", code="git_missing") from exc
        except subprocess.TimeoutExpired as exc:
            raise Unavailable("git timed out", code="git_timeout") from exc
        res = GitResult(p.returncode, p.stdout, p.stderr.decode("utf-8", "replace").strip())
        if check and res.rc != 0:
            raise Conflict(f"git {args[0]} failed: {res.err[:300]}", code="git_error")
        return res

    def text(self, *args: str) -> str:
        return self.run(*args).out.decode("utf-8", "replace").strip()

    # -- lifecycle -----------------------------------------------------------------------------
    def is_repo(self) -> bool:
        try:
            return (self.path / ".git").is_dir() and self.run("rev-parse", "--git-dir", check=False).rc == 0
        except ProblemError:                       # an unsafe repo is never "ready"; require_repo reports the reason
            return False

    def init(self) -> None:
        fresh = not self.path.exists()
        self.path.mkdir(parents=True, exist_ok=True, mode=0o700)
        if fresh:
            self.path.chmod(0o700)
        if not self.is_repo():
            self.run("init", "-q", "-b", "main")

    def has_head(self) -> bool:
        return self.run("rev-parse", "--verify", "-q", "HEAD", check=False).rc == 0

    def head_tree(self) -> str:
        """The content hash: the tree id of HEAD, so a revert to identical content yields an identical hash."""
        return self.text("rev-parse", "HEAD^{tree}") if self.has_head() else EMPTY_TREE

    def head(self) -> str | None:
        return self.text("rev-parse", "HEAD") if self.has_head() else None

    # -- pending changes -----------------------------------------------------------------------
    def status(self) -> list[tuple[str, str]]:
        out = self.run("status", "--porcelain=v1", "-z", "--untracked-files=all").out.decode("utf-8", "replace")
        entries = [e for e in out.split("\x00") if e]
        res: list[tuple[str, str]] = []
        i = 0
        while i < len(entries):
            e = entries[i]
            xy, path = e[:2], e[3:]
            if "R" in xy or "C" in xy:
                i += 1                                   # the next NUL field is the rename source
            code = "added" if xy == "??" or "A" in xy else "deleted" if "D" in xy else "renamed" if "R" in xy else "modified"
            res.append((path, code))
            i += 1
        return res

    def changes(self, max_diff: int = 200_000) -> list[Change]:
        head = self.has_head()
        out: list[Change] = []
        for path, status in self.status():
            out.append(Change(path, status, self._diff_for(path, status, head, max_diff)))
        return out

    def _diff_for(self, path: str, status: str, head: bool, max_diff: int) -> str:
        if status == "added" and not self._tracked(path):
            f = self.path / path
            try:
                if f.is_symlink() or not f.is_file():
                    return ""
                raw = f.read_bytes()[:max_diff]
                if b"\x00" in raw:
                    return f"Binary file {path} added\n"
                new = raw.decode("utf-8", "replace").splitlines(keepends=True)
            except OSError:
                return ""
            return "".join(difflib.unified_diff([], new, "/dev/null", f"b/{path}"))
        if not head:
            return ""
        r = self.run("diff", "HEAD", "--no-color", "--no-ext-diff", "--no-textconv", "--", path, check=False)
        return r.out.decode("utf-8", "replace")[:max_diff]

    def _tracked(self, path: str) -> bool:
        return self.run("ls-files", "--error-unmatch", "--", path, check=False).rc == 0

    def is_clean(self) -> bool:
        return not self.status()

    # -- commit / discard / history / revert -----------------------------------------------------
    def strays(self) -> list[str]:
        """Pending paths outside the known content directories: reported, never committed."""
        return [p for p, _ in self.status() if not _is_known(p)]

    def commit(self, message: str) -> str:
        """Stage ONLY the known content paths (never `add -A` of the whole tree), so stray files are reported, not committed."""
        message = message.strip()
        if not message or len(message) > 2000:
            raise BadRequest("commit message must be 1..2000 characters")
        for k in KNOWN_CONTENT:
            if (self.path / k).exists() or self._tracked(k):
                self.run("add", "-A", "--", k, check=False)
        if self.run("diff", "--cached", "--quiet", check=False).rc == 0:
            raise Conflict("nothing to commit" + (f" (untracked/stray paths ignored: {self.strays()[:5]})" if self.strays() else ""),
                           code="nothing_to_commit")
        self.run("commit", "-q", "--no-verify", "-m", message)
        return self.text("rev-parse", "HEAD")

    def discard(self, paths: list[str] | None, dry_run: bool = False) -> list[str]:
        clean_paths = _check_paths(paths) if paths else []
        touched = [p for p, _ in self.status()]
        if paths:
            touched = [p for p in touched if any(p == c or p.startswith(c.rstrip("/") + "/") for c in clean_paths)]
        if dry_run:
            return touched
        if self.has_head():
            tracked = [p for p in touched if self._tracked(p)]
            if tracked:
                self.run("restore", "--source=HEAD", "--staged", "--worktree", "--", *tracked)
        untracked = [p for p in touched if not self._tracked_head(p)]
        if untracked:
            self.run("clean", "-fdq", "--", *untracked)
        return touched

    def _tracked_head(self, path: str) -> bool:
        return self.has_head() and self.run("cat-file", "-e", f"HEAD:{path}", check=False).rc == 0

    def history(self, limit: int = 50) -> list[dict[str, object]]:
        if not self.has_head():
            return []
        fmt = "%H%x1f%an%x1f%at%x1f%s"
        raw = self.text("log", f"-n{max(1, min(limit, 500))}", f"--format={fmt}")
        rows = []
        for line in raw.splitlines():
            h, author, at, subject = (line.split("\x1f") + ["", "", "", ""])[:4]
            rows.append({"commit": h, "author": author, "time": int(at or 0), "message": subject})
        return rows

    def revert(self, commit: str) -> str:
        if COMMIT_RE.fullmatch(commit) is None:
            raise BadRequest("commit must be a hex object id")
        if not self.is_clean():
            raise Conflict("commit or discard pending changes before reverting", code="dirty_tree")
        if self.run("cat-file", "-e", f"{commit}^{{commit}}", check=False).rc != 0:
            raise BadRequest("unknown commit", code="unknown_commit")
        r = self.run("revert", "--no-edit", commit, check=False)
        if r.rc != 0:
            self.run("revert", "--abort", check=False)
            raise Conflict(f"revert failed (conflicts or a merge commit): {r.err[:200]}", code="revert_failed")
        return self.text("rev-parse", "HEAD")

    # -- snapshot of HEAD (what plans render from) ------------------------------------------------
    def export_head(self, dest: Path) -> None:
        """Materialise HEAD's tree into `dest` from the object database (`ls-tree` + `cat-file --batch`), NOT `git archive`:
        export-ignore / export-subst attributes cannot alter what gets planned. Regular files only; symlinks/submodules skipped."""
        dest.mkdir(parents=True, exist_ok=True)
        if not self.has_head():
            return
        listing = self.run("ls-tree", "-r", "-z", "--full-tree", "HEAD").out.decode("utf-8", "replace")
        entries: list[tuple[str, str]] = []
        for rec in (r for r in listing.split("\x00") if r):
            meta, _, path = rec.partition("\t")
            mode, typ, sha = (meta.split() + ["", "", ""])[:3]
            if typ != "blob" or mode not in ("100644", "100755"):
                continue
            try:
                check_relpath(path)
            except PathError:
                continue
            entries.append((path, sha))
        if not entries:
            return
        argv = [self.git_bin, "-c", "core.hooksPath=/dev/null", "-c", "core.attributesFile=/dev/null", "-c", "core.fsmonitor=false",
                "-c", "core.sshCommand=/bin/false", "-c", "protocol.allow=never", "cat-file", "--batch"]
        try:
            p = subprocess.run(argv, cwd=self.path, env=self._env(), input="".join(f"{sha}\n" for _, sha in entries).encode(),
                               capture_output=True, umask=0o077, timeout=self.timeout * 2, check=False)
        except subprocess.TimeoutExpired as exc:
            raise Unavailable("git timed out", code="git_timeout") from exc
        if p.returncode != 0 or len(p.stdout) > MAX_ARCHIVE:
            raise Conflict("could not read the content snapshot", code="snapshot_failed")
        buf, pos, total = p.stdout, 0, 0
        for path, sha in entries:
            nl = buf.index(b"\n", pos)
            hdr = buf[pos:nl].split()
            if len(hdr) != 3 or hdr[0].decode() != sha or hdr[1] != b"blob":
                raise Conflict("unexpected git object stream", code="snapshot_failed")
            size = int(hdr[2])
            blob = buf[nl + 1: nl + 1 + size]
            pos = nl + 1 + size + 1
            total += size
            if total > MAX_ARCHIVE:
                raise Conflict("content snapshot too large", code="snapshot_too_large")
            target = dest / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(blob)


KNOWN_CONTENT = ("skills", "agents", "instructions", "rules", "memory", "mcp", "collections", "clients", "profiles",
                 "endpoints.yaml", "hub.yaml")


def _is_known(path: str) -> bool:
    return any(path == k or path.startswith(k + "/") for k in KNOWN_CONTENT)


def _check_paths(paths: list[str]) -> list[str]:
    out = []
    for p in paths:
        try:
            out.append(check_relpath(p.rstrip("/")))
        except PathError as exc:
            raise BadRequest(f"bad path: {exc}") from exc
        if p.startswith("-") or p.split("/")[0] == ".git":
            raise BadRequest("bad path")
    return out
