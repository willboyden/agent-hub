"""Undo an apply: restore the pre-apply backups and delete the files the apply created, but ONLY where the live file still
equals what that apply wrote (so later hand edits are never clobbered). Uses the applier's dirfd/O_NOFOLLOW writes and
compare-and-swap, all-or-nothing."""
from __future__ import annotations

import os
import secrets
import time
from pathlib import Path
from typing import Any

from agent_hub.adapters.base import ClientConfig
from agent_hub.domain.errors import BadRequest, Conflict, NotFound
from agent_hub.domain.pathsafe import PathError, check_relpath
from agent_hub.services.applier import Applier, _Op
from agent_hub.services.applylog import ApplyLog
from agent_hub.services.common import sha256
from agent_hub.services.fs import LiveFiles
from agent_hub.services.lock import Lock, LockEntry, LockStore
from agent_hub.services.merge import Desired, Merged
from agent_hub.services.planner import PlannedFile, unified


class Rollbacker:
    def __init__(self, applier: Applier, log: ApplyLog, locks: LockStore) -> None:
        self.applier, self.log, self.locks = applier, log, locks

    def _backup_bytes(self, rec_file: dict[str, Any]) -> bytes:
        rel = rec_file.get("backup")
        if not rel:
            raise Conflict("this apply has no backup for a file it changed", code="backup_missing")
        base = Path(os.path.realpath(self.applier.backups_dir))
        p = Path(os.path.realpath(base / rel))
        if base not in p.parents or not p.is_file():
            raise Conflict(f"backup for {rec_file['path']} is missing", code="backup_missing")
        return p.read_bytes()

    def run(self, cfg: ClientConfig, client: str, apply_id: str | None, dry_run: bool, force: set[str]) -> dict[str, Any]:
        rec = self.log.get(client, apply_id) if apply_id else (self.log.latest_active(client) or next(iter(self.log.list(client)), None))
        if rec is None:
            raise NotFound("this client has no recorded applies to roll back", code="no_applies")
        if rec.get("rolled_back_at"):
            return {"status": "already_rolled_back", "apply_id": rec["apply_id"], "changed": [], "conflicts": [],
                    "message": f"apply {rec['apply_id']} was already rolled back; nothing to do"}
        latest = self.log.latest_active(client)
        live = LiveFiles(cfg.roots)
        items: list[dict[str, Any]] = []
        conflicts: list[dict[str, str]] = []
        planned: list[tuple[dict[str, Any], bytes | None, bytes | None]] = []      # (record file, live now, bytes to restore)
        done_keys: set[str] = set(rec.get("done", []))
        for f in rec["files"]:
            key = f"{f['root']}:{f['path']}"
            if key in done_keys:
                continue
            try:
                check_relpath(f["path"])
                cur = live.read(f["root"], f["path"])
            except (PathError, OSError) as exc:
                conflicts.append({"root": f["root"], "path": f["path"], "reason": f"live file unreadable: {exc}"})
                continue
            cur_sha = sha256(cur) if cur is not None else None
            if cur_sha != f["post_sha"] and key not in force and f"{client}:{key}" not in force:
                reason = "file is gone" if cur is None else "file was edited since this apply wrote it"
                conflicts.append({"root": f["root"], "path": f["path"], "reason": reason})
                continue
            if f["action"] == "add":
                if cur is None:
                    continue                                                       # already gone: nothing to undo
                planned.append((f, cur, None))
                items.append({"root": f["root"], "path": f["path"], "action": "delete", "diff": unified(cur, None, f["path"])})
            else:
                restore = self._backup_bytes(f)
                if cur == restore:
                    continue
                planned.append((f, cur, restore))
                items.append({"root": f["root"], "path": f["path"], "action": "restore",
                              "diff": unified(cur, restore, f["path"])})
        if conflicts and latest is not None and latest["apply_id"] != rec["apply_id"]:
            raise Conflict("this is not the latest apply for the client and some of its files changed since; "
                           "roll back the newer applies first", code="rollback_not_latest", conflicts=conflicts)
        base = {"apply_id": rec["apply_id"], "changed": items, "conflicts": conflicts}
        if dry_run:
            return {**base, "status": "dry_run"}
        if not planned:
            status = "conflicts_only" if conflicts else "nothing_to_do"
            if not conflicts:
                self.log.update(client, rec["apply_id"], {"rolled_back_at": time.time()})       # every file is already back
            return {**base, "status": status, "message": "no file could be rolled back" if conflicts else "already in the pre-apply state"}

        ops: list[_Op] = []
        for f, live_now, restore_to in planned:
            d = Desired(f["root"], f["path"], f.get("kind", "skill"), "own", [], f.get("mode_before") or 0o644, [], restore_to)
            pf = PlannedFile(d, "remove" if restore_to is None else "change", merged=Merged(new=restore_to), live=live_now,
                             delete_file=restore_to is None)
            ops.append(_Op(pf, live.target(f["root"], f["path"]), root_real=os.path.realpath(cfg.roots[f["root"]])))
        bdir = self.applier.backups_dir / client / (time.strftime("%Y%m%dT%H%M%S", time.gmtime()) + f"-rollback-{secrets.token_hex(2)}")
        err = self.applier.run_ops(ops, bdir)
        if err:
            raise Conflict(f"rollback aborted, nothing was changed: {err}", code="rollback_failed")
        self.applier._prune_dirs(ops, cfg.roots)
        lock = self.locks.load(client) or Lock(client=client)
        for f, _, _ in planned:
            key = f"{f['root']}:{f['path']}"
            before = f.get("lock_before")
            if before:
                lock.files[key] = LockEntry.model_validate(before)
            else:
                lock.files.pop(key, None)
        self.locks.save(lock, touch=False)
        done_keys |= {f"{f['root']}:{f['path']}" for f, _, _ in planned}
        complete = not conflicts
        self.log.update(client, rec["apply_id"], {"done": sorted(done_keys), "rollback_backup_dir": str(bdir),
                                                  "rolled_back_at": time.time() if complete else None})
        return {**base, "status": "rolled_back" if complete else "partially_rolled_back", "backup_dir": str(bdir)}


__all__ = ["BadRequest", "Rollbacker"]
