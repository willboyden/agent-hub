# Agent Hub features

Status: **Implemented** (code exists and is reachable), **Partial** (some of it), **Not planned** (not built, no scheduled
work).

## Content and organising

| Feature | Status | Notes |
|---|---|---|
| Git-backed declarative content (`instructions, skills, agents, mcp, rules, memory, collections, clients, profiles`) | Implemented |  |
| Strict validation (safe YAML, no anchors/duplicate keys, unknown keys are errors, ids by `fullmatch`) | Implemented |  |
| SKILL.md standard preserved (sidecar `hub.yaml` for groups/tags/notes/provenance; extra skill files) | Implemented |  |
| Collections (multi-membership, icon/colour, order) | Implemented |  |
| Tags, bulk tag/collect/enable across kinds | Implemented |  |
| Deterministic collection suggestions (name prefixes, shared source/tags, keyword table; accept/dismiss) | Implemented |  |
| Per-client profile: `union(collections, enable) - disable`, then floor | Implemented |  |
| Matrix: items x clients, tri-state cells on / off / via-collection / unsupported / blocked with reason; toggle and bulk toggle | Implemented |  |
| Effective config per client ("what can it do now"), rules tagged `floor / content / profile` | Implemented |  |
| "Rendered for <client>" view of one item's artifacts | Implemented |  |
| Library: card/dense toggle, fuzzy search, filters (collection, tag, client, enabled, source, issues), sort, multi-select bulk bar | Implemented |  |
| Editors (skill form + markdown preview + file tree, agent capability chips, MCP, rules with floor-locked rows, ordered instructions) | Implemented |  |
| Command palette (Ctrl/Cmd-K), keyboard alternatives to drag-and-drop, dark/light theme, i18n file (English only), first-run checklist | Implemented |  |
| Memory inbox: propose (client token), edit/promote/reject (admin), never compiled | Implemented |  |
| Import wizard and `hubctl import`: discover, per-item conflict info, `skip / rename / replace / link` (default `link` identical, `skip` differing) | Implemented |  |
| Import adopts live files into the lock (first plan shows diffs, not a wall of conflicts) | Implemented |  |
| Import is byte-identical for agents | Partial | Skills and instructions round-trip; agents are re-rendered, so the first apply reports differing agent files as conflicts to review. |

## Plan, apply, drift

| Feature | Status | Notes |
|---|---|---|
| Plan from the committed tree (`git archive HEAD`), per-client digest, redacted unified diffs | Implemented |  |
| Preview plan from the working tree (cannot be applied: `plan_is_preview`) | Implemented |  |
| Commit gate (validation must pass), history, revert of a commit, discard | Implemented |  |
| Apply: confirm required, stale-plan refusal, stage then atomic commit per client, backups, rollback, lock update | Implemented |  |
| Merge modes: whole file, `json_keys`, `yaml_keys`, marker `block` | Implemented |  |
| Conflict protection with explicit `adopt_paths` | Implemented |  |
| Removal only of hub-created files whose hash still matches | Implemented |  |
| `verify` (asks the client where it can) / `drift` / `audit` | Implemented |  |
| Client status distinguishes `adopted_at` (import recorded live files) from `last_applied_at` (real applies only) and the states `in_sync / edited_outside / pending_changes / never_applied / error`, with a per-file drift breakdown | Implemented |  |
| Undo an apply: `hubctl rollback` / `POST /clients/{id}/rollback` (records every apply, restores files only while the live hash equals the post-apply hash, conflicts skipped) | Implemented |  |
| `hubctl doctor` / Settings, Host hardening: read-only host-exposure report with exact fixes | Implemented |  |
| Hidden key prompt and `hubctl retire-bootstrap-key` (verify, zero-fill, delete the bootstrap file) | Implemented |  |
| HMAC-bound MCP scan attestation (`hubctl attest-mcp`), content repo outside the project tree with `hubctl migrate-content` | Implemented |  |
| Automatic re-apply on a schedule or on client launch | Not planned | Compile-not-serve: content is rendered only when you apply. |
| Read-only (root-owned / `:ro`) delivery to clients | Not planned | See `SECURITY.md` section 10. |

## Policy and security

| Feature | Status | Notes |
|---|---|---|
| Policy floor from the app repo (paths, egress, tool ceilings, command patterns, MCP scan + egress), fail-closed load; second-audit hardening: delivery-target checks, digest-bound MCP `clean`, rendered-artifact re-check (JSON/YAML only), marker-injection and skill `allowed-tools`/`hooks` refusals, dirfd + compare-and-swap writes | Implemented |  |
| Audit of a live client config against the floor (advisory concerns included) | Implemented |  |
| Roles admin / viewer / per-client token; keys shown once, hashed | Implemented |  |
| Host allowlist, same-origin, CSRF header, strict CSP, no CORS, body and SSE caps, append-only audit | Implemented |  |
| Loopback trust (opt-in, off by default) | Implemented |  |
| Prometheus `/metrics` (auth required) | Implemented |  |
| Per-user accounts, SSO, multi-user use | Not planned | Single-operator design. |

