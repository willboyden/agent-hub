"""Curated memory (ARCHITECTURE decision 10): agents propose memories into a per-client inbox; only an admin promotes
one into content/memory/. Inbox files are never compiled into any client."""
from __future__ import annotations

import threading
import time
from typing import Any

from pydantic import ValidationError

from agent_hub.domain import yamlsafe
from agent_hub.domain.errors import BadRequest, Conflict, NotFound, ProblemError, Unprocessable
from agent_hub.domain.ids import slugify, valid_id
from agent_hub.domain.models import InboxMeta, issues_from
from agent_hub.services.common import new_id
from agent_hub.services.content import ContentStore

MAX_BODY = 16 * 1024
MAX_TITLE = 200


class Inbox:
    def __init__(self, work: ContentStore, rate_per_min: int, max_per_client: int) -> None:
        self.work = work
        self.rate = rate_per_min
        self.max_per_client = max_per_client
        self._hits: dict[str, list[float]] = {}
        self._lock = threading.Lock()

    def _throttle(self, client: str) -> None:
        cutoff = time.monotonic() - 60
        with self._lock:
            hits = [t for t in self._hits.get(client, []) if t > cutoff]
            if len(hits) >= self.rate:
                self._hits[client] = hits
                raise ProblemError("inbox rate limit reached; retry later", code="rate_limited", status=429)
            hits.append(time.monotonic())
            self._hits[client] = hits

    def _dir(self, client: str) -> str:
        return f"inbox/{client}"

    def _ids(self, client: str) -> list[str]:
        d = self.work.root / "inbox" / client
        if not d.is_dir() or d.is_symlink():
            return []
        return sorted(f.name[:-3] for f in d.iterdir() if f.is_file() and not f.is_symlink() and f.name.endswith(".md")
                      and valid_id(f.name[:-3]))

    def add(self, client: str, title: str, body: str, mtype: str) -> dict[str, Any]:
        if client not in self.work.names_of("clients"):
            raise NotFound(f"no such client: {client}")
        if not title.strip() or len(title) > MAX_TITLE:
            raise BadRequest(f"title must be 1..{MAX_TITLE} characters")
        if not body.strip() or len(body.encode()) > MAX_BODY:
            raise BadRequest(f"body must be 1..{MAX_BODY} bytes")
        self._throttle(client)
        if len(self._ids(client)) >= self.max_per_client:
            raise ProblemError("inbox is full; an admin must promote or reject items", code="inbox_full", status=429)
        iid = f"{slugify(title)[:40] or 'note'}-{new_id()[:6]}"
        try:
            meta = InboxMeta(id=iid, type=mtype, title=title.strip(), client=client, created=time.time())
        except ValidationError as exc:
            raise Unprocessable("invalid inbox item", errors=[i.model_dump() for i in issues_from(exc)]) from exc
        self.work._write(f"{self._dir(client)}/{iid}.md",
                         yamlsafe.join_frontmatter(meta.model_dump(mode="json"), body))
        return {"id": iid, "client": client, "type": mtype, "title": meta.title}

    def _read(self, client: str, iid: str) -> tuple[InboxMeta, str]:
        rel = f"{self._dir(client)}/{iid}.md"
        raw, body = yamlsafe.split_frontmatter(self.work._read(rel))
        try:
            return InboxMeta.model_validate(raw), body
        except ValidationError as exc:
            raise Unprocessable("inbox item is invalid", errors=[i.model_dump() for i in issues_from(exc)]) from exc

    def list(self, client: str | None = None) -> list[dict[str, Any]]:
        out = []
        clients = [client] if client else self.work.names_of("clients")
        base = self.work.root / "inbox"
        if base.is_dir():
            clients = sorted({*clients, *[p.name for p in base.iterdir() if p.is_dir() and valid_id(p.name)]}) if not client else clients
        for c in clients:
            for iid in self._ids(c):
                try:
                    meta, body = self._read(c, iid)
                except (ProblemError, yamlsafe.YamlError):
                    out.append({"id": iid, "client": c, "valid": False})
                    continue
                out.append({"id": iid, "client": c, "type": meta.type, "title": meta.title, "body": body,
                            "created": meta.created, "valid": True})
        return sorted(out, key=lambda x: (x.get("created") or 0, x["id"]))

    def _find(self, iid: str) -> str:
        if not valid_id(iid):
            raise BadRequest("invalid id", code="invalid_id")
        base = self.work.root / "inbox"
        if base.is_dir():
            for d in sorted(base.iterdir()):
                if d.is_dir() and not d.is_symlink() and valid_id(d.name) and iid in self._ids(d.name):
                    return d.name
        raise NotFound(f"no such inbox item: {iid}")

    def promote(self, iid: str, edits: dict[str, Any] | None = None) -> dict[str, Any]:
        client = self._find(iid)
        meta, body = self._read(client, iid)
        edits = edits or {}
        mem_id = str(edits.get("id") or iid)
        if not valid_id(mem_id):
            raise BadRequest("invalid memory id", code="invalid_id")
        if mem_id in self.work.names("memory"):
            raise Conflict(f"memory {mem_id} already exists", code="already_exists")
        self.work.put("memory", mem_id, {"id": mem_id, "type": edits.get("type", meta.type),
                                         "title": edits.get("title", meta.title),
                                         "body": edits.get("body", body)}, must_exist=False)
        self.work._p(f"{self._dir(client)}/{iid}.md").unlink()
        return {"id": mem_id, "promoted_from": f"{client}/{iid}"}

    def reject(self, iid: str) -> dict[str, Any]:
        client = self._find(iid)
        self.work._p(f"{self._dir(client)}/{iid}.md").unlink()
        return {"id": iid, "client": client, "rejected": True}
