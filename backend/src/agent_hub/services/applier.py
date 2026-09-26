"""Applier: turns a fresh plan into files, all-or-nothing per client.

Stage (temp files + backups; nothing live is touched) -> commit (atomic renames; roll back from the backups if any
fails) -> lock update -> adapter.verify. The plan is recomputed against the live files right before writing and must
match the reviewed plan's digest, so an apply can never do something the reviewer did not see."""
from __future__ import annotations

import contextlib
import os
import secrets
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from pydantic import BaseModel

from agent_hub.adapters.base import Check, Expected
from agent_hub.domain.errors import Conflict
from agent_hub.services import safefs
from agent_hub.services.applylog import ApplyLog
from agent_hub.services.common import atomic_write, sha256
from agent_hub.services.content import Catalog
from agent_hub.services.fs import LiveFiles, RealFS, RealRunner
from agent_hub.services.lock import Lock, LockEntry, LockStore
from agent_hub.services.planner import ClientPlanInternal, PlannedFile, Planner


class ApplyResult(BaseModel):
    client: str
    status: str                                   # applied | nothing | blocked | failed
    written: list[dict[str, str]] = []
    removed: list[dict[str, str]] = []
    skipped_conflicts: list[dict[str, str]] = []
    backup_dir: str | None = None
    checks: list[dict[str, Any]] = []
    verify_ok: bool | None = None
    blocked_reasons: list[str] = []
    error: str = ""
    apply_id: str | None = None


class ApplyError(Exception):
    pass


@dataclass
class _Op:
    pf: PlannedFile
    target: Path
    tmp: str | None = None                        # staged temp file name inside dirfd (writes)
    backup: Path | None = None
    old_mode: int | None = None
    existed: bool = False
    dirfd: int = -1                               # the target's parent directory, opened component-by-component with O_NOFOLLOW
    name: str = ""
    root_real: str = ""
    parts: list[str] = field(default_factory=list)
    expected: bytes | None = None                 # what the plan saw live: compare-and-swap before replacing
    created_dirs: list[list[str]] = field(default_factory=list)


