from __future__ import annotations

import asyncio
import os
import stat
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Any

import httpx
import pytest

from agent_knowledge.api import authenticate, build_routers
from tests.conftest import HDR, Hub

DOCS: list[dict[str, Any]] = [{"id": "d1", "text": "the quick brown fox jumps over the lazy dog", "metadata": {"lang": "en", "n": 1}},
        {"id": "d2", "text": "vector databases store embeddings for retrieval", "metadata": {"lang": "en", "n": 2}},
        {"id": "d3", "text": "le renard brun rapide saute", "metadata": {"lang": "fr", "n": 3}}]


async def test_health_open_everything_else_needs_auth(hub: Hub) -> None:
    assert (await hub.c.get("/health")).status_code == 200
    # walk the real route table (FastAPI includes routers lazily, so app.routes is not enough)
    routes = [(m, r.path) for rt in build_routers() for r in rt.routes for m in r.methods - {"HEAD"}]  # type: ignore[attr-defined]
    for rt in build_routers():
        assert any(d.dependency is authenticate for d in rt.dependencies)
    assert len(routes) == 19
    for m, p in routes:
        url = p.replace("{ns}", "docs").replace("{jid}", "x").replace("{tid}", "x").replace("{name}", "x").replace("{doc_id:path}", "x")
        r = await hub.c.request(m, url, headers={"Authorization": "Bearer nope", "Content-Type": "application/json"}, json={})
        assert r.status_code == 401, (m, p, r.status_code)
        r = await hub.c.request(m, url, json={}, headers={"X-Agent-Knowledge": "1"})
        assert r.status_code == 401, (m, p)


async def test_scopes_default_read_only_and_enforced(hub: Hub) -> None:
    await hub.mk_ns("docs")
    await hub.mk_ns("secret")
    await hub.ingest("docs", DOCS)
    ro = await hub.mk_token([{"ns": "docs"}])  # mode omitted -> read
    listing = (await hub.call("GET", "/tokens")).json()["items"]
    assert listing[0]["scopes"] == [{"ns": "docs", "mode": "read"}]
    assert (await hub.call("POST", "/namespaces/docs/query", ro, json={"text": "fox"})).status_code == 200
    # read token cannot write, delete, or touch other namespaces
    r = await hub.call("POST", "/namespaces/docs/ingest", ro, json={"documents": [{"text": "x"}]})
    assert r.status_code == 403 and r.json()["code"] == "forbidden"
    assert (await hub.call("DELETE", "/namespaces/docs/documents/d1", ro)).status_code == 403
    r = await hub.call("POST", "/namespaces/secret/query", ro, json={"text": "fox"})
    assert r.status_code == 404  # indistinguishable from a namespace that does not exist
    assert (await hub.call("POST", "/namespaces/nonexistent/query", ro, json={"text": "fox"})).status_code == 404
    assert (await hub.call("GET", "/namespaces/secret", ro)).status_code == 404
    assert [n["name"] for n in (await hub.call("GET", "/namespaces", ro)).json()["items"]] == ["docs"]
    # admin-only surfaces
    valid = {"POST /namespaces": {"name": "zz", "backend": "lancedb", "dim": 4},
             "POST /indexes": {"name": "zz", "kind": "graphify", "path": "/tmp"}, "PUT /namespaces/docs": {}}
    for m, p in [("GET", "/tokens"), ("POST", "/namespaces"), ("GET", "/backends"), ("GET", "/metrics"),
                 ("DELETE", "/namespaces/docs"), ("POST", "/indexes"), ("PUT", "/namespaces/docs")]:
        r = await hub.call(m, p, ro, json=valid.get(f"{m} {p}", {}))
        assert r.status_code == 403, (m, p, r.status_code)
    # write token may ingest inline but never `path`
    rw = await hub.mk_token([{"ns": "docs", "mode": "write"}])
    assert (await hub.ingest("docs", [{"id": "d9", "text": "hello there"}], rw))["state"] == "done"
    r = await hub.call("POST", "/namespaces/docs/ingest", rw, json={"path": "/tmp", "glob": "*.md"})
    assert r.status_code == 403
    assert (await hub.call("DELETE", "/namespaces/docs/documents/d9", rw)).json()["deleted_chunks"] == 1


