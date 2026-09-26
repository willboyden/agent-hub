from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Any

import httpx
import pytest

from agent_knowledge.app import create_app
from agent_knowledge.config import Config
from agent_knowledge.embed import HashEmbedder
from agent_knowledge.stores.memory import MemoryStore

HDR = {"Host": "127.0.0.1:8795"}


class Hub:
    def __init__(self, client: httpx.AsyncClient, admin: str, cfg: Config, app: object) -> None:
        self.c, self.admin, self.cfg, self.app = client, admin, cfg, app

    def h(self, tok: str | None = None) -> dict[str, str]:
        return {"Authorization": f"Bearer {tok or self.admin}"}

    async def call(self, method: str, path: str, tok: str | None = None, **kw: Any) -> httpx.Response:
        extra: dict[str, str] = kw.pop("headers", {})
        return await self.c.request(method, path, headers={**self.h(tok), **extra}, **kw)

    async def mk_ns(self, name: str = "docs", backend: str = "lancedb", dim: int = 32) -> None:
        r = await self.call("POST", "/namespaces", json={"name": name, "backend": backend, "dim": dim})
        assert r.status_code == 201, r.text

    async def mk_token(self, scopes: list[dict[str, str]], **kw: Any) -> str:
        r = await self.call("POST", "/tokens", json={"client": "tester", "scopes": scopes, **kw})
        assert r.status_code == 201, r.text
        return str(r.json()["token"])

    async def wait_job(self, jid: str, tok: str | None = None) -> dict[str, object]:
        for _ in range(200):
            j = (await self.call("GET", f"/jobs/{jid}", tok)).json()
            if j["state"] in ("done", "failed"):
                return dict(j)
            await asyncio.sleep(0.02)
        raise AssertionError("job did not finish")

    async def ingest(self, ns: str, docs: list[dict[str, Any]], tok: str | None = None) -> dict[str, object]:
        r = await self.call("POST", f"/namespaces/{ns}/ingest", tok, json={"documents": docs})
        assert r.status_code == 202, r.text
        return await self.wait_job(r.json()["job"]["id"], tok)


@pytest.fixture
def root(tmp_path: Path) -> Path:
    r = tmp_path / "root"
    r.mkdir()
    return r


@pytest.fixture
async def hub(tmp_path: Path, root: Path) -> AsyncIterator[Hub]:
    cfg = Config(data_dir=tmp_path / "data", ingest_roots=[root], index_roots=[root])
    app = create_app(cfg, stores={"lancedb": MemoryStore(), "qdrant": MemoryStore()}, embedder=HashEmbedder(32))
    admin = (cfg.data_dir / "admin.key").read_text().strip()
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://127.0.0.1:8795",
                                 headers=HDR) as client:
        yield Hub(client, admin, cfg, app)
