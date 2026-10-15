"""SkillEvaluator (NVIDIA) integration: evaluate one skill where it is reviewed. ADVISORY ONLY: nothing here feeds plan, apply
or the policy floor, and a failing evaluation blocks nothing.

What this module relies on about SkillEvaluator 0.5.0 (a CLI, not a service):
  * It is configured by environment only (SKILL_EVAL_*); it reads no config file.
  * `tier1 PATH [--no-llm] -r json,markdown -o DIR` writes skillevaluator-tier1.json and .md; `tier2 PATH -r json -o DIR` writes
    skillevaluator-tier2.json.
  * The exit code is 1 whenever checks fail OR are incomplete, and the reports are still written. So the verdict is the JSON
    `overall_status` (passed | failed | incomplete), never the exit code. No report at all is the verdict `error`.

How the hub contains it (docs/SECURITY.md section 14):
  * Launcher: the first of LAUNCHERS found on PATH plus ~/.local/bin. Bare names only (nothing configurable can name another
    program), argv lists, no shell, a timeout per tier, its own process group (killed on timeout), stdout/stderr capped.
  * Child environment: PATH, HOME, LC_ALL, plus SKILL_EVAL_* / SKILLEVAL_* from the hub's environment. Only these binaries get the
    SKILL_EVAL_* variables; every other subprocess of the hub keeps the plain minimal environment (services/fs.py RealRunner).
  * The skill is COPIED from the content working tree (regular files only, never through a symlink, the hub.yaml sidecar excluded,
    file-count and byte caps) into a fresh private directory under the state dir. SkillEvaluator runs there (cwd) and is given that
    copy, so it never sees the content repo or its .git. The copy is deleted after the run.
  * Reports stay under <state>/skilleval/<skill>/<run-id>/ (directories 0700, files 0600), newest KEEP_RUNS per skill.
  * One evaluation at a time: a thread lock plus an flock on <state>/skilleval/.lock, so `hubctl` and the server exclude each other.
"""
from __future__ import annotations

import contextlib
import errno
import fcntl
import hashlib
import json
import os
import re
import secrets
import shutil
import signal
import stat
import subprocess
import threading
import time
from collections.abc import Callable, Iterator, Mapping
from contextlib import AbstractContextManager
from dataclasses import dataclass, field
from pathlib import Path
from typing import IO, Any, Protocol

from agent_hub.domain.errors import BadRequest, Conflict, NotFound, Unavailable, Unprocessable
from agent_hub.domain.ids import valid_id
from agent_hub.domain.pathsafe import PathError, check_relpath
from agent_hub.services.common import scrub

LAUNCHERS = ("skillevaluator-local", "skillevaluator")       # the first one found wins; a wrapper first, then the plain CLI
TIERS = ("static", "tier1", "tier1+2")
TIER_HELP = {
    "static": "Tier 1 static checks only (--no-llm: no model calls, about 2 s)",
    "tier1": "Tier 1 with the LLM judge when a provider is configured (rubric and LLM security review)",
    "tier1+2": "Tier 1 with the LLM judge, then Tier 2 deduplication (needs an embeddings model)",
}
INSTALL_HINT = 'uv tool install --python 3.13 "skillevaluator[all] @ git+https://github.com/NVIDIA/SkillEvaluator.git"'
ENV_PREFIXES = ("SKILL_EVAL_", "SKILLEVAL_")
RUN_TIMEOUT_S = 540.0           # per tier
VERSION_TIMEOUT_S = 20.0
VERSION_TTL_S = 300.0           # `--version` is a subprocess: never spawn it on every status poll
KEEP_RUNS = 20                  # per skill; older run directories are deleted after each run
MAX_FILES = 200                 # same cap as the content store's skills
OUTPUT_CAP = 64 * 1024          # bytes kept per stream (the tail)
REPORT_CAP = 8 * 1024 * 1024    # a report larger than this is not parsed
RECORD_CAP = 1024 * 1024
MARKDOWN_CAP = 512 * 1024       # served to the UI
ERROR_TAIL = 1500
TOP_FINDINGS = 12
VERDICTS = ("passed", "failed", "incomplete", "error")
SEVERITIES = ("critical", "high", "medium", "low")
RUN_ID_RE = re.compile(r"\d{8}T\d{9}Z-[0-9a-f]{8}")         # UTC time with milliseconds, then 32 random bits
T1_JSON, T1_MD, T2_JSON, RECORD = "skillevaluator-tier1.json", "skillevaluator-tier1.md", "skillevaluator-tier2.json", "run.json"
SIDECAR = "hub.yaml"            # the hub's own metadata next to SKILL.md: not part of the skill, never evaluated
COPY_MARK = "<skill-copy>"      # replaces the temporary copy's absolute path in stored reports
WORK_MARK = "<work-dir>"
STATE_MARK = "<state-dir>"
_ANSI = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
_RANK = {"passed": 0, "incomplete": 1, "failed": 2}


# ---- the runner port ---------------------------------------------------------------------------------------------------
@dataclass
class EvalResult:
    returncode: int
    stdout: str = ""
    stderr: str = ""
    timed_out: bool = False