async def test_jobs_visible_only_with_write_scope(hub: Hub) -> None:
    await hub.mk_ns("docs")
    job = await hub.ingest("docs", DOCS)
    ro = await hub.mk_token([{"ns": "docs"}])
    assert (await hub.call("GET", f"/jobs/{job['id']}", ro)).status_code == 404
    assert (await hub.call("GET", "/jobs", ro)).json()["items"] == []
    assert len((await hub.call("GET", "/jobs")).json()["items"]) == 1


async def test_query_filters_min_score_and_k(hub: Hub) -> None:
    await hub.mk_ns("docs")
    await hub.ingest("docs", DOCS)
    r = await hub.call("POST", "/namespaces/docs/query", json={"text": "quick fox", "k": 3, "filter": {"lang": "fr"}})
    assert [h["metadata"]["doc_id"] for h in r.json()["hits"]] == ["d3"]
    r = await hub.call("POST", "/namespaces/docs/query", json={"text": "quick brown fox", "k": 1})
    assert r.json()["hits"][0]["id"] == "d1#0"
    r = await hub.call("POST", "/namespaces/docs/query", json={"text": "fox", "min_score": 0.999999, "filter": {"n": {"$gt": 100}}})
    assert r.json()["hits"] == []
    for bad in ({"text": "x", "filter": {"a; drop": 1}}, {"text": "x", "k": 0}, {"text": "x", "k": 10_000},
                {"text": "x", "vector": [0.0] * 32}, {}, {"text": "x", "unknown": 1}):
        assert (await hub.call("POST", "/namespaces/docs/query", json=bad)).status_code == 422, bad


async def test_dim_and_model_mismatch_refused(hub: Hub) -> None:
    await hub.mk_ns("wrongdim", dim=8)  # HashEmbedder emits 32-d
    job = await hub.ingest("wrongdim", DOCS)
    assert job["state"] == "failed" and job["error_code"] == "dim_mismatch"
    r = await hub.call("POST", "/namespaces/wrongdim/query", json={"text": "fox"})
    assert r.status_code == 409 and r.json()["code"] == "dim_mismatch"
    r = await hub.call("POST", "/namespaces/wrongdim/query", json={"vector": [0.1] * 32})
    assert r.status_code == 409
    await hub.mk_ns("ok")
    r = await hub.call("POST", "/namespaces/ok/query", json={"text": "fox", "embedding_model": "other-model"})
    assert r.status_code == 409 and r.json()["code"] == "model_mismatch"
    r = await hub.call("PUT", "/namespaces/ok", json={"dim": 64})
    assert r.status_code == 409 and r.json()["code"] == "immutable_field"
    assert (await hub.call("PUT", "/namespaces/ok", json={"description": "new"})).json()["description"] == "new"


async def test_quota_and_rate_limit(hub: Hub) -> None:
    await hub.mk_ns("docs")
    await hub.ingest("docs", DOCS)
    t = await hub.mk_token([{"ns": "docs"}], daily_queries=2)
    codes = [(await hub.call("POST", "/namespaces/docs/query", t, json={"text": "fox"})).status_code for _ in range(3)]
    assert codes == [200, 200, 429]
    w = await hub.mk_token([{"ns": "docs", "mode": "write"}], daily_writes=2)
    r = await hub.call("POST", "/namespaces/docs/ingest", w, json={"documents": [{"text": "a"}, {"text": "b"}, {"text": "c"}]})
    assert r.status_code == 429 and r.json()["code"] == "quota_exceeded"
    r2 = await hub.call("GET", "/tokens")
    assert r2.json()["items"][0]["usage_today"]["queries"] == 2
    lim = await hub.mk_token([{"ns": "docs"}], rate_per_min=3)
    codes = [(await hub.call("GET", "/namespaces", lim)).status_code for _ in range(5)]
    assert codes == [200, 200, 200, 429, 429]


