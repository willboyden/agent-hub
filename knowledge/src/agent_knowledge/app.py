"""FastAPI app factory."""
from __future__ import annotations

import hmac
import logging
import os
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from typing import Any

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from starlette.exceptions import HTTPException as StarletteHTTPException
from starlette.responses import JSONResponse

from agent_knowledge import __version__
from agent_knowledge.api import Ctx, build_routers
from agent_knowledge.config import Config
from agent_knowledge.db import Db, hash_secret
from agent_knowledge.embed import Embedder, RouterEmbedder
from agent_knowledge.errors import KnowledgeError, problem
from agent_knowledge.ratelimit import RateLimiter
from agent_knowledge.security import SecurityMiddleware
from agent_knowledge.service import Job, Service
from agent_knowledge.stores.base import VectorStore
from agent_knowledge.telemetry import Telemetry

log = logging.getLogger(__name__)


def default_stores(cfg: Config) -> dict[str, VectorStore]:
    stores: dict[str, VectorStore] = {}
    from agent_knowledge.stores.lance import LanceStore
    stores["lancedb"] = LanceStore(cfg.lance_path, max_scan_rows=cfg.lance_max_scan_rows, timeout_s=cfg.store_timeout_s,
                                    lock_wait_s=cfg.lance_lock_wait_s, workers=cfg.lance_workers)
    if cfg.qdrant_url:
        from agent_knowledge.stores.qdrant import QdrantStore
        stores["qdrant"] = QdrantStore(cfg.qdrant_url, cfg.qdrant_api_key_env, cfg.qdrant_vectors_on_disk,
                                     timeout=cfg.store_timeout_s,
                                     allow_unauth=os.environ.get("KNOWLEDGE_ALLOW_UNAUTH_QDRANT") == "1")
    return stores


def key_env_warnings(name: str) -> list[str]:
    n = name.upper()
    if n in ("LITELLM_API_KEY", "LITELLM_MASTER_KEY") or "MASTER" in n:
        return [f"embedder key env {name} looks like the router MASTER key; use an embeddings-only virtual key "
                "in KNOWLEDGE_EMBED_API_KEY"]
    return []


async def startup_warnings(stores: dict[str, VectorStore], cfg: Config) -> list[str]:
    out = key_env_warnings(cfg.router_api_key_env)
    # Value check (names only in the message, never values): is the embed key the router master key in disguise?
    mine = os.environ.get(cfg.router_api_key_env, "")
    for other in ("LITELLM_MASTER_KEY", "LITELLM_API_KEY"):
        theirs = os.environ.get(other, "")
        if mine and theirs and other != cfg.router_api_key_env and hmac.compare_digest(mine.encode(), theirs.encode()):
            out.append(f"SECURITY: {cfg.router_api_key_env} holds the SAME value as {other} (the router master key). "
                       "Issue an embeddings-only virtual key and use that instead.")
    q = stores.get("qdrant")
    refusal = getattr(q, "refusal", None)
    if refusal is not None and (why := await refusal()):
        out.append(f"qdrant backend REFUSED: {why}")
    return out


def create_app(cfg: Config, *, stores: dict[str, VectorStore] | None = None, embedder: Embedder | None = None) -> FastAPI:
    db = Db(cfg.data_dir)
    admin_hash = hash_secret(db.admin_key())
    tel = Telemetry()
    emb = embedder or RouterEmbedder(cfg.router_url, cfg.router_api_key_env, batch=cfg.embed_batch)
    svc = Service(cfg, db, stores if stores is not None else default_stores(cfg), emb)

    def on_job(j: Job) -> None:
        tel.jobs.labels(j.state).inc()
        tel.chunks.labels(j.ns).inc(j.chunks)

    svc.metrics_hook = on_job
    ctx = Ctx(cfg, db, svc, tel, RateLimiter(), svc.stores, emb, admin_hash)

    @asynccontextmanager
    async def lifespan(_: FastAPI) -> AsyncIterator[None]:
        for w in await startup_warnings(svc.stores, cfg):
            log.error("startup: %s", w)
        yield
        for s in svc.stores.values():
            close = getattr(s, "aclose", None)
            if close:
                await close()
        close = getattr(emb, "aclose", None)
        if close:
            await close()

    # No OpenAPI/docs endpoints: the contract is hub/docs/ARCHITECTURE.md §7.
    app = FastAPI(title="agent-knowledge", version=__version__, docs_url=None, redoc_url=None, openapi_url=None,
                  lifespan=lifespan)
    app.state.ctx = ctx
    for r in build_routers():
        app.include_router(r)

    @app.get("/health")
    async def health() -> dict[str, Any]:  # unauthenticated liveness only: no names, counts or paths
        return {"ok": True, "version": __version__}

    @app.exception_handler(KnowledgeError)
    async def _ke(_: Request, e: KnowledgeError) -> JSONResponse:
        return problem(e.status, e.code, e.detail, {"Retry-After": "30"} if e.status == 429 else None)

    @app.exception_handler(RequestValidationError)
    async def _val(_: Request, e: RequestValidationError) -> JSONResponse:
        # loc + message only: the default handler echoes the offending input, i.e. private document text
        detail = "; ".join(f"{'.'.join(str(x) for x in err['loc'])}: {err['msg']}" for err in e.errors())[:500]
        return problem(422, "validation_error", detail)

    @app.exception_handler(StarletteHTTPException)
    async def _http(_: Request, e: StarletteHTTPException) -> JSONResponse:
        return problem(e.status_code, {404: "not_found", 405: "method_not_allowed", 413: "payload_too_large"}
                       .get(e.status_code, "http_error"), str(e.detail)[:200])

    @app.exception_handler(Exception)
    async def _500(_: Request, e: Exception) -> JSONResponse:
        log.exception("unhandled error")
        return problem(500, "internal_error", "internal error")

    app.add_middleware(SecurityMiddleware, cfg=cfg, telemetry=tel)
    return app
