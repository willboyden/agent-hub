# Changelog

## 0.3.0

Upgrading from 0.2.x: the hub, `hubctl` and the knowledge service are now Rust. Stop the 0.2.x hub (a hub that the 0.2.x
desktop launcher started is not stopped by the new one), pull, `make release`, point systemd units and the desktop
launcher at `backend/target/release/` (`make install-desktop` again; `docs/OPERATIONS.md` sections 6 and 7), start it and
run `hubctl validate && hubctl plan`. The state dir, the content repo, `~/.config/agent-hub/env` and every `HUB_*`
variable are read unchanged; no migration is needed. Back up the state dir first. The old virtualenvs (`backend/.venv`,
`knowledge/.venv`) are no longer used.

Also on upgrading: the floor denies reading a few more credential files and the hub refuses to deliver into more shell
and tool config files (Security, below), so the first plan after the upgrade shows new deny entries in every client's
rendered permissions; review and apply it as usual. The binaries find `policy/` and `frontend/` from their own location
in the checkout (`backend/target/release/`); a binary copied elsewhere needs `AGENT_HUB_APP_ROOT=<checkout>`. Turnstone
verify runs `/usr/bin/python3` (or `/bin/python3`); set `HUB_PYTHON` to an absolute path to use another interpreter.

### Added
- `deepseek-harness` adapter for DeepSeek Harness (dsh). A Python version was written after 0.2.0 and never released;
  the Rust adapter renders the same bytes (`backend/PORTING.md` A-15). It renders a stage tree (`skills/`, `AGENTS.md`,
  and `hub.cordis.patch.yml` with MCP servers as `@deepseek-ai/dsh-mcp-client` rows). dsh inserts `SKILL.md` verbatim
  into the prompt between its own `<skill_...>` tags, so the plan refuses (error, not staged) a skill holding such a tag,
  a `SKILL.md` over 64 KiB, a skill folder over 4 MiB, a name dsh rejects or a frontmatter name that differs, and a
  description that is missing, over 1024 characters or holds control characters; `allowed-tools` is a warning. The patch
  is written by the safe YAML dumper and a server whose values would put `!!`, `!<` or a `%` line into it is refused (a
  YAML tag in a dsh patch can execute JavaScript). Agents, memory and permission rules are not rendered. `verify` asks
  the running container through dsh's own discovery (`node /opt/lab/list-skills.mjs`, a helper the image provides) and
  needs every staged skill listed with source `user-agents`; the container and docker context params must be plain
  names, or nothing runs. Known limit: the hub lets only `opencode` and `generic` write an `AGENTS.md`, so applying a dsh
  client with an instruction enabled is blocked (`unsafe_path`); keep `instructions` off for now.
- `deploy/dsh-deliver.sh` (opt-in, never run by the hub): copies `skills/`, `AGENTS.md` and `hub.cordis.patch.yml` from the
  stage into the docker volume dsh reads as `DSH_AGENTS_HOME`, through a throwaway pinned `alpine` container with no
  network, root-owned and world-readable; refuses symlinks, hard links, special files, the skills above and a tagged
  patch; swaps with `mv`, keeps the previous tree for `rollback`, and with `DSH_CONTAINER` set rolls back unless dsh lists
  every staged skill. See `docs/OPERATIONS.md` section 8c.

### Changed
- **The backend is rewritten in Rust** (one workspace in `backend/`: `hub-core`, `agent-hub` with `hubctl`,
  `agent-knowledge`), behaviour-compatible with 0.2.x. The HTTP API, the files written to clients and to the content repo
  (byte for byte; YAML through a port of PyYAML 6.0.3's safe loader and dumper), the state on disk and the subprocess
  argv are the same. Proof: every test of the Python backend has a Rust counterpart (`backend/TEST-MAP.md`), and a
  differential harness (`make parity`) sends the same requests to the last Python backend and to the Rust one in the
  same temporary layout and compares responses and the files each wrote. Why: ADR-0012.
- Building needs `rustup` (the toolchain is pinned in `backend/rust-toolchain.toml`) and a C linker; protoc for the
  Lance engine is vendored. Running needs no interpreter; only the opt-in delivery scripts use `python3`
  (`deploy/turnstone-deliver.py`, and `deploy/dsh-deliver.sh` for its check).
- `make` targets: `release`, `build`, `clippy` (`typecheck` is clippy), `deny`, `audit`, `crate-age`, `test-map`,
  `parity`; the Python targets (`setup-knowledge`, ruff, mypy, pytest) are gone. CI runs rustfmt, clippy, the tests,
  cargo-deny, cargo-audit, the 14-day dependency cooldown (`crate-age`, which needs the crates.io API), the frontend
  tests and the browser suite.
- Static frontend files answer `Range` requests the way Starlette did (206, multipart ranges, 416).

### Security
Three problems in 0.2.x were found while porting; 0.3.0 does not have them. 0.2.x will not get fixes.
- **High:** `hubctl migrate-content --yes`, which the doctor recommends for a content repo with unsafe git metadata,
  followed symbolic links while cleaning that metadata, so a planted link could make it delete or overwrite files
  outside the content repo. It now removes such links instead of following them (SB-1).
- **Medium:** the content-repo check read a `.git/config` that was a symbolic link and quoted parts of its target in the
  error message, which could expose part of a private file. A linked `.git/config` is now refused without being read
  (SB-2).
- **Medium:** the memory-inbox listing used the `client` query value as a path without checking it, so a key holder
  could list the `*.md` file names of another directory. A value that is not a valid client id now lists nothing (SB-3).
- Also fixed: a FIFO placed at a delivery target stalled an apply while it held the hub's lock (targets are now opened
  non-blocking and such a target is refused); a local process holding the hub's port could keep `agent-hub open`
  waiting for a long time (its probe deadline now covers the whole exchange); a mutating request with a JSON body nested
  beyond the decoder's limit left no audit row (it is now audited).
