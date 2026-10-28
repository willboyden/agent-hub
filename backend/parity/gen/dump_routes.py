"""Dump the live route tables of the Python apps (hub + knowledge) to JSON, in Starlette's MATCH ORDER.

    "$PYREL/backend/.venv/bin/python" -I backend/parity/gen/dump_routes.py hub       backend/parity/routes.python.json
    (from the repo root; PYREL=$(node backend/parity/lib/python-release.mjs) is the last Python backend, see
     backend/parity/lib/python-release.mjs)
    "$PYREL/knowledge/.venv/bin/python" -I backend/parity/gen/dump_routes.py knowledge backend/parity/routes.knowledge.json

Everything runs in a throwaway HOME/data dir: no user state is read. The output is the route contract the Rust servers
must register (backend/parity/check-routes.mjs compares it with the Rust server's own table)."""
from __future__ import annotations

import inspect
import json
import os
import sys
import tempfile
from pathlib import Path
from typing import Any


def _annotation(a: Any) -> str:
    s = str(a)
    return s.replace("typing.", "")


def _params(route: Any) -> dict[str, Any]:
    out: dict[str, Any] = {"path_params": [], "query": [], "body": None, "dependencies": []}
    dep = getattr(route, "dependant", None)
    if dep is None:
        return out
    for p in dep.path_params:
        out["path_params"].append({"name": p.name, "type": _annotation(p.field_info.annotation)})
    for q in dep.query_params:
        fi = q.field_info
        meta = [str(m) for m in getattr(fi, "metadata", [])]
        default = None if fi.is_required() else fi.default
        out["query"].append({"name": q.name, "type": _annotation(fi.annotation), "required": fi.is_required(),
                             "default": default if isinstance(default, (str, int, float, bool, type(None))) else repr(default),
                             "constraints": meta})
    for b in dep.body_params:
        fi = b.field_info
        ann = fi.annotation
        schema = None
        try:
            from pydantic import TypeAdapter
            schema = TypeAdapter(ann).json_schema()
        except Exception as exc:  # noqa: BLE001
            schema = {"error": repr(exc)}
        out["body"] = {"name": b.name, "type": _annotation(ann), "required": fi.is_required(), "schema": schema}
    def walk(d: Any, acc: list[str]) -> None:
        for sub in d.dependencies:
            call = getattr(sub, "call", None)
            if call is not None:
                acc.append(f"{getattr(call, '__module__', '?')}:{getattr(call, '__qualname__', repr(call))}")
            walk(sub, acc)
    walk(dep, out["dependencies"])
    return out


def _flatten(routes: Any) -> list[tuple[Any, Any]]:
    """(original route, effective starlette route) pairs in MATCH ORDER. FastAPI 0.141 keeps an included router as one
    `_IncludedRouter` candidate whose own candidates are tried in order, so a depth-first walk is the match order."""
    from fastapi.routing import _IncludedRouter
    out: list[tuple[Any, Any]] = []
    for r in routes:
        if isinstance(r, _IncludedRouter):
            for cand in r.effective_candidates():
                if isinstance(cand, _IncludedRouter):
                    out += _flatten([cand])
                else:
                    out.append((cand.original_route, cand))
        else:
            out.append((r, r))
    return out


def dump(app: Any) -> list[dict[str, Any]]:
    rows = []
    for i, (orig, eff) in enumerate(_flatten(app.routes)):
        endpoint = getattr(orig, "endpoint", None)
        from starlette.routing import compile_path
        path = getattr(eff, "path", None) or getattr(orig, "path", None)
        if eff is not orig and getattr(eff, "starlette_route", None) is not None:
            path = eff.starlette_route.path
        row: dict[str, Any] = {
            "order": i,
            "path": path,
            "methods": sorted(getattr(orig, "methods", None) or []),
            "name": getattr(orig, "name", None),
            "endpoint": f"{endpoint.__module__}:{endpoint.__qualname__}" if endpoint else None,
            "kind": type(orig).__name__,
            "include_in_schema": getattr(orig, "include_in_schema", None),
            "status_code": getattr(orig, "status_code", None),
            "path_regex": compile_path(path)[0].pattern if path else None,
            "is_async": inspect.iscoroutinefunction(endpoint) if endpoint else None,
        }
        row.update(_params(orig))
        rows.append(row)
    return rows


def main() -> None:
    which, out = sys.argv[1], Path(sys.argv[2])
    tmp = Path(tempfile.mkdtemp(prefix="routes-"))
    os.environ["HOME"] = str(tmp / "home")
    (tmp / "home").mkdir()
    if which == "hub":
        os.environ["AGENT_HUB_ENV_FILE"] = str(tmp / "no-env")
        from agent_hub.config import Settings
        from agent_hub.main import create_app
        cfg = Settings(data_dir=tmp / "data", content_dir=tmp / "content")
        app = create_app(cfg)
        meta = {"app": "agent-hub", "prefix": "/api/v1", "middleware": "agent_hub.api.security:SecurityMiddleware"}
    else:
        from agent_knowledge.app import create_app
        from agent_knowledge.config import Config
        from agent_knowledge.stores.memory import MemoryStore
        cfg = Config(data_dir=tmp / "data")
        app = create_app(cfg, stores={"lancedb": MemoryStore()})
        meta = {"app": "agent-knowledge", "prefix": "", "middleware": "agent_knowledge.security:SecurityMiddleware"}
    doc = {"generated_by": "backend/parity/gen/dump_routes.py", "meta": meta, "routes": dump(app)}
    out.write_text(json.dumps(doc, indent=1, sort_keys=False, default=str) + "\n")
    print(f"{which}: {len(doc['routes'])} routes -> {out}")


if __name__ == "__main__":
    main()
