# Content model

Mermaid. Layout from `ARCHITECTURE.md` section 3.
Names and ids match `^[a-z0-9][a-z0-9._-]{0,63}$`.

```mermaid
erDiagram
  HUB ||--o{ CLIENT : registers
  HUB ||--|| ENDPOINTS : "router and knowledge URLs"
  CLIENT ||--|| PROFILE : "one per client"
  PROFILE }o--o{ COLLECTION : "collections listed"
  PROFILE }o--o{ ITEM : "enable and disable lists"
  COLLECTION }o--o{ ITEM : "members, multi-membership"
  ITEM ||--o| SIDECAR : "skills only, hub.yaml"
  INBOX_ITEM }o--|| CLIENT : "proposed by"
  INBOX_ITEM ||--o| MEMORY : "promoted by an admin"
  RULE }o--o{ CLIENT : applies_to
  INSTRUCTION }o--o{ CLIENT : applies_to

  ITEM {
    string kind "skill agent instruction mcp rule memory"
    string name_or_id
  }
  CLIENT {
    string id
    string adapter
    map roots
    bool strict
    map manage "per concern, managed or advisory"
    map spec "generic adapter only"
  }
  PROFILE {
    list collections
    map enable
    map disable
  }
  MCP {
    string transport
    list env_names "names only"
    list egress_hosts
    string scan_status "unscanned clean findings"
  }
  AGENT {
    list capabilities "canonical vocabulary"
    string model_tier
  }
```

Files: `hub.yaml`, `endpoints.yaml`, `instructions/<id>.md`, `skills/<name>/SKILL.md` (+ `hub.yaml` sidecar),
`agents/<name>.md`, `mcp/<name>.yaml`, `rules/<id>.yaml`, `memory/<id>.md`, `inbox/<client>/<id>.md` (never compiled),
`collections/<id>.yaml`, `clients/<id>.yaml`, `profiles/<client>.yaml`. Effective set per client =
union(collection members, `enable`) minus `disable`, then the policy floor, then `adapter.render`.
