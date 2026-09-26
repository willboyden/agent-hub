"""RFC 7807 problem+json errors. Details never echo request bodies (they may hold private text)."""
from __future__ import annotations

from typing import Any

from starlette.responses import JSONResponse


class KnowledgeError(Exception):
    def __init__(self, status: int, code: str, detail: str) -> None:
        super().__init__(detail)
        self.status, self.code, self.detail = status, code, detail


def problem_body(status: int, code: str, detail: str) -> dict[str, Any]:
    return {"type": f"urn:agent-knowledge:{code}", "title": code.replace("_", " "), "status": status,
            "code": code, "detail": detail}


def problem(status: int, code: str, detail: str, headers: dict[str, str] | None = None) -> JSONResponse:
    return JSONResponse(problem_body(status, code, detail), status_code=status,
                        media_type="application/problem+json", headers=headers)