- From an independent review of the new code before release:
  - Every file the hub reads from a place someone else can write (discovery, verify, plan, rollback, doctor, the
    content repo's untracked files, content migration) is opened without blocking and must be a regular file: a FIFO
    planted in a skills folder or at a delivered path could otherwise stall the hub while it held its lock.
  - Output of commands the hub runs to verify a client (such as `docker exec` into the Hermes container) is capped
    while it is read, stderr included, so a misbehaving container cannot exhaust the hub's memory.
  - Turnstone verify no longer looks `python3` up on `PATH`, where a directory you can write such as `~/.local/bin`
    often comes first: it runs `HUB_PYTHON` or the system Python.
  - Hermes verify checks the compose project, service, docker context, user, `hermes_bin` and container name with the
    same plain-name rules as observe before any of them reaches a docker command line.
  - A host name that merely starts with `127.` is no longer accepted as a loopback bind address.
  - The hub refuses to deliver into `.bash_profile`, `.bash_login`, `.zprofile`, `.zshenv`, `~/.config/fish`,
    `~/.config/environment.d`, `~/.config/git`, `.gitconfig`, `.git-credentials`, `~/.docker`, `~/.kube`, `.npmrc` and
    `.pypirc`; the floor denies agents reading `~/.docker`, `~/.kube`, `~/.git-credentials`, `~/.config/gh` and
    `~/.npmrc`.
  - The HTTP server closes connections that do not send their headers within 30 s or stay idle that long, serves at
    most 256 connections at once, answers 408 to a request body that does not arrive within 60 s and 400 to one that
    breaks off (it was handed on as a shorter body), and answers 500 (with an audit row) if a handler panics. A call to
    the knowledge service has a total deadline, and a knowledge stream's slot is released when the browser goes away
    early. The knowledge service's own server has the same connection limits and body rules.
  - Release binaries contain no file path of the machine that built them.

### Python bugs: kept or fixed
- Kept, for compatibility: a failure to write an evaluation's `run.json` skips the clean-up after it, as before; the
  generic adapter still accepts a flat-skill or memory `filename` template with `..` when a spec is validated and
  refuses it only when rendering.
- Fixed: the three security problems and the three smaller ones above.

### Known limitations
- The MCP scan attestation (`scan_digest`) covers a server's command, arguments, URL, pinned ref and transport, but not
  its `env_names`, `sandbox_profile` or `egress_hosts`; changing those keeps a `clean` status. The change is still a
  content commit you review in the plan, and the floor still checks egress. Widening the attestation would invalidate
  every existing one, so it is planned for a later release.

### Deliberate differences from 0.2.x
The full list, with reasons, is in `backend/PORTING.md` (section 18 and its addenda). The ones you may notice:
- Unexpected-error 500 responses carry the same security headers and `Cache-Control: no-store` as every other response.
- A request body nested deeper than 256 levels is refused with 400 "There was an error parsing the body" (0.2.x decoded
  up to about 9,991 levels). `NaN`/`Infinity` in a request body reach the handlers as `null`, and integers beyond 64 bits
  as floats.
- The desktop launcher starts the hub in its own process group rather than its own session, and `agent-hub stop` does
  not stop a hub that the 0.2.x launcher started.
- Turnstone verification runs `HUB_PYTHON`, else `/usr/bin/python3` or `/bin/python3`, to call
  `deploy/turnstone-deliver.py` (0.2.x used its own interpreter).
- `HUB_HOST` must be `localhost` or a loopback IP address unless `HUB_ALLOW_NONLOOPBACK=1`; a name starting with `127.`
  no longer counts as loopback.
- A request body that breaks off is a 400 (`body_incomplete`) and one slower than 60 s a 408 (`body_timeout`).
- `hubctl` help and usage-error text comes from a different argument parser; exit codes are unchanged. `hubctl import
  --item x` without `kind:` prints an error (0.2.x crashed; both exit 1).
- A knowledge-proxy stream whose upstream fails answers with an error response instead of starting the stream and
  aborting it.
- YAML edge cases: `!!omap`/`!!pairs` items are lists, a recursive alias is refused, a lone surrogate escape becomes
  U+FFFD, and an integer of more than 4300 digits is refused when loaded rather than when used.
- An evaluation report containing `NaN`, `Infinity` or nesting deeper than 128 levels is read as an `error` verdict.
- Lowercasing (hostnames, git config keys) uses newer Unicode tables than Python 3.13: 55 code points added in Unicode
  16 and later lower differently.
- Knowledge service: transport errors are named after the closest httpx class; the offline hash embedder may tokenise
  exotic text differently; a startup failure prints the final error line without a Python traceback.

### Fixed
- A signed-out tab no longer polls forever. Views refresh on timers, and after the hub's 401 the UI asked for a key but
  kept sending the refused requests. The API client now holds every request made with the same key (or with none) after a
  hub 401 and answers it locally, until the user signs in again (even with the same key) or a request succeeds; `/health`
  stays open, and a knowledge-service refusal holds nothing. The same fix went into Nemo Console and Engine Console.
- Compiled Python bytecode (`__pycache__/`, `*.pyc`, `*.pyo`) is no longer evaluated, stored with an observation or
  imported. A skill that carried a `__pycache__/` made SkillSpector report partial coverage and the import gate block it
  (6 of the 25 lab skills still blocked in the 0.2.0 measurement); bytecode is generated, unreadable to the scanners and
  never delivered, so what is evaluated stays what gets imported. An observation notes how many files it left out.

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
