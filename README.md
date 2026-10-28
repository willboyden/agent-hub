# Agent Hub

One place to organise and control what your agent clients know and may do: shared **instructions, skills, subagents,
rules, memories, MCP servers, permissions and egress**, rendered into each client's native format, plus a separate
**knowledge plane** (vector stores, shared indexes) reached through scoped tokens and broker sidecars.

Supported clients out of the box: **Claude Code**, **opencode**, **Hermes Agent**, **Turnstone**, **DeepSeek Harness** (dsh),
and anything file-based through a declarative `generic` adapter (a YAML spec; no code). License: MIT (`LICENSE`). Architecture contract:
`docs/ARCHITECTURE.md`. Per-feature status: `docs/FEATURES.md`. Reporting a vulnerability: `SECURITY.md`.

Status: **0.3.0** (what changed: `CHANGELOG.md`). Since 0.3.0 the hub, `hubctl` and the knowledge service are written in
Rust, behaviour-compatible with the Python 0.2.x line: a differential harness (`backend/parity/`) runs the same requests
against both and compares responses and the files each writes; the deliberate differences are listed in `CHANGELOG.md`
and `backend/PORTING.md`. Developed and used on Linux by one operator; every feature's status is in `docs/FEATURES.md`,
and the browser suite (`make e2e`) gates every push.

Security: read `docs/SECURITY.md`, including its residual risks. The hub cannot protect you from an unsandboxed client
that runs as your own OS user; run `hubctl doctor` to see what is exposed on your host.

## What it is, and why

Agent clients each keep instructions, skills, agents, MCP servers and permissions in a different format and place.
Shared content drifts, and nothing shows what a client is allowed to do. The hub fixes that with three ideas:

1. **A git repo of declarative files is the source of truth** (the *content repo*, by default
   `~/.local/share/agent-hub/content`). The hub is a *compiler and change-controlled control plane*, never a runtime
   service the clients query (containerised clients often cannot reach a host service).
2. **Edit, commit, plan, apply.** UI and CLI edits change the content working tree (pending changes). A *commit* is the
   approval of content. A *plan* renders the committed tree per client and shows a diff. *Apply* writes only from that
   plan, only after `confirm`, and refuses if anything moved since the plan.
3. **A policy floor** (`policy/floor.yaml`, in the app repo, not editable through the API) that content can only make
   stricter: credential paths denied, egress deny-by-default, no unscanned MCP server, no catch-all shell allow.

## Quickstart

