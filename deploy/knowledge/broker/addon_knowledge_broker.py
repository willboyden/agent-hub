"""
mitmproxy addon: read-only credential broker in front of agent-knowledge, for an internal-net client.

A generic credential-broker pattern (as with any mitmproxy reverse proxy). The client holds a DUMMY key and can reach only this
broker; the real per-client, READ-ONLY knowledge token lives only here (env AGENT_KNOWLEDGE_TOKEN) and is injected
on the way through. Allowed: POST /namespaces/<ns>/query (ns from KNOWLEDGE_ALLOWED_NS) and GET /health. Everything
else -- ingest, delete, tokens, namespaces, indexes, backends, metrics, jobs -- is 403'd here, whatever the token
would permit. Metadata-only deny logs; never log bodies (queries are private) or the token.

Example addon: adapt to your proxy.
"""
import json
import os
import re
import sys
import time

_NS = re.compile(r"/namespaces/([a-z0-9][a-z0-9_-]{0,47})/query")
MAX_BODY = 64 * 1024


def allowed(method: str, path: str, allowed_ns: set[str], content_length: int) -> bool:
    path = path.split("?", 1)[0]
    if method == "GET" and path == "/health":
        return True
    m = _NS.fullmatch(path)
    return bool(method == "POST" and m and m.group(1) in allowed_ns and 0 <= content_length <= MAX_BODY)


def _ns_set() -> set[str]:
    return {n.strip() for n in os.environ.get("KNOWLEDGE_ALLOWED_NS", "").split(",") if n.strip()}


def load(loader: object) -> None:
    # Fail closed and loudly rather than forwarding unauthenticated (or unscoped) traffic.
    if not os.environ.get("AGENT_KNOWLEDGE_TOKEN") or not _ns_set():
        sys.exit("addon_knowledge_broker: AGENT_KNOWLEDGE_TOKEN and KNOWLEDGE_ALLOWED_NS must be set")


def request(flow) -> None:  # type: ignore[no-untyped-def]  # mitmproxy.http.HTTPFlow
    from mitmproxy import http

    path = flow.request.path.split("?", 1)[0]
    clen = len(flow.request.raw_content or b"")
    if not allowed(flow.request.method, path, _ns_set(), clen):
        print(json.dumps({"ts": round(time.time(), 3), "broker": "deny", "method": flow.request.method,
                          "path": path[:120]}), flush=True)
        flow.response = http.Response.make(403, b'{"code":"forbidden","detail":"not allowed by the knowledge broker"}',
                                           {"Content-Type": "application/problem+json"})
        return
    # Drop whatever the client sent; only the broker's read-only token goes upstream.
    flow.request.headers["Authorization"] = f"Bearer {os.environ['AGENT_KNOWLEDGE_TOKEN']}"