class EvalRunner(Protocol):
    def run(self, argv: list[str], *, cwd: Path, env: Mapping[str, str], timeout: float) -> EvalResult: ...

    def cancel(self) -> None: ...


class _Tail:
    """The last `cap` bytes of a stream: memory stays bounded whatever the child prints."""

    def __init__(self, cap: int) -> None:
        self.cap, self.buf, self.dropped = cap, bytearray(), 0

    def feed(self, chunk: bytes) -> None:
        self.buf += chunk
        extra = len(self.buf) - self.cap
        if extra > 0:
            self.dropped += extra
            del self.buf[:extra]

    def text(self) -> str:
        return ("[...] " if self.dropped else "") + self.buf.decode("utf-8", "replace")


def _drain(stream: IO[bytes] | None, tail: _Tail) -> None:
    if stream is None:
        return
    with contextlib.suppress(OSError, ValueError):
        while chunk := stream.read(65536):
            tail.feed(chunk)


def _kill_group(proc: subprocess.Popen[bytes]) -> None:
    if proc.poll() is not None:
        return
    with contextlib.suppress(ProcessLookupError, PermissionError):
        os.killpg(proc.pid, signal.SIGKILL)


class RealEvalRunner:
    """argv list, no shell, the given cwd and env, a hard timeout. The child gets its own session (process group) so a timeout or
    a hub shutdown kills everything it started; stdout/stderr are drained by threads that keep only a bounded tail."""

    def __init__(self, output_cap: int = OUTPUT_CAP) -> None:
        self.output_cap = output_cap
        self._lock = threading.Lock()
        self._proc: subprocess.Popen[bytes] | None = None

    def run(self, argv: list[str], *, cwd: Path, env: Mapping[str, str], timeout: float) -> EvalResult:
        if not argv or not all(isinstance(a, str) and a and "\x00" not in a for a in argv):
            return EvalResult(127, "", "invalid argv")
        try:
            proc = subprocess.Popen(argv, cwd=cwd, env=dict(env), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                    stderr=subprocess.PIPE, start_new_session=True, close_fds=True)
        except FileNotFoundError:
            return EvalResult(127, "", "command not found")
        except OSError as exc:
            return EvalResult(126, "", f"cannot start: {type(exc).__name__}")
        with self._lock:
            self._proc = proc
        out, err = _Tail(self.output_cap), _Tail(self.output_cap)
        readers = [threading.Thread(target=_drain, args=(proc.stdout, out), daemon=True),
                   threading.Thread(target=_drain, args=(proc.stderr, err), daemon=True)]
        for r in readers:
            r.start()
        timed_out = False
        try:
            proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            _kill_group(proc)
            with contextlib.suppress(subprocess.TimeoutExpired):
                proc.wait(timeout=10)
        finally:
            with self._lock:
                self._proc = None
        for r in readers:                 # a grandchild that left the group could hold a pipe open: bounded wait only
            r.join(timeout=5)
        for s in (proc.stdout, proc.stderr):
            with contextlib.suppress(OSError):
                if s is not None:
                    s.close()
        rc = proc.returncode if proc.returncode is not None else -9
        return EvalResult(rc, out.text(), err.text(), timed_out)

    def cancel(self) -> None:
        with self._lock:
            proc = self._proc
        if proc is not None:
            _kill_group(proc)


# ---- reading a skill from the working tree -----------------------------------------------------------------------------
@dataclass
class Snapshot:
    skill: str
    files: dict[str, bytes]
    modes: dict[str, int] = field(default_factory=dict)

    @property
    def content_hash(self) -> str:
        return content_hash(self.files)

    @property
    def total_bytes(self) -> int:
        return sum(len(b) for b in self.files.values())


def content_hash(files: Mapping[str, bytes]) -> str:
    """sha256 over the (relative path, bytes) pairs sorted by path. Both are length-prefixed, so no two different file sets
    can produce the same byte stream."""
    h = hashlib.sha256()
    for rel in sorted(files):
        p, data = rel.encode("utf-8"), files[rel]
        h.update(len(p).to_bytes(8, "big"))
        h.update(p)
        h.update(len(data).to_bytes(8, "big"))
        h.update(data)
    return h.hexdigest()


class _TooBig(Exception):
    pass


