# ADR-0004: A policy floor in the app repo that content can only tighten

Status: accepted (decision 4). Implemented.

## Context

AGENTS.md hard rules (deny-by-default egress, no secrets access, scan MCP servers before trust) must not be weakenable by
a toggle in a UI that untrusted content can influence.

## Decision

`policy/floor.yaml` lives in the **app** repo, is loaded at start (missing or invalid means the app does not start:
`floor_unavailable`), is exposed read-only (`GET /policy/floor`), and is never writable through the API. Effective config
is `resolve(profile)` then floor enforcement: violating rules are dropped from the rendered output and reported as
findings; a violation blocks apply for that client (for advisory concerns it is a warning, and `audit` still flags it as
an error against the live config). Floor deny rules are always merged into the rendered permission set.

## Consequences

* A rule that allows reading an SSH key directory cannot reach a client through the hub.
* Cost: enforcement is at plan time in the hub. In the client it exists only for what is **delivered**; permissions are
  advisory by default, so most floor rules do not protect anything in a client until permissions are managed there.
* Cost: heuristics. Path overlap is an over-approximate glob comparison; command rules only reject catch-all patterns, not
  dangerous specific ones (`allow git *` passes).
* Cost: changing the floor is a git edit in the checkout; anyone who can write the checkout can weaken it.
* A ceiling (`web_fetch: max ask`) is a hard cap by design; going higher is a floor edit.

## Alternatives considered

* **Floor as content in the content repo.** Rejected: the thing being constrained could edit its constraint.
* **Floor editable through the UI with an admin key.** Rejected for the same reason, plus CSRF and key-compromise blast radius.
* **Only lint, never block.** Rejected: a warning is what silently fails on a busy day; blocking is the point.
* **Delegate entirely to each client's own permission system.** Not enough alone: the formats differ, and the hub is the only place that sees all clients.
