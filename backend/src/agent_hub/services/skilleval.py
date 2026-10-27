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

Addendum F (observed skills, queue, gate) and F14 (security of the evaluator invocation):
  * Subjects: `hub:<skill>` (content working tree) or `<client>:<key>` (a snapshot stored by services/observe.py).
  * EVERY tier1 argv carries `--checks SAFE_CHECKS`: SkillEvaluator's default check set includes `dependency`, which runs pip-audit
    on the skill's requirements files, i.e. installs packages an (agent-written) skill chose. It is never selected.
  * EVERY evaluation runs a STATIC pass first (`--no-llm`, output in <run>/static/); tiers tier1/tier1+2 add a second pass with the
    LLM judge (output in <run>/). The LLM verifier can downgrade security findings, and skill text can steer it, so the gate
    (`gate()`) uses the static pass plus only the HIGH/CRITICAL security findings of the LLM pass: the LLM pass can only make the
    gate stricter, never clear a static finding.
  * One FIFO worker thread drains an in-memory queue; the same flock is held per job. Finished runs are indexed by content hash
    (<root>/@by-hash/<hash>.json) so an identical skill is never evaluated twice (`reused_from`) unless forced.
  * Record revision 2 (EVAL_REV): a static Security Scan that is `incomplete` only because SkillSpector called its analysis
    `partial` gets one more contained SkillSpector run on the same copy to read why; `unresolved_references` alone is stored in
    `scan_partial` and the gate warns instead of blocking. Older records with an incomplete Security Scan are not reused.
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
from collections import deque
from collections.abc import Callable, Iterator, Mapping
from contextlib import AbstractContextManager
from dataclasses import dataclass, field
from pathlib import Path
from typing import IO, Any, Protocol

from agent_hub.domain.errors import BadRequest, Conflict, NotFound, ProblemError, Unavailable, Unprocessable
from agent_hub.domain.ids import valid_id
from agent_hub.domain.pathsafe import PathError, check_relpath
from agent_hub.services.common import atomic_write, scrub

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
# Why a static Security Scan came back `incomplete` (record revision 2). SkillSpector (the scanner behind "Security Scan")
# reports `analysis_completeness.status: partial` whenever a SKILL.md mentions a local path the skill does not bundle (for
# example a repository script), even when every bundled file was fully analysed; SkillEvaluator 0.5.0 then marks the scan
# incomplete, and the gate blocked such skills as "did not finish" (2026-10-15: 94 of 161 blocks). For that one pattern the
# hub reads SkillSpector's own static report and records `unresolved_references`, which the gate shows as a warning. Any
# other partial reason (a file not fully inspected, excluded executable content, a parse limit, a fatal ledger entry, a
# degraded analyzer, a limitation) still blocks, as does anything the hub cannot read.
EVAL_REV = 2
SKILLSPECTOR = "skillspector"
SKILLSPECTOR_TIMEOUT_S = 300.0     # SkillEvaluator's own limit for the same scan
SKILLSPECTOR_REPORT = "skillspector.json"
PARTIAL_ERROR = "skillspector JSON field 'analysis_completeness' reports incomplete analysis (status 'partial')"
UNRESOLVED_REFERENCES = "unresolved_references"
FINDING_LINE = re.compile(r"\[SECURITY-(CRITICAL|HIGH|MEDIUM|LOW|INFO)\] ")
SIDECAR = "hub.yaml"            # the hub's own metadata next to SKILL.md: not part of the skill, never evaluated
COPY_MARK = "<skill-copy>"      # replaces the temporary copy's absolute path in stored reports
WORK_MARK = "<work-dir>"
STATE_MARK = "<state-dir>"
_ANSI = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
_RANK = {"passed": 0, "incomplete": 1, "failed": 2}
TIER_RANK = {"static": 0, "tier1": 1, "tier1+2": 2}
# F14: the checks every tier1 run selects. Deliberately NOT the tool's default set, which adds `dependency` (pip-audit installs packages).
SAFE_CHECKS = "schema,version,security,pii,license,code-integrity,unicode,quality,lint"
# F7: validators that judge publication/quality, not safety. Every OTHER validator (including unknown ones) is a security validator.
NON_SECURITY_VALIDATORS = frozenset({"Schema & Repository Governance", "Semantic Version Validation", "License Compliance",
                                     "QUALITY", "SCRIPT_LINT", "RUBRIC_EVAL"})
# F7/M1: the static pass must report all of these; an absent one means the scan did not run, which must not read as "clean".
EXPECTED_SECURITY_VALIDATORS = ("Security Scan", "PII Scan", "Code Risk Analysis", "Secrets Detection",
                                "Code Integrity & Hygiene", "Unicode Smuggling Detection")
GATE_STATUSES = ("missing", "blocked", "warn", "ok")
STATIC_DIR = "static"
BY_HASH = "@by-hash"
OBSERVED_DIR = "@observed"
SUBJECT_RE = re.compile(r"([a-z0-9][a-z0-9._-]{0,63}):([a-z0-9][a-z0-9._-]{0,80})")
KEY_RE = re.compile(r"[a-z0-9][a-z0-9._-]{0,80}")
HASH_RE = re.compile(r"[0-9a-f]{64}")
QUEUE_CAP = 5000
QUEUE_SHOWN = 200
INDEX_CAP = 32 * 1024 * 1024


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
        self._procs: set[subprocess.Popen[bytes]] = set()      # every live child: a version probe must not hide a running evaluation

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
            self._procs.add(proc)
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
                self._procs.discard(proc)
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
            procs = list(self._procs)
        for proc in procs:
            _kill_group(proc)


# ---- reading a skill from the working tree -----------------------------------------------------------------------------
@dataclass
class Snapshot:
    skill: str
    files: dict[str, bytes]
    modes: dict[str, int] = field(default_factory=dict)
    subject: str = ""
    path: str = ""                  # where the skill lives (hub: its name; observed: the path under the client's skills root)
    origin: str = "hub"

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


def _walk_skill(skill_dir: Path, skip_sidecar: bool = True) -> Iterator[tuple[str, Path]]:
    """(relative posix path, absolute path) of every candidate file. Symlinked directories and `.git` are not entered, the
    top-level hub.yaml sidecar is skipped (not for an observed skill: it is the client's file), and unsafe relative paths are dropped."""
    for dirpath, dirnames, filenames in os.walk(skill_dir, followlinks=False):
        dirnames[:] = sorted(d for d in dirnames if d != ".git" and not os.path.islink(os.path.join(dirpath, d)))
        for name in sorted(filenames):
            full = Path(dirpath) / name
            rel = full.relative_to(skill_dir).as_posix()
            if skip_sidecar and rel == SIDECAR:
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


