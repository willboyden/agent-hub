# Design decisions (narrative)

The numbered decisions are in `ARCHITECTURE.md` section 1 and recorded one per file in `adr/` (ADR-0001..0010). This
page explains the choices a newcomer asks about first, with the trade-offs. Where an "alternative" was not recorded at the time, it is a reconstruction of why it was not chosen.

| Question | Short answer | ADR |
|---|---|---|
| Why a separate app? | Different domain, trust model and change model | (below) |
| Why a git repo? | History, review, offline; containerised clients cannot query a service | 0001 |
| Why compile, not serve? | containerised clients cannot reach host services; nothing live to break | 0001 |
| Why advisory as well as managed? | Existing sync scripts may own some files | 0005 |
| Why a capability vocabulary? | Tool names differ; lossy mappings must be visible | 0002 |
| Why two vector backends? | Operator's request; each wins in a different regime | 0009 |

## Why a separate app, and not a feature of an engine-management UI

Agent policy could have been folded into a local UI for serving and monitoring inference engines. That was rejected: the
domains differ (engines and resources vs policy and content); the trust models differ (a localhost ops UI vs an authority over
agents that must not be able to reach it); the change models differ (operational start/stop/tune vs diff-and-approve); and the
lifecycles differ (a general-purpose tool should not absorb one site's policy, and doing so would widen its blast radius).

What is shared is the **pattern**, not code: a hexagonal core, Host/Origin/CSRF/CSP hardening, audit, secrets-by-name and a
zero-build UI. Each was re-implemented, at the cost of duplicated middleware to keep in sync by hand. The one deliberate
difference is `HUB_TRUST_LOOPBACK` (off here; ADR-0007). A possible seam, the hub reading a model catalog from an engine
manager to map model tiers, is not built.

## Why git-backed content, and why compile-not-serve

Two reasons carry the decision (ADR-0001). First, a containerised client such as Hermes Agent may live on an internal-only network with no host mounts, and
rootless Docker cannot reach host loopback, so **a service it must query cannot work**; config has to be pushed. Second,
what agents are told and may do is security-relevant, so it should have history, diff, review and revert. A git repo of
declarative files gives both, and a compiler (content in, native files out) needs no runtime availability.

Trade-offs accepted: no live toggles (everything goes edit, commit, plan, apply); delivered files are copies that can
diverge (handled by conflict protection); Hermes needs a manual delivery step; one fixed git identity. Trade-off *not*
solved: delivered files are not read-only to the agents (`SECURITY.md` section 10).

## Why advisory vs managed

Many setups already have authoritative sync scripts. A typical one renders a Claude Code sandbox settings file from a
hardened template on every launch, with the model denied access to it; a containerised client may render its config in its own
script. If the hub wrote those files it would either be overwritten or weaken a boundary. So each concern
(`skills, agents, instructions, mcp, permissions, memory`) is per client either **managed** (the hub writes and locks it)
or **advisory** (the plan shows the would-be change and `audit` checks the live file against the floor).

Defaults: managed for skills, agents, instructions (low blast radius, the hub is the natural editor); advisory for
mcp, permissions, memory (owned by other processes, or high blast radius). You opt a concern in per client and the plan
shows the diff before you do. Effective `manage` per concern is the client's `manage`, else `params.manage`, else the
adapter's documented default (Hermes: agents and instructions off; opencode: skills off), else the hub default (Addendum
D fixed an earlier gap where the adapter defaults were ignored). Costs: during adoption there are two writers; floor
rules on advisory concerns are reported, not enforced (and `permissions` is advisory for all four current clients, so the
floor's state-dir read-deny reaches none of them until you add it by hand); and if the lock state is lost every existing
file looks like a conflict.

## Why a capability vocabulary

Claude Code says `Bash`, opencode says `bash`, Hermes has toolsets. Writing content against native names would fork
every agent per client. Content therefore uses 12 canonical capabilities (`read write edit shell web_fetch web_search mcp
subagent browser notebook todo image`) and model tiers (`fast standard deep`); each adapter maps them (`tool_map`,
`model_tiers`; `null` means unsupported). `read` also grants the search tools in the Claude mapping, because a reader that
cannot Grep or Glob cannot navigate a repo. Two rules keep this honest: a lossy or missing mapping always produces a diagnostic
(an error on a `strict` client), and an agent that would end up with **no tools** is an error, because several clients
treat an empty tool list as "all tools".

Cost: a lowest-common-denominator vocabulary; a client-specific tool with no canonical name cannot be expressed without
extending the list (a core change to `CAPABILITIES`). Some mappings depend on the client version (`SECURITY.md`, `FEATURES.md`).

## Why both Qdrant and LanceDB

The operator asked for both, and they win in different regimes (`KNOWLEDGE-PLANE.md`):

| Use | When |
|---|---|
| **LanceDB** (embedded) | Default for per-project corpora up to a few hundred thousand chunks; zero extra service, one directory to back up, lowest idle RAM. Search is a flat scan (no index built), so latency grows with rows; filters are evaluated in Python over ids with a 50k cap. |
| **Qdrant** (service) | Large corpora (> about 1M chunks), several concurrent readers, heavy filtering, or a wish for an isolated blast radius. Costs a container, its page cache and an image to pin. HNSW and vectors are on disk to spare RAM. |

The backend is chosen **per namespace**, so a small notes corpus can stay on LanceDB while a code index uses Qdrant.
Sizing (RAM can be scarce): raw vectors are chunks x dim x 4 bytes (768-d: about 3 KB per chunk), everything
lives on SSD, and the Qdrant template caps memory (`mem_limit: 8g`).

## What was deliberately not done

* No runtime query API for agents to fetch policy or skills (ADR-0001).
* No editing of the policy floor through the UI or API (ADR-0004).
* No secret storage (ADR-0006).
* No per-user accounts or multi-user use (the same-user residual risk would remain, `SECURITY.md` section 4).
* No automatic scanning of MCP servers; `scan_status` is a field you set after running your own MCP scanner (`hubctl attest-mcp`).
* No read-only delivery and no re-render at Hermes launch. Undoing an apply is `hubctl rollback` or a manual copy from backups.
