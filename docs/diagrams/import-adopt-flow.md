# Import and adopt

Mermaid.

```mermaid
flowchart TD
  start["hubctl import --client X<br/>or UI Import wizard"] --> disc["adapter.discover, read-only<br/>skills, agents, instructions, MCP, permissions"]
  disc --> conf{"item name already<br/>in content?"}
  conf -- "no" --> add["write into content WORKING TREE"]
  conf -- "yes, identical" --> link["link: enable existing item for this client"]
  conf -- "yes, differs" --> pol{"on_conflict?"}
  pol -- "unset" --> skip["skip, report differences"]
  pol -- "skip" --> skip
  pol -- "rename" --> ren["import under a free name"]
  pol -- "replace" --> rep["overwrite the content item"]
  pol -- "link" --> link
  add --> en["enable in the client profile"]
  ren --> en
  rep --> en
  link --> en
  en --> adopt["record what the client has NOW<br/>as last applied in the lock<br/>adopted_at set, no client file written"]
  adopt --> review["operator reviews pending diff"]
  review --> commit["commit"]
  commit --> plan["plan: ordinary change or unchanged<br/>instead of a wall of conflicts"]
  plan --> ag{"file re-rendered<br/>differently, e.g. agents?"}
  ag -- "yes" --> cf["conflict: review, then --adopt root:path"]
  ag -- "no" --> ok["unchanged"]
```

Notes: `adopt_live` only locks files that exist, are managed, are parseable and are adoptable; anything else stays
un-locked and shows as a conflict in the first plan. A client with a lock but `applied_at` 0 has never had a real apply.
