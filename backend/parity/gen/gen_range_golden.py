"""Golden cases for static-file Range requests, taken from Starlette's own FileResponse range parser (the version in
backend/.venv). Writes backend/crates/agent-hub/tests/golden/ranges.json; the Rust test static_ranges.rs replays them.

    "$PYREL/backend/.venv/bin/python" -I backend/parity/gen/gen_range_golden.py
    (from the repo root; PYREL=$(node backend/parity/lib/python-release.mjs) is the last Python backend, see
     backend/parity/lib/python-release.mjs)
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

import starlette
from starlette.responses import FileResponse, MalformedRangeHeader, RangeNotSatisfiable

HEADERS = [
    "bytes=0-0", "bytes=0-9", "bytes=5-", "bytes=-5", "bytes=-0", "bytes=0-", "bytes=9-9", "bytes=10-", "bytes=10-20",
    "bytes=0-99999999999999999999999", "bytes=-99999999999999999999999", "bytes=99999999999999999999999-",
    "bytes=0-4,6-8", "bytes=6-8,0-4", "bytes=0-4,3-8", "bytes=0-4,5-8", "bytes=0-4,4-8", "bytes=0-2,2-2,7-",
    "bytes=-3,0-1", "bytes= 0 - 3 , 5 - 6 ", "BYTES=0-3", " bytes =0-3", "bytes=", "bytes=-", "bytes=,", "bytes=a-b",
    "bytes=1-a,2-3", "bytes=3", "bytes=3-1", "bytes=5-4", "bytes=+1-+3", "bytes=1_0-2_0", "bytes=--3", "bytes=-1-2",
    "bytes=0x1-3", "items=0-3", "bytes", "", "=", "bytes=0-1=2", "bytes=١-3",
    "bytes=" + ",".join(["0-1"] * 100), "bytes=" + ",".join(["0-1"] * 101), "bytes=0-1,,,2-3", "bytes=0-1, -,2-3",
]
SIZES = [0, 1, 10, 50]


def outcome(header: str, size: int) -> dict[str, object]:
    try:
        ranges = FileResponse._parse_range_header(header, size)
    except MalformedRangeHeader as exc:
        return {"malformed": exc.content}
    except RangeNotSatisfiable as exc:
        return {"unsatisfiable": exc.max_size}
    return {"ranges": [list(r) for r in ranges]}


def main() -> int:
    cases = [{"header": h, "size": s, **outcome(h, s)} for h in HEADERS for s in SIZES]
    out = Path(__file__).resolve().parents[2] / "crates" / "agent-hub" / "tests" / "golden" / "ranges.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    doc = {"generator": "backend/parity/gen/gen_range_golden.py", "starlette": starlette.__version__, "cases": cases}
    out.write_text(json.dumps(doc, indent=1, ensure_ascii=False) + "\n")
    print(f"{len(cases)} cases -> {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
