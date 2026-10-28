import asyncio, sys
from pathlib import Path
from agent_knowledge.stores.lance import LanceStore
from agent_knowledge.stores.base import Record
from agent_knowledge.filters import Cmp
async def main(p):
    s = LanceStore(Path(p))
    await s.ensure("demo", 4)
    recs = [Record(id=f"d{i}#c0", text=f"text {i} é", vector=[0.1*i, 0.2, 0.3, 0.4], meta={"doc_id": f"d{i}", "n": i}) for i in range(5)]
    print("upserted", await s.upsert("demo", recs))
    print("upserted again", await s.upsert("demo", [Record(id="d1#c0", text="updated", vector=[1,0,0,0], meta={"doc_id":"d1"})]))
    print("deleted", await s.delete("demo", Cmp("doc_id","eq","d3")))
    hits = await s.query("demo", [0.1,0.2,0.3,0.4], 3, None, None)
    print([(h.id, round(h.score,4), h.text) for h in hits])
    print(await s.stats("demo"))
asyncio.run(main(sys.argv[1]))
