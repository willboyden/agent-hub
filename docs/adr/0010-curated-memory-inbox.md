# ADR-0010: Curated memory: agents write to an inbox, a human promotes

Status: accepted (decision 10). Implemented.

## Context

A model that can write memories later injected into every client can poison all of them. This formalises the direction of trust: agents propose, a human decides.

## Decision

Agents (client tokens with `inbox:write` for their own client) may only POST proposals to `/memory/inbox`, stored as
`content/inbox/<client>/<id>.md` and **never compiled**. Only an admin can promote (into `content/memory/`, still needing
commit, plan and apply) or reject. Limits: 16 KiB body, 200-character title, 20 per minute and 200 items per client.
Audit records size and field names, not text. Memory is an advisory concern by default.

## Consequences

* An injected note reaches no client without a human reading it and a normal commit.
* Cost: human toil; an unread inbox fills (429 `inbox_full`) and blocks legitimate proposals.
* Residual: a hurried admin can promote poison; the promoted text is prompt content like any other.
* Cost: inbox files are written straight into the content working tree, so they show up in `hubctl status` as pending
  changes until committed or discarded (read from the code: `inbox` is not excluded from git status; not run).

## Alternatives considered

* **Live shared memory written by agents.** Rejected: the poisoning channel this ADR exists to close.
* **Store the inbox in the DB rather than files.** Not chosen: files keep one storage model and promotion is a plain move
  into content; the trade-off is the working-tree noise noted above.
* **No agent write path at all.** Rejected: proposals are cheap and useful, and the human gate makes them safe enough.