## Clients and adapters

| Feature | Status | Notes |
|---|---|---|
| `claude-code` adapter (Claude Code, sandboxed or not): skills, agents, `CLAUDE.md` block; `.mcp.json` and `.claude/settings.json` advisory | Implemented |  |
| `opencode` adapter: agents, `AGENTS.md` (own or block), `opencode.json` advisory | Implemented |  |
| `hermes-agent` (and variants that reuse it): stage tree with skills, `AGENTS.md`, config fragments | Implemented |  |
| Hermes delivery `deploy/hermes-deliver.sh` (+ `hermes skills list` check) | Implemented |  |
| `generic` spec adapter (skills, agents, instructions, MCP, permissions, memory; json/yaml/toml) and live spec validation + dry render | Implemented |  |
| Cursor / Codex CLI / Gemini CLI example specs | Partial | Example specs; adjust to your client's current config format. |
| Capability vocabulary (`read write edit shell web_fetch web_search mcp subagent browser notebook todo image`), model tiers `fast/standard/deep` | Implemented |  |
| Hub reads a model catalog from an engine manager | Not planned | Nothing in the hub calls one. |
| Hub runs an MCP scanner itself | Not planned | `scan_status` is a field you set after running your own scanner. |

## Knowledge plane

| Feature | Status | Notes |
|---|---|---|
| Namespaces bound to a backend, embedding model and dimension (immutable) | Implemented |  |
| LanceDB backend (embedded, flat scan, Python-side filter to id prefilter) | Implemented |  |
| Qdrant backend (REST, on-disk HNSW and vectors) | Implemented |  |
| Ingest inline documents or allowlisted `{path, glob}` as a job, with SSE progress | Implemented |  |
| Query by text or vector, filter grammar (AST, never string-built) | Implemented |  |
| Per-client tokens (read default, namespace-scoped), quotas, rate limits, per-namespace store locks and timeouts, shared-index registry | Implemented |  |
| Keyed Qdrant (service sends `KNOWLEDGE_QDRANT_API_KEY`; template requires the key) and a dedicated embeddings key name `KNOWLEDGE_EMBED_API_KEY` | Implemented |  |
| Hub Knowledge view (namespaces, ingest, query playground, tokens, indexes) via same-origin proxy, with a health chip (reachable / authenticated / backends) | Implemented |  |
| Knowledge proxy reports a hub-side key problem as `502 knowledge_upstream_unauthorized` (not the browser's 401) | Implemented |  |
| Broker sidecar (read-only, query-only allowlist) for internal-net clients | Partial | Query-only allowlist for internal-net clients; the compose file is a template. |
| Containerised knowledge service image | Partial | Template: adapt the base image and install step to your environment. |
| LanceDB ANN index (IVF-PQ) | Not planned | Add if a table passes a few hundred thousand rows. |
| Router-side semantic cache, engine KV sharing | Not planned | Out of scope for the hub. |

## Operations

| Feature | Status | Notes |
|---|---|---|
| `hubctl` (init, validate, plan, apply, verify, drift, audit, import, status, commit, rollback, applies, migrate-content, attest-mcp, retire-bootstrap-key, doctor, adapters, serve), exit codes 0/1/2, `--json` | Implemented |  |
| Opt-in systemd `--user` units for the hub and knowledge service (narrow `ReadWritePaths`, venv entry points) | Implemented |  |
| Zero-build UI served by the backend; contract mock (`frontend/dev/mock-server.mjs`) | Implemented |  |
| Windows or cgroup-only hosts | Not planned | Developed and used on Linux; other platforms are not supported. |

## How the UX compares (facts only)

Descriptions of other tools are general and may lag their current releases.

* **Open WebUI** organises its own prompts, tools and knowledge in its admin and workspace pages, and changes take effect
  directly in that application. The hub differs in scope: it organises content for *several* agent clients and shows a plan
  with a diff before anything changes.
* **Claude Code**: skills are directories under `.claude/skills/<name>/SKILL.md`, agents are `.claude/agents/*.md` with
  `name/description/tools/model` frontmatter. Management is by editing files; there is no cross-client view.
* **opencode**: agents in `.opencode/agent/`, instructions in `AGENTS.md`, MCP and permissions in `opencode.json`; no skills
  directory is known to the hub's adapter (whether `.claude/skills` is read depends on your opencode version).
* **Hermes Agent**: skills live in a skills directory inside the container and `hermes skills list` reports what it loaded;
  the hub only stages files and (by a script you run) copies skills.