async def test_namespace_capacity_cap(hub: Hub) -> None:
    hub.app.state.ctx.cfg.max_chunks_per_namespace = 2  # type: ignore[attr-defined]
    await hub.mk_ns("docs")
    r = await hub.call("POST", "/namespaces/docs/ingest", json={"documents": [{"text": "a"}, {"text": "b"}, {"text": "c"}]})
    assert r.status_code == 409 and r.json()["code"] == "namespace_full"


async def test_job_lifecycle_and_sse(hub: Hub) -> None:
    await hub.mk_ns("docs")
    r = await hub.call("POST", "/namespaces/docs/ingest", json={"documents": DOCS})
    job = r.json()["job"]
    assert job["state"] in ("queued", "running", "done")
    async with hub.c.stream("GET", f"/jobs/{job['id']}/stream", headers=hub.h()) as s:
        body = "".join([c async for c in s.aiter_text()])
    assert "event: progress" in body and "event: end" in body and '"state": "done"' in body
    done = await hub.wait_job(job["id"])
    assert done["docs_total"] == 3 and done["docs_done"] == 3 and done["chunks"] == 3 and done["error"] is None
    assert (await hub.call("GET", "/namespaces/docs")).json()["count"] == 3
    assert (await hub.call("GET", "/jobs/nope")).status_code == 404


async def test_reingest_replaces_chunks_and_delete(hub: Hub) -> None:
    await hub.mk_ns("docs")
    long = " ".join(f"word{i}" for i in range(400))
    await hub.ingest("docs", [{"id": "big", "text": long}])
    n1 = (await hub.call("GET", "/namespaces/docs")).json()["count"]
    assert n1 > 1
    await hub.ingest("docs", [{"id": "big", "text": "tiny now"}])
    assert (await hub.call("GET", "/namespaces/docs")).json()["count"] == 1
    assert (await hub.call("DELETE", "/namespaces/docs/documents/big")).json()["deleted_chunks"] == 1
    assert (await hub.call("DELETE", "/namespaces/docs/documents/bad id!")).status_code == 422


async def test_ingest_validation(hub: Hub) -> None:
    await hub.mk_ns("docs")
    p = "/namespaces/docs/ingest"
    bodies: list[dict[str, Any]] = [{}, {"documents": [{"text": "a"}], "path": "/x"}, {"documents": []},
                 {"documents": [{"text": "a", "metadata": {"doc_id": "spoof"}}]},
                 {"documents": [{"text": "a", "metadata": {"bad key": 1}}]},
                 {"documents": [{"text": "a", "metadata": {"k": [1]}}]},
                 {"documents": [{"id": "../x", "text": "a"}]},
                 {"documents": [{"id": "a", "text": "x"}, {"id": "a", "text": "y"}]},
                 {"documents": [{"text": "a"}], "chunk_size": 10}]
    for body in bodies:
        r = await hub.call("POST", p, json=body)
        assert r.status_code == 422, body


