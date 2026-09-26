from __future__ import annotations

import httpx
import pytest
import respx

from agent_knowledge.chunking import chunk_text
from agent_knowledge.embed import EmbedError, HashEmbedder, RouterEmbedder
from agent_knowledge.filters import Cmp, FilterError, In, Or, evaluate, parse_filter, to_qdrant


def test_chunk_overlap_and_coverage() -> None:
    text = " ".join(f"w{i}" for i in range(600))
    chunks = chunk_text(text, 200, 50)
    assert len(chunks) > 3 and all(len(c) <= 200 for c in chunks)
    # consecutive chunks share text (the overlap) and together cover every word
    assert set(text.split()) <= {w for c in chunks for w in c.split()}
    assert any(chunks[i][-20:].split()[-1] in chunks[i + 1] for i in range(len(chunks) - 1))


def test_chunk_edge_cases() -> None:
    assert chunk_text("   ") == []
    assert chunk_text("short", 100, 10) == ["short"]
    with pytest.raises(ValueError):
        chunk_text("x" * 500, 100, 100)
    with pytest.raises(ValueError):
        chunk_text("x", 10, 0)


@pytest.mark.parametrize("bad", [
    {}, [], "x", {"a b": 1}, {"a; DROP": 1}, {"a": {"$regex": "x"}}, {"a": {"$eq": 1, "$ne": 2}}, {"a": [1]},
    {"a": {"$in": []}}, {"$and": []}, {"a": None}, {"a": "x" * 600}, {"$where": "1"}, {"a": {"$in": list(range(65))}},
    {"$not": {"$not": {"$not": {"$not": {"$not": {"a": 1}}}}}},
])
def test_filter_rejects(bad: object) -> None:
    with pytest.raises(FilterError):
        parse_filter(bad)


def test_filter_semantics_and_qdrant_translation() -> None:
    f = parse_filter({"lang": "py", "n": {"$gte": 3}, "$or": [{"tag": {"$in": ["a", "b"]}}, {"$not": {"x": True}}]})
    assert evaluate(f, {"lang": "py", "n": 3, "tag": "a"})
    assert not evaluate(f, {"lang": "py", "n": 2, "tag": "a"})
    assert not evaluate(f, {"lang": "py", "n": 5, "tag": "z", "x": True})
    q = to_qdrant(parse_filter({"lang": "py", "n": {"$gt": 3}}))
    assert q == {"must": [{"must": [{"key": "meta.lang", "match": {"value": "py"}}]},
                          {"must": [{"key": "meta.n", "range": {"gt": 3}}]}]}
    with pytest.raises(FilterError):
        to_qdrant(Cmp("a", "eq", 1.5))
    with pytest.raises(FilterError):
        to_qdrant(In("a", (1.5,)))
    assert to_qdrant(Or((Cmp("a", "eq", "x"),)))["should"]


def test_injection_strings_are_only_ever_data() -> None:
    nasty = "x' OR 1=1 --\" ]}"
    f = parse_filter({"lang": nasty})
    assert to_qdrant(f)["must"][0]["match"]["value"] == nasty  # typed JSON slot, never concatenated
    assert not evaluate(f, {"lang": "x"})


async def test_hash_embedder_deterministic() -> None:
    e = HashEmbedder(16)
    a, b = await e.embed(["hello world", "hello world"], "m")
    assert a == b and len(a) == 16


@respx.mock
async def test_router_embedder_batches_retries_and_hides_key(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("KEY_ENV", "sekrit-value-123")
    sleeps: list[float] = []

    async def nap(s: float) -> None:
        sleeps.append(s)

    calls = []

    def handler(request: httpx.Request) -> httpx.Response:
        calls.append(request)
        if len(calls) == 1:
            return httpx.Response(503)
        n = len(__import__("json").loads(request.content)["input"])
        return httpx.Response(200, json={"data": [{"index": i, "embedding": [float(i), 1.0]} for i in reversed(range(n))]})

    respx.post("http://router:4000/v1/embeddings").mock(side_effect=handler)
    emb = RouterEmbedder("http://router:4000", "KEY_ENV", batch=2, sleep=nap)
    out = await emb.embed(["a", "b", "c"], "example-embed")
    assert out == [[0.0, 1.0], [1.0, 1.0], [0.0, 1.0]] and len(calls) == 3  # 1 retry + 2 batches
    assert sleeps == [0.5]
    assert calls[0].headers["authorization"] == "Bearer sekrit-value-123"


@respx.mock
async def test_router_embedder_failure_messages_have_no_key(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("KEY_ENV", "sekrit-value-123")
    respx.post("http://router:4000/v1/embeddings").mock(return_value=httpx.Response(500, text="Bearer sekrit-value-123"))

    async def nap(_: float) -> None:
        return None

    with pytest.raises(EmbedError) as ei:
        await RouterEmbedder("http://router:4000", "KEY_ENV", retries=2, sleep=nap).embed(["a"], "m")
    assert "sekrit" not in str(ei.value)
    respx.post("http://router:4000/v1/embeddings").mock(return_value=httpx.Response(404))
    with pytest.raises(EmbedError) as ei2:
        await RouterEmbedder("http://router:4000", "KEY_ENV", sleep=nap).embed(["a"], "m")
    assert ei2.value.status == 400


@respx.mock
async def test_router_embedder_does_not_follow_redirects() -> None:
    respx.post("http://router:4000/v1/embeddings").mock(
        return_value=httpx.Response(307, headers={"location": "http://evil.example/steal"}))
    evil = respx.post("http://evil.example/steal").mock(return_value=httpx.Response(200, json={"data": []}))

    async def nap(_: float) -> None:
        return None

    with pytest.raises(EmbedError):
        await RouterEmbedder("http://router:4000", "K", retries=0, sleep=nap).embed(["a"], "m")
    assert not evil.called


def test_router_url_env_default_and_validation(tmp_path: object, monkeypatch: pytest.MonkeyPatch) -> None:
    from agent_knowledge.config import load_config
    monkeypatch.delenv("AGENT_KNOWLEDGE_CONFIG", raising=False)
    monkeypatch.setenv("KNOWLEDGE_ROUTER_URL", "http://router.example:4000/")
    assert load_config().router_url == "http://router.example:4000"
    monkeypatch.setenv("KNOWLEDGE_ROUTER_URL", "http://user:pw@evil/x?y=1")
    with pytest.raises(ValueError):
        load_config()
