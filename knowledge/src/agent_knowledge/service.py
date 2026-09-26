"""Application service: namespaces, ingest jobs, query, delete. Route handlers do authn/authz; this layer enforces
data-shape rules (dims, caps, metadata validation) so they hold no matter who calls it."""
from __future__ import annotations

import asyncio
import hashlib
import logging
import re
import time
import uuid
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

from agent_knowledge.chunking import chunk_text
from agent_knowledge.config import Config
from agent_knowledge.db import Db
from agent_knowledge.embed import Embedder, EmbedError
from agent_knowledge.errors import KnowledgeError
from agent_knowledge.filters import FIELD, Cmp, FilterError, Scalar, parse_filter
from agent_knowledge.paths import check_glob, iter_files, resolve_base
from agent_knowledge.stores.base import NS_RE, Hit, Record, StoreError, VectorStore

log = logging.getLogger(__name__)
DOC_ID = re.compile(r"(?!.*\.\.)(?!.*//)[A-Za-z0-9._:@/-]{1,150}")  # no ".." anywhere: ids may look like paths
RESERVED = {"doc_id", "chunk", "source"}
TERMINAL = {"done", "failed"}


@dataclass
class Job:
    id: str
    ns: str
    state: str = "queued"  # queued | running | done | failed
    created: float = field(default_factory=time.time)
    started: float | None = None
    finished: float | None = None
    docs_total: int = 0
    docs_done: int = 0
    chunks: int = 0
    skipped: int = 0
    error: str | None = None
    error_code: str | None = None
    reserved: int = field(default=0, repr=False)

    def view(self) -> dict[str, Any]:
        d = asdict(self)
        d.pop("reserved")
        return d


@dataclass
class PreparedDoc:
    doc_id: str
    chunks: list[str]
    meta: dict[str, Scalar]


def clean_meta(meta: dict[str, Any]) -> dict[str, Scalar]:
    if len(meta) > 32:
        raise KnowledgeError(422, "invalid_metadata", "at most 32 metadata keys")
    out: dict[str, Scalar] = {}
    for k, v in meta.items():
        if not FIELD.fullmatch(k) or k in RESERVED:
            raise KnowledgeError(422, "invalid_metadata", "metadata keys must match [A-Za-z_][A-Za-z0-9_]* and not be reserved")
        if not isinstance(v, bool | int | float | str) or (isinstance(v, str) and len(v) > 1024):
            raise KnowledgeError(422, "invalid_metadata", "metadata values must be scalars (strings <= 1024 chars)")
        out[k] = v
    return out