async def test_path_ingest_allowlist_traversal_and_symlinks(hub: Hub, root: Path, tmp_path: Path) -> None:
    await hub.mk_ns("docs")
    (root / "sub").mkdir()
    (root / "sub" / "a.md").write_text("alpha document about foxes")
    (root / "b.txt").write_text("beta document")
    (root / ".env").write_text("TOKEN=hunter2")
    (root / "notes.env.txt").write_text("fine")
    (root / "img.bin").write_bytes(b"\x00\x01")
    outside = tmp_path / "outside"
    outside.mkdir()
    (outside / "leak.md").write_text("OUTSIDE SECRET")
    (root / "link.md").symlink_to(outside / "leak.md")
    (root / "linkdir").symlink_to(outside, target_is_directory=True)

    async def go(path: str, glob: str = "**/*") -> httpx.Response:
        return await hub.call("POST", "/namespaces/docs/ingest", json={"path": path, "glob": glob})

    # escapes are refused up front
    for bad in (str(outside), str(root) + "/../outside", "/etc", "relative/path", str(root / "linkdir"), "/"):
        assert (await go(bad)).status_code in (403, 422), bad
    for bad_glob in ("../*", "/etc/*", "**/../../*", "a b", "*;rm"):
        assert (await go(str(root), bad_glob)).status_code == 422, bad_glob
    assert (await go(str(root / "missing"))).status_code == 422
    r = await go(str(root))
    done = await hub.wait_job(r.json()["job"]["id"])
    assert done["state"] == "done"
    hits = (await hub.call("POST", "/namespaces/docs/query", json={"text": "document secret hunter2 outside", "k": 20})).json()["hits"]
    sources = {h["metadata"]["source"] for h in hits}
    assert sources == {"sub/a.md", "b.txt", "notes.env.txt"}  # no .env, no symlink escapes, no binary
    assert not any("OUTSIDE" in h["text"] or "hunter2" in h["text"] for h in hits)


async def test_path_ingest_file_cap(hub: Hub, root: Path) -> None:
    await hub.mk_ns("docs")
    hub.cfg.max_files_per_job = 2
    for i in range(4):
        (root / f"f{i}.md").write_text(f"doc {i}")
    r = await hub.call("POST", "/namespaces/docs/ingest", json={"path": str(root), "glob": "*.md"})
    job = await hub.wait_job(r.json()["job"]["id"])
    assert job["state"] == "failed" and job["error_code"] == "too_many_files"


async def test_tokens_hashed_at_rest_and_revocable(hub: Hub) -> None:
    await hub.mk_ns("docs")
    tok = await hub.mk_token([{"ns": "docs"}])
    dbfile = hub.cfg.data_dir / "knowledge.db"
    assert tok.encode() not in dbfile.read_bytes() and b"WAL" not in b""
    rows = hub.app.state.ctx.db.q("SELECT hash FROM tokens")  # type: ignore[attr-defined]
    assert rows[0]["hash"] != tok and len(rows[0]["hash"]) == 64
    assert stat.S_IMODE(os.stat(hub.cfg.data_dir / "admin.key").st_mode) == 0o600
    assert stat.S_IMODE(os.stat(dbfile).st_mode) == 0o600
    listing = (await hub.call("GET", "/tokens")).text
    assert tok not in listing and "hash" not in listing
    tid = (await hub.call("GET", "/tokens")).json()["items"][0]["id"]
    assert (await hub.call("DELETE", f"/tokens/{tid}")).status_code == 204
    assert (await hub.call("GET", "/namespaces", tok)).status_code == 401


async def test_dropping_namespace_revokes_scopes(hub: Hub) -> None:
    await hub.mk_ns("docs")
    tok = await hub.mk_token([{"ns": "docs", "mode": "write"}])
    assert (await hub.call("DELETE", "/namespaces/docs")).status_code == 204
    await hub.mk_ns("docs")  # same name, new namespace: the old token must not inherit access
    assert (await hub.call("POST", "/namespaces/docs/query", tok, json={"text": "x"})).status_code == 404


async def test_token_creation_validation(hub: Hub) -> None:
    await hub.mk_ns("docs")
    for body in ({"client": "bad client!", "scopes": []}, {"client": "ok", "scopes": [{"ns": "nope"}]},
                 {"client": "ok", "scopes": [{"ns": "docs", "mode": "admin"}]},
                 {"client": "ok", "scopes": [{"ns": "*"}]}):
        assert (await hub.call("POST", "/tokens", json=body)).status_code in (404, 422), body


