"""VectorStore port. Async so the Qdrant REST client and thread-offloaded LanceDB share one contract."""
from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Any, Protocol

from agent_knowledge.filters import Node, Scalar

ID_RE = re.compile(r"[A-Za-z0-9._:#@/-]{1,200}")
NS_RE = re.compile(r"[a-z0-9][a-z0-9_-]{0,47}")


class StoreError(Exception):
    def __init__(self, message: str, *, code: str = "backend_error", status: int = 502) -> None:
        super().__init__(message)
        self.code, self.status = code, status


class StoreDimMismatch(StoreError):
    def __init__(self, message: str) -> None:
        super().__init__(message, code="dim_mismatch", status=409)


@dataclass
class Record:
    id: str
    text: str
    vector: list[float]
    meta: dict[str, Scalar] = field(default_factory=dict)


@dataclass
class Hit:
    id: str
    score: float
    text: str
    meta: dict[str, Any]


@dataclass
class StoreStats:
    count: int
    disk_bytes: int | None
    disk_bytes_estimated: bool = False


class VectorStore(Protocol):
    name: str

    async def ensure(self, ns: str, dim: int) -> None: ...
    async def upsert(self, ns: str, records: list[Record]) -> int: ...
    async def query(self, ns: str, vector: list[float], k: int, flt: Node | None, min_score: float | None) -> list[Hit]: ...
    async def delete(self, ns: str, flt: Node) -> int: ...
    async def stats(self, ns: str) -> StoreStats: ...
    async def drop(self, ns: str) -> None: ...
    async def health(self) -> dict[str, Any]: ...


def check_ids(ids: list[str]) -> None:
    for i in ids:
        if not ID_RE.fullmatch(i):
            raise StoreError("invalid id", code="invalid_id", status=422)
