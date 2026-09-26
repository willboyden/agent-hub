"""LanceStore abuse limits, run against the real embedded LanceDB in a tmp dir (fast, offline)."""
from __future__ import annotations

import asyncio
import time
from pathlib import Path
from typing import Any

import pytest

from agent_knowledge.filters import Cmp, parse_filter
from agent_knowledge.stores.base import Record, StoreError
from agent_knowledge.stores.lance import LanceStore


def recs(n: int, doc: str = "d") -> list[Record]:
    return [Record(f"{doc}#{i}", f"t{i}", [1.0, float(i % 7), 0.0, 0.0], {"doc_id": f"{doc}{i % 50}", "n": i}) for i in range(n)]


async def test_scan_cap_rejects_broad_filters_but_not_doc_id(tmp_path: Path) -> None:
    s = LanceStore(tmp_path, max_scan_rows=100)
    await s.ensure("big", 4)
    await s.upsert("big", recs(300))
    with pytest.raises(StoreError) as ei:
        await s.query("big", [1, 0, 0, 0], 3, parse_filter({"n": {"$gt": 5}}), None)
    assert ei.value.status == 422 and ei.value.code == "scan_cap"
    with pytest.raises(StoreError):
        await s.delete("big", parse_filter({"n": {"$gt": 5}}))
    # unfiltered search and doc_id filters (ingest's replace path) still work on a table over the cap
    assert len(await s.query("big", [1, 0, 0, 0], 3, None, None)) == 3
    hits = await s.query("big", [1, 0, 0, 0], 50, Cmp("doc_id", "eq", "d3"), None)
    assert hits and all(h.meta["doc_id"] == "d3" for h in hits)
    assert await s.delete("big", Cmp("doc_id", "eq", "d3")) == 6
    assert (await s.stats("big")).count == 294
    # under the cap, arbitrary filters work
    s2 = LanceStore(tmp_path / "b", max_scan_rows=1000)
    await s2.ensure("small", 4)
    await s2.upsert("small", recs(30))
    assert len(await s2.query("small", [1, 0, 0, 0], 50, parse_filter({"n": {"$lt": 10}}), None)) == 10


async def test_slow_query_times_out_and_other_namespaces_are_not_blocked(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    s = LanceStore(tmp_path, timeout_s=0.3, lock_wait_s=0.2)
    for ns in ("slow", "fast"):
        await s.ensure(ns, 4)
        await s.upsert(ns, recs(5))
    real = s._query

    def slow(ns: str, *a: Any) -> Any:
        if ns == "slow":
            time.sleep(1.0)
        return real(ns, *a)

    monkeypatch.setattr(s, "_query", slow)
    t0 = time.monotonic()
    first = asyncio.create_task(s.query("slow", [1, 0, 0, 0], 3, None, None))
    await asyncio.sleep(0.05)
    # same namespace, lock held by the slow query: bounded wait then 429, not a stall
    with pytest.raises(StoreError) as busy:
        await s.query("slow", [1, 0, 0, 0], 3, None, None)
    assert busy.value.status == 429 and busy.value.code == "store_busy"
    # a different namespace answers immediately while "slow" is stuck
    assert len(await s.query("fast", [1, 0, 0, 0], 3, None, None)) == 3
    with pytest.raises(StoreError) as to:
        await first
    assert to.value.status == 429 and to.value.code == "store_timeout"
    assert time.monotonic() - t0 < 0.9  # caller released well before the 1s query finished
    await asyncio.sleep(1.0)  # let the worker drain so the pool shuts down cleanly


async def test_filter_complexity_cap_via_api(tmp_path: Path) -> None:
    from agent_knowledge.config import Config
    from agent_knowledge.errors import KnowledgeError
    from agent_knowledge.service import Service

    svc = Service(Config(data_dir=tmp_path, max_filter_nodes=4), None, {}, None)  # type: ignore[arg-type]
    big = {"$or": [{"a": i} for i in range(6)]}
    svc.ns_row = lambda ns: {"embedding_model": "m", "dim": 4, "backend": "x"}  # type: ignore[method-assign]
    with pytest.raises(KnowledgeError) as ei:
        await svc.query("ns", None, [0.0] * 4, 3, big, None, None)
    assert ei.value.status == 422 and ei.value.code == "invalid_filter"
