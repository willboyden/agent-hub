"""One pure-ASGI middleware: Host allowlist (DNS rebinding), same-origin Origin check, `X-Agent-Hub: 1` CSRF header on
non-GET, authentication, role gates, request body caps, audit of every mutating call, metrics, and strict security
headers on EVERY response (including static files and errors). Pure ASGI so SSE bodies are never buffered.
There is deliberately no CORS: cross-origin browsers get nothing."""
from __future__ import annotations

import json
import logging
import re
import time
from typing import Any
from urllib.parse import parse_qsl, urlsplit

from starlette.exceptions import HTTPException
from starlette.types import ASGIApp, Message, Receive, Scope, Send

from agent_hub.api.errors import problem
from agent_hub.services.auth import Principal
from agent_hub.services.common import redact
from agent_hub.services.hub import Hub

log = logging.getLogger(__name__)
LOOPBACK = {"127.0.0.1", "::1", "localhost"}
OPEN_PATHS = {"/api/v1/health"}
SAFE = {"GET", "HEAD", "OPTIONS"}
MUTATING = {"POST", "PUT", "PATCH", "DELETE"}
KNOWN_METHODS = {"GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"}
CSRF_HEADER = "x-agent-hub"
# The ONLY mutating (method, path) pairs a viewer may perform: read-only dry runs and the knowledge query playground.
VIEWER_ALLOWED = re.compile(r"/api/v1/(policy/check|clients/validate-spec|knowledge/namespaces/[A-Za-z0-9._-]+/query)")
INBOX = "/api/v1/memory/inbox"
CONTENT_PATHS = (INBOX, "/api/v1/knowledge")            # audited as metadata only: memory text and documents are private
CSP = ("default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; "
       "connect-src 'self'; base-uri 'none'; form-action 'none'; object-src 'none'; frame-ancestors 'none'")
SECURITY_HEADERS = [
    (b"content-security-policy", CSP.encode()),
    (b"x-content-type-options", b"nosniff"),
    (b"referrer-policy", b"no-referrer"),
    (b"x-frame-options", b"DENY"),
    (b"permissions-policy", b"camera=(), microphone=(), geolocation=()"),
    (b"cross-origin-resource-policy", b"same-origin"),
]
BODY_CAP_AUDIT = 64 * 1024


def viewer_may(method: str, path: str) -> bool:
    return method == "POST" and VIEWER_ALLOWED.fullmatch(path) is not None


def client_may(method: str, path: str) -> bool:
    return method == "POST" and path == INBOX


class BodyTooLarge(HTTPException):
    def __init__(self) -> None:
        super().__init__(413, "request body too large")


def _guarded(path: str) -> bool:
    return (path.startswith("/api/") or path == "/metrics") and path not in OPEN_PATHS


