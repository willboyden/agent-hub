"""Append-only audit trail (the DB itself rejects UPDATE/DELETE) and the metrics registry."""
from __future__ import annotations

import json
from typing import Any

from prometheus_client import CollectorRegistry, Counter, Histogram

from agent_hub.services.common import now, redact
from agent_hub.services.store import Store, paginate


class AuditService:
    def __init__(self, store: Store) -> None:
        self._db = store

    def record(self, *, actor: str, role: str, method: str, path: str, status: int, params: Any) -> None:
        self._db.execute("INSERT INTO audit(ts,actor,role,method,path,status,params) VALUES(?,?,?,?,?,?,?)",
                         (now(), actor, role, method, path, status, json.dumps(redact(params), default=str)))

    def event(self, actor: str, what: str, detail: Any) -> None:
        """Domain events (apply results, commits) beyond the per-request record."""
        self.record(actor=actor, role="system", method="EVENT", path=what, status=200, params=detail)

    def list_page(self, limit: int, cursor: str | None) -> dict[str, Any]:
        limit, off = paginate(limit, cursor)
        rows = self._db.all("SELECT * FROM audit ORDER BY id DESC LIMIT ? OFFSET ?", (limit + 1, off))
        items = [{**{k: r[k] for k in ("id", "ts", "actor", "role", "method", "path", "status")},
                  "params": json.loads(r["params"])} for r in rows[:limit]]
        return {"items": items, "next_cursor": str(off + limit) if len(rows) > limit else None}


class Telemetry:
    """Own registry (not the global one) so tests and multiple apps in a process never collide. Labels are bounded:
    method is from a fixed set, route is the matched template (never the raw path), status is a code."""

    def __init__(self) -> None:
        self.registry = CollectorRegistry()
        self.requests = Counter("agent_hub_http_requests_total", "HTTP requests", ["method", "route", "status"],
                                registry=self.registry)
        self.latency = Histogram("agent_hub_http_request_seconds", "HTTP latency", ["method", "route"],
                                 registry=self.registry)
        self.applies = Counter("agent_hub_applies_total", "Client applies", ["status"], registry=self.registry)
        self.plans = Counter("agent_hub_plans_total", "Plans computed", registry=self.registry)