async def test_indexes_registry(hub: Hub, root: Path, tmp_path: Path) -> None:
    (root / "graphify-out").mkdir()
    r = await hub.call("POST", "/indexes", json={"name": "demo", "kind": "graphify", "path": str(root / "graphify-out"), "project": "demo"})
    assert r.status_code == 201 and r.json()["exists"] is True
    assert (await hub.call("POST", "/indexes", json={"name": "x", "kind": "serena", "path": str(tmp_path)})).status_code == 403
    assert (await hub.call("POST", "/indexes", json={"name": "y", "kind": "other", "path": str(root)})).status_code == 422
    assert "path" in (await hub.call("GET", "/indexes")).json()["items"][0]
    assert (await hub.call("DELETE", "/indexes/demo")).status_code == 204


async def test_backends_and_metrics(hub: Hub) -> None:
    await hub.mk_ns("docs")
    b = (await hub.call("GET", "/backends")).json()
    assert set(b["backends"]) == {"qdrant", "lancedb"} and b["embedder"]["api_key_env"] == "KNOWLEDGE_EMBED_API_KEY" and b["warnings"] == []
    assert b["namespaces"][0]["dim"] == 32 and "sk-" not in str(b)
    await hub.call("POST", "/namespaces/docs/query", json={"text": "x"})
    m = (await hub.call("GET", "/metrics")).text
    assert "agent_knowledge_queries_total" in m and 'ns="docs"' in m


async def test_hardening_host_origin_csrf_headers_bodycap(hub: Hub) -> None:
    r = await hub.c.get("/health", headers={"Host": "evil.example"})
    assert r.status_code == 421 and r.headers["content-type"].startswith("application/problem+json")
    assert (await hub.c.post("/namespaces", headers={"Origin": "http://evil.example"})).status_code == 403
    assert (await hub.c.post("/namespaces", headers={"Origin": "null"})).status_code == 403
    r = await hub.c.post("/namespaces", json={})  # no bearer, no CSRF header
    assert r.status_code == 403 and r.json()["code"] == "csrf_header_required"
    h = (await hub.c.get("/health")).headers
    assert "default-src 'none'" in h["content-security-policy"] and h["x-content-type-options"] == "nosniff"
    assert h["cache-control"] == "no-store" and "access-control-allow-origin" not in h
    r = await hub.c.options("/namespaces", headers={"Origin": "http://127.0.0.1:8795", "Access-Control-Request-Method": "POST"})
    assert "access-control-allow-origin" not in r.headers
    big = b'{"documents":[{"text":"' + b"a" * (5 * 1024 * 1024) + b'"}]}'
    r = await hub.call("POST", "/namespaces/docs/ingest", content=big, headers={"Content-Type": "application/json"})
    assert r.status_code == 413 and r.json()["code"] == "payload_too_large"
    # chunked (no content-length) body over the cap is also stopped
    async def gen() -> AsyncIterator[bytes]:
        for _ in range(6):
            yield b"a" * (1024 * 1024)
    r = await hub.c.post("/namespaces/docs/ingest", content=gen(), headers={**hub.h(), "Content-Type": "application/json"})
    assert r.status_code == 413
    assert (await hub.c.get("/nope", headers=hub.h())).json()["code"] == "not_found"
    assert HDR  # keep import used


async def test_validation_errors_do_not_echo_input(hub: Hub) -> None:
    r = await hub.call("POST", "/namespaces", json={"name": "PRIVATE-TEXT-abc", "backend": "nope", "dim": "x"})
    assert r.status_code == 422 and "PRIVATE-TEXT" not in r.text


async def test_audit_log_is_metadata_only(hub: Hub) -> None:
    await hub.mk_ns("docs")
    await hub.call("POST", "/namespaces/docs/ingest", json={"documents": [{"text": "TOP-SECRET-BODY"}]})
    log = (hub.cfg.data_dir / "audit.jsonl").read_text()
    assert "TOP-SECRET-BODY" not in log and '"who": "admin"' in log
    assert stat.S_IMODE(os.stat(hub.cfg.data_dir / "audit.jsonl").st_mode) == 0o600


