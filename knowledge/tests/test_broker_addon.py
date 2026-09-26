"""Offline test of the broker's allow/deny decision (the template addon lives in hub/deploy/knowledge/broker)."""
from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest

SPEC = importlib.util.spec_from_file_location(
    "addon", Path(__file__).resolve().parents[2] / "deploy/knowledge/broker/addon_knowledge_broker.py")
assert SPEC and SPEC.loader
addon = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(addon)
NS = {"docs"}


@pytest.mark.parametrize(("method", "path", "length", "ok"), [
    ("POST", "/namespaces/docs/query", 100, True), ("GET", "/health", 0, True),
    ("POST", "/namespaces/docs/query?x=1", 10, True),
    ("POST", "/namespaces/docs/ingest", 10, False), ("POST", "/namespaces/other/query", 10, False),
    ("DELETE", "/namespaces/docs/documents/x", 0, False), ("GET", "/tokens", 0, False), ("GET", "/metrics", 0, False),
    ("GET", "/backends", 0, False), ("POST", "/tokens", 10, False), ("GET", "/namespaces/docs/query", 0, False),
    ("POST", "/namespaces/docs/query", 10**6, False), ("POST", "/namespaces/docs/query/../ingest", 5, False),
    ("POST", "/namespaces//query", 5, False), ("POST", "/namespaces/docs/query/", 5, False),
])
def test_allowed(method: str, path: str, length: int, ok: bool) -> None:
    assert addon.allowed(method, path, NS, length) is ok
