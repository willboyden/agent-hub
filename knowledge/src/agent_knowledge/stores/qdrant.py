"""Qdrant over REST (httpx). One collection per namespace (`ak_<ns>`), HNSW graph and (optionally) vectors on disk.
Filters are translated from the validated AST into Qdrant's structured JSON filter; no string building."""
from __future__ import annotations

import logging
import os
import time
import uuid
from typing import Any

import httpx

from agent_knowledge.filters import Node, to_qdrant
from agent_knowledge.stores.base import Hit, Record, StoreDimMismatch, StoreError, StoreStats, check_ids

log = logging.getLogger(__name__)
_UUID_NS = uuid.UUID("6f1a4c1e-3b52-4d6a-9f0e-6b1b6a1f7a10")


def point_id(rid: str) -> str:
    return str(uuid.uuid5(_UUID_NS, rid))


class QdrantStore:
    name = "qdrant"

    def __init__(self, url: str, api_key_env: str | None = None, vectors_on_disk: bool = True,
                 transport: httpx.AsyncBaseTransport | None = None, timeout: float = 30.0,
                 allow_unauth: bool = False) -> None:
        self.allow_unauth = allow_unauth  # KNOWLEDGE_ALLOW_UNAUTH_QDRANT=1 (set by the app), off by default
        self._probe_at = 0.0
        self._probe_result: bool | None = None
        self._url, self._key_env, self._vec_disk = url.rstrip("/"), api_key_env, vectors_on_disk
        self._client = httpx.AsyncClient(base_url=self._url, timeout=timeout, follow_redirects=False, transport=transport)

    async def aclose(self) -> None:
        await self._client.aclose()

    @staticmethod
    def collection(ns: str) -> str:
        return f"ak_{ns}"

    def _headers(self) -> dict[str, str]:
        key = os.environ.get(self._key_env, "") if self._key_env else ""
        return {"api-key": key} if key else {}

    def key_configured(self) -> bool:
        return bool(self._headers())

    async def probe_unauthenticated(self) -> bool | None:
        """True if Qdrant answers a data-plane request WITHOUT a key (any local process could then bypass token scopes)."""
        try:
            r = await self._client.get("/collections")  # deliberately no api-key header
        except httpx.TransportError:
            return None
        return r.status_code == 200

    REFUSAL = ("qdrant answers without an API key, so any local process could bypass token scopes; enable "
               "QDRANT__SERVICE__API_KEY (or set KNOWLEDGE_ALLOW_UNAUTH_QDRANT=1 to accept the risk)")

    async def refusal(self) -> str | None:
        """Reason this backend is refused, or None. Re-probed at most every 30s so a later downgrade is caught too."""
        if self.allow_unauth:
            return None
        if time.monotonic() - self._probe_at > 30.0:
            self._probe_result, self._probe_at = await self.probe_unauthenticated(), time.monotonic()
        return self.REFUSAL if self._probe_result is True else None

    async def _req(self, method: str, path: str, *, ok_404: bool = False, **kw: Any) -> dict[str, Any] | None:
        why = await self.refusal()
        if why:
            raise StoreError(why, code="qdrant_unauthenticated", status=503)
        try:
            r = await self._client.request(method, path, headers=self._headers(), **kw)
        except httpx.TransportError as e:
            raise StoreError(f"qdrant unreachable ({type(e).__name__})") from e
        if r.status_code == 404 and ok_404:
            return None
        if r.status_code >= 400:
            # Do not relay the body: it can echo payload text back into logs/responses.
            raise StoreError(f"qdrant returned HTTP {r.status_code}")
        try:
            body = r.json()
        except ValueError as e:
            raise StoreError("qdrant returned non-JSON") from e
        return body if isinstance(body, dict) else {}

    async def ensure(self, ns: str, dim: int) -> None:
        c = self.collection(ns)
        info = await self._req("GET", f"/collections/{c}", ok_404=True)
        if info is not None:
            size = (((info.get("result") or {}).get("config") or {}).get("params") or {}).get("vectors", {}).get("size")
            if size != dim:
                raise StoreDimMismatch("existing qdrant collection has a different dimension")
            return
        await self._req("PUT", f"/collections/{c}", json={
            "vectors": {"size": dim, "distance": "Cosine", "on_disk": self._vec_disk},
            "hnsw_config": {"on_disk": True},
            "on_disk_payload": True,
        })
        # index the fields we always filter on
        await self._req("PUT", f"/collections/{c}/index", json={"field_name": "meta.doc_id", "field_schema": "keyword"})

    async def upsert(self, ns: str, records: list[Record]) -> int:
        check_ids([r.id for r in records])
        c = self.collection(ns)
        for i in range(0, len(records), 128):
            pts = [{"id": point_id(r.id), "vector": r.vector, "payload": {"_id": r.id, "text": r.text, "meta": r.meta}}
                   for r in records[i:i + 128]]
            await self._req("PUT", f"/collections/{c}/points", params={"wait": "true"}, json={"points": pts})
        return len(records)

    async def query(self, ns: str, vector: list[float], k: int, flt: Node | None, min_score: float | None) -> list[Hit]:
        body: dict[str, Any] = {"query": vector, "limit": k, "with_payload": True}
        if flt is not None:
            body["filter"] = to_qdrant(flt)
        if min_score is not None:
            body["score_threshold"] = min_score
        res = await self._req("POST", f"/collections/{self.collection(ns)}/points/query", json=body)
        try:
            pts = ((res or {}).get("result") or {}).get("points") or []
            return [Hit(p["payload"]["_id"], float(p["score"]), p["payload"].get("text", ""), p["payload"].get("meta", {}))
                    for p in pts]
        except (KeyError, TypeError, ValueError) as e:
            raise StoreError("qdrant returned an unexpected query response") from e

    async def _count(self, ns: str, flt: Node | None) -> int:
        body: dict[str, Any] = {"exact": True}
        if flt is not None:
            body["filter"] = to_qdrant(flt)
        res = await self._req("POST", f"/collections/{self.collection(ns)}/points/count", json=body)
        return int((((res or {}).get("result")) or {}).get("count", 0))

    async def delete(self, ns: str, flt: Node) -> int:
        n = await self._count(ns, flt)
        await self._req("POST", f"/collections/{self.collection(ns)}/points/delete", params={"wait": "true"},
                        json={"filter": to_qdrant(flt)})
        return n

    async def stats(self, ns: str) -> StoreStats:
        info = await self._req("GET", f"/collections/{self.collection(ns)}")
        res = (info or {}).get("result") or {}
        n = int(res.get("points_count") or 0)
        dim = int((((res.get("config") or {}).get("params") or {}).get("vectors") or {}).get("size") or 0)
        # Qdrant's REST API exposes no on-disk size; this is vectors + a rough graph/payload overhead.
        return StoreStats(n, n * (dim * 4 + 1024), disk_bytes_estimated=True)

    async def drop(self, ns: str) -> None:
        await self._req("DELETE", f"/collections/{self.collection(ns)}", ok_404=True)

    async def health(self) -> dict[str, Any]:
        try:
            r = await self._client.get("/readyz")
        except httpx.TransportError as e:
            return {"ok": False, "url": self._url, "error": type(e).__name__}
        why = await self.refusal()
        return {"ok": r.status_code == 200 and why is None, "url": self._url, "status": r.status_code,
                "refused": why is not None, "reason": why,
                "api_key": "[set]" if self.key_configured() else "[unset]",
                "unauthenticated_access": await self.probe_unauthenticated()}
