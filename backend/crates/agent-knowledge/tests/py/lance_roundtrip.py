"""Python half of tests/lance_compat.rs: the PYTHON LanceStore (lancedb 0.39.0) reads tables the Rust port wrote,
then writes to them (merge-insert, delete, a new table) so the Rust side can read Python's writes back.

    python -I lance_roundtrip.py read  <lance dir>   -> JSON on stdout: what Python sees
    python -I lance_roundtrip.py write <lance dir>   -> applies Python writes, then prints the same JSON
Hermetic: only the given directory is touched.
"""
from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

from agent_knowledge.filters import Cmp, parse_filter
from agent_knowledge.stores.base import Record
from agent_knowledge.stores.lance import LanceStore

Q = [0.1, 0.2, 0.3, 0.4]


def hits(hs: list) -> list:
    return [{"id": h.id, "score": h.score, "text": h.text, "meta": h.meta} for h in hs]


async def view(s: LanceStore, ns: str) -> dict:
    return {
        "count": (await s.stats(ns)).count,
        "query": hits(await s.query(ns, Q, 10, None, None)),
        "doc_d1": hits(await s.query(ns, Q, 10, Cmp("doc_id", "eq", "d1"), None)),
        "n_ge_2": hits(await s.query(ns, Q, 10, parse_filter({"n": {"$gte": 2}}), None)),
    }


async def main(mode: str, path: str) -> None:
    s = LanceStore(Path(path))
    if mode == "write":
        await s.upsert("demo", [Record("py#0", "from python é", [0.4, 0.3, 0.2, 0.1], {"doc_id": "py", "n": 7, "ok": True}),
                                Record("d1#c0", "python updated d1", [0.0, 0.0, 1.0, 0.0], {"doc_id": "d1", "n": 1})])
        await s.delete("demo", Cmp("doc_id", "eq", "d4"))
        await s.ensure("pynew", 4)
        await s.upsert("pynew", [Record("x#0", "python table", [1.0, 0.0, 0.0, 0.0], {"doc_id": "x", "f": 1.5})])
    out = {"tables": sorted(s._tables()), "demo": await view(s, "demo")}
    if "ns_pynew" in out["tables"]:
        out["pynew"] = await view(s, "pynew")
    print(json.dumps(out))


asyncio.run(main(sys.argv[1], sys.argv[2]))
