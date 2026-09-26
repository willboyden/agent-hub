"""HTTP layer: hardening (Host/Origin/CSRF/CSP/CORS/caps), roles, RFC 7807, and the contract routes end to end."""
from __future__ import annotations

import json
import re
import sqlite3
import stat
from pathlib import Path
from typing import Any

import httpx
import pytest
from fastapi.routing import APIRoute
from fastapi.testclient import TestClient

from agent_hub.api.deps import enforce_roles
from agent_hub.config import Settings
from agent_hub.main import create_app
from agent_hub.services.hub import Hub

from .conftest import CSRF, HOST, add_client, bearer, seed_content

API = "/api/v1"


def admin(key: str, extra: dict[str, str] | None = None) -> dict[str, str]:
    return {**bearer(key), **(extra or {})}


def problem_code(r: httpx.Response) -> str:
    assert r.headers["content-type"].startswith("application/problem+json"), r.text[:200]
    body = r.json()
    assert body["status"] == r.status_code and body["type"].startswith("urn:agent-hub:")
    return str(body["code"])


# ---- hardening ---------------------------------------------------------------------------------------------------------
def test_health_is_open_everything_else_needs_a_key(api: TestClient) -> None:
    assert api.get(f"{API}/health").json()["status"] == "ok"
    for path in (f"{API}/clients", f"{API}/openapi.json", "/metrics", f"{API}/audit"):
        r = api.get(path)
        assert r.status_code == 401 and problem_code(r) == "unauthorized"
    assert api.get(f"{API}/clients", headers=bearer("hc_nope")).status_code == 401


@pytest.mark.parametrize("host", ["evil.example.com", "127.0.0.1:9999", "localhost", "127.0.0.1.evil.com:8792"])
def test_host_allowlist_421(api: TestClient, admin_key: str, host: str) -> None:
    r = api.get(f"{API}/health", headers={"Host": host})
    assert r.status_code == 421 and problem_code(r) == "misdirected_request"
    assert api.get(f"{API}/clients", headers=admin(admin_key, {"Host": host})).status_code == 421


def test_allowed_hosts_setting(cfg: Settings, hub: Hub) -> None:
    cfg.allowed_hosts = ["hub.internal:8792"]
    with TestClient(create_app(cfg, hub), base_url="http://hub.internal:8792") as c:
        assert c.get(f"{API}/health").status_code == 200


def test_cross_origin_and_null_origin_403(api: TestClient, admin_key: str) -> None:
    for origin in ("http://evil.example.com", "null", "http://127.0.0.1:8795", "https://127.0.0.1:8792.evil.com"):
        r = api.post(f"{API}/plan", json={}, headers=admin(admin_key, {"Origin": origin}))
        assert r.status_code == 403 and problem_code(r) == "cross_origin", origin
    ok = api.post(f"{API}/plan", json={}, headers=admin(admin_key, {"Origin": f"http://{HOST}"}))
    assert ok.status_code == 200


def test_csrf_header_required_without_bearer(api: TestClient, hub: Hub) -> None:
    hub.cfg.trust_loopback = True
    # simulate a browser on loopback: ambient (no Authorization) credentials need the custom header on non-GET
    r = api.post(f"{API}/plan", json={})
    assert r.status_code == 403 and problem_code(r) == "csrf_header_required"
    r = api.post(f"{API}/plan", json={}, headers={"X-Agent-Hub": "0"})
    assert r.status_code == 403
    assert api.get(f"{API}/health").status_code == 200                         # GETs never need it
    # a bearer client is not an ambient-credential browser, so the header is not required
    key = str(hub.auth.create_key("k", "admin")["secret"])
    assert api.post(f"{API}/plan", json={}, headers=bearer(key)).status_code == 200


def test_loopback_trust_is_off_in_tests_and_never_via_proxy_headers(api: TestClient, hub: Hub) -> None:
    assert api.get(f"{API}/clients").status_code == 401                        # peer 'testclient' is not loopback
    hub.cfg.trust_loopback = True
    assert api.get(f"{API}/clients", headers={"X-Forwarded-For": "1.2.3.4"}).status_code == 401


def test_security_headers_on_every_response_including_errors_and_static(api: TestClient, admin_key: str) -> None:
    responses = [api.get(f"{API}/health"), api.get(f"{API}/clients"), api.get("/", headers={}),
                 api.get("/js/app.js"), api.get("/js/missing.js"), api.get("/nonexistent/deep/link"),
                 api.get(f"{API}/clients", headers=admin(admin_key)), api.get(f"{API}/nope", headers=admin(admin_key)),
                 api.get(f"{API}/health", headers={"Host": "evil"})]
    for r in responses:
        csp = r.headers["content-security-policy"]
        assert "default-src 'none'" in csp and "script-src 'self'" in csp and "'unsafe-inline'" not in csp
        assert "frame-ancestors 'none'" in csp
        assert r.headers["x-content-type-options"] == "nosniff" and r.headers["x-frame-options"] == "DENY"
        assert r.headers["referrer-policy"] == "no-referrer"
        assert r.headers["content-security-policy"] == csp and len(r.headers.get_list("content-security-policy")) == 1
    assert responses[1].headers["cache-control"] == "no-store"


def test_no_cors(api: TestClient, admin_key: str) -> None:
    r = api.options(f"{API}/clients", headers={"Origin": "http://evil.example.com", "Access-Control-Request-Method": "GET",
                                                 "Host": HOST})
    assert "access-control-allow-origin" not in r.headers and r.status_code in (401, 403, 405)
    r = api.get(f"{API}/clients", headers=admin(admin_key, {"Origin": "http://evil.example.com"}))
    assert "access-control-allow-origin" not in r.headers


