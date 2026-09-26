# ADR-0002: One ClientAdapter per client, plus a declarative generic adapter

Status: accepted (decision 2). Implemented.

## Context

Tool names (`Bash` / `bash` / terminal), frontmatter, config files and merge rules differ per client. New clients (Cursor,
Codex CLI and Gemini CLI were used as examples) are expected. The core, planner, API and UI should not change for each.

## Decision

A locked port (`adapters/base.py`): `caps()`, `default_config()`, a **pure** `render(ctx)` returning artifacts and
diagnostics, read-only `discover()` for import, and `verify()` that must ask the client where it can. I/O goes through
injected `FileSystem` and `CommandRunner` ports. The registry imports every module in `adapters/` that exposes `ADAPTER`;
a broken module is logged and skipped. A `generic` adapter driven by a validated YAML `spec:` covers file-based clients
without code. Content uses a canonical capability vocabulary (12 names) and model tiers (`fast/standard/deep`) that
each adapter maps.

## Consequences

* Adding a client is a spec file or one module; caps grey out unsupported concerns in the UI; lossy mappings emit
  diagnostics (errors when the client is `strict`).
* Pure `render` makes plans deterministic and unit-testable offline.
* Cost: the spec language is deliberately small (no `str.format`, no eval, no shell); clients needing logic need Python.
* Cost: adapters encode assumptions about each client's format (opencode skills directory, `{env:VAR}` MCP syntax, `type: remote`,
  bash glob matching order, `mcp__*` in Claude Code agent `tools`, Hermes skill indexing) that depend on the client version.
* Defaults: adapters document recommended `manage` defaults in `default_config()`; the resolver now honours them
  (Addendum D) after the client's own `manage` and `params.manage`.

## Alternatives considered

* **One hard-coded renderer per client inside the core.** Rejected: every new client touches core and UI.
* **Only the declarative spec.** Rejected: Hermes (stage tree, container verify) and Claude Code (marker blocks, key
  merges, wrapper commands) need logic.
* **A plugin system with entry points.** Not chosen: in-tree modules with a registry are simpler for a single-operator
  lab; an out-of-tree adapter would need packaging we do not need yet.