class Service:
    def __init__(self, cfg: Config, db: Db, stores: dict[str, VectorStore], embedder: Embedder) -> None:
        self.cfg, self.db, self.stores, self.embedder = cfg, db, stores, embedder
        self.jobs: dict[str, Job] = {}
        self._sem = asyncio.Semaphore(2)
        self._ns_locks: dict[str, asyncio.Lock] = {}  # serialise write jobs/deletes per namespace
        self._cap_lock = asyncio.Lock()
        self._reserved: dict[str, int] = {}  # chunks promised to in-flight writes, counted against the cap
        self._tasks: set[asyncio.Task[None]] = set()
        self.metrics_hook: Any = None

    # --- namespaces ---------------------------------------------------------------------------------------------
    def ns_row(self, ns: str) -> dict[str, Any]:
        r = self.db.q("SELECT * FROM namespaces WHERE name=?", (ns,))
        if not r:
            raise KnowledgeError(404, "namespace_not_found", "no such namespace")
        return dict(r[0])

    def store_for(self, row: dict[str, Any]) -> VectorStore:
        s = self.stores.get(row["backend"])
        if s is None:
            raise KnowledgeError(503, "backend_unavailable", f"backend {row['backend']} is not configured")
        return s

    async def create_namespace(self, name: str, backend: str, model: str | None, dim: int, description: str) -> dict[str, Any]:
        if not NS_RE.fullmatch(name):
            raise KnowledgeError(422, "invalid_namespace", "namespace must match [a-z0-9][a-z0-9_-]{0,47}")
        if not 1 <= dim <= 8192:
            raise KnowledgeError(422, "invalid_dim", "dim must be 1..8192")
        if backend not in self.stores:
            raise KnowledgeError(422, "backend_unavailable", f"backend {backend} is not configured")
        if self.db.q("SELECT 1 FROM namespaces WHERE name=?", (name,)):
            raise KnowledgeError(409, "namespace_exists", "namespace already exists")
        try:
            await self.stores[backend].ensure(name, dim)
        except StoreError as e:
            raise KnowledgeError(e.status, e.code, str(e)) from e
        self.db.x("INSERT INTO namespaces VALUES(?,?,?,?,?,?)",
                  (name, backend, model or self.cfg.default_embedding_model, dim, description, time.time()))
        return await self.ns_view(name)

    async def ns_view(self, name: str) -> dict[str, Any]:
        row = self.ns_row(name)
        try:
            st = await self.store_for(row).stats(name)
            count, disk, est = st.count, st.disk_bytes, st.disk_bytes_estimated
        except (StoreError, KnowledgeError):
            count, disk, est = None, None, False  # backend down: still list the namespace
        return {"name": row["name"], "backend": row["backend"], "embedding_model": row["embedding_model"],
                "dim": row["dim"], "description": row["description"], "created": row["created"],
                "count": count, "disk_bytes": disk, "disk_bytes_estimated": est}

    async def drop_namespace(self, name: str) -> None:
        row = self.ns_row(name)
        try:
            await self.store_for(row).drop(name)
        except StoreError as e:
            raise KnowledgeError(e.status, e.code, str(e)) from e
        self.db.x("DELETE FROM namespaces WHERE name=?", (name,))
        self.db.drop_ns_scopes(name)

    # --- embedding helpers ------------------------------------------------------------------------------------
    async def _embed(self, row: dict[str, Any], texts: list[str]) -> list[list[float]]:
        try:
            vecs = await self.embedder.embed(texts, row["embedding_model"])
        except EmbedError as e:
            raise KnowledgeError(e.status, "embedding_failed", str(e)) from e
        if any(len(v) != row["dim"] for v in vecs):
            raise KnowledgeError(409, "dim_mismatch",
                                 f"embedding model {row['embedding_model']} returned a different dimension than the namespace")
        return vecs

    # --- query ----------------------------------------------------------------------------------------------
    async def query(self, ns: str, text: str | None, vector: list[float] | None, k: int, flt_raw: Any,
                    min_score: float | None, model: str | None) -> list[Hit]:
        row = self.ns_row(ns)
        if (text is None) == (vector is None):
            raise KnowledgeError(422, "invalid_query", "send exactly one of text or vector")
        if model is not None and model != row["embedding_model"]:
            raise KnowledgeError(409, "model_mismatch", f"namespace is embedded with {row['embedding_model']}")
        if not 1 <= k <= self.cfg.max_k:
            raise KnowledgeError(422, "invalid_k", f"k must be 1..{self.cfg.max_k}")
        try:
            flt = parse_filter(flt_raw, self.cfg.max_filter_nodes) if flt_raw is not None else None
        except FilterError as e:
            raise KnowledgeError(422, "invalid_filter", str(e)) from e
        if text is not None:
            if len(text) > self.cfg.max_query_chars:
                raise KnowledgeError(413, "query_too_long", "query text too long")
            vector = (await self._embed(row, [text]))[0]
        assert vector is not None
        if len(vector) != row["dim"]:
            raise KnowledgeError(409, "dim_mismatch", f"vector has {len(vector)} dims; namespace expects {row['dim']}")
        try:
            return await self.store_for(row).query(ns, vector, k, flt, min_score)
        except FilterError as e:
            raise KnowledgeError(422, "invalid_filter", str(e)) from e
        except StoreError as e:
            raise KnowledgeError(e.status, e.code, str(e)) from e

    # --- delete -----------------------------------------------------------------------------------------------
    async def delete_document(self, ns: str, doc_id: str) -> int:
        row = self.ns_row(ns)
        if not DOC_ID.fullmatch(doc_id):
            raise KnowledgeError(422, "invalid_id", "invalid document id")
        try:
            async with self.ns_lock(ns):
                return await self.store_for(row).delete(ns, Cmp("doc_id", "eq", doc_id))
        except StoreError as e:
            raise KnowledgeError(e.status, e.code, str(e)) from e

    # --- ingest -----------------------------------------------------------------------------------------------
    def prepare_inline(self, docs: list[dict[str, Any]], size: int, overlap: int) -> list[PreparedDoc]:
        if not 1 <= len(docs) <= self.cfg.max_docs_per_request:
            raise KnowledgeError(422, "invalid_documents", f"send 1..{self.cfg.max_docs_per_request} documents")
        out, seen = [], set()
        for d in docs:
            text = d.get("text")
            if not isinstance(text, str) or not text.strip() or len(text) > self.cfg.max_doc_chars:
                raise KnowledgeError(422, "invalid_documents", f"document text must be 1..{self.cfg.max_doc_chars} chars")
            did = d.get("id") or hashlib.sha256(text.encode()).hexdigest()[:16]
            if not isinstance(did, str) or not DOC_ID.fullmatch(did) or did in seen:
                raise KnowledgeError(422, "invalid_id", "document ids must match [A-Za-z0-9._:@/-]{1,150} and be unique")
            seen.add(did)
            meta = clean_meta(d.get("metadata") or {})
            out.append(PreparedDoc(did, self._chunk(text, size, overlap), meta))
        return out

    @staticmethod
    def _chunk(text: str, size: int, overlap: int) -> list[str]:
        try:
            return chunk_text(text, size, overlap)
        except ValueError as e:
            raise KnowledgeError(422, "invalid_chunking", str(e)) from e

    def ns_lock(self, ns: str) -> asyncio.Lock:
        return self._ns_locks.setdefault(ns, asyncio.Lock())

    async def reserve(self, ns: str, new_chunks: int) -> None:
        """Atomically check the namespace chunk cap and reserve room for a write (released by `release` when the write
        finishes, by which point the store's own count includes the chunks). Two concurrent writers cannot both fit."""
        row = self.ns_row(ns)
        async with self._cap_lock:
            try:
                have = (await self.store_for(row).stats(ns)).count
            except StoreError as e:
                raise KnowledgeError(e.status, e.code, str(e)) from e
            if have + self._reserved.get(ns, 0) + new_chunks > self.cfg.max_chunks_per_namespace:
                raise KnowledgeError(409, "namespace_full", "namespace chunk limit reached")
            self._reserved[ns] = self._reserved.get(ns, 0) + new_chunks

    def release(self, ns: str, n: int) -> None:
        left = self._reserved.get(ns, 0) - n
        if left > 0:
            self._reserved[ns] = left
        else:
            self._reserved.pop(ns, None)

    def start_job(self, ns: str, docs: list[PreparedDoc] | None, path: str | None, glob: str, size: int,
                  overlap: int, reserved: int = 0) -> Job:
        """`reserved` chunks were taken with `reserve()`; the job releases them when it ends (or is refused here)."""
        if len([j for j in self.jobs.values() if j.state not in TERMINAL]) >= 16:
            self.release(ns, reserved)
            raise KnowledgeError(429, "too_many_jobs", "too many active ingest jobs")
        job = Job(uuid.uuid4().hex[:12], ns)
        job.reserved = reserved
        self.jobs[job.id] = job
        while len(self.jobs) > self.cfg.max_jobs_kept:
            oldest = next((j for j in self.jobs.values() if j.state in TERMINAL), None)
            if oldest is None:
                break
            del self.jobs[oldest.id]
        task = asyncio.create_task(self._run(job, docs, path, glob, size, overlap))
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)
        return job

    def plan_path(self, path: str, glob: str) -> tuple[Path, Path, str]:
        base, root = resolve_base(path, self.cfg.ingest_roots)
        return base, root, check_glob(glob)

    async def _run(self, job: Job, docs: list[PreparedDoc] | None, path: str | None, glob: str, size: int,
                   overlap: int) -> None:
        async with self._sem, self.ns_lock(job.ns):  # one write job at a time per namespace: no interleaved replace
            job.state, job.started = "running", time.time()
            try:
                row = self.ns_row(job.ns)
                store = self.store_for(row)
                if docs is not None:
                    job.docs_total = len(docs)
                    for d in docs:
                        await self._ingest_doc(job, row, store, d)
                else:
                    assert path is not None
                    await self._ingest_path(job, row, store, path, glob, size, overlap)
                job.state = "done"
            except KnowledgeError as e:
                job.state, job.error, job.error_code = "failed", e.detail, e.code
            except StoreError as e:
                job.state, job.error, job.error_code = "failed", str(e), e.code
            except Exception:  # noqa: BLE001
                log.exception("ingest job crashed")
                job.state, job.error, job.error_code = "failed", "internal error", "internal_error"
            finally:
                self.release(job.ns, job.reserved)
                job.finished = time.time()
                if self.metrics_hook:
                    self.metrics_hook(job)

    async def _ingest_doc(self, job: Job, row: dict[str, Any], store: VectorStore, d: PreparedDoc,
                          reserve: bool = False) -> None:
        if d.chunks:
            if reserve:  # path jobs only learn their size file by file; inline jobs reserved up front
                await self.reserve(job.ns, len(d.chunks))
                job.reserved += len(d.chunks)
            n = len(d.chunks)
            try:
                vecs = await self._embed(row, d.chunks)
                # replace semantics: drop the document's previous chunks first, so a shrinking doc leaves no stale ones
                await store.delete(job.ns, Cmp("doc_id", "eq", d.doc_id))
                recs = [Record(f"{d.doc_id}#{i}", t, v, {**d.meta, "doc_id": d.doc_id, "chunk": i})
                        for i, (t, v) in enumerate(zip(d.chunks, vecs, strict=True))]
                await store.upsert(job.ns, recs)
                job.chunks += len(recs)
            finally:
                # committed (or failed): the store's own count is now the truth, so drop this doc's reservation
                self.release(job.ns, n)
                job.reserved = max(job.reserved - n, 0)
        job.docs_done += 1

    async def _ingest_path(self, job: Job, row: dict[str, Any], store: VectorStore, path: str, glob: str,
                           size: int, overlap: int) -> None:
        base, root, glob = self.plan_path(path, glob)  # re-resolve at run time: the tree may have changed
        files = await asyncio.to_thread(lambda: list(iter_files(base, root, glob, self.cfg.max_files_per_job)))
        job.docs_total, total_bytes = len(files), 0
        for f in files:
            try:
                raw = await asyncio.to_thread(f.read_bytes)
            except OSError:
                job.skipped += 1
                continue
            total_bytes += len(raw)
            if total_bytes > self.cfg.max_job_bytes:
                raise KnowledgeError(413, "job_too_large", "ingest byte cap reached")
            if b"\x00" in raw[:4096]:
                job.skipped += 1
                continue
            rel = str(f.relative_to(root))
            did = rel if DOC_ID.fullmatch(rel) else "h_" + hashlib.sha256(rel.encode()).hexdigest()[:16]
            text = raw.decode("utf-8", errors="replace")[: self.cfg.max_doc_chars]
            chunks = self._chunk(text, size, overlap)
            await self._ingest_doc(job, row, store, PreparedDoc(did, chunks, {"source": rel[:1024]}),
                                  reserve=True)