def is_security_validator(name: str) -> bool:
    return name not in NON_SECURITY_VALIDATORS


def classify_partial_scan(report: Mapping[str, Any]) -> dict[str, Any] | None:
    """SkillSpector's static JSON report -> {reason: unresolved_references, references, ...} when its analysis is `partial`
    ONLY because of local references the skill does not bundle; None for anything else (the caller keeps blocking)."""
    ac = _dict(report.get("analysis_completeness"))
    meta = _dict(report.get("metadata"))
    exceptions = ac.get("ledger_exceptions")
    analyzers = ac.get("analyzer_statuses")
    counts = [ac.get(k) for k in ("total_components", "scanned_components", "fully_inspected_files",
                                  "partially_inspected_files", "entirely_uninspected_files")]
    if not (report.get("execution_successful") is True and ac.get("execution_successful") is True
            and ac.get("status") == "partial" and ac.get("is_complete") is False
            and all(isinstance(c, int) and not isinstance(c, bool) for c in counts)
            and counts[0] == counts[1] == counts[2] and counts[3] == 0 and counts[4] == 0
            and ac.get("coverage_percent") in (100, 100.0) and ac.get("limitations") == []
            and isinstance(exceptions, list) and exceptions and isinstance(analyzers, list) and analyzers):
        return None
    refs: list[dict[str, Any]] = []
    for e in exceptions:
        e = _dict(e)
        if (e.get("fatal") is not False or e.get("phase") != "reference_resolution" or e.get("reason_code") != "reference_missing"
                or e.get("outcome") != "partial"):
            return None
        refs.append({"path": str(e.get("path") or "?")[:200], "line": _int(e.get("start_line"))})
    # A static (`--no-llm`) scan disables SkillSpector's LLM-backed analyzers; it names them in llm_provenance. Only those may
    # be `disabled`, and only by configuration with no LLM requested. Anything else (`degraded`, `failed`, ...) is a real gap.
    llm_ids = {str(_dict(x).get("analyzer_id")) for x in _list(_dict(meta.get("llm_provenance")).get("analyzers"))}
    for a in analyzers:
        a = _dict(a)
        status = a.get("status")
        llm_off = (status == "disabled" and a.get("reason_code") == "disabled_by_configuration"
                   and meta.get("llm_requested") is False and str(a.get("analyzer_id")) in llm_ids)
        if status not in ("completed", "not_applicable") and not llm_off:
            return None
    version = meta.get("skillspector_version")
    return {"reason": UNRESOLVED_REFERENCES, "count": len(refs), "references": refs[:20],
            "skillspector_version": version[:20] if isinstance(version, str) else None}


def validator_summaries(d: Mapping[str, Any]) -> list[dict[str, Any]]:
    """Per validator {validator, status, severity counts}: what the gate needs, so it never re-reads a big report."""
    out: list[dict[str, Any]] = []
    for r in _list(d.get("results"))[:64]:
        r = _dict(r)
        counts = dict.fromkeys(SEVERITIES, 0)
        for f in _list(r.get("findings")):
            sev = str(_dict(f).get("severity", "")).lower()
            if sev in counts:
                counts[sev] += 1
        st = r.get("status")
        out.append({"validator": str(r.get("validator") or "?")[:100], "status": st[:20] if isinstance(st, str) else "?",
                    "severity": counts})
    return out


def security_findings(d: Mapping[str, Any], which: str, limit: int = 30) -> list[dict[str, str]]:
    """critical/high/medium findings of SECURITY validators (most severe first), kept in the run record for gate reasons."""
    order = {"critical": 0, "high": 1, "medium": 2}
    rows: list[tuple[int, int, dict[str, str]]] = []
    for r in _list(d.get("results")):
        r = _dict(r)
        name = str(r.get("validator") or "?")[:100]
        if not is_security_validator(name):
            continue
        for f in _list(r.get("findings")):
            f = _dict(f)
            sev = str(f.get("severity", "")).lower()
            if sev in order:
                msg = str(f.get("message") or f.get("title") or f.get("check") or "")[:200]
                rows.append((order[sev], len(rows), {"pass": which, "validator": name, "severity": sev, "message": msg}))
    rows.sort(key=lambda x: (x[0], x[1]))
    return [row for _, _, row in rows[:limit]]


def security_severity(rec: Mapping[str, Any]) -> dict[str, int] | None:
    """critical/high counts over SECURITY validators of a run record: the static pass plus the LLM pass (per validator the larger of
    the two, so one finding seen by both passes is counted once). None for records without per-validator summaries."""
    if not isinstance(rec.get("validators"), list):
        return None
    best: dict[str, dict[str, int]] = {}
    for key in ("validators", "validators_llm"):
        for v in _list(rec.get(key)):
            v = _dict(v)
            name = str(v.get("validator") or "?")
            if not is_security_validator(name):
                continue
            sev = _dict(v.get("severity"))
            cur = best.setdefault(name, {"critical": 0, "high": 0})
            for k in cur:
                cur[k] = max(cur[k], _int(sev.get(k)) or 0)
    return {"critical": sum(b["critical"] for b in best.values()), "high": sum(b["high"] for b in best.values())}


def parse_subject(subject: str) -> tuple[str, str]:
    """('hub', skill) or (client, key). fullmatch only; a hub skill must also be a valid content id."""
    m = SUBJECT_RE.fullmatch(subject) if isinstance(subject, str) else None
    if m is None:
        raise BadRequest("invalid subject: expected hub:<skill> or <client>:<key>", code="invalid_subject")
    src, key = m.group(1), m.group(2)
    if src == "hub" and not valid_id(key):
        raise BadRequest("invalid skill name", code="invalid_subject")
    return src, key


def normalize_subject(value: str) -> str:
    """A bare skill name means hub:<name> (the old routes); anything with a colon is a full subject."""
    if isinstance(value, str) and ":" in value:
        src, key = parse_subject(value)
        return f"{src}:{key}"
    if not valid_id(value):
        raise BadRequest("invalid skill name", code="invalid_id")
    return f"hub:{value}"


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
class Job:
    subject: str
    tiers: str
    force: bool
    actor: str
    run_id: str
    cancelled: bool = False


@dataclass
class _Prepared:
    subject: str
    source: str
    name: str                       # the skill name / observed key: the directory name of the private copy
    tiers: str
    run_id: str
    launcher: str
    exe: str
    snap: Snapshot
    run_dir: Path
    started_at: float
    version: str | None
    job: Job | None = None


