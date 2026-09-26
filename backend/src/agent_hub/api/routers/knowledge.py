"""Same-origin proxy to the knowledge service (GET/POST only, allowlisted prefixes, caps, 503 when down)."""
from __future__ import annotations

from fastapi import APIRouter, Request, Response
from fastapi.responses import JSONResponse

from agent_hub.api.deps import H, principal_of, sse_response
from agent_hub.api.deps import SseLimiter as _Limiter
from agent_hub.domain.errors import Forbidden

router = APIRouter()
ADMIN_ONLY_READS = {"tokens", "indexes", "backends"}
_limiters: dict[int, _Limiter] = {}


def _limiter(hub: H) -> _Limiter:
    key = id(hub)
    if key not in _limiters:
        _limiters[key] = _Limiter(hub.cfg.max_sse_connections)
    return _limiters[key]


@router.api_route("/knowledge/{path:path}", methods=["GET", "POST", "DELETE"])
async def proxy(path: str, request: Request, hub: H) -> Response:
    query = request.url.query
    if path.split("/", 1)[0] in ADMIN_ONLY_READS and principal_of(request).role != "admin":
        raise Forbidden("this knowledge path lists credentials or backend internals: admin only")
    if path == "health" and request.method == "GET":            # a summary for the UI, not a raw passthrough
        return JSONResponse(await hub.knowledge.summary())
    if request.method == "GET" and path.endswith("/stream"):
        return sse_response(hub.knowledge.stream(path, query), _limiter(hub), hub.cfg.sse_idle_timeout_s)
    body = await request.body() if request.method == "POST" else b""
    status, data, ctype = await hub.knowledge.forward(request.method, path, query, body)
    return Response(content=data, status_code=status, media_type=ctype)
