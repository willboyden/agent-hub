# ADR-0001: Source of truth is a git repo of declarative files; the hub compiles, it does not serve

Status: accepted (ARCHITECTURE decision 1). Implemented.

## Context

Agent clients (for example Claude Code, opencode, Hermes Agent) each keep instructions, skills, agents, MCP servers and
permissions in a different format and place, and nothing is shared between them. Containerised clients such as Hermes Agent
may sit on internal-only Docker networks with no host mounts, and rootless Docker cannot reach host loopback, so a host
service can be invisible to them. What agents are told and may do also deserves history, review and rollback.

## Decision

Content is a set of declarative files in a **separate git repository** (`HUB_CONTENT_DIR`; the default is
`~/.local/share/agent-hub/content`, outside the project tree, after re-audit finding N1: agents can write inside the project
tree and a planted `.git/config` would run commands as the hub user). The hub reads it, renders it per client through an
adapter, and writes native files. Clients never query the hub at runtime. Plans render the **committed** tree; the working
tree is only "pending changes".

## Consequences

* Good: git gives history, diff, revert and offline backup for free; containerised clients need nothing reachable; a hub
  outage cannot break a running agent.
* Good: a plan is bound to a tree hash, so review and apply refer to the same bytes.
* Cost: changes only take effect after commit + plan + apply, and for Hermes after a manual delivery step. There is no live toggle.
* Cost: delivered files are copies; they can be edited by the client or the operator (handled by conflict protection,
  ADR-0005, not prevented).
* Cost: all commits carry one fixed identity (`Agent Hub`), so git does not say who approved; the audit DB records the
  actor. The content repo is a separate repository and needs its own backup.
* Cost: content is text that becomes prompts; git history records it but cannot judge it.

## Alternatives considered

* **A runtime service the clients query** (the hub as an MCP server or HTTP config endpoint). Rejected: containerised clients
  cannot reach it, and it puts a live control plane on every agent's critical path.
* **Database-backed content** (SQLite). Rejected: it loses free
  diff/blame/revert and makes hand-editing and review awkward.
* **Hub renders inputs and existing sync scripts deliver** (an early proposal). Not what was built: the hub writes directly
  with its own atomic-write and lock machinery, and offers a delivery script for Hermes. Existing sync scripts stay
  authoritative for what they own (ADR-0005). The proposal's benefit, inheriting those scripts' tested delivery, was given
  up.
