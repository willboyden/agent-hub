"""QdrantStore against a faked REST surface (respx). Proves request shapes only; real behaviour is in test_live.py."""
from __future__ import annotations

import json

import httpx
import pytest
import respx

from agent_knowledge.filters import Cmp, parse_filter
from agent_knowledge.stores.base import Record, StoreDimMismatch, StoreError
from agent_knowledge.stores.qdrant import QdrantStore, point_id

U = "http://qd:6333"


@pytest.fixture
def store() -> QdrantStore:
    return QdrantStore(U, allow_unauth=True)


@respx.mock
async def test_ensure_creates_on_disk_collection(store: QdrantStore) -> None:
    respx.get(f"{U}/collections/ak_docs").mock(return_value=httpx.Response(404))
    put = respx.put(f"{U}/collections/ak_docs").mock(return_value=httpx.Response(200, json={"result": True}))
    idx = respx.put(f"{U}/collections/ak_docs/index").mock(return_value=httpx.Response(200, json={}))
    await store.ensure("docs", 768)
    body = json.loads(put.calls[0].request.content)
    assert body["vectors"] == {"size": 768, "distance": "Cosine", "on_disk": True}
    assert body["hnsw_config"] == {"on_disk": True} and body["on_disk_payload"] is True
    assert idx.called


@respx.mock
async def test_ensure_existing_dim_mismatch(store: QdrantStore) -> None:
    info = {"result": {"config": {"params": {"vectors": {"size": 384}}}}}
    respx.get(f"{U}/collections/ak_docs").mock(return_value=httpx.Response(200, json=info))
    await store.ensure("docs", 384)
    with pytest.raises(StoreDimMismatch):
        await store.ensure("docs", 768)


@respx.mock
async def test_upsert_query_delete_shapes(store: QdrantStore) -> None:
    up = respx.put(f"{U}/collections/ak_docs/points").mock(return_value=httpx.Response(200, json={}))
    await store.upsert("docs", [Record("d#0", "hello", [1.0, 0.0], {"doc_id": "d", "lang": "en"})])
    pt = json.loads(up.calls[0].request.content)["points"][0]
    assert pt["id"] == point_id("d#0") and pt["payload"] == {"_id": "d#0", "text": "hello", "meta": {"doc_id": "d", "lang": "en"}}
    assert up.calls[0].request.url.params["wait"] == "true"

    q = respx.post(f"{U}/collections/ak_docs/points/query").mock(return_value=httpx.Response(200, json={"result": {"points": [
        {"id": "x", "score": 0.9, "payload": {"_id": "d#0", "text": "hello", "meta": {"lang": "en"}}}]}}))
    hits = await store.query("docs", [1.0, 0.0], 3, parse_filter({"lang": "en x' OR 1=1"}), 0.5)
    body = json.loads(q.calls[0].request.content)
    assert body["limit"] == 3 and body["score_threshold"] == 0.5 and body["with_payload"] is True
    assert body["filter"] == {"must": [{"key": "meta.lang", "match": {"value": "en x' OR 1=1"}}]}
    assert hits[0].id == "d#0" and hits[0].score == 0.9

    respx.post(f"{U}/collections/ak_docs/points/count").mock(return_value=httpx.Response(200, json={"result": {"count": 4}}))
    de = respx.post(f"{U}/collections/ak_docs/points/delete").mock(return_value=httpx.Response(200, json={}))
    assert await store.delete("docs", Cmp("doc_id", "eq", "d")) == 4
    assert json.loads(de.calls[0].request.content) == {"filter": {"must": [{"key": "meta.doc_id", "match": {"value": "d"}}]}}


@respx.mock
async def test_errors_do_not_echo_body_and_unreachable(store: QdrantStore) -> None:
    respx.post(f"{U}/collections/ak_docs/points/query").mock(return_value=httpx.Response(400, text="payload: PRIVATE TEXT"))
    with pytest.raises(StoreError) as ei:
        await store.query("docs", [1.0], 1, None, None)
    assert "PRIVATE" not in str(ei.value)
    respx.get(f"{U}/readyz").mock(side_effect=httpx.ConnectError("boom"))
    assert (await store.health())["ok"] is False


@respx.mock
async def test_api_key_header_from_env_name(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("QD_KEY", "k-123")
    s = QdrantStore(U, api_key_env="QD_KEY", allow_unauth=True)
    route = respx.get(f"{U}/collections/ak_docs").mock(return_value=httpx.Response(404))
    respx.put(f"{U}/collections/ak_docs").mock(return_value=httpx.Response(200, json={}))
    respx.put(f"{U}/collections/ak_docs/index").mock(return_value=httpx.Response(200, json={}))
    await s.ensure("docs", 4)
    assert route.calls[0].request.headers["api-key"] == "k-123"


@respx.mock
async def test_keyless_qdrant_is_refused_unless_explicitly_allowed(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("KNOWLEDGE_QDRANT_API_KEY", raising=False)
    respx.get(f"{U}/readyz").mock(return_value=httpx.Response(200))
    col = respx.get(f"{U}/collections").mock(return_value=httpx.Response(200, json={}))
    other = respx.get(f"{U}/collections/ak_docs").mock(return_value=httpx.Response(404))
    s = QdrantStore(U, api_key_env="KNOWLEDGE_QDRANT_API_KEY")
    h = await s.health()
    assert h["unauthenticated_access"] is True and h["api_key"] == "[unset]" and h["refused"] is True and h["ok"] is False
    assert "WITHOUT" not in h["reason"] and "API key" in h["reason"]
    assert "api-key" not in col.calls[0].request.headers  # the probe must not send a key
    with pytest.raises(StoreError) as ei:
        await s.ensure("docs", 4)
    assert ei.value.code == "qdrant_unauthenticated" and ei.value.status == 503 and not other.called
    from agent_knowledge.app import startup_warnings
    from agent_knowledge.config import Config
    w = await startup_warnings({"qdrant": s}, Config())
    assert len(w) == 1 and "REFUSED" in w[0]
    # explicit opt-in restores use
    ok = QdrantStore(U, allow_unauth=True)
    respx.put(f"{U}/collections/ak_docs").mock(return_value=httpx.Response(200, json={}))
    respx.put(f"{U}/collections/ak_docs/index").mock(return_value=httpx.Response(200, json={}))
    await ok.ensure("docs", 4)
    # a Qdrant that enforces the key passes the probe
    col.mock(return_value=httpx.Response(401))
    keyed = QdrantStore(U)
    assert (await keyed.health())["refused"] is False and await keyed.refusal() is None


async def test_embed_key_value_equal_to_master_key_warns_without_leaking(monkeypatch: pytest.MonkeyPatch) -> None:
    from agent_knowledge.app import startup_warnings
    from agent_knowledge.config import Config
    monkeypatch.setenv("KNOWLEDGE_EMBED_API_KEY", "sk-same-value-123")
    monkeypatch.setenv("LITELLM_MASTER_KEY", "sk-same-value-123")
    monkeypatch.setenv("LITELLM_API_KEY", "sk-different")
    w = await startup_warnings({}, Config())
    assert len(w) == 1 and "LITELLM_MASTER_KEY" in w[0] and "sk-same" not in w[0]
    monkeypatch.setenv("KNOWLEDGE_EMBED_API_KEY", "sk-own-virtual")
    assert await startup_warnings({}, Config()) == []
