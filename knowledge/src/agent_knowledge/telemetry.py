"""Prometheus metrics on a per-app registry (no global state, so tests can build many apps)."""
from __future__ import annotations

from prometheus_client import CollectorRegistry, Counter, Histogram, generate_latest


class Telemetry:
    def __init__(self) -> None:
        self.registry = CollectorRegistry()
        self.requests = Counter("agent_knowledge_requests_total", "HTTP requests", ["method", "route", "status"],
                                registry=self.registry)
        self.latency = Histogram("agent_knowledge_request_seconds", "HTTP latency", ["method", "route"],
                                 registry=self.registry)
        self.queries = Counter("agent_knowledge_queries_total", "vector queries", ["ns"], registry=self.registry)
        self.chunks = Counter("agent_knowledge_chunks_ingested_total", "chunks ingested", ["ns"], registry=self.registry)
        self.jobs = Counter("agent_knowledge_jobs_total", "ingest jobs finished", ["state"], registry=self.registry)
        self.denied = Counter("agent_knowledge_denied_total", "auth/scope/quota denials", ["reason"],
                              registry=self.registry)

    def render(self) -> bytes:
        return generate_latest(self.registry)
