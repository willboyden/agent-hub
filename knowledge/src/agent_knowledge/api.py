"""HTTP routes. EVERY router carries the `authenticate` dependency, and every handler then names the scope it needs
(admin / write on ns / read on ns). tests/test_scopes.py walks the route table to prove no route is left open."""
from __future__ import annotations

import asyncio
import hmac
import json
import re
import shutil
import time
from collections.abc import AsyncIterator
from dataclasses import dataclass
from pathlib import Path
from typing import Annotated, Any, Literal

from fastapi import APIRouter, Depends, Request
from fastapi.responses import Response, StreamingResponse
from pydantic import BaseModel, ConfigDict, Field

from agent_knowledge.config import Config
from agent_knowledge.db import Db, Principal, hash_secret
from agent_knowledge.embed import Embedder
from agent_knowledge.errors import KnowledgeError
from agent_knowledge.ratelimit import RateLimiter
from agent_knowledge.service import TERMINAL, Job, Service
from agent_knowledge.stores.base import NS_RE, VectorStore
from agent_knowledge.telemetry import Telemetry

CLIENT_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}")
INDEX_NAME = re.compile(r"[a-z0-9][a-z0-9_-]{0,63}")


@dataclass
class Ctx:
    cfg: Config
    db: Db
    svc: Service
    tel: Telemetry
    limiter: RateLimiter
    stores: dict[str, VectorStore]
    embedder: Embedder
    admin_hash: str
    streams: int = 0


def ctx_of(request: Request) -> Ctx:
    c: Ctx = request.app.state.ctx
    return c


async def authenticate(request: Request) -> Principal:
    c = ctx_of(request)
    auth = request.headers.get("authorization", "")
    scheme, _, tok = auth.partition(" ")
    tok = tok.strip()
    p: Principal | None = None
    if scheme.lower() == "bearer" and tok:
        if hmac.compare_digest(hash_secret(tok), c.admin_hash):
            p = Principal("admin", "admin", True, {})
        else:
            p = c.db.token_by_secret(tok)
    if p is None:
        c.tel.denied.labels("unauthenticated").inc()
        raise KnowledgeError(401, "unauthorized", "a valid bearer token is required")
    request.state.principal_id = p.id
    request.scope["state"]["principal_id"] = p.id
    limit = c.cfg.default_rate_per_min if p.admin else (p.rate_per_min or c.cfg.default_rate_per_min)
    if not c.limiter.allow(p.id, limit):
        c.tel.denied.labels("rate_limited").inc()
        raise KnowledgeError(429, "rate_limited", "too many requests; slow down")
    return p


Auth = Annotated[Principal, Depends(authenticate)]


def need_admin(p: Principal) -> None:
    if not p.admin:
        raise KnowledgeError(403, "forbidden", "this operation needs the admin key")


def need_scope(c: Ctx, p: Principal, ns: str, mode: Literal["read", "write"]) -> None:
    # Unknown namespace and no-scope look identical to a non-admin: no namespace enumeration by probing.
    if not NS_RE.fullmatch(ns) or not p.can(ns, mode):
        c.tel.denied.labels("scope").inc()
        raise KnowledgeError(403 if p.can(ns, "read") else 404, "forbidden" if p.can(ns, "read") else "namespace_not_found",
                             f"token lacks {mode} scope on this namespace" if p.can(ns, "read") else "no such namespace")


def spend(c: Ctx, p: Principal, field: Literal["queries", "writes"], n: int) -> None:
    if p.admin:
        return
    limit = p.daily_queries if field == "queries" else p.daily_writes
    if not c.db.bump(p.id, field, n, limit):
        c.tel.denied.labels("quota").inc()
        raise KnowledgeError(429, "quota_exceeded", f"daily {field} quota exhausted for this token")


async def admin_only(p: Auth) -> None:
    need_admin(p)


def scoped(mode: Literal["read", "write"]) -> Any:
    """Route dependency: runs BEFORE body validation, so a wrong token never sees a 422 that reveals the schema."""
    async def dep(request: Request, p: Auth) -> None:
        need_scope(ctx_of(request), p, request.path_params["ns"], mode)
    return Depends(dep)


ADMIN = Depends(admin_only)


class Strict(BaseModel):
    model_config = ConfigDict(extra="forbid")


class NsCreate(Strict):
    name: str
    backend: Literal["qdrant", "lancedb"]
    embedding_model: str | None = Field(default=None, max_length=128, pattern=r"^[A-Za-z0-9._:/-]+$")
    dim: int
    description: str = Field(default="", max_length=500)


