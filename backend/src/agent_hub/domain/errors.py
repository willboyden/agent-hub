"""Errors that map 1:1 onto RFC 7807 problem responses (see api/errors.py)."""
from __future__ import annotations

from typing import Any


class ProblemError(Exception):
    status = 400
    code = "bad_request"

    def __init__(self, detail: str, *, code: str | None = None, status: int | None = None, **extra: Any) -> None:
        super().__init__(detail)
        self.detail = detail
        if code:
            self.code = code
        if status:
            self.status = status
        self.extra = extra


class BadRequest(ProblemError):
    status, code = 400, "bad_request"


class Unauthorized(ProblemError):
    status, code = 401, "unauthorized"


class Forbidden(ProblemError):
    status, code = 403, "forbidden"


class NotFound(ProblemError):
    status, code = 404, "not_found"


class Conflict(ProblemError):
    status, code = 409, "conflict"


class Unprocessable(ProblemError):
    status, code = 422, "validation_error"


class Unavailable(ProblemError):
    status, code = 503, "unavailable"
