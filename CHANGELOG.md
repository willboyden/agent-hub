# Changelog

## 0.2.0

Upgrading from 0.1.0: pull, run `make setup` (it also enables the pre-push hook), restart the hub (the UI is served from the
checkout), and reload the browser once with Ctrl+Shift+R. Skills whose recorded Security Scan was `incomplete` are evaluated
again the next time they are queued. Back up the state dir first; no state migration is needed.

### Added
- Browser end-to-end suite (`frontend/e2e/run.mjs`, `make e2e`, part of `make all`, CI job `e2e`): `hubctl init` and the
  real server in a throwaway HOME (loopback not trusted, signed in with the bootstrap key), a skill, an agent, a
  collection and a Claude Code client created through the API, then every page, detail route and tab in headless Chrome
  over the DevTools protocol with Node 22's built-in WebSocket (no npm), once before and once after committing that
  content. It fails on an error or not-found state, a dialog that opened by itself, the sign-in dialog, an uncaught
  exception, a console error, an API 5xx (the knowledge proxy's designed `503 knowledge_unavailable` excepted while that
  service is not running) and a frontend file without `Cache-Control: no-cache`. The backend gets `PATH=/usr/bin:/bin`,
  so locally installed tools are as absent as in CI.
- Ship gate: a versioned `.githooks/pre-push` (enabled by `make hooks`, which `make setup` now runs) refuses to push a
  commit or tag unless the suite passed on its exact `backend/`, `frontend/` and `policy/` trees (`scripts/e2e-gate.sh`,
  record kept in `.git`); when the pushed commit is the clean HEAD it runs the suite first. A Chrome that is not on PATH
  is named once per checkout with `git config agent-hub.chrome <path>`.
- Desktop launcher (Linux, opt-in): `agent-hub open` starts the hub in the background if needed and opens the UI;
  `agent-hub stop` stops the hub it started. `make install-desktop` adds the app-menu entry and `make install-desktop-icon`
  also places it on the desktop. See `docs/OPERATIONS.md` section 2.
- Sign-in dialog is a standard username + password form, so the browser's password manager can save the key and fill
  it in. The username is only a label; the hub still authenticates the key alone. Caveats in `docs/SECURITY.md` section 13.
- docs: recovering from a lost admin key (`docs/OPERATIONS.md`, "Lost admin key").

- `turnstone` adapter for Turnstone (github.com/turnstonelabs/turnstone): renders a stage tree (skills,
  `instructions.md`, `mcp.json`); `deploy/turnstone-deliver.py` pushes it through Turnstone's admin API as hub-owned
  skills with `auto_approve`/`is_default` off and no `allowed_tools`, and `verify` asks the console (loopback console and
  a token file under `~/.config/agent-hub/` only). The script refuses non-loopback plain http, unnamed remote hosts,
  redirects, proxies, a token file that is not one private token, and `--prune` on an empty stage. See
  `docs/OPERATIONS.md` section 8b.
- Turnstone skill labels: the client param `category_from_source` maps a skill's hub `source` to a Turnstone category
  (the one label Turnstone's Skills list shows). The adapter stages `skills-meta.json`; `turnstone-deliver.py` sets the
  category and adds it, plus the skill's hub tags, after the `agent-hub` tag (a tag Turnstone cannot take is left out
  with a warning). Unlisted skills keep their category.
  See `docs/OPERATIONS.md` section 8b.
- A plan is blocked with `root_missing` when a managed artifact's client root does not exist (the hub never creates a
  client root), instead of the apply failing with a file-not-found error.
- Observed inventory: `ClientAdapter.observe()` and a Hermes implementation (reads a running container's skills directory
  read-only), stored per client and compared with hub content. `hubctl observe`, `POST /api/v1/clients/{id}/observe`.
- Skill evaluation with NVIDIA SkillEvaluator as a main view (Evaluations) for hub and observed skills: FIFO queue, reuse by
  content hash, static pass first, optional LLM pass; never runs the `dependency` check. `hubctl evaluate` (needs `--wait`
  to run), `/api/v1/skill-evaluations*`.
- Import gate, fail closed: settings `eval_gate` (discover import, default `advisory`), `eval_gate_observed` (observed import,
  default `require`) and `eval_gate_min_tiers`, decided by the static security result only. A missing or unfinished security
  validator (SkillSpector's frequent `incomplete`) blocks until overridden with a recorded reason (`--override-reason`, audit
  `skill.import.override`). Observed import refuses secret-shaped text (`secret_detected`) unless overridden, and client ids
  `hub` and ids containing `:` are reserved.
  `hubctl import-observed` and `POST /api/v1/clients/{id}/observed/import` (enable defaults to off). See ADR-0011,
  `docs/OPERATIONS.md` section 4d, `docs/SECURITY.md` section 14.

### Changed
- **The import gate no longer blocks a skill only because it mentions local paths it does not bundle.** SkillSpector 2.12
  marks its analysis `partial` whenever a SKILL.md refers to a local path that is not in the skill (a repository script, a
  config file), even when every bundled file was fully analysed, and SkillEvaluator 0.5.0 reports that as an incomplete
  Security Scan, which the gate blocked as "the scan did not finish" (in a live run: 94 of 161 blocks). For that case the
  hub now runs SkillSpector's static scan on the same private copy (contained like SkillEvaluator) and reads why; when the
  only reason is unresolved local references (full coverage, no file partially inspected or skipped, no limitation, no
  fatal entry, LLM-backed analyzers off only because of `--no-llm`) it records `scan_partial.unresolved_references` and
  the gate **warns**, naming the references. Every other partial reason still blocks, and critical/high findings still
  block. Measured through a scratch hub on 61 real skills with the real launcher and SkillSpector 2.12.0: before, 55
  blocked and 6 ok; after, 25 blocked (17 scans with a real coverage gap, such as an unreadable `__pycache__/*.pyc` or a
  parse limit, and 8 with critical/high findings), 30 warn and 6 ok. Run records gain `eval_rev` (2) and `scan_partial`;
  older runs whose Security Scan was incomplete are evaluated again once instead of being reused.