Prerequisites: `git`, `rustup` (it installs the exact toolchain pinned in `backend/rust-toolchain.toml`) and a C linker
(`cc`). protoc, which the knowledge service's Lance engine needs at build time, is vendored; nothing else is installed.
No Node is needed to run anything (the UI is zero-build; Node is only for the frontend tests and the browser suite), and
no Python (only the optional delivery scripts use `python3`: `deploy/turnstone-deliver.py`, and `deploy/dsh-deliver.sh`
to read dsh's answer when `DSH_CONTAINER` is set).

```bash
make release                    # backend/target/release/{agent-hub,hubctl,agent-knowledge}; the first build takes
                                # several minutes (the knowledge service compiles the Lance engine)
export PATH="$PWD/backend/target/release:$PATH"   # or call the binaries by their path

hubctl init                     # creates the content repo (default ~/.local/share/agent-hub/content), seeds it,
                                # first commit, and writes the bootstrap admin key (0600). Prints both paths.
hubctl adapters                 # lists the installed adapter ids
```

**Register clients.** A client is a file `clients/<id>.yaml` in the content repo. Add it in the UI (Clients, add-client
wizard) or write the file. Example ids and roots:

| client id | adapter | root |
|---|---|---|
| `claude-code` | `claude-code` | `project: ~/work/my-project` |
| `opencode` | `opencode` | `project: ~/work/my-project` |
| `hermes` | `hermes-agent` | `stage: <checkout>/hub/out/hermes` |
| `turnstone` | `turnstone` | `stage: <checkout>/out/turnstone` |
| `deepseek-harness` | `deepseek-harness` | `stage: <checkout>/out/deepseek-harness` |

The Hermes root is a hub-owned **stage** directory (gitignored), not the container volume: applying writes the stage, and a
delivery script you run by hand copies it into your container. Turnstone works the same way, except that its delivery
script pushes the stage through Turnstone's admin API (`docs/OPERATIONS.md` section 8b). DeepSeek Harness too: its
delivery script copies the stage into the docker volume dsh reads as its agents home, through a throwaway container with
no network, and rolls back unless dsh lists every staged skill (section 8c). For now keep `instructions` off for a dsh
client: the hub lets only `opencode` and `generic` write an `AGENTS.md`, so a staged one blocks the apply
(`docs/ADDING-A-CLIENT.md`). Create a stage directory before the first apply: the hub never creates a client root.

```bash
hubctl import --client claude-code --all     # into the content WORKING TREE; nothing touches the client
hubctl import --client opencode --all        # identical items are linked, differing ones skipped (reported)
hubctl status                                # pending (uncommitted) content changes
hubctl validate
hubctl commit -m "import my clients"         # the content approval
hubctl plan                                  # exit 2 if any client is blocked; writes nothing
hubctl serve                                 # http://127.0.0.1:8792 (API + UI); `agent-hub` serves the same app
```

Open http://127.0.0.1:8792. To use another port set `HUB_PORT` or pass `hubctl serve --port N`. The UI asks for the API
key on first load and keeps it in the tab's `sessionStorage`.

`import` also records what each client has *right now* as its last-applied state (a lock manifest in the state directory),
so the first plan shows ordinary changes with diffs instead of a wall of conflicts. That is an adoption record, not an
apply: no client file is written.

### Keys, and why loopback trust is off

* `hubctl init` mints an admin key and writes it to `~/.local/share/agent-hub/bootstrap-admin.key` (0600, directory 0700).
  Only its SHA-256 is stored in the DB. `hubctl serve` prints the file path, never the key.
* **Recommended flow: create, store in a password manager, retire.** `hubctl retire-bootstrap-key` verifies the key, then
  zero-fills and deletes the file. Afterwards `hubctl` prompts for the key (hidden), or reads `HUB_API_KEY` from the
  environment or `~/.config/agent-hub/env` (0600).
* **`HUB_TRUST_LOOPBACK` defaults to `false`.** If it were on, any local process on `127.0.0.1` would be admin without a
  key, including an agent with an unsandboxed shell, which could then edit its own policy. So a bearer key is required
  everywhere, including `hubctl`.
* The remaining risk is the same-user process that can read your files (see `docs/SECURITY.md`). Run `hubctl doctor`.

### The loop

```
edit (UI / hubctl import / API)  ->  working tree  (pending changes)
commit                            ->  content approved (git commit, fixed identity "Agent Hub")
plan  [--client X]                ->  render the COMMITTED tree, diff vs live files, floor check; writes nothing
review                            ->  Changes view: per-file diffs, diagnostics, floor violations, conflicts
apply --yes [--adopt root:path]   ->  stage, atomic commit per client, backup, lock update, verify
verify / drift / audit            ->  did the client load it; who edited what; is the live config within the floor
```

A UI **preview plan** renders the uncommitted working tree so you can see the effect before committing. It cannot be
applied.

### Before your first apply

Follow the first-apply checklist in `docs/OPERATIONS.md`. In short: a project-root client (Claude Code, opencode) is written
**inside your project directory** (`.claude/skills/`, `.claude/agents/`, a `CLAUDE.md` marker block, `.opencode/agent/`,
`AGENTS.md`), so expect changes in that repo's `git status`. Advisory-by-default files (`.mcp.json`, `.claude/settings.json`,
`opencode.json`) are not written. Every overwritten file is copied to `~/.local/share/agent-hub/backups/` first, and
`hubctl rollback --client NAME` restores the last apply (it refuses to overwrite files edited since; `--dry-run` shows the diff).

## Relationship to your existing setup

* **Import first.** The hub reads what your clients already have, so you start from your real content, not from nothing.
* **Managed vs advisory, per client and per concern** (`skills, agents, instructions, mcp, permissions, memory`). *Managed*:
  the hub writes it. *Advisory*: the plan shows what would change and `hubctl audit` checks the live file against the floor,
  but nothing is written. Defaults: skills, agents and instructions managed; mcp, permissions and memory advisory. If another
  tool or script already owns a file (a config sync script, a dotfiles manager), leave that concern advisory.
