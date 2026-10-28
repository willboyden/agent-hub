"""Capture the OpenAPI document of the Python hub (served at GET /api/v1/openapi.json) for the Rust server to serve.

    "$PYREL/backend/.venv/bin/python" -I backend/parity/gen/dump_openapi.py backend/crates/agent-hub/assets/openapi.json
    (from the repo root; PYREL=$(node backend/parity/lib/python-release.mjs) is the last Python backend, see
     backend/parity/lib/python-release.mjs)
"""
from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path

tmp = Path(tempfile.mkdtemp(prefix="openapi-"))
os.environ["HOME"] = str(tmp)
os.environ["AGENT_HUB_ENV_FILE"] = str(tmp / "no-env")
from agent_hub.config import Settings  # noqa: E402
from agent_hub.main import create_app  # noqa: E402

app = create_app(Settings(data_dir=tmp / "data", content_dir=tmp / "content"))
Path(sys.argv[1]).write_text(json.dumps(app.openapi(), indent=1, ensure_ascii=False) + "\n")
print(f"openapi -> {sys.argv[1]}")
