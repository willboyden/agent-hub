# Changelog

## 0.1.0 (unreleased)

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
- First audit round (H1, H2, M1-M7, L1-L5) and re-audit (N1, N1b, N2, N4, N6): delivery-target checks against the floor,
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