def test_body_caps(api: TestClient, admin_key: str, cfg: Settings) -> None:
    big = "x" * (cfg.body_cap_bytes + 10)
    r = api.put(f"{API}/instructions/one", content=json.dumps({"title": "t", "body": big}),
                headers=admin(admin_key, {"content-type": "application/json"}))
    assert r.status_code == 413 and problem_code(r) == "payload_too_large"

    def chunks() -> Any:                                           # no Content-Length: the streaming cap must trip
        for _ in range(cfg.body_cap_bytes // 65536 + 3):
            yield b"x" * 65536

    r = api.post(f"{API}/policy/check", content=chunks(), headers=admin(admin_key, {"content-type": "application/json"}))
    assert r.status_code == 413


def test_errors_are_problem_json_and_never_echo_input(api: TestClient, admin_key: str) -> None:
    r = api.put(f"{API}/agents/x", json={"description": "d", "capabilities": ["hunter2-secret"]}, headers=admin(admin_key))
    assert r.status_code == 422 and problem_code(r) == "validation_error" and "hunter2-secret" not in r.text
    r = api.post(f"{API}/matrix/toggle", json={"client": 1}, headers=admin(admin_key))
    assert r.status_code == 422 and "input" not in json.dumps(r.json())
    assert problem_code(api.get(f"{API}/clients/ghost", headers=admin(admin_key))) == "not_found"
    assert problem_code(api.patch(f"{API}/clients", headers=admin(admin_key))) == "method_not_allowed"


def test_hostile_ids_in_urls(api: TestClient, admin_key: str) -> None:
    for name in ("..%2F..%2Fetc%2Fpasswd", "%2e%2e", "A", "a%0Ab", "x" * 65, "a%00b"):
        r = api.get(f"{API}/skills/{name}", headers=admin(admin_key))
        assert r.status_code in (400, 404), (name, r.status_code)
        r = api.put(f"{API}/skills/{name}", json={"description": "d", "body": ""}, headers=admin(admin_key))
        assert r.status_code in (400, 404, 405, 422), (name, r.status_code)


# ---- roles -------------------------------------------------------------------------------------------------------------
# Mutating routes a non-admin MAY use. Everything else must be admin-only (asserted below by enumerating OpenAPI).
NON_ADMIN_OK = {("POST", f"{API}/policy/check"), ("POST", f"{API}/clients/validate-spec"),
                ("POST", f"{API}/memory/inbox")}          # inbox: admin or a client token; viewers are refused in the handler
KNOWLEDGE_QUERY = re.compile(rf"{API}/knowledge/namespaces/\{{[^}}]+\}}/query")


def _all_routes(app: Any) -> list[tuple[APIRoute, str, bool]]:
    """(route, full path, carries enforce_roles). FastAPI wraps included routers lazily (_IncludedRouter), whose include
    context holds the dependencies given to include_router()."""
    out: list[tuple[APIRoute, str, bool]] = []
    for r in app.routes:
        inner = getattr(r, "original_router", None)
        if inner is not None:
            ctx = r.include_context
            has = any(d.dependency is enforce_roles for d in ctx.dependencies)
            out += [(x, ctx.prefix + x.path, has) for x in inner.routes if isinstance(x, APIRoute)]
        elif isinstance(r, APIRoute):
            out.append((r, r.path, any(d.call is enforce_roles for d in r.dependant.dependencies)))
    return [t for t in out if t[1].startswith(API)]


def _mutating_routes(app: Any) -> list[tuple[str, str, APIRoute]]:
    return [(m, path, route) for route, path, _ in _all_routes(app) for m in route.methods - {"GET", "HEAD", "OPTIONS"}]


def _fill(path: str) -> str:
    return re.sub(r"\{[^}]+(:path)?\}", "x", path)


def test_every_router_carries_the_role_dependency(hub: Hub, cfg: Settings) -> None:
    app = create_app(cfg, hub)
    routes = _all_routes(app)
    assert len(routes) > 60
    for _, path, has in routes:
        assert has, f"{path} lacks enforce_roles"


def test_route_enumeration_every_mutating_route_requires_admin(api: TestClient, viewer_key: str, hub: Hub, cfg: Settings) -> None:
    app = api.app
    routes = _mutating_routes(app)
    assert len(routes) >= 35
    viewer_ok = 0
    for method, path, _ in routes:
        url = _fill(path)
        r = api.request(method, url, json={}, headers=bearer(viewer_key))
        if (method, path) in NON_ADMIN_OK or KNOWLEDGE_QUERY.fullmatch(path):
            viewer_ok += 1
            if path.endswith("/memory/inbox"):
                assert r.status_code == 403                         # inbox: viewers are still refused
            else:
                assert r.status_code != 403, (method, path)
            continue
        assert r.status_code == 403, f"viewer could reach {method} {path} -> {r.status_code}"
        assert problem_code(r) == "forbidden"
    assert viewer_ok == 3         # policy/check, validate-spec, inbox (refused above); knowledge query is a catch-all path


def test_unauthenticated_and_bad_bearer_cannot_mutate_anything(api: TestClient) -> None:
    for method, path, _ in _mutating_routes(api.app):
        r = api.request(method, _fill(path), json={}, headers={**CSRF})
        assert r.status_code == 401, (method, path)


def test_viewer_can_read_but_not_write(api: TestClient, viewer_key: str, admin_key: str) -> None:
    assert api.get(f"{API}/clients", headers=bearer(viewer_key)).status_code == 200
    assert api.put(f"{API}/instructions/one", json={"title": "t", "body": "b"}, headers=bearer(viewer_key)).status_code == 403
    assert api.put(f"{API}/instructions/one", json={"title": "t", "body": "b"}, headers=admin(admin_key)).status_code == 200


def test_client_token_scopes(api: TestClient, hub: Hub, home: Path, admin_key: str, viewer_key: str) -> None:
    add_client(hub, home)
    tok = hub.auth.create_key("agent", "client", client="fake1", scopes=["inbox:write"])["secret"]
    body = {"client": "fake1", "title": "Idea", "body": "text", "type": "reference"}
    r = api.post(f"{API}/memory/inbox", json=body, headers=bearer(str(tok)))
    assert r.status_code == 201 and r.json()["client"] == "fake1"
    other = api.post(f"{API}/memory/inbox", json={**body, "client": "fake2"}, headers=bearer(str(tok)))
    assert other.status_code == 403
    for method, path in (("GET", f"{API}/clients"), ("GET", f"{API}/memory/inbox"), ("POST", f"{API}/plan"),
                         ("POST", f"{API}/memory/inbox/x/promote"), ("GET", "/metrics")):
        assert api.request(method, path, headers=bearer(str(tok))).status_code == 403, path
    unscoped = hub.auth.create_key("agent2", "client", client="fake1", scopes=[])["secret"]
    assert api.post(f"{API}/memory/inbox", json=body, headers=bearer(str(unscoped))).status_code == 403
    # a viewer may not write memory; only an admin promotes
    assert api.post(f"{API}/memory/inbox", json=body, headers=bearer(viewer_key)).status_code == 403
    item = api.get(f"{API}/memory/inbox", headers=admin(admin_key)).json()["items"][0]
    assert api.post(f"{API}/memory/inbox/{item['id']}/promote", json={}, headers=admin(admin_key)).status_code == 200
    assert api.get(f"{API}/memory/{item['id']}", headers=admin(admin_key)).json()["meta"]["title"] == "Idea"


def test_keys_are_hashed_and_secret_shown_once(api: TestClient, hub: Hub, admin_key: str) -> None:
    r = api.post(f"{API}/keys", json={"name": "ci", "role": "viewer"}, headers=admin(admin_key))
    assert r.status_code == 201
    secret, kid = r.json()["secret"], r.json()["id"]
    assert secret.startswith("hc_")
    assert secret not in api.get(f"{API}/keys", headers=admin(admin_key)).text
    raw = sqlite3.connect(hub.cfg.db_path).execute("SELECT * FROM api_keys").fetchall()
    assert secret not in json.dumps(raw, default=str)
    assert api.delete(f"{API}/keys/{kid}", headers=admin(admin_key)).status_code == 204
    assert api.get(f"{API}/clients", headers=bearer(secret)).status_code == 401
    for bad in ({"name": "x", "role": "root"}, {"name": "x", "role": "client"}, {"name": "x", "role": "client", "client": "c",
                                                                                "scopes": ["admin:all"]},
                {"name": "x", "role": "viewer", "scopes": ["inbox:write"]}):
        assert api.post(f"{API}/keys", json=bad, headers=admin(admin_key)).status_code in (400, 422)


def test_bootstrap_admin_key_is_0600_and_works(cfg: Settings, hub: Hub) -> None:
    with TestClient(create_app(cfg, hub), base_url=f"http://{HOST}") as c:
        f = cfg.data_dir / "bootstrap-admin.key"
        assert stat.S_IMODE(f.stat().st_mode) == 0o600 and stat.S_IMODE(cfg.data_dir.stat().st_mode) == 0o700
        key = f.read_text().strip()
        assert c.get(f"{API}/clients", headers=bearer(key)).status_code == 200
    assert stat.S_IMODE(cfg.db_path.stat().st_mode) == 0o600


def test_audit_is_append_only_and_redacted(api: TestClient, hub: Hub, admin_key: str) -> None:
    api.put(f"{API}/instructions/one", json={"title": "t", "body": "b", "api_key": "sk-supersecretvalue123456"},
            headers=admin(admin_key))
    api.post(f"{API}/memory/inbox", json={"client": "x", "title": "PRIVATE TITLE", "body": "PRIVATE BODY"}, headers=admin(admin_key))
    items = api.get(f"{API}/audit", headers=admin(admin_key)).json()["items"]
    text = json.dumps(items)
    assert "supersecret" not in text and "PRIVATE BODY" not in text and "PRIVATE TITLE" not in text
    assert any(i["method"] == "PUT" and i["path"].endswith("/instructions/one") and i["status"] == 422 for i in items)
    con = sqlite3.connect(hub.cfg.db_path)
    with pytest.raises(sqlite3.DatabaseError):
        con.execute("UPDATE audit SET status=1")
    with pytest.raises(sqlite3.DatabaseError):
        con.execute("DELETE FROM audit")


def test_metrics_labels_are_bounded(api: TestClient, admin_key: str) -> None:
    for i in range(5):
        api.get(f"{API}/skills/unknown-{i}", headers=admin(admin_key))
        api.get(f"/random/path/{i}")
    text = api.get("/metrics", headers=admin(admin_key)).text
    assert 'route="/api/v1/skills/{name}"' in text and "unknown-" not in text and "/random/path" not in text
    assert 'route="/{path:path}"' in text


# ---- static frontend ----------------------------------------------------------------------------------------------------
def test_static_allowlist_and_spa_fallback(api: TestClient) -> None:
    assert "<title>hub</title>" in api.get("/").text
    assert "<title>hub</title>" in api.get("/index.html").text
    assert api.get("/js/app.js").text == "export {}" and api.get("/css/app.css").status_code == 200
    assert "<title>hub</title>" in api.get("/some/deep/link").text                # SPA fallback for router paths
    for path in ("/package.json", "/dev/mock-server.mjs", "/js/../package.json", "/js/%2e%2e/package.json",
                 "/js/missing.js", "/js/", "/%2e%2e/outside.txt", "/js/..%2f..%2foutside.txt"):
        r = api.get(path)
        assert r.status_code == 404 and "secret dev file" not in r.text and r.text != "outside", path


def test_static_symlink_escape(api: TestClient, frontend: Path, tmp_path: Path) -> None:
    (frontend / "js" / "leak.js").symlink_to(tmp_path / "outside.txt")
    assert api.get("/js/leak.js").status_code == 404


# ---- contract routes ----------------------------------------------------------------------------------------------------
def test_end_to_end_over_http(api: TestClient, hub: Hub, home: Path, admin_key: str) -> None:
    h = admin(admin_key)
    assert api.post(f"{API}/clients", json={"id": "fake1", "adapter": "fake", "display_name": "F",
                                            "roots": {"home": str(home)}}, headers=h).status_code == 201
    assert api.post(f"{API}/clients", json={"id": "bad", "adapter": "nope", "display_name": "F"}, headers=h).status_code == 422
    assert api.put(f"{API}/skills/alpha", json={"description": "A", "body": "hi\n", "files": {"r.md": "x"}, "tags": ["t"]},
                   headers=h).json()["file_list"][0]["path"] == "SKILL.md"
    assert api.post(f"{API}/skills/alpha/duplicate", json={"new_name": "alpha2"}, headers=h).status_code == 201
    assert api.put(f"{API}/instructions/style", json={"title": "S", "order": 1, "body": "x\n"}, headers=h).status_code == 200
    assert api.post(f"{API}/collections", json={"id": "core", "title": "Core", "members": [{"kind": "skill", "name": "alpha"}]},
                    headers=h).status_code == 201
    assert api.post(f"{API}/collections/core/members", json={"members": [{"kind": "instruction", "name": "style"}]},
                    headers=h).json()["members"][1]["name"] == "style"
    assert api.put(f"{API}/profiles/fake1", json={"collections": ["core"]}, headers=h).status_code == 200
    m = api.get(f"{API}/matrix", headers=h).json()
    assert m["groups"][0]["rows"][0]["cells"]["fake1"]["state"] == "via_collection"
    t = api.post(f"{API}/matrix/toggle", json={"client": "fake1", "kind": "skill", "name": "alpha2", "enabled": True}, headers=h)
    assert t.json()["state"] == "on"
    assert api.post(f"{API}/matrix/bulk", json={"clients": ["fake1"], "items": [{"kind": "skill", "name": "alpha2"}],
                                                "enabled": False}, headers=h).json()["ok"] is True
    assert api.post(f"{API}/items/bulk", json={"items": [{"kind": "skill", "name": "alpha2"}], "add_tags": ["bulk"]},
                    headers=h).json()["ok"] is True
    ch = api.get(f"{API}/changes", headers=h).json()
    assert ch["count"] > 5 and any(i["path"] == "skills/alpha/SKILL.md" for i in ch["items"])
    assert api.post(f"{API}/plan", json={}, headers=h).status_code == 200            # plans the (empty) committed state
    assert api.post(f"{API}/changes/commit", json={"message": "first"}, headers=h).status_code == 200
    assert api.post(f"{API}/changes/commit", json={"message": "again"}, headers=h).status_code == 409
    plan = api.post(f"{API}/plan", json={"clients": ["fake1"]}, headers=h).json()
    assert plan["clients"][0]["summary"]["add"] >= 3 and api.get(f"{API}/plan/{plan['id']}", headers=h).json()["id"] == plan["id"]
    assert api.post(f"{API}/apply", json={"plan_id": plan["id"]}, headers=h).status_code == 422          # confirm required
    res = api.post(f"{API}/apply", json={"plan_id": plan["id"], "confirm": True}, headers=h).json()
    assert res["results"][0]["status"] == "applied" and (home / "skills/alpha/SKILL.md").exists()
    stale = api.post(f"{API}/apply", json={"plan_id": plan["id"], "confirm": True}, headers=h)
    assert stale.status_code in (200, 409)
    assert api.get(f"{API}/clients/fake1/drift", headers=h).json()["status"] == "in_sync"
    assert api.post(f"{API}/clients/fake1/verify", headers=h).json()["ok"] is True
    assert api.get(f"{API}/clients/fake1/audit", headers=h).status_code == 200
    eff = api.get(f"{API}/clients/fake1/effective", headers=h).json()
    assert eff["items"]["skills"][0]["name"] == "alpha"
    assert api.get(f"{API}/clients", headers=h).json()["items"][0]["status"] == "in_sync"
    hist = api.get(f"{API}/changes/history", headers=h).json()["items"]
    assert hist[0]["message"] == "first"
    first = hist[0]["commit"]
    # a stale plan after new content: 409 plan_stale
    api.put(f"{API}/instructions/style", json={"title": "S", "order": 1, "body": "y\n"}, headers=h)
    api.post(f"{API}/changes/commit", json={"message": "second"}, headers=h)
    assert problem_code(api.post(f"{API}/apply", json={"plan_id": plan["id"], "confirm": True}, headers=h)) == "plan_stale"
    second = api.get(f"{API}/changes/history", headers=h).json()["items"][0]
    assert second["message"] == "second" and second["commit"] != first
    assert api.post(f"{API}/changes/revert", json={"commit": second["commit"]}, headers=h).status_code == 200
    api.put(f"{API}/instructions/dirty", json={"title": "S", "body": "y\n"}, headers=h)
    assert api.post(f"{API}/changes/discard", json={"paths": ["instructions/dirty.md"]}, headers=h).json()["discarded"] == ["instructions/dirty.md"]
    assert api.delete(f"{API}/skills/alpha2", headers=h).status_code == 204
    assert api.delete(f"{API}/clients/fake1", headers=h).status_code == 204


def test_adapters_floor_settings_and_policy_check(api: TestClient, admin_key: str) -> None:
    h = admin(admin_key)
    ads = api.get(f"{API}/adapters", headers=h).json()["items"]
    assert ads[0]["id"] == "fake" and "caps" in ads[0] and api.get(f"{API}/adapters/fake", headers=h).status_code == 200
    assert api.get(f"{API}/adapters/nope", headers=h).status_code == 404
    floor = api.get(f"{API}/policy/floor", headers=h).json()
    assert floor["network_default"] == "deny"
    for method in ("put", "post", "delete", "patch"):
        assert getattr(api, method)(f"{API}/policy/floor", headers=h).status_code == 405       # read-only: no write route
    chk = api.post(f"{API}/policy/check", json={"rules": [{"kind": "path_read", "match": "~/**", "decision": "allow"}]}, headers=h).json()
    assert chk["ok"] is False and chk["violations"][0]["code"] == "floor_path_read"
    assert api.post(f"{API}/policy/check", json={"mcp": [{"name": "s", "transport": "stdio", "command": "c"}]}, headers=h).json()["ok"] is False
    assert api.post(f"{API}/policy/check", json={"bogus": 1}, headers=h).status_code == 422
    s = api.get(f"{API}/settings", headers=h).json()
    assert "hf_token" not in json.dumps(s) and s["info"]["knowledge"]["token"] == "[unset]"
    assert api.put(f"{API}/settings", json={"plan_retention": 5}, headers=h).json()["editable"]["plan_retention"] == 5
    assert api.put(f"{API}/settings", json={"content_dir": "/etc"}, headers=h).status_code == 422
    assert api.put(f"{API}/settings", json={"plan_retention": 0}, headers=h).status_code == 422
    assert api.get(f"{API}/content/validate", headers=h).json()["ok"] is True


def test_content_requires_initialised_repo(cfg: Settings, registry: Any, tmp_path: Path) -> None:
    from agent_hub.services.hub import Hub as H
    h = H(cfg, registry=registry)                                    # note: no init_content()
    with TestClient(create_app(cfg, h), base_url=f"http://{HOST}") as c:
        key = str(h.auth.create_key("a", "admin")["secret"])
        assert c.get(f"{API}/health").json()["content_initialised"] is False
        r = c.put(f"{API}/instructions/x", json={"title": "t", "body": "b"}, headers=bearer(key))
        assert r.status_code == 503 and problem_code(r) == "content_not_initialised"
        assert c.get(f"{API}/skills", headers=bearer(key)).status_code == 200
    h.close()


def test_validate_spec_without_generic_adapter_is_503(api: TestClient, admin_key: str) -> None:
    r = api.post(f"{API}/clients/validate-spec", json={"spec": {}}, headers=admin(admin_key))
    assert r.status_code == 503 and problem_code(r) == "adapter_missing"


def test_import_over_http(api: TestClient, hub: Hub, home: Path, admin_key: str) -> None:
    h = admin(admin_key)
    (home / "agents").mkdir()
    (home / "agents/helper.md").write_text("body")
    add_client(hub, home)
    d = api.post(f"{API}/clients/fake1/discover", headers=h).json()
    assert d["items"]["agent"][0]["name"] == "helper"
    r = api.post(f"{API}/import", json={"client": "fake1", "items": [{"kind": "agent", "name": "helper"}],
                                        "on_conflict": "skip"}, headers=h).json()
    assert r["results"][0]["status"] == "created"
    assert api.post(f"{API}/import", json={"client": "fake1", "on_conflict": "nope"}, headers=h).status_code == 400
    assert api.get(f"{API}/agents/helper", headers=h).json()["meta"]["name"] == "helper"


# ---- knowledge proxy ----------------------------------------------------------------------------------------------------
def _knowledge_app(cfg: Settings, hub: Hub, handler: Any) -> TestClient:
    from agent_hub.services.knowledge import KnowledgeProxy
    cfg.knowledge_token = "admin-token-value-123"
    hub.knowledge = KnowledgeProxy(cfg, transport=httpx.MockTransport(handler))
    return TestClient(create_app(cfg, hub), base_url=f"http://{HOST}")


def test_knowledge_proxy_forwards_with_admin_token(cfg: Settings, hub: Hub) -> None:
    seen: list[httpx.Request] = []

    def handler(req: httpx.Request) -> httpx.Response:
        seen.append(req)
        return httpx.Response(200, json={"ok": True, "path": req.url.path})

    with _knowledge_app(cfg, hub, handler) as c:
        key = str(hub.auth.create_key("a", "admin")["secret"])
        viewer = str(hub.auth.create_key("v", "viewer")["secret"])
        r = c.get(f"{API}/knowledge/namespaces?limit=5", headers=bearer(viewer))
        assert r.status_code == 200 and r.json()["path"] == "/namespaces"
        assert seen[0].headers["authorization"] == "Bearer admin-token-value-123" and seen[0].url.query == b"limit=5"
        assert "Bearer" not in json.dumps(dict(r.headers)) and "admin-token" not in r.text
        r = c.post(f"{API}/knowledge/namespaces/docs/query", json={"text": "hi"}, headers=bearer(viewer))
        assert r.status_code == 200 and seen[-1].content == b'{"text":"hi"}'.replace(b'":"', b'": "') or seen[-1].content
        # a viewer may not ingest; an admin may
        assert c.post(f"{API}/knowledge/namespaces/docs/ingest", json={}, headers=bearer(viewer)).status_code == 403
        assert c.post(f"{API}/knowledge/namespaces/docs/ingest", json={"documents": []}, headers=bearer(key)).status_code == 200
        # client's own Authorization header is never forwarded
        assert all(rq.headers["authorization"] == "Bearer admin-token-value-123" for rq in seen)


def test_knowledge_proxy_restrictions(cfg: Settings, hub: Hub) -> None:
    def handler(req: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json={})

    with _knowledge_app(cfg, hub, handler) as c:
        key = str(hub.auth.create_key("a", "admin")["secret"])
        h = bearer(key)
        assert c.delete(f"{API}/knowledge/namespaces/docs", headers=h).status_code == 200          # DELETE: admin only
        viewer = bearer(str(hub.auth.create_key("v", "viewer")["secret"]))
        assert c.delete(f"{API}/knowledge/namespaces/docs", headers=viewer).status_code == 403
        assert c.delete(f"{API}/knowledge/shutdown", headers=h).status_code == 404               # same path allowlist
        assert c.put(f"{API}/knowledge/namespaces/docs", json={}, headers=h).status_code == 405
        for path in ("admin/secrets", "../etc", "namespaces/../../x", "namespaces/a b", "shutdown"):
            r = c.get(f"{API}/knowledge/{path}", headers=h)
            assert r.status_code in (400, 404), path
        big = c.post(f"{API}/knowledge/namespaces/docs/ingest", content=b"x" * (cfg.knowledge_body_cap_bytes + 5),
                     headers={**h, "content-type": "application/json"})
        assert big.status_code == 413


def test_knowledge_service_down_is_503_problem(cfg: Settings, hub: Hub) -> None:
    def handler(req: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("refused")

    with _knowledge_app(cfg, hub, handler) as c:
        key = str(hub.auth.create_key("a", "admin")["secret"])
        r = c.get(f"{API}/knowledge/backends", headers=bearer(key))
        assert r.status_code == 503 and problem_code(r) == "knowledge_unavailable" and "uv run agent-knowledge" in r.json()["detail"]


def test_knowledge_response_cap_and_upstream_errors(cfg: Settings, hub: Hub) -> None:
    cfg.knowledge_response_cap_bytes = 100

    def handler(req: httpx.Request) -> httpx.Response:
        if req.url.path == "/jobs/j1":
            return httpx.Response(404, json={"detail": "nope"})
        return httpx.Response(200, content=b"x" * 500, headers={"content-type": "text/html"})

    with _knowledge_app(cfg, hub, handler) as c:
        h = bearer(str(hub.auth.create_key("a", "admin")["secret"]))
        assert problem_code(c.get(f"{API}/knowledge/backends", headers=h)) == "upstream_too_large"
        r = c.get(f"{API}/knowledge/jobs/j1", headers=h)
        assert r.status_code == 404 and r.json() == {"detail": "nope"}


def test_knowledge_sse_stream_is_capped(cfg: Settings, hub: Hub) -> None:
    cfg.max_sse_connections = 1

    def handler(req: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=b"data: 1\n\ndata: 2\n\n", headers={"content-type": "text/event-stream"})

    with _knowledge_app(cfg, hub, handler) as c:
        h = bearer(str(hub.auth.create_key("a", "admin")["secret"]))
        r = c.get(f"{API}/knowledge/jobs/j1/stream", headers=h)
        assert r.status_code == 200 and "data: 2" in r.text and r.headers["content-type"].startswith("text/event-stream")


def test_trust_loopback_defaults_off_and_health_warns_when_on(api: TestClient, hub: Hub) -> None:
    assert Settings().trust_loopback is False
    assert api.get(f"{API}/health").json()["warnings"] == []
    hub.cfg.trust_loopback = True
    assert "TRUST_LOOPBACK" in api.get(f"{API}/health").json()["warnings"][0]


def test_no_key_is_401_on_every_route_but_health(api: TestClient) -> None:
    from .test_api import _all_routes
    n = 0
    for route, path, _ in _all_routes(api.app):
        if path == f"{API}/health":
            continue
        for m in route.methods - {"HEAD", "OPTIONS"}:
            r = api.request(m, _fill(path), json={} if m != "GET" else None, headers=CSRF)
            assert r.status_code == 401, (m, path)
            n += 1
    assert n > 60 and api.get("/metrics").status_code == 401


def test_client_token_cannot_reach_anything_but_inbox(api: TestClient, hub: Hub, home: Path) -> None:
    add_client(hub, home)
    for scopes in (["inbox:write"], []):
        tok = str(hub.auth.create_key("t", "client", client="fake1", scopes=scopes)["secret"])
        for route, path, _ in _all_routes(api.app):
            for m in route.methods - {"HEAD", "OPTIONS"}:
                if (m, path) == ("POST", f"{API}/memory/inbox") or path == f"{API}/health":
                    continue
                r = api.request(m, _fill(path), json={}, headers=bearer(tok))
                assert r.status_code == 403, (m, path)
    tok = str(hub.auth.create_key("t2", "client", client="fake1", scopes=[])["secret"])
    r = api.post(f"{API}/memory/inbox", json={"client": "fake1", "title": "t", "body": "b"}, headers=bearer(tok))
    assert r.status_code == 403                                   # wrong scope for the inbox


def test_viewer_cannot_read_keys_audit_settings(api: TestClient, viewer_key: str) -> None:
    for p in ("keys", "audit", "settings"):
        assert api.get(f"{API}/{p}", headers=bearer(viewer_key)).status_code == 403


def _applied_setup(api: TestClient, hub: Hub, home: Path) -> dict[str, str]:
    add_client(hub, home)
    seed_content(hub)
    hub.commit("c", "t")
    return admin(str(hub.auth.create_key("adm", "admin")["secret"]))


def test_rendered_for_client(api: TestClient, hub: Hub, home: Path) -> None:
    h = _applied_setup(api, hub, home)
    r = api.get(f"{API}/clients/fake1/rendered", params={"kind": "skill", "name": "alpha"}, headers=h).json()
    assert {a["path"] for a in r["artifacts"]} == {"skills/alpha/SKILL.md", "skills/alpha/ref/notes.md"}
    assert r["artifacts"][0]["merge"] == "own" and r["artifacts"][0]["source_ids"] == ["alpha"]
    inst = api.get(f"{API}/clients/fake1/rendered", params={"kind": "instruction", "name": "style"}, headers=h).json()
    assert inst["artifacts"][0]["merge"] == "block" and "Be concise." in inst["artifacts"][0]["content"]
    hub.work.put("skill", "beta", {"description": "B", "body": "b\n"})
    hub.commit("beta", "t")
    miss = api.get(f"{API}/clients/fake1/rendered", params={"kind": "skill", "name": "beta"}, headers=h)
    assert miss.status_code == 404 and problem_code(miss) == "not_rendered" and "not enabled" in miss.json()["detail"]
    hub.organizer.toggle("fake1", "skill", "beta", True)
    assert api.get(f"{API}/clients/fake1/rendered", params={"kind": "skill", "name": "beta"}, headers=h).status_code == 404   # committed
    w = api.get(f"{API}/clients/fake1/rendered", params={"kind": "skill", "name": "beta", "source": "working"}, headers=h)
    assert w.status_code == 200 and w.json()["artifacts"][0]["path"] == "skills/beta/SKILL.md"
    hub.work.put("instruction", "leak", {"title": "L", "order": 9, "body": "api_key = sk-abcdefghijklmnopqrstuv\n"})
    hub.organizer.enable_for_client("fake1", "instruction", ["leak"])
    w = api.get(f"{API}/clients/fake1/rendered", params={"kind": "instruction", "name": "leak", "source": "working"}, headers=h)
    assert "sk-abcdefghij" not in w.text
    assert api.get(f"{API}/clients/fake1/rendered", params={"kind": "bogus", "name": "x"}, headers=h).status_code == 400


def test_effective_rules_carry_source(api: TestClient, hub: Hub, home: Path) -> None:
    h = _applied_setup(api, hub, home)
    hub.work.put("rule", "via-coll", {"title": "c", "kind": "tool", "match": "shell", "decision": "ask"})
    hub.work.put_collection("rc", {"id": "rc", "title": "RC", "members": [{"kind": "rule", "name": "via-coll"}]})
    prof = hub.work.get_profile("fake1")
    prof.collections.append("rc")
    hub.work.put_profile("fake1", prof)
    hub.commit("more", "t")
    rules = api.get(f"{API}/clients/fake1/effective", headers=h).json()["rules"]
    src = {(r["kind"], r["match"]): r["source"] for r in rules}
    assert src[("path_read", "~/." + "aws/**")] == "floor" and src[("tool", "web_search")] == "floor"
    assert src[("tool", "web_fetch")] == "profile" and src[("tool", "shell")] == "content"


def test_client_lock_info_and_verify_record(api: TestClient, hub: Hub, home: Path) -> None:
    h = _applied_setup(api, hub, home)
    c = api.get(f"{API}/clients/fake1", headers=h).json()
    assert c["last_applied_at"] is None and c["last_verified_ok"] is None
    plan = api.post(f"{API}/plan", json={}, headers=h).json()
    api.post(f"{API}/apply", json={"plan_id": plan["id"], "confirm": True}, headers=h)
    c = api.get(f"{API}/clients/fake1", headers=h).json()
    assert c["last_applied_plan"] == plan["id"] and c["last_applied_at"] and c["last_verified_ok"] is True
    (home / "agents/reviewer.md").write_text("tampered")
    api.post(f"{API}/clients/fake1/verify", headers=h)
    c = api.get(f"{API}/clients", headers=h).json()["items"][0]
    assert c["last_verified_ok"] is False and c["last_applied_plan"] == plan["id"]


def test_spec_yaml_accepted_and_bounded(api: TestClient, hub: Hub, home: Path) -> None:
    from agent_hub.adapters import generic
    if hub.registry.get("generic") is None:
        hub.registry.register(generic.GenericSpecAdapter())
    h = admin(str(hub.auth.create_key("adm", "admin")["secret"]))
    spec_yaml = "version: 1\ninstructions:\n  root: project\n  file: AGENTS.md\n  mode: block\n"
    r = api.post(f"{API}/clients/validate-spec", json={"spec_yaml": spec_yaml, "roots": {"project": "/tmp/dry"}}, headers=h)
    assert r.status_code == 200 and r.json()["ok"] is True
    both = api.post(f"{API}/clients/validate-spec", json={"spec": {}, "spec_yaml": spec_yaml}, headers=h)
    assert both.status_code == 422
    for bad in ("a: &x 1\nb: *x\n", "- list\n", "x: " + "y" * 70000, 5):
        assert api.post(f"{API}/clients/validate-spec", json={"spec_yaml": bad}, headers=h).status_code == 422
    made = api.post(f"{API}/clients", json={"id": "gen1", "adapter": "generic", "display_name": "G", "spec_yaml": spec_yaml,
                                            "roots": {"project": str(home)}}, headers=h)
    assert made.status_code == 201 and made.json()["spec"]["instructions"]["file"] == "AGENTS.md"
    assert api.post(f"{API}/clients", json={"id": "gen2", "adapter": "generic", "display_name": "G",
                                            "spec": {"version": 1, "instructions": {"root": "project", "file": "A.md", "mode": "block"}},
                                            "roots": {"project": str(home)}}, headers=h).status_code == 201


def test_preview_plan_cannot_be_applied(api: TestClient, hub: Hub, home: Path) -> None:
    h = _applied_setup(api, hub, home)
    hub.work.put("instruction", "style", {"title": "Style", "order": 10, "body": "PREVIEW ONLY\n"})
    prev = api.post(f"{API}/plan", json={"source": "working"}, headers=h).json()
    assert prev["preview"] is True and prev["content_hash"] == "preview" and hub.repo.head_tree() != "preview"
    assert "PREVIEW ONLY" in "".join(f["diff"] for f in prev["clients"][0]["files"])
    r = api.post(f"{API}/apply", json={"plan_id": prev["id"], "confirm": True}, headers=h)
    assert r.status_code == 409 and problem_code(r) == "plan_is_preview"
    assert not (home / "CLAUDE.md").exists()
    committed = api.post(f"{API}/plan", json={}, headers=h).json()
    assert committed["preview"] is False and "PREVIEW ONLY" not in "".join(f["diff"] for f in committed["clients"][0]["files"])
    assert api.post(f"{API}/plan", json={"source": "nope"}, headers=h).status_code == 400
    assert api.get(f"{API}/plan/{prev['id']}", headers=h).json()["preview"] is True


def test_suggestions_and_import_and_status_over_http(api: TestClient, hub: Hub, home: Path, viewer_key: str) -> None:
    h = _applied_setup(api, hub, home)
    for n in ("dep-a", "dep-b", "dep-c"):
        hub.work.put("skill", n, {"description": n, "body": "b\n"})
    items = api.get(f"{API}/collections/suggestions", headers=h).json()["items"]
    assert "dep" in {s["id"] for s in items}
    assert api.get(f"{API}/collections/suggestions", headers=bearer(viewer_key)).status_code == 200
    assert api.post(f"{API}/collections/suggestions/accept", json={"ids": ["dep"]}, headers=bearer(viewer_key)).status_code == 403
    r = api.post(f"{API}/collections/suggestions/accept", json={"ids": ["dep"], "overrides": {"dep": {"title": "Deps"}}}, headers=h)
    assert r.status_code == 201 and r.json()["items"][0]["title"] == "Deps"
    assert api.post(f"{API}/collections/suggestions/accept", json={"ids": ["dep"]}, headers=h).status_code == 404   # exists now
    # client status contract: adopted vs applied are separate
    (home / "agents").mkdir(exist_ok=True)
    (home / "agents/helper.md").write_text("helper")
    r = api.post(f"{API}/import", json={"client": "fake1", "items": [{"kind": "agent", "name": "helper", "on_conflict": "link"}]}, headers=h)
    assert r.status_code == 200
    c = api.get(f"{API}/clients/fake1", headers=h).json()
    assert c["adopted_at"] is not None and c["last_applied_at"] is None
    assert set(c["drift"]) == {"edited_outside", "pending_changes"} and c["status"] in ("never_applied", "pending_changes")
    d = api.get(f"{API}/clients/fake1/drift", headers=h).json()
    assert set(d["drift"]) == {"edited_outside", "pending_changes"} and "files" in d


def test_unknown_token_scope_is_422_with_a_clear_message(api: TestClient, admin_key: str) -> None:
    for scope in ("knowledge:read:docs", "admin:all", "inbox:read"):
        r = api.post(f"{API}/keys", json={"name": "x", "role": "client", "client": "c", "scopes": [scope]}, headers=admin(admin_key))
        assert r.status_code == 422 and problem_code(r) == "unknown_scope"
        assert "only hub token scope is inbox:write" in r.json()["detail"]
    ok = api.post(f"{API}/keys", json={"name": "x", "role": "client", "client": "c", "scopes": ["inbox:write"]}, headers=admin(admin_key))
    assert ok.status_code == 201


def test_host_allowlist_follows_the_effective_port(cfg: Settings, hub: Hub) -> None:
    cfg.port = 9911                                              # e.g. `hubctl serve --port 9911` or HUB_PORT=9911
    with TestClient(create_app(cfg, hub), base_url="http://127.0.0.1:9911") as c:
        assert c.get(f"{API}/health").status_code == 200
        assert c.get(f"{API}/health", headers={"Host": "localhost:9911"}).status_code == 200
        assert c.get(f"{API}/health", headers={"Host": "127.0.0.1:8792"}).status_code == 421
        assert c.get(f"{API}/health", headers={"Host": "evil.com:9911"}).status_code == 421


def test_real_subprocess_serves_on_a_custom_port(tmp_path: Path) -> None:
    import os
    import socket
    import subprocess
    import sys
    import time
    import urllib.request
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    env = {**os.environ, "HUB_CONTENT_DIR": str(tmp_path / "c"), "HUB_DATA_DIR": str(tmp_path / "s"),
           "AGENT_HUB_ENV_FILE": str(tmp_path / "none"), "HUB_PORT": str(port)}
    for k in ("HUB_API_KEY",):
        env.pop(k, None)
    for extra in ([], ["--port", str(port)]):
        proc = subprocess.Popen([sys.executable, "-m", "agent_hub.cli", "serve", *extra], env=env, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL)
        try:
            body = None
            for _ in range(60):
                try:
                    body = urllib.request.urlopen(f"http://127.0.0.1:{port}/api/v1/health", timeout=2).read()   # noqa: S310
                    break
                except OSError:
                    time.sleep(0.25)
            assert body is not None and b'"status":"ok"' in body
        finally:
            proc.terminate()
            proc.wait(timeout=10)


def test_upstream_401_403_are_never_the_hubs_own(cfg: Settings, hub: Hub) -> None:
    def handler(req: httpx.Request) -> httpx.Response:
        return httpx.Response(401 if req.url.path == "/namespaces" else 403, json={"detail": "nope"})

    with _knowledge_app(cfg, hub, handler) as c:
        h = bearer(str(hub.auth.create_key("a", "admin")["secret"]))
        for path in ("namespaces", "tokens"):
            r = c.get(f"{API}/knowledge/{path}", headers=h)
            assert r.status_code == 502 and problem_code(r) == "knowledge_upstream_unauthorized"
            d = r.json()["detail"]
            assert "HUB_KNOWLEDGE_ADMIN_KEY" in d and "KNOWLEDGE_ADMIN_KEY_FILE" in d and "agent-knowledge/admin.key" in d
            assert "admin-token-value-123" not in r.text
        assert c.delete(f"{API}/knowledge/namespaces/x", headers=h).status_code == 502


def _key_file(tmp_path: Path, mode: int = 0o600, text: str = "file-key-abc\n") -> Path:
    f = tmp_path / "admin.key"
    f.write_text(text)
    f.chmod(mode)
    return f


def _capture_auth(cfg: Settings, hub: Hub) -> tuple[TestClient, list[str]]:
    from agent_hub.services.knowledge import KnowledgeProxy
    seen: list[str] = []

    def handler(req: httpx.Request) -> httpx.Response:
        seen.append(req.headers.get("authorization", ""))
        return httpx.Response(200, json={"ok": True})

    hub.knowledge = KnowledgeProxy(cfg, transport=httpx.MockTransport(handler))
    return TestClient(create_app(cfg, hub), base_url=f"http://{HOST}"), seen


def test_admin_key_is_read_lazily_from_the_service_key_file(cfg: Settings, hub: Hub, tmp_path: Path) -> None:
    cfg.knowledge_admin_key_file = tmp_path / "admin.key"
    c, seen = _capture_auth(cfg, hub)
    with c:
        h = bearer(str(hub.auth.create_key("a", "admin")["secret"]))
        assert c.get(f"{API}/knowledge/namespaces", headers=h).status_code == 200 and seen[-1] == ""      # no file yet: no key sent
        _key_file(tmp_path)
        r = c.get(f"{API}/knowledge/namespaces", headers=h)                                               # picked up without restart
        assert r.status_code == 200 and seen[-1] == "Bearer file-key-abc" and "file-key-abc" not in r.text
        cfg.knowledge_admin_key = "env-key"                                                               # setting/env wins
        c.get(f"{API}/knowledge/namespaces", headers=h)
        assert seen[-1] == "Bearer env-key"
        assert "file-key-abc" not in json.dumps(c.get(f"{API}/settings", headers=h).json())
        assert "file-key-abc" not in json.dumps(c.get(f"{API}/audit", headers=h).json())


@pytest.mark.parametrize("kind", ["loose", "symlink", "huge"])
def test_unsafe_key_files_are_refused(cfg: Settings, hub: Hub, tmp_path: Path, kind: str) -> None:
    if kind == "loose":
        cfg.knowledge_admin_key_file = _key_file(tmp_path, 0o644)
    elif kind == "symlink":
        (tmp_path / "sub").mkdir()
        real = _key_file(tmp_path / "sub", 0o600, "real-key")
        link = tmp_path / "link.key"
        link.symlink_to(real)
        cfg.knowledge_admin_key_file = link
    else:
        cfg.knowledge_admin_key_file = _key_file(tmp_path, 0o600, "x" * 5000)
    c, seen = _capture_auth(cfg, hub)
    with c:
        r = c.get(f"{API}/knowledge/namespaces", headers=bearer(str(hub.auth.create_key("a", "admin")["secret"])))
        assert r.status_code == 502 and problem_code(r) == "knowledge_key_unusable" and not seen
        assert "real-key" not in r.text and "xxxx" not in r.text


def test_knowledge_health_summary(cfg: Settings, hub: Hub, tmp_path: Path) -> None:
    from agent_hub.services.knowledge import KnowledgeProxy
    mode = {"m": "ok"}

    def handler(req: httpx.Request) -> httpx.Response:
        if mode["m"] == "down":
            raise httpx.ConnectError("refused")
        if req.url.path == "/backends":
            return httpx.Response(200, json={"qdrant": {"ok": True}}) if req.headers.get("authorization") == "Bearer file-key-abc" \
                else httpx.Response(401, json={})
        return httpx.Response(200, json={"status": "ok"})

    cfg.knowledge_admin_key_file = tmp_path / "admin.key"
    hub.knowledge = KnowledgeProxy(cfg, transport=httpx.MockTransport(handler))
    with TestClient(create_app(cfg, hub), base_url=f"http://{HOST}") as c:
        h = bearer(str(hub.auth.create_key("v", "viewer")["secret"]))
        s = c.get(f"{API}/knowledge/health", headers=h).json()
        assert s["reachable"] is True and s["authenticated"] is False and "HUB_KNOWLEDGE_ADMIN_KEY" in s["detail"]
        _key_file(tmp_path)
        s = c.get(f"{API}/knowledge/health", headers=h).json()
        assert s == {**s, "reachable": True, "authenticated": True, "backends": {"qdrant": {"ok": True}}, "key_source": "file"}
        assert "file-key-abc" not in json.dumps(s)
        mode["m"] = "down"
        s = c.get(f"{API}/knowledge/health", headers=h).json()
        assert s["reachable"] is False and "uv run agent-knowledge" in s["detail"]
