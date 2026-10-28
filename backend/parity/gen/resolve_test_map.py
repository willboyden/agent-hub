"""Resolve merge-conflict blocks in backend/TEST-MAP.md by row owner: inside each `<<<<<<< / ======= / >>>>>>>` block, a
row owned by the workstream being merged comes from its side ("theirs"), every other row from rust-backend ("ours").
Both sides must list the same pytest ids in the block, else the script refuses and changes nothing.

    python3 -I backend/parity/gen/resolve_test_map.py WS-C
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

ROW = re.compile(r"^\| `(?P<id>.+)` \| (?P<owner>[^|]+) \| .* \| (?:mapped|pending|N/A) \|$")


def rows(block: list[str]) -> dict[str, tuple[str, str]]:
    out: dict[str, tuple[str, str]] = {}
    for line in block:
        m = ROW.match(line)
        if not m:
            raise SystemExit(f"not a TEST-MAP row inside a conflict block: {line[:120]!r}")
        out[m["id"]] = (m["owner"].strip(), line)
    return out


def main() -> int:
    theirs_owner = sys.argv[1]
    path = Path(__file__).resolve().parents[2] / "TEST-MAP.md"
    lines = path.read_text().split("\n")
    out: list[str] = []
    i = resolved = 0
    while i < len(lines):
        if not lines[i].startswith("<<<<<<< "):
            out.append(lines[i])
            i += 1
            continue
        mid = lines.index("=======", i)
        end = next(j for j in range(mid, len(lines)) if lines[j].startswith(">>>>>>> "))
        ours, theirs = lines[i + 1:mid], lines[mid + 1:end]
        o, t = rows(ours), rows(theirs)
        if list(o) != list(t):
            raise SystemExit(f"conflict block at line {i + 1} lists different ids on each side; resolve by hand")
        for pid, (owner, line) in o.items():
            out.append(t[pid][1] if owner == theirs_owner else line)
        resolved += 1
        i = end + 1
    path.write_text("\n".join(out))
    print(f"{resolved} conflict block(s) resolved ({theirs_owner} rows from the merged branch)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