class NsUpdate(Strict):
    description: str | None = Field(default=None, max_length=500)
    backend: str | None = None
    embedding_model: str | None = None
    dim: int | None = None


class DocIn(Strict):
    id: str | None = None
    text: str
    metadata: dict[str, Any] = Field(default_factory=dict)


class IngestReq(Strict):
    documents: list[DocIn] | None = None
    path: str | None = None
    glob: str = "**/*"
    chunk_size: int = Field(default=1000, ge=50, le=8000)
    chunk_overlap: int = Field(default=200, ge=0, le=4000)


class QueryReq(Strict):
    text: str | None = None
    vector: list[float] | None = Field(default=None, max_length=8192)
    k: int = 5
    filter: dict[str, Any] | None = None
    min_score: float | None = None
    embedding_model: str | None = None


class ScopeIn(Strict):
    ns: str
    mode: Literal["read", "write"] = "read"  # least privilege unless the admin says otherwise


class TokenCreate(Strict):
    client: str
    scopes: list[ScopeIn] = Field(max_length=64)
    rate_per_min: int | None = Field(default=None, ge=1, le=6000)
    daily_queries: int | None = Field(default=None, ge=0, le=1_000_000)
    daily_writes: int | None = Field(default=None, ge=0, le=10_000_000)


class IndexIn(Strict):
    name: str
    kind: Literal["graphify", "serena"]
    path: str
    project: str = Field(default="", max_length=128)