def _read_regular(path: Path, limit: int) -> tuple[bytes, int] | None:
    """(bytes, mode) of a REGULAR file opened without following a symlink, or None for anything else (symlink, FIFO, socket,
    device, vanished). Raises _TooBig past `limit` bytes."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    except OSError as exc:
        if exc.errno in (errno.ELOOP, errno.ENOENT, errno.ENXIO, errno.EACCES, errno.ENOTDIR, errno.EISDIR):
            return None
        raise
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode):
            return None
        if st.st_size > limit:
            raise _TooBig
        chunks: list[bytes] = []
        got = 0
        while chunk := os.read(fd, 65536):
            got += len(chunk)
            if got > limit:
                raise _TooBig
            chunks.append(chunk)
        return b"".join(chunks), stat.S_IMODE(st.st_mode)
    finally:
        os.close(fd)


def _walk_skill(skill_dir: Path) -> Iterator[tuple[str, Path]]:
    """(relative posix path, absolute path) of every candidate file. Symlinked directories and `.git` are not entered, the
    top-level hub.yaml sidecar is skipped, and unsafe relative paths are dropped."""
    for dirpath, dirnames, filenames in os.walk(skill_dir, followlinks=False):
        dirnames[:] = sorted(d for d in dirnames if d != ".git" and not os.path.islink(os.path.join(dirpath, d)))
        for name in sorted(filenames):
            full = Path(dirpath) / name
            rel = full.relative_to(skill_dir).as_posix()
            if rel == SIDECAR:
                continue
            try:
                check_relpath(rel)
            except PathError:
                continue
            yield rel, full


def _num(v: object) -> float | None:
    return float(v) if isinstance(v, int | float) and not isinstance(v, bool) else None


def _int(v: object) -> int | None:
    return int(v) if isinstance(v, int | float) and not isinstance(v, bool) else None


def _dict(v: object) -> dict[str, Any]:
    return v if isinstance(v, dict) else {}


def _list(v: object) -> list[Any]:
    return v if isinstance(v, list) else []


def summarize_tier1(d: Mapping[str, Any]) -> dict[str, Any]:
    """The fields the UI shows, from a tier1 JSON report. A missing field stays None; nothing is invented. The judge score
    counts only when the judge actually ran (`rubric_eval.execution_status == "succeeded"`)."""
    q = _list(d.get("quality_summary"))
    q0 = _dict(q[0]) if q else {}
    rub = _dict(d.get("rubric_eval"))
    sev = _dict(d.get("severity_counts"))
    inc = d.get("incomplete_scans")
    status = d.get("overall_status")
    judge_status = rub.get("execution_status")
    grade = q0.get("grade")
    return {
        "status": status if status in ("passed", "failed", "incomplete") else None,
        "quality_score": _num(q0.get("overall_score")),
        "grade": grade[:4] if isinstance(grade, str) else None,
        "judge_status": judge_status[:40] if isinstance(judge_status, str) else None,
        "judge_score": _num(rub.get("overall_score")) if judge_status == "succeeded" else None,
        "errors": _int(d.get("total_errors")),
        "warnings": _int(d.get("total_warnings")),
        "severity": {k: _int(sev.get(k)) for k in SEVERITIES},
        "incomplete": [str(x)[:80] for x in inc][:10] if isinstance(inc, list) else [],
    }


def top_findings(d: Mapping[str, Any], limit: int = TOP_FINDINGS) -> tuple[list[dict[str, str]], int]:
    """(medium-and-above findings, most severe first, at most `limit`; their total count)."""
    order = {"critical": 0, "high": 1, "medium": 2}
    rows: list[tuple[int, int, dict[str, str]]] = []
    for r in _list(d.get("results")):
        r = _dict(r)
        for f in _list(r.get("findings")):
            f = _dict(f)
            sev = str(f.get("severity", "")).lower()
            if sev not in order:
                continue
            msg = str(f.get("message") or f.get("title") or f.get("check") or "")
            rows.append((order[sev], len(rows), {"severity": sev, "validator": str(r.get("validator") or "?")[:100],
                                                 "message": msg[:300]}))
    rows.sort(key=lambda x: (x[0], x[1]))
    return [row for _, _, row in rows[:limit]], len(rows)


def combine_verdicts(tier1: str, tier2: str) -> str:
    """Tier 2 can only make a result worse. A tier1 error stays an error; a tier2 that produced no report counts as incomplete."""
    if tier1 not in _RANK:
        return "error"
    t2 = tier2 if tier2 in _RANK else "incomplete"
    return max((tier1, t2), key=lambda v: _RANK[v])


# ---- the global one-at-a-time lock --------------------------------------------------------------------------------------
class _EvalLock:
    """Thread lock (this process) + flock on a file (other processes, e.g. `hubctl` next to the server)."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self._t = threading.Lock()
        self._fd: int | None = None

    def try_acquire(self) -> bool:
        # A short wait absorbs a concurrent status probe (held_elsewhere); a real evaluation holds the lock far longer.
        if not self._t.acquire(timeout=0.5):
            return False
        try:
            fd = os.open(self.path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        except OSError:
            self._t.release()
            raise
        for attempt in range(10):
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except OSError:
                if attempt == 9:
                    os.close(fd)
                    self._t.release()
                    return False
                time.sleep(0.05)
        self._fd = fd
        return True

    def release(self) -> None:
        fd, self._fd = self._fd, None
        if fd is not None:
            with contextlib.suppress(OSError):
                fcntl.flock(fd, fcntl.LOCK_UN)
            os.close(fd)
        self._t.release()

    def held_elsewhere(self) -> bool:
        """True when another PROCESS holds the file lock (a probe that never keeps it). Taking the thread lock first means a
        probe cannot make an evaluation in this process fail; when this process is evaluating, the answer is False."""
        if not self._t.acquire(blocking=False):
            return False
        try:
            try:
                fd = os.open(self.path, os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC)
            except OSError:
                return False
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except OSError:
                return True
            finally:
                os.close(fd)                 # closing the descriptor drops a lock the probe obtained
            return False
        finally:
            self._t.release()


@dataclass
class _Prepared:
    skill: str
    tiers: str
    run_id: str
    launcher: str
    exe: str
    snap: Snapshot
    run_dir: Path
    started_at: float


def _busy() -> Conflict:
    return Conflict("another skill evaluation is running; wait for it to finish, then try again", code="evaluation_running")


def _mkdir_private(d: Path) -> None:
    """mkdir -p where every directory this call creates is 0700 (Path.mkdir gives parents the default mode)."""
    missing: list[Path] = []
    p = d
    while not p.exists() and p != p.parent:
        missing.append(p)
        p = p.parent
    for m in reversed(missing):
        with contextlib.suppress(FileExistsError):
            m.mkdir(mode=0o700)
    if d.is_symlink() or not d.is_dir():
        raise OSError(errno.ENOTDIR, "not a directory", str(d))
    d.chmod(0o700)


# ---- the service --------------------------------------------------------------------------------------------------------
class SkillEvalService:
    def __init__(self, root: Path, content_dir: Path, *, runner: EvalRunner | None = None,
                 which: Callable[[str, str], str | None] | None = None, environ: Mapping[str, str] | None = None,
                 home: Path | None = None, guard: Callable[[], AbstractContextManager[Any]] | None = None,
                 skill_names: Callable[[], list[str]] | None = None, on_event: Callable[[str, str, Any], None] | None = None,
                 max_files: int = MAX_FILES, max_bytes: int = 4 * 1024 * 1024, keep: int = KEEP_RUNS,
                 timeout: float = RUN_TIMEOUT_S, clock: Callable[[], float] = time.time) -> None:
        self.root = root                                  # <state>/skilleval
        self.content_dir = content_dir
        self.runner: EvalRunner = runner or RealEvalRunner()
        self.which = which or (lambda name, path: shutil.which(name, path=path))
        self.environ = environ if environ is not None else os.environ
        self.home = home or Path.home()
        self.guard = guard or contextlib.nullcontext
        self.skill_names = skill_names or self._skill_dirs
        self.on_event = on_event
        self.max_files, self.max_bytes, self.keep, self.timeout, self.clock = max_files, max_bytes, keep, timeout, clock
        self._lock = _EvalLock(root / ".lock")
        self._mu = threading.Lock()
        self._running: dict[str, Any] | None = None
        self._thread: threading.Thread | None = None
        self._version: tuple[float, str | None, str | None] = (0.0, None, None)    # (when, exe probed, version)
        self._hashes: dict[str, tuple[tuple[Any, ...], str | None]] = {}           # skill -> (stat signature, hash)

    # -- environment and launcher ---------------------------------------------------------------------------------------
    def search_path(self) -> str:
        parts = [p for p in str(self.environ.get("PATH") or "/usr/bin:/bin").split(os.pathsep) if p]
        local_bin = str(self.home / ".local" / "bin")
        if local_bin not in parts:
            parts.append(local_bin)
        return os.pathsep.join(parts)

    def child_env(self) -> dict[str, str]:
        """The hub's minimal subprocess environment plus SKILL_EVAL_* / SKILLEVAL_* (for SkillEvaluator only)."""
        env = {"PATH": self.search_path(), "HOME": str(self.environ.get("HOME") or self.home), "LC_ALL": "C"}
        for k, v in self.environ.items():
            if k.startswith(ENV_PREFIXES) and isinstance(v, str):
                env[k] = v
        return env

    def env_names(self) -> list[str]:
        return sorted(k for k in self.environ if k.startswith(ENV_PREFIXES))

    def find_launcher(self) -> tuple[str, str] | None:
        """(bare name, resolved path) of the first launcher on the search path."""
        path = self.search_path()
        for name in LAUNCHERS:
            exe = self.which(name, path)
            if exe:
                return name, exe
        return None

    def _probe_version(self, exe: str) -> str | None:
        self._ensure_root()
        r = self.runner.run([exe, "--version"], cwd=self.root, env=self.child_env(), timeout=VERSION_TIMEOUT_S)
        m = re.search(r"version\s+([0-9A-Za-z.+_-]{1,40})", r.stdout)
        return m.group(1) if r.returncode == 0 and m else None

    def status(self) -> dict[str, Any]:
        """Launcher and version (the version probe is cached; it re-runs when the launcher changes). No secret values."""
        found = self.find_launcher()
        version = None
        if found is not None:
            ts, exe, ver = self._version
            if exe != found[1] or self.clock() - ts > VERSION_TTL_S:
                ver = self._probe_version(found[1])
                self._version = (self.clock(), found[1], ver)
            version = ver
        return {"installed": found is not None, "launcher": found[0] if found else None, "version": version,
                "wrapper": bool(found and found[0] == "skillevaluator-local"), "env_names": self.env_names(),
                "install_hint": INSTALL_HINT, "tiers": [{"id": t, "help": TIER_HELP[t]} for t in TIERS],
                "keep_runs": self.keep, "timeout_s": self.timeout, "running": self.running()}

    def doctor_info(self) -> dict[str, Any]:
        """For `hubctl doctor`: launcher lookup only (no subprocess); the version only if a status call already probed it."""
        found = self.find_launcher()
        _, exe, ver = self._version
        return {"installed": found is not None, "launcher": found[0] if found else None,
                "version": ver if found and exe == found[1] else None, "env_names": self.env_names()}

    # -- skills in the working tree -----------------------------------------------------------------------------------------
    def _skills_root(self) -> Path:
        return self.content_dir / "skills"

    def _skill_dirs(self) -> list[str]:
        d = self._skills_root()
        if not d.is_dir() or d.is_symlink():
            return []
        return sorted(p.name for p in d.iterdir() if valid_id(p.name) and p.is_dir() and not p.is_symlink())

    def _skill_dir(self, skill: str) -> Path:
        if not valid_id(skill):
            raise BadRequest("invalid skill name", code="invalid_id")
        root = self._skills_root()
        d = root / skill
        try:
            if root.is_symlink() or not stat.S_ISDIR(os.lstat(d).st_mode):
                raise NotFound(f"no such skill: {skill}")
        except OSError as exc:
            raise NotFound(f"no such skill: {skill}") from exc
        if Path(os.path.realpath(d)).parent != Path(os.path.realpath(root)):
            raise NotFound(f"no such skill: {skill}")
        return d

    def snapshot(self, skill: str) -> Snapshot:
        """Exactly the files that will be evaluated: regular files under skills/<skill>/ (no symlinks, no hub.yaml)."""
        base = self._skill_dir(skill)
        snap = Snapshot(skill=skill, files={})
        total = 0
        for rel, full in _walk_skill(base):
            try:
                got = _read_regular(full, self.max_bytes - total)
            except _TooBig as exc:
                raise Unprocessable(f"skill {skill} is larger than {self.max_bytes} bytes; not evaluated",
                                    code="skill_too_large") from exc
            if got is None:
                continue
            if len(snap.files) >= self.max_files:
                raise Unprocessable(f"skill {skill} has more than {self.max_files} files; not evaluated", code="skill_too_large")
            data, mode = got
            total += len(data)
            snap.files[rel], snap.modes[rel] = data, mode
        if "SKILL.md" not in snap.files:
            raise Unprocessable(f"skill {skill} has no SKILL.md", code="skill_invalid")
        return snap

    def current_hash(self, skill: str) -> str | None:
        """The working-tree content hash, cached on a stat signature so listing many skills stays cheap. None when the skill
        cannot be read within the caps (then no evaluation of it can be current)."""
        try:
            base = self._skill_dir(skill)
            sig_rows: list[tuple[Any, ...]] = []
            for rel, full in _walk_skill(base):
                st = os.lstat(full)
                if stat.S_ISREG(st.st_mode):
                    sig_rows.append((rel, st.st_size, st.st_mtime_ns, st.st_ino, st.st_ctime_ns))
            sig = tuple(sig_rows)
        except (OSError, NotFound, BadRequest):
            return None
        cached = self._hashes.get(skill)
        if cached is not None and cached[0] == sig:
            return cached[1]
        try:
            h: str | None = self.snapshot(skill).content_hash
        except (Unprocessable, NotFound, BadRequest, OSError):
            h = None
        self._hashes[skill] = (sig, h)
        return h

    # -- stored runs --------------------------------------------------------------------------------------------------------
    def _ensure_root(self) -> None:
        _mkdir_private(self.root)

    def _skill_runs_dir(self, skill: str) -> Path:
        if not valid_id(skill):
            raise BadRequest("invalid skill name", code="invalid_id")
        return self.root / skill

    def _run_dirs(self, skill: str) -> list[Path]:
        d = self._skill_runs_dir(skill)
        try:
            entries = list(d.iterdir()) if d.is_dir() and not d.is_symlink() else []
        except OSError:
            return []
        dirs = [p for p in entries if RUN_ID_RE.fullmatch(p.name) and p.is_dir() and not p.is_symlink()]
        return sorted(dirs, key=lambda p: p.name, reverse=True)

    def _load_record(self, run_dir: Path) -> dict[str, Any] | None:
        try:
            got = _read_regular(run_dir / RECORD, RECORD_CAP)
        except (_TooBig, OSError):
            return None
        if got is None:
            return None
        try:
            rec = json.loads(got[0])
        except ValueError:
            return None
        if not isinstance(rec, dict) or rec.get("run_id") != run_dir.name:
            return None
        return rec

    def records(self, skill: str, limit: int = KEEP_RUNS) -> list[dict[str, Any]]:
        out: list[dict[str, Any]] = []
        for d in self._run_dirs(skill):
            rec = self._load_record(d)
            if rec is not None:
                out.append(rec)
                if len(out) >= limit:
                    break
        return out

    @staticmethod
    def _freshness(rec: Mapping[str, Any] | None, current: str | None) -> str:
        if rec is None:
            return "none"
        return "current" if current is not None and rec.get("content_hash") == current else "stale"

    def running(self) -> dict[str, Any] | None:
        with self._mu:
            if self._running is not None:
                return dict(self._running)
        if self._lock.held_elsewhere():
            return {"skill": None, "run_id": None, "tiers": None, "started_at": None, "elsewhere": True}
        return None

    def list_latest(self) -> list[dict[str, Any]]:
        """Latest evaluation per skill of the working tree, for library badges. Reads files only; never starts a process."""
        run = self.running()
        out: list[dict[str, Any]] = []
        for skill in self.skill_names():
            if not valid_id(skill):
                continue
            recs = self.records(skill, limit=1)
            latest = recs[0] if recs else None
            out.append({"skill": skill, "state": self._freshness(latest, self.current_hash(skill) if latest else None),
                        "latest": latest, "running": bool(run and run.get("skill") == skill)})
        return out

    def detail(self, skill: str, limit: int = KEEP_RUNS) -> dict[str, Any]:
        self._skill_dir(skill)                                         # 400 / 404 for a bad or unknown skill
        current = self.current_hash(skill)
        history = [{**r, "state": self._freshness(r, current)} for r in self.records(skill, limit)]
        run = self.running()
        return {"skill": skill, "content_hash": current, "state": history[0]["state"] if history else "none",
                "latest": history[0] if history else None, "history": history,
                "running": run if run and (run.get("skill") in (skill, None)) else None}

    def report(self, skill: str, run_id: str) -> dict[str, Any]:
        """The Markdown report of one stored run, secret-shaped text redacted. Path-safe: both ids are checked by fullmatch."""
        if not isinstance(run_id, str) or not RUN_ID_RE.fullmatch(run_id):
            raise BadRequest("invalid run id", code="invalid_id")
        runs = self._skill_runs_dir(skill)
        run_dir = runs / run_id
        if runs.is_symlink() or run_dir.is_symlink() or self._load_record(run_dir) is None:
            raise NotFound("no such evaluation run")
        try:
            got = _read_regular(run_dir / T1_MD, REPORT_CAP)
        except _TooBig:
            return {"skill": skill, "run_id": run_id, "markdown": None, "truncated": True}
        if got is None:
            return {"skill": skill, "run_id": run_id, "markdown": None, "truncated": False}
        text = got[0].decode("utf-8", "replace")
        return {"skill": skill, "run_id": run_id, "markdown": scrub(text[:MARKDOWN_CAP]), "truncated": len(text) > MARKDOWN_CAP}

    # -- evaluation ---------------------------------------------------------------------------------------------------------
    def _new_run_id(self) -> str:
        ts = self.clock()
        return time.strftime("%Y%m%dT%H%M%S", time.gmtime(ts)) + f"{int(ts * 1000) % 1000:03d}Z-" + secrets.token_hex(4)

    def _acquire(self) -> None:
        self._ensure_root()
        if not self._lock.try_acquire():
            raise _busy()

    def _prepare(self, skill: str, tiers: str) -> _Prepared:
        if tiers not in TIERS:
            raise Unprocessable(f"tiers must be one of {', '.join(TIERS)}", code="bad_tiers")
        if not valid_id(skill):
            raise BadRequest("invalid skill name", code="invalid_id")
        found = self.find_launcher()
        if found is None:
            raise Unavailable("SkillEvaluator is not installed (neither skillevaluator-local nor skillevaluator is on the hub's "
                              "PATH or in ~/.local/bin)", code="evaluator_missing", fix=INSTALL_HINT)
        with self.guard():                              # the content lock: a consistent copy, never mid-edit
            snap = self.snapshot(skill)
        run_id = self._new_run_id()
        run_dir = self.root / skill / run_id
        _mkdir_private(run_dir.parent)
        run_dir.mkdir(mode=0o700)
        run_dir.chmod(0o700)
        return _Prepared(skill, tiers, run_id, found[0], found[1], snap, run_dir, self.clock())

    def evaluate(self, skill: str, tiers: str = "static", actor: str = "hub") -> dict[str, Any]:
        """Synchronous (`hubctl`). Raises Conflict 409 `evaluation_running` when another evaluation holds the lock."""
        self._acquire()
        try:
            return self._execute(self._prepare(skill, tiers), actor)
        finally:
            self._lock.release()

    def evaluate_many(self, skills: list[str], tiers: str, actor: str) -> list[dict[str, Any]]:
        """`hubctl evaluate-skills`: one lock for the whole batch, so a UI run cannot slip in between two skills."""
        self._acquire()
        try:
            out: list[dict[str, Any]] = []
            for s in skills:
                try:
                    out.append(self._execute(self._prepare(s, tiers), actor))
                except (BadRequest, NotFound, Unprocessable, Unavailable) as exc:
                    out.append({"skill": s, "tiers": tiers, "verdict": "error", "error": exc.detail, "run_id": None})
            return out
        finally:
            self._lock.release()

    def start(self, skill: str, tiers: str = "static", actor: str = "hub") -> dict[str, Any]:
        """Background run for the API: validation, the launcher check and the copy happen NOW (errors are immediate);
        SkillEvaluator then runs in a thread and the caller polls the skill's evaluations."""
        self._acquire()
        try:
            prep = self._prepare(skill, tiers)
        except BaseException:
            self._lock.release()
            raise
        info = {"skill": prep.skill, "run_id": prep.run_id, "tiers": prep.tiers, "started_at": prep.started_at}
        with self._mu:
            self._running = {**info, "elsewhere": False}

        def work() -> None:
            try:
                self._execute(prep, actor)
            finally:
                self._lock.release()

        t = threading.Thread(target=work, name=f"skilleval-{prep.run_id}", daemon=True)
        self._thread = t
        t.start()
        return {**info, "status": "running"}

    def join(self, timeout: float | None = None) -> None:
        t = self._thread
        if t is not None:
            t.join(timeout)

    def shutdown(self) -> None:
        """Hub stopping: kill a running SkillEvaluator process group, give the worker a moment to record the result."""
        self.runner.cancel()
        self.join(timeout=5)

    def _execute(self, p: _Prepared, actor: str) -> dict[str, Any]:
        work = self.root / ".work" / p.run_id
        rec: dict[str, Any] = {"run_id": p.run_id, "skill": p.skill, "tiers": p.tiers, "launcher": p.launcher,
                               "started_at": p.started_at, "content_hash": p.snap.content_hash,
                               "files": len(p.snap.files), "bytes": p.snap.total_bytes, "actor": actor[:64],
                               "model_calls": p.tiers != "static", "tier1": None, "tier2": None, "findings": [],
                               "findings_total": 0, "error": None}
        with self._mu:
            self._running = {"skill": p.skill, "run_id": p.run_id, "tiers": p.tiers, "started_at": p.started_at,
                             "elsewhere": False}
        t0 = time.monotonic()
        copy = work / p.skill
        try:
            self._clean_work()
            _mkdir_private(work)
            self._write_copy(copy, p.snap)
            env = self.child_env()
            argv = [p.exe, "tier1", str(copy), *(["--no-llm"] if p.tiers == "static" else []),
                    "-r", "json,markdown", "-o", str(p.run_dir)]
            r1 = self.runner.run(argv, cwd=work, env=env, timeout=self.timeout)
            t1 = self._load_report(p.run_dir / T1_JSON)
            if t1 is None:
                verdict = "error"
                rec["error"] = self._error_text(r1, copy)
            else:
                rec["tier1"] = self._clean(summarize_tier1(t1), copy)
                findings, total = top_findings(t1)
                rec["findings"], rec["findings_total"] = self._clean(findings, copy), total
                verdict = rec["tier1"]["status"] or "error"
                if verdict == "error":
                    rec["error"] = "the tier1 report has no overall_status"
            if p.tiers == "tier1+2":
                r2 = self.runner.run([p.exe, "tier2", str(copy), "-r", "json", "-o", str(p.run_dir)], cwd=work, env=env,
                                     timeout=self.timeout)
                t2 = self._load_report(p.run_dir / T2_JSON)
                st2 = t2.get("overall_status") if t2 is not None else None
                st2 = st2 if st2 in ("passed", "failed", "incomplete") else "error"
                rec["tier2"] = {"status": st2, "errors": _int(t2.get("total_errors")) if t2 else None,
                                "warnings": _int(t2.get("total_warnings")) if t2 else None,
                                "error": self._error_text(r2, copy) if st2 == "error" else None}
                verdict = combine_verdicts(verdict, st2)
            rec["verdict"] = verdict
        except Exception as exc:  # noqa: BLE001 - recorded as an error run; the hub keeps serving
            rec["verdict"], rec["error"] = "error", f"the evaluation could not run ({type(exc).__name__})"
        finally:
            shutil.rmtree(work, ignore_errors=True)
            rec["duration_ms"] = int((time.monotonic() - t0) * 1000)
            rec["finished_at"] = self.clock()
            self._finalize_reports(p.run_dir, copy)
            rec["reports"] = sorted(n for n in (T1_JSON, T1_MD, T2_JSON) if (p.run_dir / n).is_file())
            self._write_record(p.run_dir, rec)
            self._prune(p.skill)
            self._hashes.pop(p.skill, None)
            with self._mu:
                self._running = None
        if self.on_event is not None:
            with contextlib.suppress(Exception):
                self.on_event(actor, "skill.evaluate", {"skill": p.skill, "run_id": p.run_id, "tiers": p.tiers,
                                                       "verdict": rec["verdict"], "launcher": p.launcher,
                                                       "content_hash": p.snap.content_hash[:12]})
        return rec

    # -- helpers ------------------------------------------------------------------------------------------------------------
    def _clean_work(self) -> None:
        """Leftovers of an interrupted run (the hub was killed mid-run). Only called while holding the global lock."""
        w = self.root / ".work"
        if w.is_dir() and not w.is_symlink():
            for d in w.iterdir():
                if d.is_symlink():
                    d.unlink()
                else:
                    shutil.rmtree(d, ignore_errors=True)

    @staticmethod
    def _write_copy(dest: Path, snap: Snapshot) -> None:
        dest.mkdir(mode=0o700)
        for rel, data in sorted(snap.files.items()):
            target = dest / rel
            if target.parent != dest:
                _mkdir_private(target.parent)
            mode = 0o700 if snap.modes.get(rel, 0) & 0o111 else 0o600          # keep "is a script" for the script checks
            fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, mode)
            with os.fdopen(fd, "wb") as fh:
                fh.write(data)

    def _scrub_paths(self, text: str, copy: Path) -> str:
        """The temporary copy's path (gone after the run), its work dir and the state dir, replaced by markers; whole path
        prefixes only."""
        work = copy.parent
        for p, token in ((str(copy), COPY_MARK), (os.path.realpath(copy), COPY_MARK), (str(work), WORK_MARK),
                         (os.path.realpath(work), WORK_MARK), (str(self.root.parent), STATE_MARK),
                         (os.path.realpath(self.root.parent), STATE_MARK)):
            if len(p) > 1 and p in text:
                text = re.sub(re.escape(p) + r"(?![\w.-])", token, text)      # tokens hold no backslash or group ref
        return text

    def _clean(self, obj: Any, copy: Path) -> Any:
        """Strings from a report: the temp-copy and state paths replaced, secret-shaped values redacted."""
        if isinstance(obj, str):
            return scrub(self._scrub_paths(obj, copy))
        if isinstance(obj, list):
            return [self._clean(x, copy) for x in obj]
        if isinstance(obj, dict):
            return {k: self._clean(v, copy) for k, v in obj.items()}
        return obj

    def _error_text(self, r: EvalResult, copy: Path) -> str:
        if r.timed_out:
            return f"SkillEvaluator timed out after {int(self.timeout)} s"
        text = _ANSI.sub("", (r.stderr or "").strip() or (r.stdout or "").strip())
        text = scrub(self._scrub_paths(text, copy))
        tail = text[-ERROR_TAIL:] if len(text) > ERROR_TAIL else text
        return f"exit {r.returncode}, no report" + (f": {tail}" if tail else "")

    @staticmethod
    def _load_report(path: Path) -> dict[str, Any] | None:
        try:
            got = _read_regular(path, REPORT_CAP)
        except (_TooBig, OSError):
            return None
        if got is None:
            return None
        try:
            d = json.loads(got[0])
        except (ValueError, RecursionError):
            return None
        return d if isinstance(d, dict) else None

    def _finalize_reports(self, run_dir: Path, copy: Path) -> None:
        """Make what SkillEvaluator wrote private (0600 / 0700), drop symlinks it may have left, and replace the temporary
        copy's absolute path in text reports (it is gone, and it would reveal the state-dir layout)."""
        try:
            entries = list(run_dir.iterdir())
        except OSError:
            return
        for e in entries:
            try:
                st = os.lstat(e)
                if stat.S_ISLNK(st.st_mode):
                    e.unlink()
                elif stat.S_ISDIR(st.st_mode):
                    for dirpath, dirnames, filenames in os.walk(e, followlinks=False):
                        os.chmod(dirpath, 0o700)
                        for n in [*dirnames, *filenames]:
                            if os.path.islink(os.path.join(dirpath, n)):
                                os.unlink(os.path.join(dirpath, n))
                        for n in filenames:
                            with contextlib.suppress(OSError):
                                os.chmod(os.path.join(dirpath, n), 0o600)
                elif stat.S_ISREG(st.st_mode):
                    if e.suffix in (".json", ".md"):
                        got = _read_regular(e, REPORT_CAP)
                        if got is not None:
                            text = got[0].decode("utf-8", "replace")
                            new = self._scrub_paths(text, copy)
                            if new != text:
                                tmp = run_dir / f".{e.name}.tmp"
                                fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
                                with os.fdopen(fd, "wb") as fh:
                                    fh.write(new.encode("utf-8"))
                                os.replace(tmp, e)
                    os.chmod(e, 0o600)
                else:
                    e.unlink()                         # FIFOs, sockets: nothing a report needs
            except (OSError, _TooBig):
                continue

    @staticmethod
    def _write_record(run_dir: Path, rec: Mapping[str, Any]) -> None:
        tmp = run_dir / f".{RECORD}.tmp"
        with contextlib.suppress(FileNotFoundError):
            tmp.unlink()
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        with os.fdopen(fd, "w") as fh:
            json.dump(rec, fh, indent=1, sort_keys=True, default=str)
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, run_dir / RECORD)

    def _prune(self, skill: str) -> None:
        """Keep the newest `keep` run directories of this skill (including unfinished ones, so a crash loop cannot grow it)."""
        for d in self._run_dirs(skill)[self.keep:]:
            shutil.rmtree(d, ignore_errors=True)