class Applier:
    def __init__(self, planner: Planner, locks: LockStore, backups_dir: Path, runner: RealRunner | None = None,
                 log: ApplyLog | None = None) -> None:
        self.log = log
        self.planner = planner
        self.locks = locks
        self.backups_dir = backups_dir
        self.runner = runner or RealRunner()

    def apply_client(self, catalog: Catalog, content_hash: str, client_id: str, expected_digest: str | None,
                     adopt: frozenset[str] | set[str] = frozenset(), plan_id: str = "") -> ApplyResult:
        ip = self.planner.plan_client(catalog, client_id)               # the digest is always over the un-adopted plan
        if expected_digest is not None and ip.plan.digest != expected_digest:
            raise Conflict("live files or content changed since the plan was made; plan again", code="plan_stale")
        if adopt:
            ip = self.planner.plan_client(catalog, client_id, adopt)
        if ip.plan.blocked:
            return ApplyResult(client=client_id, status="blocked", blocked_reasons=ip.plan.blocked_reasons)
        cfg = ip.resolved.cfg
        assert cfg is not None
        live = LiveFiles(cfg.roots)
        ops: list[_Op] = []
        skipped: list[dict[str, str]] = []
        for pf in ip.files:
            d = pf.desired
            if pf.action in ("add", "change", "remove") or pf.action == "conflict" and pf.adopted and pf.adoptable and not pf.merged.unparseable:
                pass
            else:
                if pf.action == "conflict":
                    skipped.append({"root": d.root, "path": d.path, "reason": pf.reason})
                continue
            if pf.action == "conflict" and d.content is None:            # adopted removal
                pf.delete_file = d.merge == "own" or (pf.merged.empty_after and bool(pf.entry and pf.entry.created))
                if not pf.delete_file and pf.merged.new is None:
                    continue
            ops.append(_Op(pf, live.target(d.root, d.path), root_real=os.path.realpath(cfg.roots[d.root])))

        ts = time.strftime("%Y%m%dT%H%M%S", time.gmtime()) + f"-{secrets.token_hex(2)}"
        bdir = self.backups_dir / client_id / ts
        result = ApplyResult(client=client_id, status="nothing", skipped_conflicts=skipped)
        err = self.run_ops(ops, bdir)
        if err:
            return ApplyResult(client=client_id, status="failed", skipped_conflicts=skipped, error=f"{err}; nothing was changed")
        prev = {op.pf.key: (ip.lock.files[op.pf.key].model_dump() if ip.lock and op.pf.key in ip.lock.files else None) for op in ops}
        for op in ops:
            if op.backup is not None:
                result.backup_dir = str(bdir)
            entry = {"root": op.pf.desired.root, "path": op.pf.desired.path}
            (result.removed if op.pf.delete_file else result.written).append(entry)
        self._prune_dirs(ops, cfg.roots)
        self._update_lock(ip, content_hash, ops, skipped)
        result.status = "applied" if ops else "nothing"
        if ops and self.log is not None:
            result.apply_id = self._record(client_id, plan_id, content_hash, ops, prev)
        self._verify(ip, ops, cfg, result)
        return result

    def run_ops(self, ops: list[_Op], bdir: Path) -> str | None:
        """Stage every op (temp files + backups), then commit them; on any failure roll everything back. Returns an error
        message (starting with `conflict:` for a race) or None. Shared by apply and rollback."""
        done: list[_Op] = []
        try:
            for op in ops:
                self._stage(op, bdir)
            for op in ops:
                self._commit(op)
                done.append(op)
        except Exception as exc:  # noqa: BLE001 - roll everything back, whatever the cause
            self._rollback(ops, done)
            self._close(ops)
            kind = "conflict" if isinstance(exc, safefs.RaceError) else type(exc).__name__
            return f"{kind}: {str(exc)[:200]}"
        self._close(ops)
        return None

    def _record(self, client: str, plan_id: str, content_hash: str, ops: list[_Op], prev: dict[str, Any]) -> str:
        assert self.log is not None
        apply_id = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()) + "-" + (plan_id or f"x{secrets.token_hex(3)}")
        files = []
        for op in ops:
            d = op.pf.desired
            files.append({"root": d.root, "path": d.path, "kind": d.kind,
                          "action": "remove" if op.pf.delete_file else ("change" if op.existed else "add"),
                          "existed_before": op.existed,
                          "backup": str(op.backup.relative_to(self.backups_dir)) if op.backup else None,
                          "pre_sha": sha256(op.expected) if op.expected is not None else None,
                          "post_sha": None if op.pf.delete_file else sha256(op.pf.merged.new or b""),
                          "mode_before": op.old_mode, "lock_before": prev.get(op.pf.key)})
        self.log.save({"apply_id": apply_id, "client": client, "time": time.time(), "plan_id": plan_id,
                       "content_hash": content_hash, "files": files})
        return apply_id

    # -- stage / commit / rollback ----------------------------------------------------------------
    def _stage(self, op: _Op, bdir: Path) -> None:
        pf = op.pf
        parts = pf.desired.path.split("/")
        op.parts, op.name = parts[:-1], parts[-1]
        op.dirfd = safefs.open_dir(op.root_real, op.parts, create=not pf.delete_file, created=op.created_dirs)
        live, mode = safefs.read_at(op.dirfd, op.name)
        if live != pf.live:                            # changed between planning and staging
            raise safefs.RaceError(f"{pf.desired.path} changed since it was planned")
        op.expected, op.existed, op.old_mode = live, live is not None, mode
        if live is not None:
            backup = bdir / pf.desired.root / pf.desired.path
            atomic_write(backup, live, 0o600, dir_mode=0o700)
            op.backup = backup
        if pf.delete_file:
            return
        data = pf.merged.new
        if data is None:
            raise ApplyError(f"nothing to write for {pf.desired.path}")
        m = op.old_mode if (op.existed and pf.desired.merge != "own") else pf.desired.mode
        op.tmp = safefs.write_tmp(op.dirfd, data, m if m is not None else 0o644)

    def _commit(self, op: _Op) -> None:
        # compare-and-swap: re-read the live target through the same dirfd right before touching it
        cur, _ = safefs.read_at(op.dirfd, op.name)
        if cur != op.expected:
            raise safefs.RaceError(f"{op.pf.desired.path} changed since it was planned (someone edited it)")
        if op.pf.delete_file:
            os.unlink(op.name, dir_fd=op.dirfd)
        else:
            assert op.tmp is not None
            safefs.replace(op.dirfd, op.tmp, op.name)
            op.tmp = None

    def _rollback(self, ops: list[_Op], done: list[_Op]) -> None:
        for op in ops:
            if op.tmp is not None and op.dirfd >= 0:
                safefs.unlink_quiet(op.dirfd, op.tmp)
        for op in reversed(done):
            with contextlib.suppress(OSError):
                if op.backup is not None:
                    tmp = safefs.write_tmp(op.dirfd, op.backup.read_bytes(), op.old_mode or 0o644)
                    safefs.replace(op.dirfd, tmp, op.name)
                elif not op.pf.delete_file:
                    safefs.unlink_quiet(op.dirfd, op.name)
        for op in reversed(ops):
            for comps in reversed(op.created_dirs):
                with contextlib.suppress(OSError):
                    pfd = safefs.open_dir(op.root_real, comps[:-1], create=False, created=[])
                    try:
                        os.rmdir(comps[-1], dir_fd=pfd)
                    finally:
                        os.close(pfd)

    @staticmethod
    def _close(ops: list[_Op]) -> None:
        for op in ops:
            if op.dirfd >= 0:
                with contextlib.suppress(OSError):
                    os.close(op.dirfd)
                op.dirfd = -1

    @staticmethod
    def _prune_dirs(ops: list[_Op], roots: dict[str, str]) -> None:
        """After deleting hub-created files, remove the directories they leave empty (never the root itself)."""
        for op in ops:
            if not op.pf.delete_file:
                continue
            root = Path(os.path.realpath(roots[op.pf.desired.root]))
            p = op.target.parent
            while p != root and root in p.parents:
                try:
                    p.rmdir()
                except OSError:
                    break
                p = p.parent

    # -- lock / verify ----------------------------------------------------------------------------
    def _update_lock(self, ip: ClientPlanInternal, content_hash: str, ops: list[_Op], skipped: list[dict[str, str]]) -> None:
        lock = ip.lock or Lock(client=ip.plan.client)
        files = dict(lock.files)
        written = {op.pf.key for op in ops}
        skipped_keys = {f"{s['root']}:{s['path']}" for s in skipped}
        for pf in ip.files:
            d = pf.desired
            key = pf.key
            if key in skipped_keys or (pf.action in ("advisory", "conflict") and key not in written):
                continue
            if d.content is None:                        # removal (or released) -> drop the entry
                files.pop(key, None)
                continue
            if not d.managed:
                continue
            prev = files.get(key)
            final = pf.merged.new if pf.action != "unchanged" else pf.live
            if final is None:
                continue
            created = bool(prev.created) if prev else (pf.live is None and pf.action == "add")
            files[key] = LockEntry(root=d.root, path=d.path, kind=d.kind, sha256=sha256(final),
                                   slice_sha256=pf.merged.slice_new or sha256(final), merge=d.merge,
                                   managed_keys=d.managed_keys, created=created, source_ids=d.source_ids,
                                   mode=d.mode)
        lock.files = files
        lock.content_hash = content_hash
        self.locks.save(lock)

    def _verify(self, ip: ClientPlanInternal, ops: list[_Op], cfg: Any, result: ApplyResult) -> None:
        adapter = ip.resolved.adapter
        if adapter is None:
            return
        expected: list[Expected] = []
        written = {o.pf.key for o in ops}
        for pf in ip.files:
            d = pf.desired
            if d.content is None or not d.managed or pf.action == "advisory" or (pf.action == "conflict" and pf.key not in written):
                continue
            intended = pf.merged.new if pf.action != "unchanged" else pf.live       # what SHOULD be on disk
            # key/block merges are verified on the hub-owned SLICE (the rest of the file legitimately differs)
            digest = sha256(intended) if (intended is not None and d.merge == "own") else pf.merged.slice_new
            if intended is not None and digest is not None:
                expected.append(Expected(root=d.root, path=d.path, sha256=digest, kind=d.kind,
                                         source_ids=d.source_ids, merge=d.merge,
                                         managed_keys=d.managed_keys))
        try:
            checks = adapter.verify(cfg, expected, RealFS(list(cfg.roots.values())), self.runner)
        except Exception as exc:  # noqa: BLE001
            checks = [Check(name="adapter.verify", ok=False, detail=f"raised {type(exc).__name__}")]
        result.checks = [c.model_dump() for c in checks]
        result.verify_ok = all(c.ok for c in checks)
