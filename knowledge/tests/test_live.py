"""LIVE tests: real LanceDB in a tmp dir and a real Qdrant. Skipped unless AK_LIVE=1 (Qdrant part also needs
AK_LIVE_QDRANT_URL). Run: AK_LIVE=1 AK_LIVE_QDRANT_URL=http://127.0.0.1:<port> uv run pytest -m live"""
from __future__ import annotations

import os
import uuid
from collections.abc import AsyncIterator
from pathlib import Path

import httpx
import pytest

from agent_knowledge.app import create_app
from agent_knowledge.config import Config
from agent_knowledge.embed import HashEmbedder
from agent_knowledge.filters import Cmp, parse_filter
from agent_knowledge.stores.base import Record, StoreDimMismatch, VectorStore
from agent_knowledge.stores.lance import LanceStore
from agent_knowledge.stores.qdrant import QdrantStore
from tests.conftest import HDR

pytestmark = [pytest.mark.live, pytest.mark.skipif(os.environ.get("AK_LIVE") != "1", reason="set AK_LIVE=1")]
QDRANT = os.environ.get("AK_LIVE_QDRANT_URL")


@pytest.fixture(params=["lancedb", "qdrant"])
async def store(request: pytest.FixtureRequest, tmp_path: Path) -> AsyncIterator[VectorStore]:
    if request.param == "lancedb":
        yield LanceStore(tmp_path / "lance")
    else:
        if not QDRANT:
            pytest.skip("AK_LIVE_QDRANT_URL not set")
        # AK_LIVE_QDRANT_KEY_ENV names the env var holding the throwaway key Qdrant was started with
        s = QdrantStore(QDRANT, api_key_env=os.environ.get("AK_LIVE_QDRANT_KEY_ENV"))
        yield s
        await s.aclose()


async def test_store_contract(store: VectorStore) -> None:
    ns = "t" + uuid.uuid4().hex[:8]
    await store.ensure(ns, 4)
    await store.ensure(ns, 4)  # idempotent
    with pytest.raises(StoreDimMismatch):
        await store.ensure(ns, 8)
    recs = [Record("a#0", "alpha", [1, 0, 0, 0], {"doc_id": "a", "lang": "en", "n": 1, "ok": True}),
            Record("b#0", "beta", [0, 1, 0, 0], {"doc_id": "b", "lang": "fr", "n": 2, "ok": False}),
            Record("c#0", "it's \"quoted\" ' OR 1=1", [0.9, 0.1, 0, 0], {"doc_id": "c", "lang": "en", "n": 3})]
    assert await store.upsert(ns, recs) == 3
    await store.upsert(ns, [recs[0]])  # upsert twice must not duplicate
    assert (await store.stats(ns)).count == 3
    hits = await store.query(ns, [1, 0, 0, 0], 3, None, None)
    assert [h.id for h in hits][:2] == ["a#0", "c#0"] and hits[0].score > 0.99 and hits[0].meta["lang"] == "en"
    assert [h.id for h in await store.query(ns, [1, 0, 0, 0], 3, parse_filter({"lang": "fr"}), None)] == ["b#0"]
    assert {h.id for h in await store.query(ns, [1, 0, 0, 0], 3, parse_filter({"n": {"$gte": 2}}), None)} == {"b#0", "c#0"}
    assert {h.id for h in await store.query(ns, [1, 0, 0, 0], 3, parse_filter({"ok": True}), None)} == {"a#0"}
    assert {h.id for h in await store.query(ns, [1, 0, 0, 0], 3, parse_filter({"$or": [{"lang": "fr"}, {"n": 3}]}), None)} == {"b#0", "c#0"}
    assert {h.id for h in await store.query(ns, [1, 0, 0, 0], 3, parse_filter({"$not": {"lang": "en"}}), None)} == {"b#0"}
    assert await store.query(ns, [1, 0, 0, 0], 3, parse_filter({"lang": "x' OR 1=1 --"}), None) == []
    assert [h.id for h in await store.query(ns, [1, 0, 0, 0], 3, None, 0.95)] == ["a#0", "c#0"]
    with pytest.raises(StoreDimMismatch if isinstance(store, LanceStore) else Exception):
        await store.query(ns, [1, 0], 3, None, None)
    assert await store.delete(ns, Cmp("doc_id", "eq", "a")) == 1
    assert (await store.stats(ns)).count == 2
    assert (await store.stats(ns)).disk_bytes
    assert (await store.health())["ok"] is True
    await store.drop(ns)
    await store.drop(ns)  # dropping twice is fine


@pytest.fixture
async def live_client(tmp_path: Path, store: VectorStore) -> AsyncIterator[tuple[httpx.AsyncClient, dict[str, str], str]]:
    root = tmp_path / "root"
    root.mkdir()
    (root / "a.md").write_text("The mitochondria is the powerhouse of the cell. " * 60)
    (root / "b.md").write_text("Rust ownership and borrowing keep memory safe.")
    cfg = Config(data_dir=tmp_path / "data", ingest_roots=[root])
    app = create_app(cfg, stores={store.name: store}, embedder=HashEmbedder(32))
    admin = (cfg.data_dir / "admin.key").read_text().strip()
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://127.0.0.1:8795", headers=HDR) as c:
        yield c, {"Authorization": f"Bearer {admin}"}, str(root)


async def test_end_to_end_through_api(live_client: tuple[httpx.AsyncClient, dict[str, str], str], store: VectorStore) -> None:
    import asyncio
    c, h, root = live_client
    ns = "e2e" + uuid.uuid4().hex[:6]
    r = await c.post("/namespaces", headers=h, json={"name": ns, "backend": store.name, "dim": 32})
    assert r.status_code == 201, r.text
    r = await c.post(f"/namespaces/{ns}/ingest", headers=h, json={"path": root, "glob": "*.md", "chunk_size": 300, "chunk_overlap": 50})
    jid = r.json()["job"]["id"]
    for _ in range(300):
        j = (await c.get(f"/jobs/{jid}", headers=h)).json()
        if j["state"] in ("done", "failed"):
            break
        await asyncio.sleep(0.05)
    assert j["state"] == "done" and j["docs_done"] == 2, j
    r = await c.post(f"/namespaces/{ns}/query", headers=h, json={"text": "rust borrowing memory", "k": 2, "filter": {"source": "b.md"}})
    assert r.json()["hits"][0]["metadata"]["source"] == "b.md"
    view = (await c.get(f"/namespaces/{ns}", headers=h)).json()
    assert view["count"] >= 2 and view["disk_bytes"]
    assert (await c.delete(f"/namespaces/{ns}", headers=h)).status_code == 204


@pytest.mark.skipif(not QDRANT or not os.environ.get("AK_LIVE_QDRANT_KEY_ENV"), reason="needs a keyed live Qdrant")
async def test_qdrant_enforces_key() -> None:
    assert QDRANT
    keyless = QdrantStore(QDRANT)
    assert await keyless.probe_unauthenticated() is False  # keyless request is refused
    keyed = QdrantStore(QDRANT, api_key_env=os.environ["AK_LIVE_QDRANT_KEY_ENV"])
    h = await keyed.health()
    assert h["unauthenticated_access"] is False and h["refused"] is False and h["ok"] is True
    with pytest.raises(Exception, match="401|403"):
        await keyless.ensure("t" + uuid.uuid4().hex[:6], 4)
    await keyless.aclose()
    await keyed.aclose()
