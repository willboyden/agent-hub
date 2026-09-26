"""FastAPI application factory + `agent-hub` entry point (127.0.0.1:8792 by default)."""
from __future__ import annotations

import contextlib
import logging
from collections.abc import AsyncIterator
from pathlib import Path

import uvicorn
from fastapi import Depends, FastAPI
from fastapi.responses import FileResponse, Response

from agent_hub import __version__
from agent_hub.api.deps import enforce_roles
from agent_hub.api.errors import install_error_handlers, problem
from agent_hub.api.routers import changes, clients, content, knowledge, organize, system
from agent_hub.api.security import SecurityMiddleware
from agent_hub.config import Settings, check_bind, load_settings
from agent_hub.services.hub import Hub

log = logging.getLogger("agent_hub")
API = "/api/v1"
STATIC_DIRS = ("css", "js", "i18n")


def default_frontend_dir() -> Path:
    return Path(__file__).resolve().parents[3] / "frontend"


def create_app(cfg: Settings | None = None, hub: Hub | None = None) -> FastAPI:
    cfg = cfg or load_settings()
    h = hub or Hub(cfg)

    @contextlib.asynccontextmanager
    async def lifespan(app: FastAPI) -> AsyncIterator[None]:
        key_file = h.auth.ensure_bootstrap_key(cfg.data_dir)
        if key_file:
            log.info("first start: admin API key written to %s (0600)", key_file)      # path only, never the key
        elif (cfg.data_dir / "bootstrap-admin.key").exists():
            log.warning("bootstrap admin key file exists: copy the key somewhere safe and DELETE the file after first use")
        try:
            yield
        finally:
            await h.knowledge.aclose()

    # No interactive docs page (Swagger loads scripts from a CDN, which our CSP forbids). The OpenAPI JSON stays, behind auth.
    app = FastAPI(title="Agent Hub", version=__version__, lifespan=lifespan, docs_url=None, redoc_url=None,
                  openapi_url=f"{API}/openapi.json")
    app.state.hub = h
    install_error_handlers(app)
    # inbox first: /memory/inbox must not be captured by /memory/{name}
    for r in (content.inbox_router, system.router, clients.router, content.router, organize.router, changes.router,
              knowledge.router):
        app.include_router(r, prefix=API, dependencies=[Depends(enforce_roles)])
    if cfg.enable_metrics:
        app.include_router(system.root_router)
    app.add_middleware(SecurityMiddleware, hub=h)
    _mount_frontend(app, cfg.frontend_dir or default_frontend_dir())
    return app


def _mount_frontend(app: FastAPI, root: Path) -> None:
    """Static files at `/` with SPA fallback to index.html. Serves ONLY index.html and css/, js/, i18n/."""
    if not root.is_dir():
        return
    root = root.resolve()

    @app.get("/{path:path}", include_in_schema=False)
    async def spa(path: str) -> Response:
        if path.startswith("api/") or path == "api":
            return problem(404, "not_found", "not found", "no such API route")
        top = path.split("/", 1)[0]
        if top in STATIC_DIRS and "/" in path:
            base = (root / top).resolve()
            target = (root / path).resolve()
            if target.is_file() and base in target.parents:          # defeats ../ and symlink escapes
                return FileResponse(target)
            return problem(404, "not_found", "not found", "no such file")
        if "." in path.rsplit("/", 1)[-1] and path != "index.html":
            return problem(404, "not_found", "not found", "no such file")     # a missing asset must not return the SPA
        index = root / "index.html"                                             # SPA fallback for hash-less deep links
        if index.is_file():
            return FileResponse(index)
        return problem(404, "not_found", "not found", "frontend not built")


def run() -> None:
    cfg = load_settings()
    try:
        check_bind(cfg.host)
    except ValueError as exc:
        raise SystemExit(f"error: {exc}") from exc
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s %(message)s")
    uvicorn.run(create_app(cfg), host=cfg.host, port=cfg.port, log_config=None)


if __name__ == "__main__":
    run()