### Security
- **The floor's re-check of rendered output now covers TOML MCP blocks.** The generic adapter's `format: toml` MCP output
  (`[<key_path>.<name>]` tables between hub markers) was not inspected, so an adapter bug or a hostile spec could have
  rendered an unapproved server there. Each server table must now be in the approved (clean) set, and a block that does
  not parse, or holds anything besides server tables under one key path, is a `floor_violation_rendered` finding (it
  blocks a managed concern). Tests include a block with a smuggled server, which is refused, and the old planner fails
  them. Markdown instruction blocks and skill/agent files are still not re-checked after rendering (`docs/SECURITY.md`
  section 12).

### Fixed
- **Leaving the editor opened its "Delete <item>?" confirmation, and leaving Collections threw an error.** Both views
  defined a method named `remove` (their delete flow), which replaced the DOM's `Element.remove()` that the router calls
  to detach a view, so every navigation away ran the delete flow: the editor asked to delete the item you had just left
  (confirming would have deleted it), and Collections called its handler with no collection and threw. Renamed to
  `confirmDelete`; a frontend test now fails when a view or component defines a method named like a DOM method. Found
  by the new browser suite.
- **Frontend files were served without `Cache-Control`,** so after an update a browser could keep old ES modules for days
  (heuristic caching) and pair them with new views, which then fail their imports. Every frontend file is now served with
  `Cache-Control: no-cache` and answers a matching `If-None-Match` with 304; the API keeps `no-store`. A view whose code
  fails to load now says to reload with Ctrl+Shift+R. A browser that holds files from before this release needs that
  reload once.
- The editor's Rendered tab showed an error ("client not committed") for a client created but not committed yet. It is
  a normal step, so the tab now says the client is not committed yet and links to Changes.
- Turning a concern off for a client (it becomes advisory) now releases the lock entries of its files and leaves the
  files in place. Before, the old entry stayed, so `hubctl verify` kept comparing a file the hub no longer manages with
  the content it last delivered, and failed once that file was edited. A file that still holds a managed concern's slice
  keeps its entry. A released entry is not restored by rollback.

## 0.1.0

First public build. Review your first apply in the UI plan view.

### Implemented
- Content repo (its own git repository, by default `~/.local/share/agent-hub/content`) with instructions, skills, agents, MCP
  servers, rules, memory, collections, clients and profiles; strict validation; plans from the committed tree; edit, commit,
  plan, apply with a lock manifest, conflict protection, backups and per-client all-or-nothing writes.
- `hubctl`: init, validate, plan, apply, verify, drift, audit, import, status, commit, rollback, applies, migrate-content,
  attest-mcp, retire-bootstrap-key, doctor, adapters, serve.
- Adapters: `claude-code`, `opencode`, `hermes-agent` (and variants), and a declarative `generic` adapter with live spec
  validation and dry render.
- Policy floor (`policy/floor.yaml`), roles and scoped keys, Host/Origin/CSRF/CSP hardening, append-only audit, metrics.
- Zero-build UI: Home, Library, Collections (with deterministic suggestions), Matrix, Editor, Clients (status separating
  adopted, applied, edited outside and pending), Import (link identical items), Changes/Plan (with working-tree previews),
  Inbox, Knowledge (with a health chip), Settings (including Host hardening).
- Knowledge plane (`knowledge/`, default port 8795): LanceDB and Qdrant backends, scoped tokens, ingest jobs, query,
  shared-index registry, read-only broker addon, compose templates for Qdrant and the broker.
- Opt-in `deploy/agent-hub.service` and `deploy/agent-knowledge.service` (narrow `ReadWritePaths`, venv entry points).
- CI workflow (lint, `mypy --strict`, tests, dependency audit per project).

### Security work included in 0.1.0
- Internal review fixes (H1, H2, M1-M7, L1-L5, then N1, N1b, N2, N4, N6; self and AI-assisted review, not an independent audit): delivery-target checks against the floor,
  HMAC-bound MCP scan attestation, marker-injection and skill-frontmatter refusals, floor pattern probes and a
  rendered-artifact re-check, directory-fd writes with compare-and-swap, content repo moved out of the project tree with
  unsafe-repo refusal, per-namespace store locks and keyed Qdrant in the knowledge service. See `docs/SECURITY.md` section 12.
- Residual-risk tooling: `hubctl doctor` (also `GET /api/v1/doctor`), hidden key prompt, `hubctl retire-bootstrap-key`.
- `HUB_TRUST_LOOPBACK` defaults to false; `hubctl serve --port` and the Host allowlist agree; a non-loopback `HUB_HOST` is
  refused unless `HUB_ALLOW_NONLOOPBACK=1`.

### Platform
- Developed and used on Linux; other platforms are not supported.

### Known limits
- The rendered-output floor re-check covers JSON and YAML permission and MCP artifacts only; TOML and block artifacts are
  not inspected.
- A project root that contains the app tree is allowed; targets inside the app tree except `out/` are refused.
- A tiny window remains between the compare-and-swap read and the rename.
- A same-user process (including an unsandboxed agent client) can read the admin key and anything else you can read; the
  fix is a separate OS user for that client, a host-level decision.
- Delivered files are not read-only to clients; `hubctl import` is not audited.
- Under the narrow systemd unit, root-level `AGENTS.md`/`CLAUDE.md` cannot be applied from the UI (use `hubctl apply`).
