"""Knowledge proxy: same-origin access to the knowledge service (127.0.0.1:8795) with an admin token from settings.
GET/POST/DELETE (DELETE admin-only via the role gate), allowlisted path prefixes, size caps, and a 503 problem when the service is down (never a hang)."""
from __future__ import annotations

import re
import stat
from collections.abc import AsyncIterator
from typing import Any

import httpx

from agent_hub.config import Settings
from agent_hub.domain.errors import BadRequest, ProblemError, Unavailable

UNREACHABLE = ("the knowledge service is not reachable; start it with `cd hub/knowledge && uv run agent-knowledge` "
               "(it listens on the URL in HUB_KNOWLEDGE_URL)")
DEFAULT_KEY_FILE = "~/.local/share/agent-knowledge/admin.key"
ALLOWED_PREFIXES = ("health", "backends", "namespaces", "jobs", "indexes", "tokens")
SEGMENT = re.compile(r"[A-Za-z0-9._-]{1,128}")
MAX_QUERY = 1024


def check_path(path: str) -> list[str]:
    parts = path.split("/")
    if not parts or parts[0] not in ALLOWED_PREFIXES or len(parts) > 6:
        raise BadRequest("path is not proxied", code="knowledge_path_denied", status=404)
    for p in parts:
        if p in (".", "..") or SEGMENT.fullmatch(p) is None:
            raise BadRequest("invalid path segment", code="knowledge_path_denied", status=400)
    return parts


class KnowledgeProxy:
    def __init__(self, cfg: Settings, transport: httpx.AsyncBaseTransport | None = None) -> None:
        self.cfg = cfg
        self.client = httpx.AsyncClient(base_url=cfg.knowledge_url.rstrip("/") + "/", timeout=cfg.knowledge_timeout_s,
                                        transport=transport, follow_redirects=False, trust_env=False)

    def _key(self) -> tuple[str | None, str]:
        """(admin key, source). Setting/env wins; else the knowledge service's own 0600 key file, read at call time.
        The key is never logged, returned, or audited."""
        direct = self.cfg.knowledge_admin_key or self.cfg.knowledge_token
        if direct and direct.strip():
            return direct.strip(), "setting"
        f = self.cfg.knowledge_admin_key_file.expanduser()
        try:
            st = f.lstat()
        except OSError:
            return None, "none"
        if not stat.S_ISREG(st.st_mode):
            raise ProblemError(f"{f} must be a regular file (not a symlink)", code="knowledge_key_unusable", status=502)
        if st.st_mode & 0o077:
            raise ProblemError(f"{f} must be mode 0600 or stricter; refusing to use it (chmod 600 it)",
                               code="knowledge_key_unusable", status=502)
        if st.st_size > 4096:
            raise ProblemError(f"{f} is too large to be a key file", code="knowledge_key_unusable", status=502)
        try:
            return f.read_text().strip() or None, "file"
        except OSError:
            return None, "none"

    def _headers(self, has_body: bool) -> dict[str, str]:
        h = {"accept": "application/json"}
        key, _ = self._key()
        if key:
            h["authorization"] = "Bearer " + key
        if has_body:
            h["content-type"] = "application/json"
        return h

    async def forward(self, method: str, path: str, query: str, body: bytes) -> tuple[int, bytes, str]:
        if method not in ("GET", "POST", "DELETE"):
            raise ProblemError("method not allowed on the knowledge proxy", code="method_not_allowed", status=405)
        check_path(path)
        if len(query) > MAX_QUERY:
            raise BadRequest("query string too long")
        if len(body) > self.cfg.knowledge_body_cap_bytes:  # DELETE bodies are dropped below
            raise ProblemError("request body too large", code="payload_too_large", status=413)
        try:
            resp = await self.client.request(method, path + (("?" + query) if query else ""), content=(body or None) if method == "POST" else None,
                                             headers=self._headers(bool(body)))
        except (httpx.TransportError, httpx.InvalidURL) as exc:
            raise Unavailable(UNREACHABLE, code="knowledge_unavailable") from exc
        if resp.status_code in (401, 403):
            raise self._unauthorized()
        data = resp.content
        if len(data) > self.cfg.knowledge_response_cap_bytes:
            raise ProblemError("knowledge response too large", code="upstream_too_large", status=502)
        ctype = resp.headers.get("content-type", "application/json").split(";")[0].strip()
        if ctype not in ("application/json", "application/problem+json", "text/plain"):
            ctype = "application/octet-stream"
        return resp.status_code, data, ctype

    async def stream(self, path: str, query: str) -> AsyncIterator[bytes]:
        """SSE passthrough with a byte cap; the caller wraps this in the connection-capped, idle-timed response."""
        check_path(path)
        sent = 0
        try:
            async with self.client.stream("GET", path + (("?" + query) if query else ""),
                                          headers={**self._headers(False), "accept": "text/event-stream"}) as resp:
                if resp.status_code in (401, 403):
                    raise self._unauthorized()
                if resp.status_code != 200:
                    raise Unavailable("knowledge stream unavailable", code="knowledge_unavailable")
                async for chunk in resp.aiter_bytes():
                    sent += len(chunk)
                    if sent > self.cfg.knowledge_response_cap_bytes:
                        return
                    yield chunk
        except httpx.TransportError as exc:
            raise Unavailable(UNREACHABLE, code="knowledge_unavailable") from exc

    def _unauthorized(self) -> ProblemError:
        """The KNOWLEDGE service refused our key. That is a hub configuration problem (502), never the browser's 401/403."""
        return ProblemError(
            "the knowledge service rejected the hub's admin key: the hub is not configured with a valid knowledge admin key. "
            "Set HUB_KNOWLEDGE_ADMIN_KEY (or point KNOWLEDGE_ADMIN_KEY_FILE at the key file); by default the hub reads "
            f"{DEFAULT_KEY_FILE}, which the knowledge service writes on its first start.",
            code="knowledge_upstream_unauthorized", status=502)

    async def summary(self) -> dict[str, Any]:
        """Small health view for the UI: {reachable, authenticated, backends, key_source}; never any key material."""
        try:
            key, source = self._key()
        except ProblemError as exc:
            key, source = None, "unusable"
            note = exc.detail
        else:
            note = ""
        out: dict[str, Any] = {"reachable": False, "authenticated": False, "backends": None, "key_source": source,
                               "url": self.cfg.knowledge_url}
        try:
            await self.client.get("health", headers={"accept": "application/json"})
            out["reachable"] = True
            r = await self.client.get("backends", headers=self._headers(False) if key else {"accept": "application/json"})
        except (httpx.TransportError, httpx.InvalidURL):
            out["detail"] = UNREACHABLE
            return out
        except ProblemError as exc:
            out["detail"] = exc.detail
            return out
        if r.status_code == 200:
            out["authenticated"] = True
            try:
                out["backends"] = r.json()
            except ValueError:
                out["backends"] = None
        elif r.status_code in (401, 403):
            out["detail"] = self._unauthorized().detail
        else:
            out["detail"] = f"knowledge service answered {r.status_code}"
        if note:
            out["detail"] = note
        return out

    async def aclose(self) -> None:
        await self.client.aclose()

    def status(self) -> dict[str, Any]:
        t = self.cfg.knowledge_admin_key or self.cfg.knowledge_token
        return {"url": self.cfg.knowledge_url, "token": f"[set, {len(t)} chars]" if t else "[unset]",
                "key_file": str(self.cfg.knowledge_admin_key_file)}
