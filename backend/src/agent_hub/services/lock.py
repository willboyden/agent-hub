"""Lock manifest: what the hub last delivered to each client (per-file sha256, managed keys). Lives in the state dir."""
from __future__ import annotations

import contextlib
import json
import os
from pathlib import Path

from pydantic import BaseModel, ValidationError

from agent_hub.domain.ids import valid_id
from agent_hub.services.common import atomic_write, now


class LockEntry(BaseModel):
    root: str
    path: str
    kind: str
    sha256: str                     # whole file as delivered
    slice_sha256: str               # the hub-owned slice (== sha256 for `own`)
    merge: str = "own"
    managed_keys: list[str] = []
    created: bool = False           # the hub created this file (only then may it ever delete it)
    source_ids: list[str] = []
    mode: int = 0o644


class Lock(BaseModel):
    version: int = 1
    client: str
    applied_at: float = 0.0
    content_hash: str = ""
    files: dict[str, LockEntry] = {}
    adopted_at: float = 0.0          # when import adopted the live files (NOT an apply)
    plan_id: str = ""
    verified_at: float | None = None
    verified_ok: bool | None = None


class LockStore:
    def __init__(self, directory: Path) -> None:
        self.dir = directory

    def _path(self, client: str) -> Path:
        if not valid_id(client):
            raise ValueError("bad client id")
        return self.dir / f"{client}.json"

    def load(self, client: str) -> Lock | None:
        try:
            lock = Lock.model_validate(json.loads(self._path(client).read_text()))
        except (OSError, ValueError, ValidationError):
            return None
        if not lock.content_hash and not lock.plan_id and lock.applied_at and not lock.adopted_at:
            # legacy lock written by an import (no plan was ever applied): that timestamp is an adoption, not an apply
            lock.adopted_at, lock.applied_at = lock.applied_at, 0.0
        return lock

    def save(self, lock: Lock, touch: bool = True) -> None:
        if touch:
            lock.applied_at = now()
        atomic_write(self._path(lock.client), (lock.model_dump_json(indent=2) + "\n").encode(), 0o600, dir_mode=0o700)

    def delete(self, client: str) -> None:
        with contextlib.suppress(OSError):
            os.unlink(self._path(client))
