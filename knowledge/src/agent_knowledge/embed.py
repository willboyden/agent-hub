"""Embedders. RouterEmbedder talks only to the configured router URL; the API key comes from an env var NAME and is
never logged or included in errors. HashEmbedder is a deterministic feature-hashing embedder for tests/dev."""
from __future__ import annotations

import asyncio
import hashlib
import logging
import math
import os
import re
from collections.abc import Awaitable, Callable
from typing import Any, Protocol

import httpx

log = logging.getLogger(__name__)


class EmbedError(Exception):
    """Embedding backend failure. `retryable` tells the API whether to answer 502 or 400-class."""

    def __init__(self, message: str, *, status: int = 502) -> None:
        super().__init__(message)
        self.status = status


class Embedder(Protocol):
    async def embed(self, texts: list[str], model: str) -> list[list[float]]: ...


class HashEmbedder:
    """Bag-of-words feature hashing into `dim` buckets, L2-normalised. Similar texts get similar vectors."""

    def __init__(self, dim: int = 32) -> None:
        self.dim = dim

    async def embed(self, texts: list[str], model: str) -> list[list[float]]:
        return [self._one(t) for t in texts]

    def _one(self, text: str) -> list[float]:
        v = [0.0] * self.dim
        for tok in re.findall(r"\w+", text.lower()):
            h = int.from_bytes(hashlib.sha256(tok.encode()).digest()[:8], "big")
            v[h % self.dim] += 1.0 if (h >> 63) & 1 else -1.0
        norm = math.sqrt(sum(x * x for x in v)) or 1.0
        return [x / norm for x in v]


class RouterEmbedder:
    def __init__(self, base_url: str, api_key_env: str, batch: int = 64, retries: int = 4, timeout: float = 60.0,
                 transport: httpx.AsyncBaseTransport | None = None,
                 sleep: Callable[[float], Awaitable[None]] = asyncio.sleep) -> None:
        self._url = f"{base_url.rstrip('/')}/v1/embeddings"
        self._key_env, self._batch, self._retries, self._sleep = api_key_env, batch, retries, sleep
        # follow_redirects=False: a redirect must never carry the bearer key to another host.
        self._client = httpx.AsyncClient(timeout=timeout, follow_redirects=False, transport=transport)

    async def aclose(self) -> None:
        await self._client.aclose()

    def _headers(self) -> dict[str, str]:
        key = os.environ.get(self._key_env, "")
        return {"Authorization": f"Bearer {key}"} if key else {}

    async def embed(self, texts: list[str], model: str) -> list[list[float]]:
        out: list[list[float]] = []
        for i in range(0, len(texts), self._batch):
            out.extend(await self._batch_call(texts[i:i + self._batch], model))
        return out

    async def _batch_call(self, batch: list[str], model: str) -> list[list[float]]:
        last = "unknown"
        for attempt in range(self._retries + 1):
            try:
                r = await self._client.post(self._url, json={"model": model, "input": batch}, headers=self._headers())
            except httpx.TransportError as e:
                last = type(e).__name__  # class name only: messages can embed URLs
            else:
                if r.status_code == 200:
                    return self._parse(r, len(batch))
                last = f"HTTP {r.status_code}"
                if r.status_code not in (408, 429, 500, 502, 503, 504):
                    raise EmbedError(f"router rejected the embeddings request ({last})",
                                     status=400 if r.status_code in (400, 404, 422) else 502)
            if attempt < self._retries:
                await self._sleep(min(0.5 * 2**attempt, 8.0))
        raise EmbedError(f"router embeddings failed after {self._retries + 1} attempts ({last})")

    @staticmethod
    def _parse(r: httpx.Response, n: int) -> list[list[float]]:
        try:
            data: Any = r.json()["data"]
            rows = sorted(data, key=lambda d: d["index"])
            vecs = [[float(x) for x in d["embedding"]] for d in rows]
        except (ValueError, KeyError, TypeError) as e:
            raise EmbedError("router returned a malformed embeddings response") from e
        if len(vecs) != n or len({len(v) for v in vecs}) > 1:
            raise EmbedError("router returned the wrong number of embeddings")
        return vecs