* **Nothing is overwritten silently.** A file whose live content differs from what the hub last applied is a `conflict` and
  is skipped unless you adopt it explicitly.
* **See what a client really has, and evaluate it before import.** `hubctl observe` reads a client's actual skills (Hermes:
  from the running container, read-only); NVIDIA SkillEvaluator (optional install) can evaluate them, and an import gate
  (`eval_gate`, default advisory) can require a clean static security scan first. See `docs/OPERATIONS.md` section 4d.
* **The hub does not replace your other tools.** It writes native files; it does not call or wrap them.

## Adding a client

Two ways, neither needs a change to the core or the UI (`docs/ADDING-A-CLIENT.md`): a declarative `generic` spec (a YAML
file, validated live in the wizard), or one Rust file in `backend/crates/agent-hub/src/adapters/` exposing
`pub fn adapter()`; the build discovers it, nothing else is edited. The example specs under `examples/clients/` are
examples; adjust them to your client's current config format.

## Repository map

```
backend/    the Rust workspace (toolchain pinned in rust-toolchain.toml):
            crates/hub-core        Python-compatible primitives (JSON, YAML = a PyYAML port, pydantic-style
                                   validation), domain models, path safety
            crates/agent-hub       the hub server (`agent-hub`, also `open`/`stop`), `hubctl`, services/, api/, adapters/
            crates/agent-knowledge agent-knowledge: separate service, Lance (embedded) + Qdrant backends
            parity/                differential harness against the last Python backend; TEST-MAP.md, PORTING.md
frontend/   zero-build SPA (ES modules + web components), dev/mock-server.mjs
policy/     floor.yaml (app repo, not content)
deploy/     opt-in systemd --user templates, knowledge compose/Dockerfile/broker templates
docs/       ARCHITECTURE, ADDING-A-CLIENT, KNOWLEDGE-PLANE, SECURITY, OPERATIONS, FEATURES, DESIGN-DECISIONS, adr/, diagrams/
examples/   example generic client specs
```

State lives outside the tree: `~/.local/share/agent-hub/` (DB, keys, locks, plans, snapshots, backups, content repo) and
optionally `~/.config/agent-hub/env`. Ports: hub 8792, knowledge service 8795. Diagrams in `docs/diagrams/` are Mermaid.

## Developer commands

`make help` lists them: `setup`, `build`, `release`, `hooks`, `test`, `lint` (rustfmt + clippy), `typecheck` (clippy),
`deny`, `audit`, `crate-age`, `test-map`, `parity`, `frontend-test`, `knowledge-test`, `e2e`, `mock`, `all`.
`make install-desktop` adds an app-menu launcher (Linux, after `make release`); see `docs/OPERATIONS.md` section 2.
CI (`.github/workflows/ci.yml`) runs rustfmt, clippy, the tests, the test-map check, cargo-deny and cargo-audit, the
14-day dependency cooldown (`crate-age`, which queries the crates.io API), the frontend tests and the browser suite.
`make parity` replays the differential corpus against the last Python backend (it prepares that version's venvs with
`uv` under `backend/target/`).

`make e2e` (part of `make all`) builds the backend, then runs a browser end-to-end check: `hubctl init` and the real
server in a throwaway HOME,
signed in with the bootstrap key, a few items created through the API, then every page, detail route and tab in a
headless Chrome over the DevTools protocol (no npm), before and after a commit. It fails on an error or not-found state,
a dialog that opened by itself, an uncaught exception, a console error, an API 5xx (except the knowledge proxy's
designed 503 when that service is not running) and a frontend file served without `Cache-Control: no-cache`. It needs
a Chrome: `CHROME_BIN`, or `git config agent-hub.chrome /path/to/chrome` once. `make setup` enables the versioned
`.githooks/pre-push`, which refuses to push a commit the suite has not passed on (its `backend/`, `frontend/` and
`policy/` trees are recorded inside `.git` by `scripts/e2e-gate.sh`).

## License

MIT, see `LICENSE`.
