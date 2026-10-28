"""Golden cases for hub_core::py::float: Python's repr(float), json.dumps(float) and pydantic-core's float JSON, with
many exact ties (CPython and pydantic-core round a tie between two shortest candidates to EVEN). Writes
backend/crates/hub-core/tests/golden/floats.json; replayed by tests/float_golden.rs.

    "$PYREL/backend/.venv/bin/python" -I backend/parity/gen/gen_float_golden.py
    (from the repo root; PYREL=$(node backend/parity/lib/python-release.mjs) is the last Python backend, see
     backend/parity/lib/python-release.mjs)
"""
from __future__ import annotations

import json
import random
import struct
import sys
from pathlib import Path

import pydantic_core


def main() -> int:
    rnd = random.Random(20261028)
    xs: list[float] = [-1133092563735992.25, 1133092563735992.25, 0.1, 2.5, 1e16, 1e22, 5e-324, 1.7976931348623157e308,
                       -0.0, 0.0, 9007199254740993.0, 123456789012345.6, 1e-5, 1e-4, 0.001, 1e15, 1e17, 0.30000000000000004]
    # exact ties: between 2**50 and 2**51 the spacing is 0.25, so n + 0.25 and n + 0.75 sit halfway between two
    # 17-digit decimals; likewise between 2**51 and 2**52 (spacing 0.5) for n + 0.5
    for _ in range(400):
        n = rnd.randrange(2**50, 2**51)
        xs += [n + 0.25, n + 0.75, -(n + 0.25)]
        m = rnd.randrange(2**51, 2**52)
        xs.append(m + 0.5)
    # ties at other scales: multiply exact binary fractions by powers of two
    for _ in range(300):
        n = rnd.randrange(2**52, 2**53)
        e = rnd.randrange(-60, 60)
        xs.append(float(n) * 2.0**e)
    # random bit patterns (finite)
    while len(xs) < 3000:
        (x,) = struct.unpack("<d", rnd.getrandbits(64).to_bytes(8, "little"))
        if x == x and x not in (float("inf"), float("-inf")):
            xs.append(x)
    cases = [{"bits": struct.pack("<d", x).hex(), "repr": repr(x), "json": json.dumps(x),
              "pydantic": pydantic_core.to_json(x).decode()} for x in xs]
    out = Path(__file__).resolve().parents[2] / "crates" / "hub-core" / "tests" / "golden" / "floats.json"
    out.write_text(json.dumps({"generator": "backend/parity/gen/gen_float_golden.py", "cases": cases}, indent=0) + "\n")
    print(f"{len(cases)} cases -> {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
