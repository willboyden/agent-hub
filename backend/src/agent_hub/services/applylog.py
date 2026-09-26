"""Per-client history of applies (what each apply wrote, its pre-apply backups and post-apply hashes), so an apply can be undone."""
from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any

from agent_hub.domain.errors import BadRequest, NotFound
from agent_hub.domain.ids import valid_id
from agent_hub.services.common import atomic_write

APPLY_ID_RE = re.compile(r"\d{8}T\d{6}Z-[A-Za-z0-9_]{1,40}")
KEEP = 200


class ApplyLog:
    def __init__(self, directory: Path) -> None:
        self.dir = directory

    def _dir(self, client: str) -> Path:
        if not valid_id(client):
            raise BadRequest("invalid client id", code="invalid_id")
        return self.dir / client

    def save(self, rec: dict[str, Any]) -> None:
        d = self._dir(rec["client"])
        atomic_write(d / f"{rec['apply_id']}.json", json.dumps(rec, indent=1).encode(), 0o600, dir_mode=0o700)
        recs = self.list(rec["client"])
        for old in recs[KEEP:]:
            (d / f"{old['apply_id']}.json").unlink(missing_ok=True)

    def get(self, client: str, apply_id: str) -> dict[str, Any]:
        if APPLY_ID_RE.fullmatch(apply_id) is None:
            raise BadRequest("invalid apply id", code="invalid_id")
        try:
            rec = json.loads((self._dir(client) / f"{apply_id}.json").read_text())
        except (OSError, ValueError) as exc:
            raise NotFound(f"no such apply: {apply_id}") from exc
        assert isinstance(rec, dict)
        return rec

    def list(self, client: str) -> list[dict[str, Any]]:
        d = self._dir(client)
        out: list[dict[str, Any]] = []
        for f in d.glob("*.json") if d.is_dir() else []:
            try:
                rec = json.loads(f.read_text())
            except (OSError, ValueError):
                continue
            if isinstance(rec, dict):
                out.append(rec)
        return sorted(out, key=lambda r: (r.get("time", 0), r.get("apply_id", "")), reverse=True)     # newest first, by real time

    def latest_active(self, client: str) -> dict[str, Any] | None:
        return next((r for r in self.list(client) if not r.get("rolled_back_at")), None)

    def update(self, client: str, apply_id: str, patch: dict[str, Any]) -> None:
        rec = self.get(client, apply_id)
        rec.update(patch)
        self.save(rec)

    @staticmethod
    def summary(rec: dict[str, Any]) -> dict[str, Any]:
        files = rec.get("files", [])
        return {"apply_id": rec["apply_id"], "time": rec.get("time"), "plan_id": rec.get("plan_id"),
                "content_hash": rec.get("content_hash"), "rolled_back_at": rec.get("rolled_back_at"),
                "counts": {a: sum(1 for f in files if f["action"] == a) for a in ("add", "change", "remove")}}