async def test_backend_unavailable_and_unknown_backend(hub: Hub) -> None:
    hub.app.state.ctx.svc.stores.pop("qdrant")  # type: ignore[attr-defined]
    r = await hub.call("POST", "/namespaces", json={"name": "q", "backend": "qdrant", "dim": 4})
    assert r.status_code == 422 and r.json()["code"] == "backend_unavailable"


@pytest.mark.parametrize("token", ["", "Bearer", "Basic abc", "bearer  "])
async def test_malformed_auth(hub: Hub, token: str) -> None:
    r = await hub.c.get("/namespaces", headers={"Authorization": token})
    assert r.status_code == 401


async def test_authz_precedes_body_validation_on_every_route(hub: Hub) -> None:
    """INVALID bodies must still yield 401/403/404, never a 422 that reveals the schema."""
    await hub.mk_ns("docs")
    await hub.mk_ns("other")
    ro = await hub.mk_token([{"ns": "docs"}])
    other = await hub.mk_token([{"ns": "other", "mode": "write"}])
    junk = {"documents": 5, "text": 5, "k": "x", "name": 1, "zzz": 1}
    seen = 0
    for rt in build_routers():
        for r in rt.routes:
            for m in r.methods - {"HEAD"}:  # type: ignore[attr-defined]
                path: str = r.path  # type: ignore[attr-defined]
                url = (path.replace("{ns}", "docs").replace("{jid}", "x").replace("{tid}", "x")
                       .replace("{name}", "x").replace("{doc_id:path}", "x"))
                seen += 1
                r0 = await hub.c.request(m, url, json=junk, headers={"X-Agent-Knowledge": "1"})
                assert r0.status_code == 401, (m, path)  # (a) no token
                if path.startswith("/namespaces/{ns}"):
                    r2 = await hub.call(m, url, other, json=junk)  # (c) token scoped to another namespace
                    assert r2.status_code in (403, 404), (m, path, r2.status_code)
                if (m, path) in {("POST", "/namespaces/{ns}/ingest"), ("DELETE", "/namespaces/{ns}/documents/{doc_id:path}")}:
                    assert (await hub.call(m, url, ro, json=junk)).status_code == 403, (m, path)  # (b) read-only on write
                if path in ("/tokens", "/backends", "/metrics", "/indexes"):
                    assert (await hub.call(m, url, ro, json=junk)).status_code == 403, (m, path)
    assert seen == 19


async def test_backends_warns_on_master_key_env_name(hub: Hub) -> None:
    for name in ("LITELLM_API_KEY", "LITELLM_MASTER_KEY", "MY_MASTER_KEY"):
        hub.cfg.router_api_key_env = name
        w = (await hub.call("GET", "/backends")).json()["warnings"]
        assert len(w) == 1 and "MASTER key" in w[0], name


async def test_indexes_require_admin(hub: Hub) -> None:
    ro = await hub.mk_token([])
    assert (await hub.call("GET", "/indexes", ro)).status_code == 403


async def test_capacity_is_atomic_across_concurrent_ingests(hub: Hub) -> None:
    hub.cfg.max_chunks_per_namespace = 3
    await hub.mk_ns("docs")
    two = [{"id": "a", "text": "one"}, {"id": "b", "text": "two"}]
    two2 = [{"id": "c", "text": "three"}, {"id": "d", "text": "four"}]
    r1, r2 = await asyncio.gather(hub.call("POST", "/namespaces/docs/ingest", json={"documents": two}),
                                  hub.call("POST", "/namespaces/docs/ingest", json={"documents": two2}))
    assert sorted([r1.status_code, r2.status_code]) == [202, 409]
    for r in (r1, r2):
        if r.status_code == 202:
            await hub.wait_job(r.json()["job"]["id"])
    assert (await hub.call("GET", "/namespaces/docs")).json()["count"] == 2
    assert hub.app.state.ctx.svc._reserved == {}  # type: ignore[attr-defined]  # reservations released


