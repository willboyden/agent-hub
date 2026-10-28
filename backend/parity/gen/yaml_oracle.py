"""Live PyYAML oracle for hub-core's property tests (backend/crates/hub-core/tests/yaml_props.rs).

stdin:  {"values": [<yaml_codec value>, ...], "texts": ["<yaml text>", ...], "structured": N, "seed": S}
        (`structured`: N more texts generated here by gen_yaml_golden.structured_text with random.Random(seed))
stdout: {"dumps": [dump cases], "loads": [load cases]}  (the formats of the golden files, see gen_yaml_golden.py)

Run by the Rust test with the backend venv's interpreter, `-I`, a cleared environment and PYTHONHASHSEED=0."""
from __future__ import annotations

import json
import random
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from gen_yaml_golden import dump_case, load_case, structured_text  # noqa: E402


def main() -> None:
    data = json.load(sys.stdin)
    out = {
        "dumps": [c for c in (dump_case(v) for v in data["values"]) if c is not None],
        "loads": [load_case(t) for t in data["texts"]],
    }
    r = random.Random(data.get("seed", 0))
    out["loads"] += [load_case(structured_text(r)) for _ in range(data.get("structured", 0))]
    json.dump(out, sys.stdout, ensure_ascii=False)


if __name__ == "__main__":
    main()