def build_routers() -> list[APIRouter]:
    dep = [Depends(authenticate)]
    r_ns, r_jobs, r_tok, r_idx, r_sys = (APIRouter(dependencies=dep) for _ in range(5))

    # ---------------------------------------------------------------- namespaces
    @r_ns.get("/namespaces")
    async def list_ns(request: Request, p: Auth) -> dict[str, Any]:
        c = ctx_of(request)
        names = [r["name"] for r in c.db.q("SELECT name FROM namespaces ORDER BY name")]
        items = [await c.svc.ns_view(n) for n in names if p.can(n, "read")]
        return {"items": items, "next_cursor": None}

    @r_ns.post("/namespaces", status_code=201, dependencies=[ADMIN])
    async def create_ns(request: Request, p: Auth, body: NsCreate) -> dict[str, Any]:
        need_admin(p)
        return await ctx_of(request).svc.create_namespace(body.name, body.backend, body.embedding_model, body.dim,
                                                          body.description)

    @r_ns.get("/namespaces/{ns}", dependencies=[scoped("read")])
    async def get_ns(request: Request, p: Auth, ns: str) -> dict[str, Any]:
        c = ctx_of(request)
        need_scope(c, p, ns, "read")
        return await c.svc.ns_view(ns)

    @r_ns.put("/namespaces/{ns}", dependencies=[ADMIN])
    async def put_ns(request: Request, p: Auth, ns: str, body: NsUpdate) -> dict[str, Any]:
        c = ctx_of(request)
        need_admin(p)
        row = c.svc.ns_row(ns)
        for f in ("backend", "embedding_model", "dim"):
            v = getattr(body, f)
            if v is not None and v != row[f]:
                raise KnowledgeError(409, "immutable_field", f"{f} is fixed at creation; create a new namespace and re-ingest")
        if body.description is not None:
            c.db.x("UPDATE namespaces SET description=? WHERE name=?", (body.description, ns))
        return await c.svc.ns_view(ns)

    @r_ns.delete("/namespaces/{ns}", status_code=204, dependencies=[ADMIN])
    async def del_ns(request: Request, p: Auth, ns: str) -> Response:
        need_admin(p)
        await ctx_of(request).svc.drop_namespace(ns)
        return Response(status_code=204)

    @r_ns.post("/namespaces/{ns}/ingest", status_code=202, dependencies=[scoped("write")])
    async def ingest(request: Request, p: Auth, ns: str, body: IngestReq) -> dict[str, Any]:
        c = ctx_of(request)
        need_scope(c, p, ns, "write")
        c.svc.ns_row(ns)
        if (body.documents is None) == (body.path is None):
            raise KnowledgeError(422, "invalid_ingest", "send exactly one of documents or path")
        if body.path is not None:
            need_admin(p)  # reading the host filesystem is an operator action, never a client-token one
            c.svc.plan_path(body.path, body.glob)  # fail fast on traversal / bad glob
            job = c.svc.start_job(ns, None, body.path, body.glob, body.chunk_size, body.chunk_overlap)
        else:
            assert body.documents is not None
            docs = c.svc.prepare_inline([d.model_dump() for d in body.documents], body.chunk_size, body.chunk_overlap)
            n = sum(len(d.chunks) for d in docs)
            await c.svc.reserve(ns, n)  # atomic cap check + reservation, released by the job
            try:
                spend(c, p, "writes", n)
            except KnowledgeError:
                c.svc.release(ns, n)
                raise
            job = c.svc.start_job(ns, docs, None, "", body.chunk_size, body.chunk_overlap, reserved=n)
        return {"job": job.view()}

    @r_ns.post("/namespaces/{ns}/query", dependencies=[scoped("read")])
    async def query(request: Request, p: Auth, ns: str, body: QueryReq) -> dict[str, Any]:
        c = ctx_of(request)
        need_scope(c, p, ns, "read")
        spend(c, p, "queries", 1)
        hits = await c.svc.query(ns, body.text, body.vector, body.k, body.filter, body.min_score, body.embedding_model)
        c.tel.queries.labels(ns).inc()
        return {"hits": [{"id": h.id, "score": h.score, "text": h.text, "metadata": h.meta} for h in hits]}

    @r_ns.delete("/namespaces/{ns}/documents/{doc_id:path}", dependencies=[scoped("write")])
    async def del_doc(request: Request, p: Auth, ns: str, doc_id: str) -> dict[str, Any]:
        c = ctx_of(request)
        need_scope(c, p, ns, "write")
        return {"deleted_chunks": await c.svc.delete_document(ns, doc_id)}

    # ---------------------------------------------------------------- jobs
    def visible(c: Ctx, p: Principal, j: Job) -> bool:
        return p.can(j.ns, "write")

    @r_jobs.get("/jobs")
    async def list_jobs(request: Request, p: Auth) -> dict[str, Any]:
        c = ctx_of(request)
        items = [j.view() for j in reversed(list(c.svc.jobs.values())) if visible(c, p, j)]
        return {"items": items, "next_cursor": None}

    def get_job(c: Ctx, p: Principal, jid: str) -> Job:
        j = c.svc.jobs.get(jid)
        if j is None or not visible(c, p, j):
            raise KnowledgeError(404, "job_not_found", "no such job")
        return j

    @r_jobs.get("/jobs/{jid}")
    async def job(request: Request, p: Auth, jid: str) -> dict[str, Any]:
        return get_job(ctx_of(request), p, jid).view()

    @r_jobs.get("/jobs/{jid}/stream")
    async def job_stream(request: Request, p: Auth, jid: str) -> StreamingResponse:
        c = ctx_of(request)
        j = get_job(c, p, jid)
        if c.streams >= c.cfg.sse_max_streams:
            raise KnowledgeError(429, "too_many_streams", "too many open event streams")
        c.streams += 1

        async def gen() -> AsyncIterator[str]:
            deadline = time.monotonic() + c.cfg.sse_max_seconds
            last = ""
            try:
                while time.monotonic() < deadline:
                    snap = json.dumps(j.view())
                    if snap != last:
                        last = snap
                        yield f"event: progress\ndata: {snap}\n\n"
                    if j.state in TERMINAL:
                        yield f"event: end\ndata: {json.dumps({'state': j.state})}\n\n"
                        return
                    await asyncio.sleep(0.25)
            finally:
                c.streams -= 1

        return StreamingResponse(gen(), media_type="text/event-stream")

    # ---------------------------------------------------------------- tokens
    @r_tok.get("/tokens", dependencies=[ADMIN])
    async def list_tokens(request: Request, p: Auth) -> dict[str, Any]:
        c = ctx_of(request)
        need_admin(p)
        items = [{**t, "usage_today": c.db.usage(t["id"])} for t in c.db.list_tokens()]
        return {"items": items, "next_cursor": None}

    @r_tok.post("/tokens", status_code=201, dependencies=[ADMIN])
    async def create_token(request: Request, p: Auth, body: TokenCreate) -> dict[str, Any]:
        c = ctx_of(request)
        need_admin(p)
        if not CLIENT_RE.fullmatch(body.client):
            raise KnowledgeError(422, "invalid_client", "client must match [A-Za-z0-9][A-Za-z0-9._-]{0,63}")
        scopes: dict[str, str] = {}
        for s in body.scopes:
            c.svc.ns_row(s.ns)  # must exist
            if scopes.get(s.ns) == "write":
                continue
            scopes[s.ns] = s.mode
        tid, secret = c.db.add_token(
            body.client, [{"ns": n, "mode": m} for n, m in sorted(scopes.items())],
            body.rate_per_min or c.cfg.default_rate_per_min,
            c.cfg.default_daily_queries if body.daily_queries is None else body.daily_queries,
            c.cfg.default_daily_writes if body.daily_writes is None else body.daily_writes)
        # The secret is shown exactly once; only its hash is stored.
        return {"id": tid, "client": body.client, "scopes": [{"ns": n, "mode": m} for n, m in sorted(scopes.items())],
                "token": secret}

    @r_tok.delete("/tokens/{tid}", status_code=204, dependencies=[ADMIN])
    async def delete_token(request: Request, p: Auth, tid: str) -> Response:
        need_admin(p)
        if not ctx_of(request).db.drop_token(tid):
            raise KnowledgeError(404, "token_not_found", "no such token")
        return Response(status_code=204)

    # ---------------------------------------------------------------- shared-index registry (read-only metadata)
    def idx_view(row: Any, admin: bool) -> dict[str, Any]:
        path = Path(row["path"])
        out: dict[str, Any] = {"name": row["name"], "kind": row["kind"], "project": row["project"],
                               "exists": path.is_dir()}
        if admin:
            out["path"] = row["path"]
            try:
                out["updated"] = path.stat().st_mtime
            except OSError:
                out["updated"] = None
        return out

    @r_idx.get("/indexes", dependencies=[ADMIN])
    async def list_idx(request: Request, p: Auth) -> dict[str, Any]:
        rows = ctx_of(request).db.q("SELECT * FROM indexes ORDER BY name")
        return {"items": [idx_view(r, p.admin) for r in rows], "next_cursor": None}

    @r_idx.post("/indexes", status_code=201, dependencies=[ADMIN])
    async def add_idx(request: Request, p: Auth, body: IndexIn) -> dict[str, Any]:
        c = ctx_of(request)
        need_admin(p)
        if not INDEX_NAME.fullmatch(body.name):
            raise KnowledgeError(422, "invalid_name", "name must match [a-z0-9][a-z0-9_-]{0,63}")
        try:
            real = Path(body.path).resolve(strict=True)
        except (OSError, RuntimeError) as e:
            raise KnowledgeError(422, "path_not_found", "path does not exist") from e
        if not body.path.startswith("/") or not real.is_dir() or not any(
                real == r.resolve() or real.is_relative_to(r.resolve()) for r in c.cfg.index_roots):
            raise KnowledgeError(403, "path_not_allowed", "index path must be a directory under a configured index root")
        if c.db.q("SELECT 1 FROM indexes WHERE name=?", (body.name,)):
            raise KnowledgeError(409, "index_exists", "index already registered")
        c.db.x("INSERT INTO indexes VALUES(?,?,?,?)", (body.name, body.kind, str(real), body.project))
        return idx_view(c.db.q("SELECT * FROM indexes WHERE name=?", (body.name,))[0], True)

    @r_idx.delete("/indexes/{name}", status_code=204, dependencies=[ADMIN])
    async def del_idx(request: Request, p: Auth, name: str) -> Response:
        need_admin(p)
        if ctx_of(request).db.x("DELETE FROM indexes WHERE name=?", (name,)) == 0:
            raise KnowledgeError(404, "index_not_found", "no such index")
        return Response(status_code=204)

    # ---------------------------------------------------------------- system
    @r_sys.get("/backends", dependencies=[ADMIN])
    async def backends(request: Request, p: Auth) -> dict[str, Any]:
        c = ctx_of(request)
        need_admin(p)
        health = {n: await s.health() for n, s in c.stores.items()}
        nss = [await c.svc.ns_view(r["name"]) for r in c.db.q("SELECT name FROM namespaces ORDER BY name")]
        free = shutil.disk_usage(c.cfg.data_dir).free
        from agent_knowledge.app import startup_warnings
        return {"warnings": await startup_warnings(c.stores, c.cfg), "backends": health,
                "namespaces": [{k: n[k] for k in ("name", "backend", "embedding_model", "dim", "count", "disk_bytes")}
                               for n in nss],
                "data_dir_free_bytes": free,
                "embedder": {"router_url": c.cfg.router_url, "api_key_env": c.cfg.router_api_key_env,
                             "api_key": "[set]" if _env_set(c.cfg.router_api_key_env) else "[unset]"}}

    @r_sys.get("/metrics", dependencies=[ADMIN])
    async def metrics(request: Request, p: Auth) -> Response:
        need_admin(p)
        return Response(ctx_of(request).tel.render(), media_type="text/plain; version=0.0.4")

    return [r_ns, r_jobs, r_tok, r_idx, r_sys]


def _env_set(name: str) -> bool:
    import os
    return bool(os.environ.get(name))
