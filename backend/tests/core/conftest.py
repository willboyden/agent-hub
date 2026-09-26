from __future__ import annotations

import os
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest
from fastapi.testclient import TestClient

from agent_hub import config as config_mod
from agent_hub.adapters import AdapterRegistry
from agent_hub.config import APP_ROOT, Settings
from agent_hub.main import create_app
from agent_hub.services.hub import Hub

from .fakes import FakeAdapter, FakeRunner

HOST = "127.0.0.1:8792"
CSRF = {"X-Agent-Hub": "1"}


@pytest.fixture(autouse=True)
def _isolate_env(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    """Never read the developer's real env file or HUB_* variables."""
    monkeypatch.setattr(config_mod, "ENV_FILE", tmp_path / "no-such-env-file")
    for k in list(os.environ):
        if k.startswith("HUB_") or k == "AGENT_HUB_ENV_FILE":
            monkeypatch.delenv(k)


@pytest.fixture
def frontend(tmp_path: Path) -> Path:
    fe = tmp_path / "frontend"
    for d in ("css", "js", "i18n", "dev", "tests"):
        (fe / d).mkdir(parents=True)
    (fe / "index.html").write_text("<!doctype html><title>hub</title>")
    (fe / "css" / "app.css").write_text("body{}")
    (fe / "js" / "app.js").write_text("export {}")
    (fe / "dev" / "mock-server.mjs").write_text("secret dev file")
    (fe / "package.json").write_text("{}")
    (tmp_path / "outside.txt").write_text("outside")
    return fe


@pytest.fixture
def cfg(tmp_path: Path, frontend: Path) -> Settings:
    return Settings(content_dir=tmp_path / "content", data_dir=tmp_path / "state", policy_file=APP_ROOT / "policy" / "floor.yaml",
                    frontend_dir=frontend, trust_loopback=False,
                    knowledge_admin_key_file=tmp_path / "no-knowledge-key", inbox_rate_per_min=100)


@pytest.fixture
def registry() -> AdapterRegistry:
    r = AdapterRegistry()
    r.register(FakeAdapter())
    return r


@pytest.fixture
def runner() -> FakeRunner:
    return FakeRunner()


@pytest.fixture
def hub(cfg: Settings, registry: AdapterRegistry, runner: FakeRunner) -> Iterator[Hub]:
    h = Hub(cfg, registry=registry, runner=runner)  # type: ignore[arg-type]
    h.init_content()
    yield h
    h.close()


@pytest.fixture
def home(tmp_path: Path) -> Path:
    """The fake client's config dir: a temp dir, never a real client location."""
    d = tmp_path / "client-home"
    d.mkdir()
    return d


def add_client(hub: Hub, home: Path, cid: str = "fake1", **extra: Any) -> None:
    doc = {"adapter": "fake", "display_name": "Fake 1", "roots": {"home": str(home)}, **extra}
    hub.put_client(cid, doc, create=True)


def seed_content(hub: Hub, client: str = "fake1") -> None:
    """One of every kind, all enabled for `client` through a collection and a direct enable."""
    w = hub.work
    w.put("skill", "alpha", {"description": "Alpha skill", "body": "Do alpha.\n", "files": {"ref/notes.md": "notes"},
                             "tags": ["core"]})
    w.put("agent", "reviewer", {"description": "Reviews code", "capabilities": ["read"], "body": "Review carefully.\n"})
    w.put("instruction", "style", {"title": "Style", "order": 10, "body": "Be concise.\n"})
    w.put("rule", "no-web", {"title": "No web", "kind": "tool", "match": "web_fetch", "decision": "deny"})
    w.put("memory", "fact1", {"type": "reference", "title": "A fact", "body": "The sky is blue.\n"})
    w.put_collection("core", {"id": "core", "title": "Core", "members": [{"kind": "skill", "name": "alpha"},
                                                                          {"kind": "instruction", "name": "style"}]})
    prof = {"collections": ["core"], "enable": {"agents": ["reviewer"], "rules": ["no-web"]}}
    w.put_profile(client, prof)


@pytest.fixture
def api(hub: Hub, cfg: Settings) -> Iterator[TestClient]:
    app = create_app(cfg, hub)
    with TestClient(app, base_url=f"http://{HOST}") as c:
        yield c


@pytest.fixture
def admin_key(hub: Hub, api: TestClient) -> str:
    return str(hub.auth.create_key("t-admin", "admin")["secret"])


@pytest.fixture
def viewer_key(hub: Hub, api: TestClient) -> str:
    return str(hub.auth.create_key("t-viewer", "viewer")["secret"])


def bearer(key: str) -> dict[str, str]:
    return {"Authorization": f"Bearer {key}"}