class SecurityMiddleware:
    def __init__(self, app: ASGIApp, hub: Hub) -> None:
        self.app = app
        self.hub = hub

    def _allowed_hosts(self, scope: Scope) -> set[str]:
        """Derived from the EFFECTIVE bind (configured port and the listening socket's own port, never a header) plus extras."""
        cfg = self.hub.cfg
        ports = {cfg.port}
        server = scope.get("server")
        if server and isinstance(server[1], int):
            ports.add(server[1])
        hosts = {f"{h}:{p}" for p in ports for h in ("127.0.0.1", "localhost", "[::1]", cfg.host)}
        return hosts | {h.lower() for h in cfg.allowed_hosts}

    def _principal(self, scope: Scope, headers: dict[str, str]) -> Principal | None:
        auth = headers.get("authorization", "")
        if auth:
            scheme, _, tok = auth.partition(" ")
            return self.hub.auth.authenticate(tok.strip()) if scheme.lower() == "bearer" and tok.strip() else None
        client = scope.get("client")
        host = client[0] if client else ""
        if self.hub.cfg.trust_loopback and host in LOOPBACK and "x-forwarded-for" not in headers:
            return Principal("loopback", "admin")
        return None

    def _cap(self, path: str) -> int:
        cfg = self.hub.cfg
        if path == "/api/v1/import" or path.endswith("/discover"):
            return cfg.import_body_cap_bytes
        if path.startswith("/api/v1/knowledge"):
            return cfg.knowledge_body_cap_bytes
        return cfg.body_cap_bytes

    def _precheck(self, scope: Scope, headers: dict[str, str]) -> tuple[int, str, str] | None:
        path, method = scope["path"], scope["method"]
        if headers.get("host", "").lower() not in {h.lower() for h in self._allowed_hosts(scope)}:
            return 421, "misdirected_request", "unrecognised Host header"
        if method not in SAFE:
            origin = headers.get("origin")
            if origin is not None and (origin == "null" or urlsplit(origin).netloc.lower() != headers.get("host", "").lower()):
                return 403, "cross_origin", "cross-origin requests are not allowed"
            # A custom header cannot be sent cross-site without a CORS preflight (which we never grant). Bearer-key
            # clients are not ambient-credential browsers, so they are exempt.
            if "authorization" not in headers and headers.get(CSRF_HEADER) != "1":
                return 403, "csrf_header_required", "send the X-Agent-Hub: 1 header on non-GET requests"
            cl = headers.get("content-length")
            if cl and cl.isdigit() and int(cl) > self._cap(path):
                return 413, "payload_too_large", "request body too large"
        return None

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] != "http":
            await self.app(scope, receive, send)
            return
        path, method = scope["path"], scope["method"]
        headers = {k.decode("latin-1").lower(): v.decode("latin-1") for k, v in scope["headers"]}
        principal = Principal("public", "public")
        deny = self._precheck(scope, headers)
        if deny is None and _guarded(path):
            p = self._principal(scope, headers)
            if p is None:
                deny = (401, "unauthorized", "a valid bearer API key is required")
            elif p.role == "client" and not client_may(method, path):
                deny = (403, "forbidden", "client tokens may only post to the memory inbox")
                principal = p
            elif p.role == "viewer" and method in MUTATING and not viewer_may(method, path):
                deny = (403, "forbidden", "this key has the viewer role (read-only)")
                principal = p
            else:
                principal = p
        scope.setdefault("state", {})["principal"] = principal

        status = 500
        body_chunks: list[bytes] = []
        captured = 0
        total = 0
        cap = self._cap(path)
        started = False

        async def recv() -> Message:
            nonlocal captured, total
            msg = await receive()
            if msg["type"] == "http.request":
                chunk = msg.get("body", b"")
                total += len(chunk)
                if total > cap:
                    raise BodyTooLarge
                if captured < BODY_CAP_AUDIT:
                    part = chunk[: BODY_CAP_AUDIT - captured]
                    captured += len(part)
                    body_chunks.append(part)
            return msg

        async def snd(message: Message) -> None:
            nonlocal status, started
            if message["type"] == "http.response.start":
                status, started = message["status"], True
                names = {n for n, _ in SECURITY_HEADERS} | ({b"cache-control"} if path.startswith("/api/") else set())
                hdrs = [(k, v) for k, v in message.get("headers", []) if k.lower() not in names]
                hdrs += SECURITY_HEADERS
                if path.startswith("/api/"):
                    hdrs.append((b"cache-control", b"no-store"))
                message = {**message, "headers": hdrs}
            await send(message)

        t0 = time.perf_counter()
        try:
            if deny is not None:
                await problem(deny[0], deny[1], deny[1].replace("_", " "), deny[2])(scope, recv, snd)
            else:
                await self.app(scope, recv, snd)
        except BodyTooLarge:
            if not started:
                status = 413
                await problem(413, "payload_too_large", "payload too large", "request body too large")(scope, receive, snd)
        finally:
            tel = self.hub.telemetry
            route = getattr(scope.get("route"), "path", None)
            if route and path.startswith("/api/v1") and not route.startswith("/api/"):
                route = "/api/v1" + route              # included routers report their un-prefixed template
            route = route or ("unmatched" if _guarded(path) else "static")
            mlabel = method if method in KNOWN_METHODS else "OTHER"            # bounded label cardinality
            tel.requests.labels(mlabel, route, str(status)).inc()
            tel.latency.labels(mlabel, route).observe(time.perf_counter() - t0)
            if _guarded(path) and method in MUTATING:
                self._audit(principal, method, path, status, scope, headers, b"".join(body_chunks), total)

    def _audit(self, principal: Principal, method: str, path: str, status: int, scope: Scope, headers: dict[str, str],
               raw: bytes, size: int) -> None:
        params: dict[str, Any] = {}
        qs = scope.get("query_string", b"").decode("latin-1")
        if qs:
            params["query"] = redact(dict(parse_qsl(qs, keep_blank_values=True)))
        if path.startswith(CONTENT_PATHS):
            meta: dict[str, Any] = {"bytes": size}
            try:
                doc = json.loads(raw) if raw else None
                if isinstance(doc, dict):
                    meta["fields"] = sorted(doc)[:30]
            except ValueError:
                pass
            params["body"] = meta
        elif raw and "json" in headers.get("content-type", ""):
            try:
                params["body"] = redact(json.loads(raw))
            except ValueError:
                params["body"] = "[unparseable json]"
        elif raw:
            params["body"] = f"[{size} bytes, non-json]"
        try:
            self.hub.audit_svc.record(actor=principal.key_id or principal.name, role=principal.role, method=method,
                                      path=path, status=status, params=params)
        except Exception:  # noqa: BLE001 - never fail a request because audit storage hiccuped
            log.error("audit write failed")
