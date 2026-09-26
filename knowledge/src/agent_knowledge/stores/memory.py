"""In-memory store: the reference implementation of query/filter semantics (used by offline tests)."""
from __future__ import annotations

import math
from typing import Any

from agent_knowledge.filters import Node, evaluate
from agent_knowledge.stores.base import Hit, Record, StoreDimMismatch, StoreStats, check_ids


class MemoryStore:
    name = "memory"

    def __init__(self) -> None:
        self.data: dict[str, dict[str, Record]] = {}
        self.dims: dict[str, int] = {}

    async def ensure(self, ns: str, dim: int) -> None:
        if self.dims.setdefault(ns, dim) != dim:
            raise StoreDimMismatch("existing collection has a different dimension")
        self.data.setdefault(ns, {})

    async def upsert(self, ns: str, records: list[Record]) -> int:
        check_ids([r.id for r in records])
        for r in records:
            if len(r.vector) != self.dims[ns]:
                raise StoreDimMismatch("vector dimension does not match the collection")
            self.data[ns][r.id] = r
        return len(records)

    async def query(self, ns: str, vector: list[float], k: int, flt: Node | None, min_score: float | None) -> list[Hit]:
        if len(vector) != self.dims[ns]:
            raise StoreDimMismatch("vector dimension does not match the collection")
        hits = []
        for r in self.data[ns].values():
            if flt is not None and not evaluate(flt, r.meta):
                continue
            s = _cos(vector, r.vector)
            if min_score is None or s >= min_score:
                hits.append(Hit(r.id, s, r.text, dict(r.meta)))
        hits.sort(key=lambda h: (-h.score, h.id))
        return hits[:k]

    async def delete(self, ns: str, flt: Node) -> int:
        gone = [i for i, r in self.data[ns].items() if evaluate(flt, r.meta)]
        for i in gone:
            del self.data[ns][i]
        return len(gone)

    async def stats(self, ns: str) -> StoreStats:
        return StoreStats(len(self.data.get(ns, {})), 0)

    async def drop(self, ns: str) -> None:
        self.data.pop(ns, None)
        self.dims.pop(ns, None)

    async def health(self) -> dict[str, Any]:
        return {"ok": True, "kind": "memory"}


def _cos(a: list[float], b: list[float]) -> float:
    na, nb = math.sqrt(sum(x * x for x in a)), math.sqrt(sum(x * x for x in b))
    return sum(x * y for x, y in zip(a, b, strict=True)) / (na * nb) if na and nb else 0.0