async def test_path_ingest_enforces_namespace_cap(hub: Hub, root: Path) -> None:
    hub.cfg.max_chunks_per_namespace = 2
    await hub.mk_ns("docs")
    for i in range(4):
        (root / f"f{i}.md").write_text(f"document {i}")
    r = await hub.call("POST", "/namespaces/docs/ingest", json={"path": str(root), "glob": "*.md"})
    job = await hub.wait_job(r.json()["job"]["id"])
    assert job["state"] == "failed" and job["error_code"] == "namespace_full"
    assert (await hub.call("GET", "/namespaces/docs")).json()["count"] == 2
    assert hub.app.state.ctx.svc._reserved == {}  # type: ignore[attr-defined]


async def test_same_namespace_jobs_do_not_interleave(hub: Hub) -> None:
    from agent_knowledge.stores.memory import MemoryStore

    class Slow(MemoryStore):
        async def delete(self, ns: str, flt: object) -> int:
            await asyncio.sleep(0.03)
            return await super().delete(ns, flt)  # type: ignore[arg-type]

        async def upsert(self, ns: str, records: list[Any]) -> int:
            await asyncio.sleep(0.03)
            return await super().upsert(ns, records)

    svc = hub.app.state.ctx.svc  # type: ignore[attr-defined]
    svc.stores["lancedb"] = Slow()
    await hub.mk_ns("docs")
    long = " ".join(f"alpha{i}" for i in range(300))  # several chunks
    a = await hub.call("POST", "/namespaces/docs/ingest", json={"documents": [{"id": "x", "text": long}], "chunk_size": 200, "chunk_overlap": 20})
    b = await hub.call("POST", "/namespaces/docs/ingest", json={"documents": [{"id": "x", "text": "beta only"}]})
    await hub.wait_job(a.json()["job"]["id"])
    await hub.wait_job(b.json()["job"]["id"])
    hits = (await hub.call("POST", "/namespaces/docs/query", json={"text": "alpha beta", "k": 50})).json()["hits"]
    texts = {h["text"] for h in hits}
    # last writer wins whole: either only B's single chunk, never a mix of A's tail chunks under B's head
    assert len(hits) == 1 and texts == {"beta only"}, texts


@pytest.mark.parametrize("path", ["//tokens", "/namespaces//query", "/tokens//", "/namespaces/./docs", "/a/..", "/x\\y"])
async def test_empty_and_dot_path_segments_rejected(hub: Hub, path: str) -> None:
    # raw ASGI: httpx would normalise these paths client-side before they ever reached the app
    sent: list[dict[str, Any]] = []

    async def receive() -> dict[str, Any]:
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(m: dict[str, Any]) -> None:
        sent.append(m)

    scope = {"type": "http", "method": "GET", "path": path, "raw_path": path.encode(), "query_string": b"",
             "headers": [(b"host", f"127.0.0.1:{hub.cfg.port}".encode()), (b"authorization", f"Bearer {hub.admin}".encode())],
             "client": ("127.0.0.1", 1), "server": ("127.0.0.1", hub.cfg.port), "scheme": "http", "http_version": "1.1"}
    await hub.app(scope, receive, send)  # type: ignore[operator]
    assert sent[0]["status"] == 400 and b"invalid_path" in sent[1]["body"], sent


async def test_service_boundary_rejects_double_slash_ids(hub: Hub) -> None:
    await hub.mk_ns("docs")
    r = await hub.call("POST", "/namespaces/docs/ingest", json={"documents": [{"id": "a//b", "text": "x"}]})
    assert r.status_code == 422
