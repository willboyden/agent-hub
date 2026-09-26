"""RFC 7807 problem+json for every error path (with a stable `code`). Never echoes request bodies."""
from __future__ import annotations

import logging
from typing import Any

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from starlette.exceptions import HTTPException as StarletteHTTPException

from agent_hub.domain.errors import ProblemError

log = logging.getLogger(__name__)
PROBLEM = "application/problem+json"


def problem(status: int, code: str, title: str, detail: str, **extra: Any) -> JSONResponse:
    body = {"type": f"urn:agent-hub:{code}", "title": title, "status": status, "code": code, "detail": detail, **extra}
    return JSONResponse(body, status_code=status, media_type=PROBLEM)


def install_error_handlers(app: FastAPI) -> None:
    @app.exception_handler(ProblemError)
    async def _problem(_: Request, exc: ProblemError) -> JSONResponse:
        return problem(exc.status, exc.code, exc.code.replace("_", " "), exc.detail, **exc.extra)

    @app.exception_handler(StarletteHTTPException)
    async def _http(_: Request, exc: StarletteHTTPException) -> JSONResponse:
        code = {404: "not_found", 413: "payload_too_large", 405: "method_not_allowed", 401: "unauthorized",
                403: "forbidden"}.get(exc.status_code, "http_error")
        return problem(exc.status_code, code, code.replace("_", " "), str(exc.detail))

    @app.exception_handler(RequestValidationError)
    async def _validation(_: Request, exc: RequestValidationError) -> JSONResponse:
        # drop `input`/`ctx`: they can echo submitted values (potentially secrets)
        errs = [{"loc": list(e.get("loc", ())), "msg": e.get("msg"), "type": e.get("type")} for e in exc.errors()]
        return problem(422, "validation_error", "validation error", "request validation failed", errors=errs)

    @app.exception_handler(Exception)
    async def _unhandled(_: Request, exc: Exception) -> JSONResponse:
        log.error("unhandled %s", type(exc).__name__)
        return problem(500, "internal_error", "internal error", "unexpected server error")
