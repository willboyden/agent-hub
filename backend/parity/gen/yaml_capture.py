"""pytest plugin: record every `yaml.load` input and every `yaml.dump_all` (safe_dump) input the backend's own test suite
produces, so the YAML golden corpus covers exactly the shapes Agent Hub reads and writes. Loaded from outside the
backend tree (no backend change):

    cd "$PYREL/backend" && YAML_CAPTURE_OUT=/tmp/x.jsonl PYTHONPATH="$REPO/backend/parity/gen" \\
        .venv/bin/python -m pytest -q -p yaml_capture
    (from the repo root; PYREL=$(node backend/parity/lib/python-release.mjs) is the last Python backend, see
     backend/parity/lib/python-release.mjs)

Each line: {"op": "load", "text": ..., "loader": "SafeLoader"|"_StrictLoader"} or
{"op": "dump", "data": <yaml_codec>, "kwds": {...}}."""
from __future__ import annotations

import json
import os
from typing import Any

import yaml

from yaml_codec import Unencodable, enc

_OUT = os.environ.get("YAML_CAPTURE_OUT")
_orig_load = yaml.load
_orig_dump_all = yaml.dump_all


def _write(rec: dict[str, Any]) -> None:
    if not _OUT:
        return
    with open(_OUT, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(rec, ensure_ascii=False) + "\n")


def _load(stream: Any, Loader: Any) -> Any:  # noqa: N803 - PyYAML's signature
    if isinstance(stream, str):
        _write({"op": "load", "text": stream, "loader": Loader.__name__})
    return _orig_load(stream, Loader)


def _dump_all(documents: Any, stream: Any = None, Dumper: Any = yaml.Dumper, **kwds: Any) -> Any:  # noqa: N803
    docs = list(documents)
    if Dumper is yaml.SafeDumper and len(docs) == 1:
        try:
            keep = {k: kwds.get(k) for k in ("sort_keys", "allow_unicode", "default_flow_style", "width")}
            _write({"op": "dump", "data": enc(docs[0]), "kwds": keep})
        except Unencodable:
            pass
    return _orig_dump_all(docs, stream, Dumper=Dumper, **kwds)


yaml.load = _load
yaml.dump_all = _dump_all
