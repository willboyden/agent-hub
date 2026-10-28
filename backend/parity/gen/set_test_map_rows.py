"""Set the rust-test column of TEST-MAP.md rows in place (after phase 1 the file is edited row by row, never regenerated,
so the workstreams' own row edits merge cleanly). Input: a JSON object {"<pytest id or id prefix>": "<file>::<fn>" |
"N/A: <reason>"}; a key ending in "[" matches every parametrisation of that test.

    python3 -I backend/parity/gen/set_test_map_rows.py mapping.json
"""
from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROW = re.compile(r"^\| `(?P<id>.+)` \| (?P<owner>[^|]+) \| (?P<rust>.*) \| (?P<status>mapped|pending|N/A) \|$")


def main() -> int:
    mapping: dict[str, str] = json.loads(Path(sys.argv[1]).read_text())
    path = Path(__file__).resolve().parents[2] / "TEST-MAP.md"
    lines = path.read_text().split("\n")
    used: set[str] = set()
    changed = 0
    for i, line in enumerate(lines):
        m = ROW.match(line)
        if not m:
            continue
        pid = m["id"].replace("\\|", "|")
        for key, target in mapping.items():
            if pid == key or (key.endswith("[") and pid.startswith(key)):
                status = "N/A" if target.startswith("N/A: ") else "mapped"
                col = target if status == "N/A" else f"`{target}`"
                lines[i] = f"| `{m['id']}` | {m['owner'].strip()} | {col} | {status} |"
                used.add(key)
                changed += 1
    path.write_text("\n".join(lines))
    missing = sorted(set(mapping) - used)
    print(f"{changed} rows set; unmatched keys: {missing}")
    return 1 if missing else 0


if __name__ == "__main__":
    sys.exit(main())
