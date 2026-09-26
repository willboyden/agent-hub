from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator
from typing import Annotated

from fastapi import Depends, Request
from fastapi.responses import StreamingResponse
from starlette.background import BackgroundTask

from agent_hub.domain.errors import Forbidden, ProblemError
from agent_hub.services.auth import Principal
from agent_hub.services.hub import Hub

INBOX_PATH = "/api/v1/memory/inbox"


def get_hub(request: Request) -> Hub:
    h: Hub = request.app.state.hub
    return h


H = Annotated[Hub, Depends(get_hub)]


def principal_of(request: Request) -> Principal:
    p: Principal = request.scope.get("state", {}).get("principal", Principal("public", "public"))
    return p


def require_admin(request: Request) -> Principal:
    p = principal_of(request)
    if p.role != "admin":
        raise Forbidden("admin role required")
    return p


Admin = Annotated[Principal, Depends(require_admin)]
SAFE_METHODS = {"GET", "HEAD", "OPTIONS"}


def enforce_roles(request: Request) -> None:
    """Defence in depth behind SecurityMiddleware, attached to EVERY router: a mutating route is admin-only unless it is
    on the exact (method, path) list a lesser role may use, even if the middleware were bypassed or changed."""
    if request.method in SAFE_METHODS:
        return
    p = principal_of(request)
    if p.role == "admin":
        return
    from agent_hub.api.security import client_may, viewer_may
    if p.role == "viewer" and viewer_may(request.method, request.url.path):
        return
    if p.role == "client" and client_may(request.method, request.url.path):
        return
    raise Forbidden("admin role required for this action")


class SseLimiter:
    def __init__(self, limit: int) -> None:
        self.limit, self.active = limit, 0

    def acquire(self) -> bool:
        if self.active >= self.limit:
            return False
        self.active += 1
        return True

    def release(self) -> None:
        self.active = max(0, self.active - 1)


def sse_response(gen: AsyncIterator[bytes], limiter: SseLimiter, idle_s: float) -> StreamingResponse:
    """Connection cap (429 when full) and an idle timeout that ends silent streams."""
    if not limiter.acquire():
        raise ProblemError("too many open streams; close one and retry", code="too_many_streams", status=429)
    released = False

    def release() -> None:
        nonlocal released
        if not released:
            released = True
            limiter.release()

    async def limited() -> AsyncIterator[bytes]:
        try:
            while True:
                try:
                    item = await asyncio.wait_for(gen.__anext__(), idle_s)
                except (StopAsyncIteration, TimeoutError):
                    return
                yield item
        finally:
            release()
            aclose = getattr(gen, "aclose", None)
            if aclose is not None:
                await aclose()

    return StreamingResponse(limited(), media_type="text/event-stream",
                             headers={"Cache-Control": "no-cache", "X-Accel-Buffering": "no"},
                             background=BackgroundTask(release))
