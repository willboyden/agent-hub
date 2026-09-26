"""LanceDB embedded store: one table per namespace under a configured (SSD) directory.

Metadata is stored as a JSON string. LanceDB where-clauses are SQL text, so filters are NOT compiled into SQL:
the validated AST is evaluated in Python over (id, meta_json) to resolve matching ids, and only those ids (which
match a strict regex and therefore cannot carry SQL syntax) are used in `id IN (...)`."""
from __future__ import annotations

import asyncio
import json
import threading
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any

import lancedb
import pyarrow as pa

from agent_knowledge.filters import Cmp, Node, evaluate
from agent_knowledge.stores.base import Hit, Record, StoreDimMismatch, StoreError, StoreStats, check_ids

MAX_FILTER_IDS = 50_000


def _in_clause(ids: list[str]) -> str:
    check_ids(ids)  # regex has no quote/backslash/paren, so the literals below cannot break out
    return "id IN (" + ", ".join(f"'{i}'" for i in ids) + ")"


class LanceStore:
    name = "lancedb"

    def __init__(self, path: Path, *, max_scan_rows: int = 200_000, timeout_s: float = 20.0,
                 lock_wait_s: float = 5.0, workers: int = 4) -> None:
        path.mkdir(parents=True, exist_ok=True, mode=0o700)
        self._path = path
        self._db = lancedb.connect(str(path))
        self._max_scan, self._timeout, self._lock_wait = max_scan_rows, timeout_s, lock_wait_s
        # Blocking Lance work runs on this bounded pool, never on the event loop. Locks are PER NAMESPACE, so a slow
        # or hostile query on one namespace cannot stall the others; waiting for a busy namespace is bounded (429).
        self._pool = ThreadPoolExecutor(max_workers=workers, thread_name_prefix="lance")
        self._locks: dict[str, threading.Lock] = {}
        self._locks_guard = threading.Lock()

    def _ns_lock(self, ns: str) -> threading.Lock:
        with self._locks_guard:
            return self._locks.setdefault(ns, threading.Lock())

    @staticmethod
    def table_name(ns: str) -> str:
        return f"ns_{ns}"

    def _tables(self) -> list[str]:
        return list(self._db.list_tables().tables)

    def _tbl(self, ns: str) -> Any:
        return self._db.open_table(self.table_name(ns))

    # --- sync bodies, run in a worker thread under the lock -------------------------------------------------
    def _ensure(self, ns: str, dim: int) -> None:
        name = self.table_name(ns)
        if name in self._tables():
            have = self._tbl(ns).schema.field("vector").type.list_size
            if have != dim:
                raise StoreDimMismatch("existing lance table has a different dimension")
            return
        schema = pa.schema([pa.field("id", pa.string()), pa.field("text", pa.string()),
                            pa.field("meta_json", pa.string()), pa.field("doc_id", pa.string()),
                            pa.field("vector", pa.list_(pa.float32(), dim))])
        self._db.create_table(name, schema=schema)

    def _upsert(self, ns: str, records: list[Record]) -> int:
        check_ids([r.id for r in records])
        if not records:
            return 0
        tbl = self._tbl(ns)
        dim = tbl.schema.field("vector").type.list_size
        if any(len(r.vector) != dim for r in records):
            raise StoreDimMismatch("vector dimension does not match the table")
        rows = [{"id": r.id, "text": r.text, "meta_json": json.dumps(r.meta),
                 "doc_id": str(r.meta.get("doc_id", "")), "vector": r.vector} for r in records]
        tbl.merge_insert("id").when_matched_update_all().when_not_matched_insert_all().execute(rows)
        return len(records)

    def _where(self, ns: str, flt: Node) -> str | None:
        """Cheap path: a plain doc_id equality is answered by an indexed-column predicate, no scan. The doc id is
        re-validated against the strict id regex, so the literal cannot carry SQL."""
        if isinstance(flt, Cmp) and flt.field == "doc_id" and flt.op == "eq" and isinstance(flt.value, str):
            check_ids([flt.value])
            return f"doc_id = '{flt.value}'"
        return None

    def _matching_ids(self, ns: str, flt: Node) -> list[str]:
        tbl = self._tbl(ns)
        if int(tbl.count_rows()) > self._max_scan:
            raise StoreError("namespace too large for this filter on the embedded backend; use a doc_id filter or Qdrant",
                             code="scan_cap", status=422)
        tab = tbl.search().select(["id", "meta_json"]).limit(max(int(tbl.count_rows()), 1)).to_arrow()  # no vector column read
        ids = [i for i, m in zip(tab.column("id").to_pylist(), tab.column("meta_json").to_pylist(), strict=True)
               if evaluate(flt, json.loads(m))]
        if len(ids) > MAX_FILTER_IDS:
            raise StoreError("filter matches too many rows for the embedded backend; narrow it",
                             code="filter_too_broad", status=422)
        return ids

    def _query(self, ns: str, vector: list[float], k: int, flt: Node | None, min_score: float | None) -> list[Hit]:
        tbl = self._tbl(ns)
        dim = tbl.schema.field("vector").type.list_size
        if len(vector) != dim:
            raise StoreDimMismatch("vector dimension does not match the table")
        q = tbl.search(vector).metric("cosine")
        if flt is not None:
            where = self._where(ns, flt)
            if where is None:
                ids = self._matching_ids(ns, flt)
                if not ids:
                    return []
                where = _in_clause(ids)
            q = q.where(where, prefilter=True)
        hits = []
        for row in q.limit(k).to_list():
            score = 1.0 - float(row["_distance"])
            if min_score is None or score >= min_score:
                hits.append(Hit(row["id"], score, row["text"], json.loads(row["meta_json"])))
        return hits

    def _delete(self, ns: str, flt: Node) -> int:
        where = self._where(ns, flt)
        if where is not None:
            n = int(self._tbl(ns).count_rows(where))
            self._tbl(ns).delete(where)
            return n
        ids = self._matching_ids(ns, flt)
        for i in range(0, len(ids), 1000):
            self._tbl(ns).delete(_in_clause(ids[i:i + 1000]))
        return len(ids)

    def _stats(self, ns: str) -> StoreStats:
        tbl = self._tbl(ns)
        d = self._path / f"{self.table_name(ns)}.lance"
        size = sum(f.stat().st_size for f in d.rglob("*") if f.is_file()) if d.exists() else 0
        return StoreStats(int(tbl.count_rows()), size)

    def _drop(self, ns: str) -> None:
        if self.table_name(ns) in self._tables():
            self._db.drop_table(self.table_name(ns))

    async def _run(self, ns: str, fn: Any, *a: Any) -> Any:
        lock = self._ns_lock(ns)

        def call() -> Any:
            if not lock.acquire(timeout=self._lock_wait):
                raise StoreError("namespace is busy; retry shortly", code="store_busy", status=429)
            try:
                return fn(ns, *a)
            finally:
                lock.release()

        fut = asyncio.get_running_loop().run_in_executor(self._pool, call)
        try:
            return await asyncio.wait_for(fut, self._timeout)
        except TimeoutError as e:
            # The worker cannot be killed, but the caller is released and the namespace lock bounds pile-ups.
            raise StoreError("store operation timed out", code="store_timeout", status=429) from e

    async def ensure(self, ns: str, dim: int) -> None:
        await self._run(ns, self._ensure, dim)

    async def upsert(self, ns: str, records: list[Record]) -> int:
        return int(await self._run(ns, self._upsert, records))

    async def query(self, ns: str, vector: list[float], k: int, flt: Node | None, min_score: float | None) -> list[Hit]:
        out: list[Hit] = await self._run(ns, self._query, vector, k, flt, min_score)
        return out

    async def delete(self, ns: str, flt: Node) -> int:
        return int(await self._run(ns, self._delete, flt))

    async def stats(self, ns: str) -> StoreStats:
        out: StoreStats = await self._run(ns, self._stats)
        return out

    async def drop(self, ns: str) -> None:
        await self._run(ns, self._drop)

    async def health(self) -> dict[str, Any]:
        try:
            n = len(self._tables())
        except Exception as e:  # noqa: BLE001
            return {"ok": False, "path": str(self._path), "error": type(e).__name__}
        return {"ok": True, "path": str(self._path), "tables": n}
