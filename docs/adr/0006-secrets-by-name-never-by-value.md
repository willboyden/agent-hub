# ADR-0006: Secrets never live in the hub; names only, redacted output

Status: accepted (decision 6). Implemented.

## Context

The project's principle: never let secrets touch disk unprotected. MCP servers need tokens; diffs, plans and audit logs are shown in
a browser and stored.

## Decision

MCP items carry **environment variable names** (`env_names`, validated as identifiers). Adapters emit references by name
and never values. API responses, plans, diffs and audit params pass through `redact`/`scrub` (key-name patterns and known
token shapes such as `hf_`, `sk-`, `ghp_`, `Bearer`, private-key headers). The bootstrap key is written 0600 and only its
path is logged. The knowledge admin key setting is `repr=False` and shown as `[set, N chars]`.

## Consequences

* The content repo can be committed and backed up without holding credentials.
* Cost: redaction is pattern-based. A secret in an unusual shape pasted into a skill body may be stored and rendered.
  Import does not scan for secrets.
* Cost: the hub cannot deliver a real credential to a client; that stays with env files and brokers.
* Cost: env files (`~/.config/agent-hub/env`) still hold `HUB_API_KEY` and possibly `HUB_KNOWLEDGE_ADMIN_KEY`; they must be 0600 and are the operator's responsibility. The knowledge service takes its router and Qdrant keys from env NAMES (`KNOWLEDGE_EMBED_API_KEY`, `KNOWLEDGE_QDRANT_API_KEY`).

## Alternatives considered

* **A built-in secret store.** Rejected: a second vault to protect, and env files and credential brokers already exist.
* **Reference by URI to an external secret manager.** Not needed by any current client.
