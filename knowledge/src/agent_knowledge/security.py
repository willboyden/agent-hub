"""Pure-ASGI hardening middleware (re-implementing the Engine Console pattern; nothing imported from it):
Host allowlist (DNS rebinding, 421), same-origin Origin check, custom CSRF header on non-GET for non-bearer callers,
body caps, strict response headers, no CORS, metrics and a metadata-only audit log. Authentication and per-namespace
scope checks live in `api.py` and run on every route; this layer never trusts the peer address."""
from __future__ import annotations

import json
import logging
import os
import time
from pathlib import Path
from urllib.parse import urlsplit

from starlette.exceptions import HTTPException
from starlette.types import ASGIApp, Message, Receive, Scope, Send

from agent_knowledge.config import Config
from agent_knowledge.errors import problem
from agent_knowledge.telemetry import Telemetry

log = logging.getLogger(__name__)
SAFE = {"GET", "HEAD", "OPTIONS"}
MUTATING = {"POST", "PUT", "PATCH", "DELETE"}
KNOWN_METHODS = SAFE | MUTATING
CSRF_HEADER = "x-agent-knowledge"
CSP = "default-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
SECURITY_HEADERS = [
    (b"content-security-policy", CSP.encode()),
    (b"x-content-type-options", b"nosniff"),
    (b"referrer-policy", b"no-referrer"),
    (b"x-frame-options", b"DENY"),
    (b"cache-control", b"no-store"),
    (b"cross-origin-resource-policy", b"same-origin"),
]


class BodyTooLarge(HTTPException):
    def __init__(self) -> None:
        super().__init__(413, "request body too large")


class SecurityMiddleware:
    def __init__(self, app: ASGIApp, cfg: Config, telemetry: Telemetry) -> None:
        self.app, self.cfg, self.tel = app, cfg, telemetry
        hosts = {f"127.0.0.1:{cfg.port}", f"localhost:{cfg.port}", f"[::1]:{cfg.port}", f"{cfg.host}:{cfg.port}"}
        self.hosts = {h.lower() for h in hosts} | {h.lower() for h in cfg.allowed_hosts}
        self.audit_path: Path = cfg.data_dir / "audit.jsonl"

    def _precheck(self, scope: Scope, headers: dict[str, str]) -> tuple[int, str, str] | None:
        host = headers.get("host", "").lower()
        if host not in self.hosts:
            return 421, "misdirected_request", "unrecognised Host header"
        path = scope["path"]
        segs = path.split("/")[1:]
        if "" in segs[:-1] or "." in segs or ".." in segs or "\\" in path or "\x00" in path:
            return 400, "invalid_path", "empty, dot or backslash path segments are not allowed"
        if scope["method"] not in SAFE:
            origin = headers.get("origin")
            if origin is not None and (origin == "null" or urlsplit(origin).netloc.lower() != host):
                return 403, "cross_origin", "cross-origin requests are not allowed"
            # Bearer callers hold no ambient credential a browser could replay; everyone else needs the custom header,
            # which a cross-site page cannot send without a CORS preflight (never granted).
            if "authorization" not in headers and headers.get(CSRF_HEADER) != "1":
                return 403, "csrf_header_required", "send the X-Agent-Knowledge: 1 header on non-GET requests"
            cl = headers.get("content-length")
            if cl and cl.isdigit() and int(cl) > self.cfg.body_cap_bytes:
                return 413, "payload_too_large", "request body too large"
        return None

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] != "http":
            await self.app(scope, receive, send)
            return
        path, method = scope["path"], scope["method"]
        headers = {k.decode("latin-1").lower(): v.decode("latin-1") for k, v in scope["headers"]}
        deny = self._precheck(scope, headers)
        status, total, started = 500, 0, False
        scope.setdefault("state", {})

        async def recv() -> Message:
            nonlocal total
            msg = await receive()
            if msg["type"] == "http.request":
                total += len(msg.get("body", b""))
                if total > self.cfg.body_cap_bytes:
                    raise BodyTooLarge
            return msg

        async def snd(message: Message) -> None:
            nonlocal status, started
            if message["type"] == "http.response.start":
                status, started = message["status"], True
                names = {n for n, _ in SECURITY_HEADERS}
                hdrs = [(k, v) for k, v in message.get("headers", []) if k.lower() not in names] + SECURITY_HEADERS
                message = {**message, "headers": hdrs}
            await send(message)

        t0 = time.perf_counter()
        try:
            if deny is not None:
                await problem(deny[0], deny[1], deny[2])(scope, recv, snd)
            else:
                await self.app(scope, recv, snd)
        except BodyTooLarge:
            if not started:
                await problem(413, "payload_too_large", "request body too large")(scope, receive, snd)
        finally:
            route = getattr(scope.get("route"), "path", None) or "unmatched"
            m = method if method in KNOWN_METHODS else "OTHER"  # bounded label cardinality
            self.tel.requests.labels(m, route, str(status)).inc()
            self.tel.latency.labels(m, route).observe(time.perf_counter() - t0)
            if method in MUTATING:
                self._audit(method, path, status, scope["state"].get("principal_id", "-"), total)

    def _audit(self, method: str, path: str, status: int, who: str, size: int) -> None:
        """Metadata only: never the body (documents and queries are private content)."""
        rec = {"ts": round(time.time(), 3), "who": who, "method": method, "path": path[:200], "status": status,
               "bytes": size}
        try:
            fd = os.open(self.audit_path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
            with os.fdopen(fd, "a") as f:
                f.write(json.dumps(rec) + "\n")
        except OSError:
            log.error("audit write failed")