class _Cancelled(Exception):
    pass


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
                 timeout: float = RUN_TIMEOUT_S, clock: Callable[[], float] = time.time,
                 observed_root: Path | None = None, gate_min_tiers: Callable[[], str] | None = None) -> None:
        self.root = root                                  # <state>/skilleval
        self.content_dir = content_dir
        self.observed_root = observed_root                # <state>/observed (services/observe.py writes it; this only reads)
        self.gate_min_tiers: Callable[[], str] = gate_min_tiers or (lambda: "static")
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
        self._cv = threading.Condition(self._mu)
        self._running: dict[str, Any] | None = None
        self._queue: deque[Job] = deque()
        self._queued: dict[str, Job] = {}
        self._current: Job | None = None
        self._worker: threading.Thread | None = None
        self._stop = False
        self._recovered = False
        self._finished = 0
        self._errors = 0
        self._last_errors: deque[dict[str, str]] = deque(maxlen=20)
        self._version: tuple[float, str | None, str | None] = (0.0, None, None)    # (when, exe probed, version)
        self._hashes: dict[str, tuple[tuple[Any, ...], str | None]] = {}           # skill -> (stat signature, hash)
        self._index_cache: dict[str, tuple[tuple[int, ...], dict[str, Any], dict[str, dict[str, Any]]]] = {}

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

    def _version_for(self, found: tuple[str, str]) -> str | None:
        ts, exe, ver = self._version
        if exe != found[1] or self.clock() - ts > VERSION_TTL_S:
            ver = self._probe_version(found[1])
            self._version = (self.clock(), found[1], ver)
        return ver

    def status(self) -> dict[str, Any]:
        """Launcher and version (the version probe is cached; it re-runs when the launcher changes). No secret values."""
        found = self.find_launcher()
        version = self._version_for(found) if found is not None else None
        return {"installed": found is not None, "launcher": found[0] if found else None, "version": version,
                "wrapper": bool(found and found[0] == "skillevaluator-local"), "env_names": self.env_names(),
                "install_hint": INSTALL_HINT, "tiers": [{"id": t, "help": TIER_HELP[t]} for t in TIERS],
                "keep_runs": self.keep, "timeout_s": self.timeout, "running": self.running(),
                "checks": SAFE_CHECKS.split(",")}

    def doctor_info(self) -> dict[str, Any]:
        """For `hubctl doctor`: launcher lookup only (no subprocess); the version only if a status call already probed it."""
        found = self.find_launcher()
        _, exe, ver = self._version
        return {"installed": found is not None, "launcher": found[0] if found else None,
                "version": ver if found and exe == found[1] else None, "env_names": self.env_names()}

    # -- subjects: hub skills ---------------------------------------------------------------------------------------------
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

    def _read_tree(self, base: Path, label: str, snap: Snapshot, skip_sidecar: bool) -> Snapshot:
        total = 0
        for rel, full in _walk_skill(base, skip_sidecar):
            try:
                got = _read_regular(full, self.max_bytes - total)
            except _TooBig as exc:
                raise Unprocessable(f"skill {label} is larger than {self.max_bytes} bytes; not evaluated",
                                    code="skill_too_large") from exc
            if got is None:
                continue
            if len(snap.files) >= self.max_files:
                raise Unprocessable(f"skill {label} has more than {self.max_files} files; not evaluated", code="skill_too_large")
            data, mode = got
            total += len(data)
            snap.files[rel], snap.modes[rel] = data, mode
        if "SKILL.md" not in snap.files:
            raise Unprocessable(f"skill {label} has no SKILL.md", code="skill_invalid")
        return snap

    def snapshot(self, subject: str) -> Snapshot:
        """Exactly the files that will be evaluated. Hub: regular files under skills/<skill>/ (no symlinks, no hub.yaml).
        Observed: the stored snapshot under <state>/observed/<client>/skills/<key>/ (everything the client had)."""
        subject = normalize_subject(subject)
        src, key = parse_subject(subject)
        if src == "hub":
            base = self._skill_dir(key)
            return self._read_tree(base, key, Snapshot(skill=key, files={}, subject=subject, path=key, origin="hub"), True)
        ent = self.observed_entry(src, key)
        if ent is None or self.observed_root is None:
            raise NotFound(f"no such observed skill: {subject}")
        base = self.observed_root / src / "skills" / key
        try:
            if not stat.S_ISDIR(os.lstat(base).st_mode):
                raise NotFound(f"no such observed skill: {subject}")
        except OSError as exc:
            raise NotFound(f"no such observed skill: {subject}") from exc
        return self._read_tree(base, subject, Snapshot(skill=key, files={}, subject=subject, path=str(ent.get("path") or key),
                                                       origin=str(ent.get("origin") or "client")), False)

    def current_hash(self, skill: str) -> str | None:
        """The content hash of the subject now (a bare name = hub skill). Hub: cached on a stat signature so listing many skills
        stays cheap; None when the skill cannot be read within the caps (then no evaluation of it can be current).
        Observed: the hash stored by the observation."""
        subject = normalize_subject(skill)
        src, key = parse_subject(subject)
        if src != "hub":
            ent = self.observed_entry(src, key)
            h = ent.get("content_hash") if ent else None
            return h if isinstance(h, str) and HASH_RE.fullmatch(h) else None
        try:
            base = self._skill_dir(key)
            sig_rows: list[tuple[Any, ...]] = []
            for rel, full in _walk_skill(base):
                st = os.lstat(full)
                if stat.S_ISREG(st.st_mode):
                    sig_rows.append((rel, st.st_size, st.st_mtime_ns, st.st_ino, st.st_ctime_ns))
            sig = tuple(sig_rows)
        except (OSError, NotFound, BadRequest):
            return None
        cached = self._hashes.get(key)
        if cached is not None and cached[0] == sig:
            return cached[1]
        try:
            h2: str | None = self.snapshot(subject).content_hash
        except (Unprocessable, NotFound, BadRequest, OSError):
            h2 = None
        self._hashes[key] = (sig, h2)
        return h2

    # -- subjects: observed skills (read-only view of services/observe.py's store) ---------------------------------------------
    def _index_map(self, client: str) -> tuple[dict[str, Any], dict[str, dict[str, Any]]] | None:
        if self.observed_root is None or not valid_id(client):
            return None
        path = self.observed_root / client / "index.json"
        try:
            st = os.lstat(path)
        except OSError:
            return None
        sig = (st.st_mtime_ns, st.st_size, st.st_ino)
        cached = self._index_cache.get(client)
        if cached is not None and cached[0] == sig:
            return cached[1], cached[2]
        try:
            got = _read_regular(path, INDEX_CAP)
            idx = json.loads(got[0]) if got else None
        except (_TooBig, OSError, ValueError, RecursionError):
            return None
        if not isinstance(idx, dict):
            return None
        by_key = {s["key"]: s for s in _list(idx.get("skills")) if isinstance(s, dict) and isinstance(s.get("key"), str)}
        self._index_cache[client] = (sig, idx, by_key)
        return idx, by_key

    def observed_entry(self, client: str, key: str) -> dict[str, Any] | None:
        if not KEY_RE.fullmatch(key):
            return None
        got = self._index_map(client)
        return got[1].get(key) if got else None

    def prune_observed(self, client: str, keep_keys: set[str]) -> None:
        """Drop stored runs of observed skills that are no longer in the client's current observation (never the running one)."""
        if not valid_id(client):
            return
        base = self.root / OBSERVED_DIR / client
        cur = self._current.subject if self._current is not None else None
        with contextlib.suppress(OSError):
            for d in base.iterdir():
                if (KEY_RE.fullmatch(d.name) and d.name not in keep_keys and d.is_dir() and not d.is_symlink()
                        and cur != f"{client}:{d.name}"):
                    shutil.rmtree(d, ignore_errors=True)

    def subject_exists(self, subject: str) -> bool:
        src, key = parse_subject(subject)
        if src == "hub":
            try:
                self._skill_dir(key)
            except (NotFound, BadRequest):
                return False
            return True
        return self.observed_entry(src, key) is not None

    # -- stored runs --------------------------------------------------------------------------------------------------------
    def _ensure_root(self) -> None:
        _mkdir_private(self.root)

    def _runs_dir(self, subject: str) -> Path:
        src, key = parse_subject(subject)
        if src == "hub":
            return self.root / key
        return self.root / OBSERVED_DIR / src / key

    def _run_dirs(self, subject: str) -> list[Path]:
        d = self._runs_dir(subject)
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
        for d in self._run_dirs(normalize_subject(skill)):
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
            cur = self._current
            if cur is not None:
                return {"subject": cur.subject, "skill": parse_subject(cur.subject)[1], "run_id": cur.run_id, "tiers": cur.tiers,
                        "started_at": None, "elsewhere": False}
        if self._lock.held_elsewhere():
            return {"subject": None, "skill": None, "run_id": None, "tiers": None, "started_at": None, "elsewhere": True}
        return None

    def queued_subjects(self) -> set[str]:
        with self._mu:
            return set(self._queued)

    def list_latest(self) -> list[dict[str, Any]]:
        """Latest evaluation per skill of the working tree, for library badges. Reads files only; never starts a process."""
        run = self.running()
        out: list[dict[str, Any]] = []
        for skill in self.skill_names():
            if not valid_id(skill):
                continue
            subject = f"hub:{skill}"
            recs = self.records(subject, limit=1)
            latest = recs[0] if recs else None
            out.append({"skill": skill, "subject": subject,
                        "state": self._freshness(latest, self.current_hash(subject) if latest else None),
                        "latest": latest, "running": bool(run and run.get("subject") == subject)})
        return out

    def detail(self, skill: str, limit: int = KEEP_RUNS) -> dict[str, Any]:
        subject = normalize_subject(skill)
        src, key = parse_subject(subject)
        if not self.subject_exists(subject):
            raise NotFound(f"no such skill: {subject}")
        current = self.current_hash(subject)
        history = [{**r, "state": self._freshness(r, current)} for r in self.records(subject, limit)]
        run = self.running()
        return {"skill": key, "subject": subject, "source": src, "content_hash": current,
                "state": history[0]["state"] if history else "none", "latest": history[0] if history else None,
                "history": history, "running": run if run and (run.get("subject") in (subject, None)) else None,
                "gate": self.gate(current) if current else None}

    def report(self, skill: str, run_id: str) -> dict[str, Any]:
        """The Markdown report of one stored run, secret-shaped text redacted. Path-safe: both ids are checked by fullmatch.
        A reused run shows the report of the run it was copied from."""
        subject = normalize_subject(skill)
        key = parse_subject(subject)[1]
        if not isinstance(run_id, str) or not RUN_ID_RE.fullmatch(run_id):
            raise BadRequest("invalid run id", code="invalid_id")
        runs = self._runs_dir(subject)
        run_dir = runs / run_id
        rec = None if runs.is_symlink() or run_dir.is_symlink() else self._load_record(run_dir)
        if rec is None:
            raise NotFound("no such evaluation run")
        base = {"skill": key, "subject": subject, "run_id": run_id}
        src_dir = run_dir
        ref = rec.get("reused_from")
        if isinstance(ref, dict) and isinstance(ref.get("subject"), str) and isinstance(ref.get("run_id"), str):
            try:
                cand = self._runs_dir(normalize_subject(ref["subject"])) / ref["run_id"]
            except BadRequest:
                cand = run_dir
            if RUN_ID_RE.fullmatch(ref["run_id"]) and not cand.is_symlink() and self._load_record(cand) is not None:
                src_dir = cand
        for md in (src_dir / T1_MD, src_dir / STATIC_DIR / T1_MD):
            try:
                got = _read_regular(md, REPORT_CAP)
            except _TooBig:
                return {**base, "markdown": None, "truncated": True}
            if got is not None:
                text = got[0].decode("utf-8", "replace")
                return {**base, "markdown": scrub(text[:MARKDOWN_CAP]), "truncated": len(text) > MARKDOWN_CAP}
        return {**base, "markdown": None, "truncated": False}

    # -- the by-hash index, reuse and the gate -----------------------------------------------------------------------------------
    def _pointer_path(self, h: str) -> Path:
        return self.root / BY_HASH / f"{h}.json"

    def _best_run(self, h: str) -> tuple[dict[str, Any], dict[str, Any]] | None:
        """(pointer, run record) of the best finished run of this content: highest tiers, then newest. None when absent."""
        if not isinstance(h, str) or not HASH_RE.fullmatch(h):
            return None
        try:
            got = _read_regular(self._pointer_path(h), 64 * 1024)
            ptr = json.loads(got[0]) if got else None
        except (_TooBig, OSError, ValueError):
            return None
        if not isinstance(ptr, dict) or not isinstance(ptr.get("subject"), str) or not isinstance(ptr.get("run_id"), str):
            return None
        if not RUN_ID_RE.fullmatch(ptr["run_id"]):
            return None
        try:
            d = self._runs_dir(normalize_subject(ptr["subject"])) / ptr["run_id"]
        except BadRequest:
            return None
        rec = None if d.is_symlink() else self._load_record(d)
        if rec is None or rec.get("content_hash") != h:
            return None
        return ptr, rec

    def _update_pointer(self, rec: Mapping[str, Any]) -> None:
        h, tiers = rec.get("content_hash"), rec.get("tiers")
        if not isinstance(h, str) or not HASH_RE.fullmatch(h) or tiers not in TIER_RANK or rec.get("verdict") == "error":
            return
        old = self._best_run(h)
        if old is not None and TIER_RANK.get(str(old[1].get("tiers")), -1) > TIER_RANK[tiers]:
            return
        ptr = {"subject": rec["subject"], "run_id": rec["run_id"], "tiers": tiers, "verdict": rec["verdict"],
               "evaluator_version": rec.get("evaluator_version"), "finished_at": rec.get("finished_at")}
        with contextlib.suppress(OSError):
            atomic_write(self._pointer_path(h), json.dumps(ptr, sort_keys=True).encode(), 0o600, dir_mode=0o700)

    def _reusable(self, p: _Prepared) -> tuple[dict[str, Any], dict[str, Any]] | None:
        best = self._best_run(p.snap.content_hash)
        if best is None:
            return None
        ptr, rec = best
        if (TIER_RANK.get(str(rec.get("tiers")), -1) < TIER_RANK[p.tiers] or rec.get("verdict") in (None, "error")
                or "validators" not in rec or ptr.get("evaluator_version") != p.version):
            return None
        if (_int(rec.get("eval_rev")) or 1) < EVAL_REV and any(
                _dict(v).get("validator") == "Security Scan" and _dict(v).get("status") == "incomplete"
                for v in _list(rec.get("validators"))):
            return None             # recorded before the hub explained partial scans: evaluate again, once
        return best

    def gate(self, content_hash: str) -> dict[str, Any]:
        """F7, fail closed. The stored reports decide, never a model: missing | blocked | warn | ok.
        Static pass (always run): every security validator must be PRESENT and must have finished (`passed` or `failed`); a
        missing expected validator, or one that is `incomplete`, `error`, `skipped` or anything else, BLOCKS (an override can
        let it through); a critical/high security finding blocks; medium security findings alone only warn. LLM pass (tiers
        tier1 and up): only its critical/high security findings count, so it can add a block but never clear one."""
        none: dict[str, Any] = {"status": "missing", "verdict": None, "run_id": None, "tiers": None, "subject": None, "reasons": []}
        best = self._best_run(content_hash)
        if best is None:
            return none
        _, rec = best
        need = self.gate_min_tiers()
        validators = [_dict(v) for v in _list(rec.get("validators"))]
        if (TIER_RANK.get(str(rec.get("tiers")), -1) < TIER_RANK.get(need, 0) or rec.get("verdict") in (None, "error")
                or not validators):
            return none
        found = {"verdict": rec.get("verdict"), "run_id": rec.get("run_id"), "tiers": rec.get("tiers"),
                 "subject": rec.get("subject")}
        blocked: list[str] = []
        warn: list[str] = []
        shown = [f for f in _list(rec.get("security_findings")) if isinstance(f, dict)]
        for f in shown:
            if f.get("severity") in ("critical", "high"):
                blocked.append(f"{f.get('validator')}: {f.get('message')}")
        present = {str(v.get("validator") or "?") for v in validators}
        for name in EXPECTED_SECURITY_VALIDATORS:
            if name not in present:
                blocked.append(f"{name}: validator missing from the report")
        for which, key in (("static", "validators"), ("llm", "validators_llm")):
            for v in _list(rec.get(key)):
                v = _dict(v)
                name = str(v.get("validator") or "?")
                if not is_security_validator(name):
                    continue
                sev = _dict(v.get("severity"))
                bad = (_int(sev.get("critical")) or 0) + (_int(sev.get("high")) or 0)
                if bad and not any(f.get("validator") == name and f.get("severity") in ("critical", "high")
                                   and f.get("pass") == which for f in shown):
                    blocked.append(f"{name}: {bad} critical/high finding(s)")
                if which == "static":
                    part = _dict(_dict(rec.get("scan_partial")).get(name))
                    if v.get("status") == "incomplete" and part.get("reason") == UNRESOLVED_REFERENCES:
                        where = ", ".join(f"{_dict(x).get('path')}:{_dict(x).get('line')}" for x in _list(part.get("references"))[:3])
                        warn.append(f"{name}: {_int(part.get('count'))} local reference(s) not bundled with the skill ({where}); "
                                    "every bundled file was scanned")
                    elif v.get("status") not in ("passed", "failed"):
                        blocked.append(f"{name}: status {str(v.get('status'))[:20]} (the scan did not finish)")
                    if _int(sev.get("medium")):
                        warn.append(f"{name}: {_int(sev.get('medium'))} medium finding(s)")
        if blocked:
            return {**found, "status": "blocked", "reasons": blocked[:8]}
        if warn:
            return {**found, "status": "warn", "reasons": warn[:5]}
        return {**found, "status": "ok", "reasons": []}

    # -- evaluation ---------------------------------------------------------------------------------------------------------
    def _new_run_id(self) -> str:
        ts = self.clock()
        return time.strftime("%Y%m%dT%H%M%S", time.gmtime(ts)) + f"{int(ts * 1000) % 1000:03d}Z-" + secrets.token_hex(4)

    def _acquire(self) -> None:
        self._ensure_root()
        if not self._lock.try_acquire():
            raise _busy()

    def _prepare(self, skill: str, tiers: str, run_id: str | None = None, job: Job | None = None) -> _Prepared:
        if tiers not in TIERS:
            raise Unprocessable(f"tiers must be one of {', '.join(TIERS)}", code="bad_tiers")
        subject = normalize_subject(skill)
        src, key = parse_subject(subject)
        found = self.find_launcher()
        if found is None:
            raise Unavailable("SkillEvaluator is not installed (neither skillevaluator-local nor skillevaluator is on the hub's "
                              "PATH or in ~/.local/bin)", code="evaluator_missing", fix=INSTALL_HINT)
        if src == "hub":
            with self.guard():                          # the content lock: a consistent copy, never mid-edit
                snap = self.snapshot(subject)
        else:
            snap = self.snapshot(subject)
        version = self._version_for(found)
        rid = run_id or self._new_run_id()
        run_dir = self._runs_dir(subject) / rid
        _mkdir_private(run_dir.parent)
        run_dir.mkdir(mode=0o700)
        run_dir.chmod(0o700)
        return _Prepared(subject, src, key, tiers, rid, found[0], found[1], snap, run_dir, self.clock(), version, job)

    def _do(self, p: _Prepared, actor: str, force: bool) -> dict[str, Any]:
        """Reuse a finished run of the same content when allowed (no process), else run SkillEvaluator."""
        if not force:
            best = self._reusable(p)
            if best is not None:
                return self._write_reused(p, actor, best[0], best[1])
        return self._execute(p, actor)

    def evaluate(self, skill: str, tiers: str = "static", actor: str = "hub", force: bool = True) -> dict[str, Any]:
        """Synchronous (`hubctl`). Raises Conflict 409 `evaluation_running` when another evaluation holds the lock. One explicit
        skill is always re-run unless the caller passes force=False."""
        self._acquire()
        try:
            return self._do(self._prepare(skill, tiers), actor, force)
        finally:
            self._lock.release()

    def evaluate_subjects(self, subjects: list[str], tiers: str, actor: str, force: bool = False) -> list[dict[str, Any]]:
        """`hubctl evaluate --wait`: one lock for the whole batch, so a UI run cannot slip in between two subjects."""
        if tiers not in TIERS:
            raise Unprocessable(f"tiers must be one of {', '.join(TIERS)}", code="bad_tiers")
        self._acquire()
        try:
            out: list[dict[str, Any]] = []
            for s in subjects:
                try:
                    out.append(self._do(self._prepare(s, tiers), actor, force))
                except ProblemError as exc:
                    out.append({"skill": s, "subject": s, "tiers": tiers, "verdict": "error", "error": exc.detail, "run_id": None})
            return out
        finally:
            self._lock.release()

    def evaluate_many(self, skills: list[str], tiers: str, actor: str) -> list[dict[str, Any]]:
        """`hubctl evaluate-skills`: every named hub skill, always re-run."""
        return self.evaluate_subjects([normalize_subject(s) if valid_id(s) else s for s in skills], tiers, actor, force=True)

    # -- the queue ------------------------------------------------------------------------------------------------------------
    def enqueue(self, subjects: list[str], tiers: str = "static", force: bool = False, actor: str = "hub") -> dict[str, Any]:
        """Queue evaluations (FIFO, one at a time). Subjects already queued or running are skipped. A malformed subject refuses
        the whole request (400); one that does not exist is skipped with `not_found`. Returns {queued, skipped, run_ids}."""
        if tiers not in TIERS:
            raise Unprocessable(f"tiers must be one of {', '.join(TIERS)}", code="bad_tiers")
        if self.find_launcher() is None:
            raise Unavailable("SkillEvaluator is not installed (neither skillevaluator-local nor skillevaluator is on the hub's "
                              "PATH or in ~/.local/bin)", code="evaluator_missing", fix=INSTALL_HINT)
        wanted = [normalize_subject(s) for s in subjects]
        exists = {s: self.subject_exists(s) for s in dict.fromkeys(wanted)}
        queued: list[str] = []
        skipped: list[dict[str, str]] = []
        run_ids: dict[str, str] = {}
        with self._cv:
            if self._stop:
                raise Unavailable("the hub is shutting down", code="shutting_down")
            for s in wanted:
                if not exists[s]:
                    skipped.append({"subject": s, "reason": "not_found"})
                elif self._current is not None and self._current.subject == s:
                    skipped.append({"subject": s, "reason": "running"})
                    run_ids[s] = self._current.run_id
                elif s in self._queued:
                    skipped.append({"subject": s, "reason": "queued"})
                    run_ids[s] = self._queued[s].run_id
                elif len(self._queue) >= QUEUE_CAP:
                    skipped.append({"subject": s, "reason": "queue_full"})
                else:
                    job = Job(s, tiers, force, actor[:64], self._new_run_id())
                    self._queue.append(job)
                    self._queued[s] = job
                    queued.append(s)
                    run_ids[s] = job.run_id
            if queued and (self._worker is None or not self._worker.is_alive()):
                self._worker = threading.Thread(target=self._work, name="skilleval-queue", daemon=True)
                self._worker.start()
            self._cv.notify_all()
        return {"queued": queued, "skipped": skipped, "run_ids": run_ids}

    def start(self, skill: str, tiers: str = "static", actor: str = "hub") -> dict[str, Any]:
        """Old per-skill route: validate now (a bad name, an unreadable skill, no evaluator are immediate errors), then enqueue
        ONE forced run. Answers `queued` or `running` instead of the old 409 while busy."""
        subject = normalize_subject(skill)
        if not self.subject_exists(subject):
            raise NotFound(f"no such skill: {subject}")
        if tiers not in TIERS:
            raise Unprocessable(f"tiers must be one of {', '.join(TIERS)}", code="bad_tiers")
        if self.find_launcher() is not None:
            if parse_subject(subject)[0] == "hub":
                with self.guard():
                    self.snapshot(subject)
            else:
                self.snapshot(subject)
        with self._mu:
            idle = self._current is None and not self._queue
        res = self.enqueue([subject], tiers, True, actor)
        reason = res["skipped"][0]["reason"] if res["skipped"] else None
        status = "running" if (reason == "running" or (reason is None and idle)) else "queued"
        return {"skill": parse_subject(subject)[1], "subject": subject, "tiers": tiers, "status": status,
                "run_id": res["run_ids"].get(subject), "started_at": None}

    def queue(self) -> dict[str, Any]:
        run = self.running()
        with self._mu:
            queued = [{"subject": j.subject, "tiers": j.tiers} for j in list(self._queue)[:QUEUE_SHOWN]]
            total = len(self._queue)
            out = {"running": ({k: run.get(k) for k in ("subject", "run_id", "tiers", "started_at", "elsewhere")}
                               if run else None),
                   "queued": queued, "queued_total": total, "finished_since_start": self._finished,
                   "errors_since_start": self._errors, "last_errors": list(self._last_errors)}
        return out

    def cancel(self) -> dict[str, Any]:
        """Clear the queue and kill the running evaluation's process group."""
        with self._cv:
            n = len(self._queue)
            self._queue.clear()
            self._queued.clear()
            cur = self._current
            if cur is not None:
                cur.cancelled = True
            self._cv.notify_all()
        if cur is not None:
            self.runner.cancel()
        return {"cancelled_queued": n, "killed_running": cur is not None}

    def join(self, timeout: float | None = None) -> None:
        """Wait until the queue is empty and nothing is running (tests, shutdown)."""
        with self._cv:
            self._cv.wait_for(lambda: not self._queue and self._current is None, timeout)

    def shutdown(self) -> None:
        """Hub stopping: drop the queue, kill a running SkillEvaluator process group, give the worker a moment to record."""
        with self._cv:
            self._stop = True
            self._queue.clear()
            self._queued.clear()
            cur = self._current
            if cur is not None:
                cur.cancelled = True
            self._cv.notify_all()
        self.runner.cancel()
        self.join(timeout=5)

    def _note_error(self, job: Job, text: str) -> None:
        with self._mu:
            self._errors += 1
            self._last_errors.append({"subject": job.subject, "error": scrub(text)[:300]})

    def _work(self) -> None:
        while True:
            with self._cv:
                while not self._queue and not self._stop:
                    self._cv.wait()
                if self._stop:
                    return
                job = self._queue.popleft()
                self._queued.pop(job.subject, None)
                self._current = job
            try:
                self._run_job(job)
            except Exception as exc:  # noqa: BLE001 - one bad job must not kill the worker
                self._note_error(job, f"{type(exc).__name__}")
            finally:
                with self._cv:
                    self._current = None
                    self._cv.notify_all()

    def _run_job(self, job: Job) -> None:
        self._ensure_root()
        while True:
            if job.cancelled or self._stop:
                return
            if self._lock.try_acquire():
                break
            time.sleep(1.0)                           # another process (hubctl) holds the evaluation lock: wait for it
        try:
            self._recover_interrupted()
            if job.cancelled:
                return
            try:
                prep = self._prepare(job.subject, job.tiers, job.run_id, job)
                rec = self._do(prep, job.actor, job.force)
            except ProblemError as exc:
                self._note_error(job, exc.detail)
                return
            with self._mu:
                self._finished += 1
                if rec.get("verdict") == "error":
                    self._errors += 1
                    self._last_errors.append({"subject": job.subject, "error": str(rec.get("error") or "error")[:300]})
        finally:
            self._lock.release()

    def _recover_interrupted(self) -> None:
        """Once per process, under the lock: a run directory without a record is a run the previous hub process died in."""
        if self._recovered:
            return
        self._recovered = True
        dirs: list[tuple[str, Path]] = []
        with contextlib.suppress(OSError):
            for p in self.root.iterdir():
                if valid_id(p.name) and p.is_dir() and not p.is_symlink():
                    dirs.append((f"hub:{p.name}", p))
        obs = self.root / OBSERVED_DIR
        with contextlib.suppress(OSError):
            for c in obs.iterdir():
                if valid_id(c.name) and c.is_dir() and not c.is_symlink():
                    for k in c.iterdir():
                        if KEY_RE.fullmatch(k.name) and k.is_dir() and not k.is_symlink():
                            dirs.append((f"{c.name}:{k.name}", k))
        for subject, d in dirs:
            with contextlib.suppress(OSError):
                for rd in d.iterdir():
                    if RUN_ID_RE.fullmatch(rd.name) and rd.is_dir() and not rd.is_symlink() and not (rd / RECORD).exists():
                        src, key = parse_subject(subject)
                        self._write_record(rd, {"run_id": rd.name, "subject": subject, "skill": key, "source": src,
                                                "tiers": "static", "verdict": "error", "error": "interrupted",
                                                "finished_at": self.clock(), "content_hash": None, "findings": []})

    # -- one run ------------------------------------------------------------------------------------------------------------------
    @staticmethod
    def _tier1_argv(p: _Prepared, copy: Path, out: Path, llm: bool) -> list[str]:
        return [p.exe, "tier1", str(copy), *([] if llm else ["--no-llm"]), "--checks", SAFE_CHECKS, "-r", "json,markdown",
                "-o", str(out)]

    @staticmethod
    def _check_cancel(p: _Prepared) -> None:
        if p.job is not None and p.job.cancelled:
            raise _Cancelled

    def _base_record(self, p: _Prepared, actor: str) -> dict[str, Any]:
        return {"run_id": p.run_id, "skill": p.name, "subject": p.subject, "source": p.source, "path": p.snap.path,
                "origin": p.snap.origin, "tiers": p.tiers, "launcher": p.launcher, "evaluator_version": p.version,
                "started_at": p.started_at, "content_hash": p.snap.content_hash, "files": len(p.snap.files),
                "bytes": p.snap.total_bytes, "actor": actor[:64], "model_calls": p.tiers != "static", "static": None,
                "tier1": None, "tier2": None, "validators": None, "validators_llm": None, "security_findings": [],
                "findings": [], "findings_total": 0, "error": None, "llm_error": None, "reused_from": None,
                "eval_rev": EVAL_REV, "scan_partial": {}}

    def _emit(self, actor: str, p: _Prepared, rec: Mapping[str, Any]) -> None:
        if self.on_event is not None:
            with contextlib.suppress(Exception):
                self.on_event(actor, "skill.evaluate", {
                    "skill": p.name, "subject": p.subject, "run_id": p.run_id, "tiers": rec.get("tiers"),
                    "verdict": rec["verdict"], "launcher": p.launcher, "content_hash": p.snap.content_hash[:12],
                    "reused_from": rec.get("reused_from")})

    def _write_reused(self, p: _Prepared, actor: str, ptr: Mapping[str, Any], src: Mapping[str, Any]) -> dict[str, Any]:
        """Same content, already evaluated (tiers at least as high, same evaluator version): a record that copies that run's
        summary and points at it. No process is started."""
        rec = {**src, **self._base_record(p, actor)}
        for k in ("tiers", "launcher", "model_calls", "static", "tier1", "tier2", "validators", "validators_llm",
                  "security_findings", "findings", "findings_total", "error", "llm_error", "evaluator_version"):
            rec[k] = src.get(k)
        rec.update({"verdict": src.get("verdict"), "finished_at": self.clock(), "duration_ms": 0, "reports": [],
                    "reused_from": {"subject": ptr.get("subject"), "run_id": ptr.get("run_id")}})
        self._write_record(p.run_dir, rec)
        self._prune(p.subject)
        self._emit(actor, p, rec)
        return rec

    def _execute(self, p: _Prepared, actor: str) -> dict[str, Any]:
        work = self.root / ".work" / p.run_id
        rec = self._base_record(p, actor)
        with self._mu:
            self._running = {"subject": p.subject, "skill": p.name, "run_id": p.run_id, "tiers": p.tiers,
                             "started_at": p.started_at, "elsewhere": False}
        t0 = time.monotonic()
        copy = work / p.name
        static_dir = p.run_dir / STATIC_DIR
        try:
            self._check_cancel(p)
            self._clean_work()
            _mkdir_private(work)
            self._write_copy(copy, p.snap)
            _mkdir_private(static_dir)
            env = self.child_env()
            r1 = self.runner.run(self._tier1_argv(p, copy, static_dir, llm=False), cwd=work, env=env, timeout=self.timeout)
            self._check_cancel(p)
            ts = self._load_report(static_dir / T1_JSON)
            if ts is None:
                verdict = "error"
                rec["error"] = self._error_text(r1, copy)
            else:
                static = self._clean(summarize_tier1(ts), copy)
                rec["static"] = rec["tier1"] = static
                rec["validators"] = self._clean(validator_summaries(ts), copy)
                partial = self._explain_partial_security(ts, copy, work, env)
                if partial is not None:
                    rec["scan_partial"] = {"Security Scan": self._clean(partial, copy)}
                rec["security_findings"] = self._clean(security_findings(ts, "static"), copy)
                findings, total = top_findings(ts)
                verdict = static["status"] or "error"
                if verdict == "error":
                    rec["error"] = "the tier1 report has no overall_status"
                elif p.tiers != "static":
                    # second pass with the LLM judge: quality and judge scores. Its security findings can only ADD to the gate.
                    r2 = self.runner.run(self._tier1_argv(p, copy, p.run_dir, llm=True), cwd=work, env=env, timeout=self.timeout)
                    self._check_cancel(p)
                    tl = self._load_report(p.run_dir / T1_JSON)
                    if tl is None:
                        rec["llm_error"] = self._error_text(r2, copy)
                        verdict = combine_verdicts(verdict, "error")
                    else:
                        rec["tier1"] = self._clean(summarize_tier1(tl), copy)
                        rec["validators_llm"] = self._clean(validator_summaries(tl), copy)
                        rec["security_findings"] += [f for f in self._clean(security_findings(tl, "llm"), copy)
                                                     if f["severity"] in ("critical", "high")]
                        findings, total = top_findings(tl)
                        verdict = combine_verdicts(verdict, rec["tier1"]["status"] or "error")
                rec["findings"], rec["findings_total"] = self._clean(findings, copy), total
            if p.tiers == "tier1+2":
                self._check_cancel(p)
                r3 = self.runner.run([p.exe, "tier2", str(copy), "-r", "json", "-o", str(p.run_dir)], cwd=work, env=env,
                                     timeout=self.timeout)
                t2 = self._load_report(p.run_dir / T2_JSON)
                st2 = t2.get("overall_status") if t2 is not None else None
                st2 = st2 if st2 in ("passed", "failed", "incomplete") else "error"
                rec["tier2"] = {"status": st2, "errors": _int(t2.get("total_errors")) if t2 else None,
                                "warnings": _int(t2.get("total_warnings")) if t2 else None,
                                "error": self._error_text(r3, copy) if st2 == "error" else None}
                verdict = combine_verdicts(verdict, st2)
            rec["verdict"] = verdict
        except _Cancelled:
            rec["verdict"], rec["error"] = "error", "cancelled"
        except Exception as exc:  # noqa: BLE001 - recorded as an error run; the hub keeps serving
            rec["verdict"], rec["error"] = "error", f"the evaluation could not run ({type(exc).__name__})"
        finally:
            shutil.rmtree(work, ignore_errors=True)
            rec["duration_ms"] = int((time.monotonic() - t0) * 1000)
            rec["finished_at"] = self.clock()
            self._finalize_reports(p.run_dir, copy)
            rec["reports"] = sorted([n for n in (T1_JSON, T1_MD, T2_JSON) if (p.run_dir / n).is_file()]
                                    + [f"{STATIC_DIR}/{n}" for n in (T1_JSON, T1_MD) if (p.run_dir / STATIC_DIR / n).is_file()])
            if "verdict" not in rec:
                rec["verdict"] = "error"
            self._write_record(p.run_dir, rec)
            self._update_pointer(rec)
            self._prune(p.subject)
            if p.source == "hub":
                self._hashes.pop(p.name, None)
            with self._mu:
                self._running = None
        self._emit(actor, p, rec)
        return rec

    # -- helpers ------------------------------------------------------------------------------------------------------------
    def _explain_partial_security(self, ts: Mapping[str, Any], copy: Path, work: Path,
                                  env: Mapping[str, str]) -> dict[str, Any] | None:
        """For a static Security Scan that is `incomplete` ONLY because SkillSpector called its analysis partial, run
        SkillSpector's static scan on the same copy (same containment, report written to the private work dir) and classify
        why (classify_partial_scan). None whenever anything differs or cannot be read: the gate then keeps blocking."""
        sec = next((_dict(r) for r in _list(ts.get("results")) if _dict(r).get("validator") == "Security Scan"), {})
        # The scan's error lines: its findings ("[SECURITY-HIGH] ...", judged by the gate anyway) plus, here, exactly one
        # line saying the analysis was partial. Any other error means something else went wrong: no classification.
        errors = [str(e) for e in _list(_dict(sec.get("legacy")).get("errors")) if not FINDING_LINE.match(str(e))]
        if (sec.get("status") != "incomplete" or sec.get("incomplete_scans") != [SKILLSPECTOR] or len(errors) != 1
                or not errors[0].startswith(PARTIAL_ERROR)):
            return None
        exe = self.which(SKILLSPECTOR, self.search_path())
        if not exe:
            return None
        out = work / SKILLSPECTOR_REPORT
        r = self.runner.run([exe, "scan", str(copy), "--format", "json", "--no-llm", "--output", str(out)], cwd=work,
                            env=env, timeout=SKILLSPECTOR_TIMEOUT_S)
        if r.timed_out:
            return None
        report = self._load_report(out)
        return classify_partial_scan(report) if report is not None else None

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
        """Make what SkillEvaluator wrote private (0600 / 0700), drop symlinks and special files it may have left, and replace
        the temporary copy's absolute path in text reports (it is gone, and it would reveal the state-dir layout)."""
        for dirpath, dirnames, filenames in os.walk(run_dir, followlinks=False):
            with contextlib.suppress(OSError):
                os.chmod(dirpath, 0o700)
            for n in list(dirnames):
                if os.path.islink(os.path.join(dirpath, n)):
                    dirnames.remove(n)
                    with contextlib.suppress(OSError):
                        os.unlink(os.path.join(dirpath, n))
            for n in filenames:
                full = Path(dirpath) / n
                try:
                    st = os.lstat(full)
                    if not stat.S_ISREG(st.st_mode):
                        full.unlink()                  # symlinks, FIFOs, sockets: nothing a report needs
                        continue
                    if full.suffix in (".json", ".md"):
                        got = _read_regular(full, REPORT_CAP)
                        if got is not None:
                            text = got[0].decode("utf-8", "replace")
                            new = self._scrub_paths(text, copy)
                            if new != text:
                                tmp = Path(dirpath) / f".{n}.tmp"
                                fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
                                with os.fdopen(fd, "wb") as fh:
                                    fh.write(new.encode("utf-8"))
                                os.replace(tmp, full)
                    os.chmod(full, 0o600)
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

    def _prune(self, subject: str) -> None:
        """Keep the newest `keep` run directories of this subject (including unfinished ones, so a crash loop cannot grow it)."""
        for d in self._run_dirs(subject)[self.keep:]:
            shutil.rmtree(d, ignore_errors=True)
