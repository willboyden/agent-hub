# Operating Agent Hub

Audience: the person running the hub on their own workstation. Review your first apply in the UI plan.

Commands run from `backend/` of the checkout unless stated. Everything optional (systemd units, Docker templates,
delivery scripts) is opt-in; nothing here is started or enabled by any script in this repo.

## 1. Install

Requirements: `uv`, `git`, Python 3.13 (managed by `uv`; system 3.14 lacks some wheels). No Node needed to run.

```bash
make setup              # backend/.venv from uv.lock (--frozen: no re-resolution, no PyPI surprises)
make setup-knowledge              # only if you will run the knowledge service
cd backend && uv run hubctl init  # content repo + first commit + bootstrap admin key
```

`hubctl init` creates `HUB_CONTENT_DIR` (default `~/.local/share/agent-hub/content`, outside your project tree, its own git repo; older builds defaulted to an in-tree `content/`),
seeds `hub.yaml`, `endpoints.yaml` and `.gitkeep` files, commits, and writes
`~/.local/share/agent-hub/bootstrap-admin.key` (0600). Running it again does not reset an existing repo or key.

## 2. Run

```bash
uv run hubctl serve          # prints the key FILE path; UI + API on http://127.0.0.1:8792
# or, equivalent and what the systemd template uses:
uv run agent-hub
```

To change the port use `HUB_PORT` (what the systemd unit does) or `hubctl serve --port N`; the Host allowlist follows the
effective bind. A non-loopback `HUB_HOST` is refused at start unless `HUB_ALLOW_NONLOOPBACK=1` (then put an
authenticating reverse proxy in front and add its Host to `HUB_ALLOWED_HOSTS`).

UI: paste the admin key into the auth dialog on first load; it lives in the tab's `sessionStorage` and is gone when the
tab closes. Use the keyboard palette with Ctrl-K.

### From the desktop (opt-in, Linux)

Run these from the checkout root, after `make setup`:

```bash
make install-desktop         # menu entry: ~/.local/share/applications/agent-hub.desktop
make install-desktop-icon    # the same, plus a copy on your desktop folder (GNOME: marked trusted with `gio`)
make uninstall-desktop       # removes both
```

The launcher runs `agent-hub open`. Its right-click action **Stop Agent Hub** runs `agent-hub stop`.

- `open` only uses a loopback address: it refuses a `HUB_HOST` that is not loopback. A wildcard `HUB_HOST` (`0.0.0.0`,
  `::`) is probed and opened on `127.0.0.1` / `::1` if a hub already runs, but the launcher never starts a hub with it.
  If the hub already answers, it opens the UI with `xdg-open`. If nothing answers, it starts the hub in the background
  (`python -I -m agent_hub.main`, so the launch directory is not on its import path), waits up to 30 s for
  `/api/v1/health`, then opens the UI. Concurrent launches (a double click) wait on `<data dir>/hub.lock`, so only one
  hub is started.
- The hub's output goes to `<data dir>/hub.log` (mode 0600). It holds the server log, including one access line per
  request (method, path, status; never a key). The size is checked at each launch: past 10 MiB the file is moved to
  `hub.log.1`, replacing the previous one. A hub that runs for a long time is not rotated while it runs. The pid goes to
  `<data dir>/hub.pid`.
- It opens the page only if, at that moment, **every socket listening on the port belongs to your user** (read from
  `/proc/net/tcp` and `tcp6`) **and** the answer has the hub's health shape. That stops another user's process holding
  the port from collecting the key you paste. A process running as your own user can still impersonate the hub
  (SECURITY.md section 13, "Inherent").
- It never reads or passes a key: each new tab asks for one, as above.
- `stop` signals only the process recorded in `hub.pid`, and only if it is yours and its command line is the launcher's
  (same interpreter, same arguments). A hub started with `make run`, `hubctl serve` or systemd is not touched.
- The launcher runs the code in this checkout's `backend/.venv`, and `install-desktop-icon` marks the desktop copy as
  trusted, so a double click runs it without a prompt. Keep the checkout out of reach of the agents the hub governs
  (SECURITY.md section 13).
- The launcher starts only the hub, not the knowledge service (section 9). The Knowledge page reports it as unavailable
  until you start that service.
- A hub started this way keeps running after you log out if lingering is enabled (`loginctl show-user $USER -p Linger`).
- Failures also show as a desktop notification when `notify-send` exists.

## 3. Environment and the env file

Settings are `HUB_*` environment variables. An optional file `~/.config/agent-hub/env` is read at start (override the
path with `AGENT_HUB_ENV_FILE`); **the real environment wins over the file**. Keep it 0600. Fixed `~/.config` and
`~/.local/share` paths are used deliberately (XDG variables are unreliable under snap-packaged terminals).

| Variable | Default | Notes |
|---|---|---|
| `HUB_HOST`, `HUB_PORT` | `127.0.0.1`, `8792` | Keep loopback. The port also feeds the Host allowlist. |
| `HUB_CONTENT_DIR` | `~/.local/share/agent-hub/content` | The content git repo. Keep it outside your project tree (SECURITY N1). Move an in-tree repo with `hubctl migrate-content --to PATH`. |
| `HUB_DATA_DIR` | `~/.local/share/agent-hub` | State: `hub.db`, `bootstrap-admin.key`, `locks/`, `plans/`, `snapshots/`, `backups/`. |
| `HUB_POLICY_FILE` | `<hub>/policy/floor.yaml` | App repo, never content. |
| `HUB_TRUST_LOOPBACK` | `false` | Leave it. See `SECURITY.md` section 4. |
| `HUB_ALLOWED_HOSTS` | empty | Extra Host values (a list; pydantic-settings expects JSON such as `["hub.lan:8792"]`). |
| `HUB_KNOWLEDGE_URL` | `http://127.0.0.1:8795` | The knowledge service. |
| `HUB_KNOWLEDGE_ADMIN_KEY` (legacy `HUB_KNOWLEDGE_TOKEN`), `KNOWLEDGE_ADMIN_KEY_FILE` | unset, `~/.local/share/agent-knowledge/admin.key` | Admin key the hub proxy sends. If no key is set the hub reads the 0600 file the knowledge service writes on first start. Secret: 0600 file only. |
| `HUB_ALLOW_NONLOOPBACK` | unset | Must be `1` to bind a non-loopback `HUB_HOST`. Process environment. |
| `HUB_DISCOVER_EXTRA_ROOTS` | empty | Extra roots that `discover`/`verify` may read besides client roots (JSON list). |
| `HUB_PLAN_RETENTION`, `HUB_INBOX_MAX_PER_CLIENT`, `HUB_INBOX_RATE_PER_MIN` | `20`, `200`, `20` | Also editable in Settings. |
| `HUB_BODY_CAP_BYTES`, `HUB_IMPORT_BODY_CAP_BYTES` | 1 MiB, 4 MiB | 413 above these. |
| `HUB_MAX_SSE_CONNECTIONS`, `HUB_SSE_IDLE_TIMEOUT_S` | `8`, `300` | 429 `too_many_streams`. |
| `HUB_ENABLE_METRICS` | `true` | `/metrics` (needs a key). |
| `HUB_API_KEY` | unset | Read by `hubctl` from the process environment, else from `~/.config/agent-hub/env`, else the bootstrap key file. The server does not use it. Never printed. |

The systemd unit reads the same file through `EnvironmentFile=`; that syntax is plain `KEY=value` (no `export`).

### Key handling

Flow: **create -> store in a password manager -> retire.**

1. `hubctl init` writes `~/.local/share/agent-hub/bootstrap-admin.key` (0600). Any same-user host process can read it, and
   opencode runs unsandboxed (`SECURITY.md`, M5).
2. Copy the key into a password manager. The browser's own password manager works too: the UI's sign-in dialog is a
   standard login form (the username is only a label for the saved login; the key is the password), so the browser offers
   to save it after you sign in and fills it in next time. Read the browser caveats in `SECURITY.md` section 13 first.
3. `hubctl retire-bootstrap-key` (needs a terminal): it asks for the key with a hidden prompt, verifies it, zero-fills and
   deletes the bootstrap file.
4. From then on `hubctl` finds no key file or `HUB_API_KEY` and prompts with a hidden prompt; the UI asks for the key per
   tab. (Alternative: `HUB_API_KEY` in `~/.config/agent-hub/env`, 0600. It works but leaves a same-user-readable copy.)
5. The floor's state-dir read-deny is not delivered by the hub unless you manage a client's `permissions`. Add those paths to
   each client's own sandbox or permission config (`SECURITY.md` section 4), then run `hubctl doctor`.

### `hubctl doctor`

`hubctl doctor [--json]` is a read-only report of how exposed the hub's secrets and policy are to other processes of the
same OS user. It writes nothing and never prints a secret. The same report is in the UI at Settings, Host hardening
(`GET /api/v1/doctor`, admin; `GET /api/v1/doctor/summary` returns counts only).

Reading it: the first line is `status: <overall>  (<n> crit, <n> warn, <n> info)`. Below it each non-OK finding is listed as
`[CRIT|WARN|INFO] title`, followed by detail and the exact fix; OK checks are hidden in the text output (use `--json` to see
them). The command exits 2 when any finding is CRIT, else 0. Work top-down: fix CRIT, then WARN; INFO is context. Advisory-permission gaps for a client the hub cannot sandbox are inherent (see `SECURITY.md` section 13). Trust the current output over any description here. Run it after any change to clients, units or key handling.

## 4. The daily loop

```bash
uv run hubctl status                       # pending content changes
uv run hubctl validate
uv run hubctl commit -m "why"              # content approval
uv run hubctl plan [--client X]            # exit 0 ok, 2 blocked by errors or floor violations
uv run hubctl apply [--client X] --yes     # exit 1 on failed apply or failed verify, 2 if blocked, 1 (no write) without --yes
uv run hubctl verify | drift | audit       # per client (audit exits 2 when the live config violates the floor)
```

`apply` always re-plans first and prints the plan; without `--yes` it applies nothing. In the UI the same flow is
Changes (commit box) then plan review then Apply. Preview plans (`working` source) cannot be applied.

### First-apply checklist (nothing has ever been applied to a real client)

Do this in order, one client at a time. Steps 1-3 write nothing.

1. `hubctl status` and `hubctl validate`: no pending surprises, content valid. Commit what you mean to apply
   (`hubctl commit -m "..."`; it reports `strays`, untracked paths it did not stage).
2. Open the UI, **Changes**, run a plan, and read **every** file diff per client: file paths, the Claude Code `CLAUDE.md`
   block, the opencode `AGENTS.md` (in `own` mode the whole file becomes hub-generated), agent files, and the diagnostics
   and floor findings.
3. Note the **backups path**: `~/.local/share/agent-hub/backups/<client>/<UTC timestamp>-<hex>/<root>/<path>`. Each
   overwritten file is copied there (0600) before it is replaced.
4. Apply the **least risky client first**: a Hermes client (writes only a stage tree under `hub/out/`, nothing
   reaches a container until `deploy/hermes-deliver.sh`), or opencode agents only. Then verify.
   `hubctl apply --client <hermes-client-id> --yes` (or the UI Apply for that client only). Leave the Claude Code client and the
   root-level `AGENTS.md`/`CLAUDE.md` files for last: they change tracked files in your project repo.
5. `hubctl verify --client X` then `hubctl drift --client X`. A failed verify means files were written but the client did
   not confirm it sees them; read the checks.
6. Look at `git status` in your project repo for what changed, and read the diff again.
7. Only then apply the next client. Do not use `--adopt` to silence a conflict you have not read.

**Reverting.** Three paths:

* *Roll back the apply:* `hubctl applies --client X` lists recorded applies; `hubctl rollback --client X [--apply-id ID]
  [--dry-run] [--yes]` (or `POST /api/v1/clients/{id}/rollback`) restores files, or deletes ones the apply created, **only
  while the live hash still equals the post-apply hash**, all-or-nothing through the same safe writes. Files edited since are
  reported as conflicts and skipped unless you pass `--force-paths ROOT:PATH`. An older apply is refused if any of its files
  changed since. Always run `--dry-run` first.
* *Content revert:* the UI History (or `POST /api/v1/changes/revert {commit}`) does a `git revert` of a content commit; it
  refuses a dirty tree (`dirty_tree`). Then commit, plan, apply: the hub renders the old content back.
* *Backups:* copy a file back by hand from `~/.local/share/agent-hub/backups/<client>/<timestamp>/<root>/<path>`, then
  `hubctl plan`: the restored file shows as an `edited_outside` conflict, so adopt or re-apply deliberately.

### Adopting a conflict

`apply` skips any file whose live content differs from what the hub last applied. To let the hub overwrite it, name it:

```bash
uv run hubctl apply --client claude-code --yes --adopt project:CLAUDE.md
```

Format: `<root>:<path>` or `<client>:<root>:<path>` (both are matched; the CLI passes the strings through), where `root` is the client's root name
(`project`, `stage`) and `path` is the relative path exactly as the plan prints it. Adopt each path deliberately; the
backup of the overwritten file goes to `~/.local/share/agent-hub/backups/<client>/<timestamp>/`.

### Restoring after an apply

See "Reverting" in the first-apply checklist above. Apply itself also rolls back automatically if any write fails partway.

## 4b. Adding a client

Full reference: `docs/ADDING-A-CLIENT.md`. Operationally:

1. UI, Clients, add-client wizard: pick an adapter (`hubctl adapters`) or paste a `generic` spec; the wizard validates it
   live (`POST /clients/validate-spec`) and dry-renders it. Or write `clients/<id>.yaml` in the content repo yourself.
2. Set the roots (an absolute path or `~`, never `..`). Roots and targets are refused if they overlap the floor's
   write-deny paths, the hub state dir, credential dirs, `.git`, or the home dir or `/` (`bad_root` / `unsafe_path`).
3. Set `manage:` explicitly for the concerns you want written. Effective `manage` per concern is the client's `manage`,
   else `params.manage`, else the adapter's documented default, else the hub default (skills, agents, instructions on;
   mcp, permissions, memory advisory). The client detail shows the result as `manage_effective`.
4. `hubctl import --client <id> --all` (identical items link, differing ones are skipped and reported), review, commit, plan.
5. Treat any example spec under `examples/clients/` as examples and check every path before enabling `manage`.

## 4c. Moving an existing content repo (N1)

If your content repo is still inside your project tree (older builds defaulted to an in-tree `content/`), move it out:
`hubctl migrate-content --to ~/.local/share/agent-hub/content` (shows what would move; add `--yes` to do it), then check
`hubctl status` and `hubctl plan`. It refuses an existing target, checks the moved repo with `git fsck --strict`, and removes
the old copy. The hub refuses to run (`content_dir_unsafe`, with the fix) on a content repo whose `.git/config`, attributes,
hooks, ownership or location look unsafe; if it refuses yours, inspect those files rather than overriding.

## 5. Backup

| What | Where | How |
|---|---|---|
| Content repo | `HUB_CONTENT_DIR` (git repo; default under the state dir) | `git bundle create hub-content.bundle --all` from inside it, or copy the directory. Not in the parent repo's history, so back it up separately. |
| State | `HUB_DATA_DIR` | Stop the server (SQLite WAL), then copy the directory. It contains the **admin key hash, locks and backups of client files**: store the copy with mode 0700 and do not put it in a shared or unencrypted location. |
| Env file(s) | `~/.config/agent-hub/env`, `~/.config/agent-knowledge/env` | Hold secrets. If you bundle them, encrypt fail-closed (stream `tar | gpg`), never leave a plaintext copy. |
| Knowledge data | `~/.local/share/agent-knowledge/` (+ `lance_dir`, Qdrant volume) | Stop the service; copy. LanceDB is one directory. |

Losing the state dir loses the lock manifests: the next plan treats every existing file as "not created by the hub" and
reports conflicts. Re-import (`hubctl import`) records the current live files as adopted.

## 6. Upgrade

```bash
git pull   # or however you update the checkout
cd hub && make setup && make all        # lint, mypy, backend/knowledge/frontend tests (offline)
cd backend && uv run hubctl validate && uv run hubctl plan
```

Read the plan: adapter changes can change rendered output for unchanged content, which shows up as file diffs. The
state schema is created on first start; migrations between versions are **not** provided or tested (the project is at
0.1.0). Back up the state dir before upgrading. Pins mean tested, not frozen: bump `uv.lock` only after `make all`.

## 7. Opt-in systemd (--user)

`deploy/agent-hub.service` and `deploy/agent-knowledge.service` are templates: review every CHANGEME. Steps are in the file headers. Things that will bite:

* Every `ReadWritePaths=` entry must exist or the unit fails to start (`status=226/NAMESPACE`). Create them first
  (including `hub/out`).
* The hub unit's `ReadWritePaths=` is deliberately narrow: the state dir (which now contains the content repo), and only the delivery directories
  (`.claude/skills`, `.claude/agents`, `.opencode/agent`, `hub/out`), **not** your project root, `policy/`, `backend/`
  or any `.git`. Trade-off: root-level `AGENTS.md` and `CLAUDE.md` cannot be replaced under it (the applier renames a temp
  file in the same directory), so apply those clients with `hubctl apply` from a shell. Delete entries for clients you do
  not apply to.
* `ProtectHome=read-only` also covers `/run/user`, where the rootless docker socket lives; Hermes `verify` may then need
  the socket directory in `BindPaths=`.
* The units run the venv entry points directly (`.venv/bin/agent-hub`, `.venv/bin/agent-knowledge`), so no uv cache or
  venv write access is granted. Run `make setup` / `make setup-knowledge` first.
* `IPAddressDeny=any` + `IPAddressAllow=localhost` may need to be commented out if the first start misbehaves.
* Do not set secrets with `Environment=`; `systemctl show` prints them. Use the 0600 env file.
* After enabling: `systemctl --user status agent-hub`, `journalctl --user -u agent-hub -n 50`, and
  `systemd-analyze --user security agent-hub` (review the score; do not assume it is low).

Do not enable both units unless you want both; the hub works without the knowledge service (the Knowledge view shows
`knowledge_unavailable`).

## 8. Delivering to Hermes Agent (`deploy/hermes-deliver.sh`)

The hub cannot reach a containerised Hermes Agent. Applying a Hermes client writes a **stage tree** under `out/<client>/`
(`skills/`, `AGENTS.md`, `config.*.fragment.yaml`). Delivery is a separate manual step: the script tars the stage's
`skills/` into a one-shot `docker compose run --rm -T --no-deps` container of your Hermes compose project, replaces **only**
a `hub` skills category inside the container's data volume, then asks `hermes skills list` and exits 1 if any delivered
skill is not listed. It refuses an empty stage, a stage without `SKILL.md`, or any symlink in the stage, and prints only
names and counts. The script assumes a particular compose layout: **read it and adjust its paths and compose
file location before running it** (it takes its locations from environment variables named in its header).

Whether Hermes indexes a `hub` skills category and whether `docker exec --user hermes` works depend on your install. Only skills are
delivered; the `AGENTS.md` and config fragments in the stage are for you to review and merge by hand.
Hermes volumes are model-writable and nothing re-renders at launch: re-run the script after each apply.

## 9. Knowledge service and Qdrant

Two deployments exist.

**A. Host process.** LanceDB and Qdrant v1.16.3 are supported by the service.

```bash
cd hub && make setup-knowledge
cd knowledge && uv run agent-knowledge --config ~/.config/agent-knowledge/config.yaml   # 127.0.0.1:8795
```

Config keys (unknown keys are rejected): `host`, `port`, `allowed_hosts`, `data_dir`, `lance_dir` (put on SSD, never
tmpfs), `qdrant_url`, `qdrant_api_key_env` (default `KNOWLEDGE_QDRANT_API_KEY`), `router_url`, `router_api_key_env`
(default `KNOWLEDGE_EMBED_API_KEY`; both are env var **names**), `default_embedding_model`, `ingest_roots`, `index_roots`,
and limits. `KNOWLEDGE_EMBED_API_KEY` must hold an **embeddings-only router virtual key, never the master key**. The admin
key appears at `<data_dir>/admin.key` (0600) on first start; the hub reads that file by default, or set
`HUB_KNOWLEDGE_ADMIN_KEY` in the hub env file. The Knowledge chip in the UI shows reachable / authenticated / backends.

Qdrant for this mode: `docker compose -f deploy/knowledge/docker-compose.qdrant.yml -p
agent-knowledge-qdrant up -d` (loopback 6333, `mem_limit: 8g`; the template now **requires** an env file,
`AGENT_KNOWLEDGE_QDRANT_ENV=<0600 file holding QDRANT__SERVICE__API_KEY>`, and the service must get the same value as
`KNOWLEDGE_QDRANT_API_KEY`; generate with `openssl rand -hex 32`. Keyless requests to a keyed Qdrant are refused), then `qdrant_url:
http://127.0.0.1:6333`. Choose the backend per namespace (`lancedb` default for up to a few hundred thousand chunks;
`qdrant` for large, shared or heavily filtered corpora; see `docs/KNOWLEDGE-PLANE.md`).

**B. Container + broker (templates).** `deploy/knowledge/docker-compose.broker.yml` runs the service
and a mitmproxy read-only broker on two internal networks so an internal-net client can query. Create the networks first
(commands are in the file header), put the token in a 0600 env file outside the repo, wire the router through its own
credential broker (not done in the template). Text queries need the router, and the model alias/dimension (for example `nomic-embed`, 768) must match your router's configuration.

### Docker build DNS problem

If package installs fail inside `docker build` steps (for example `pip install uv` in `deploy/knowledge/Dockerfile`), check that
build steps can resolve DNS (some rootless setups need a host-network builder):

```bash
docker buildx create --name hostnet --driver docker-container --driver-opt network=host --use
docker buildx build --builder hostnet --load -f deploy/knowledge/Dockerfile -t agent-knowledge:local knowledge
```

A host-network build step can reach the LAN and any local service, so it is a deliberate egress exception: run it once,
review the Dockerfile (it only installs pinned `uv` and runs `uv sync --frozen`), and do not leave the builder as the
default. If it still fails, run the host-process deployment (A) instead. Note also `docker-compose.broker.yml` builds
with the default builder; use the built `agent-knowledge:local` image, or pre-load the image and change `build:` to
`image:` yourself.

## 10. Troubleshooting

Find the string you see. The API returns RFC 7807 JSON with a `code`; the UI shows a translated message; `hubctl` prints
`error: <code>: <detail>`.

### Access

| You see | Meaning and fix |
|---|---|
| `401 unauthorized` "a valid bearer API key is required" (UI: auth dialog re-opens) | No/wrong/revoked key, or a new browser tab (key is per-tab `sessionStorage`). Send `Authorization: Bearer <key>`. |
| `hubctl`: `error: unauthorized: an admin key is required: set HUB_API_KEY or keep <path> readable` | `hubctl` found no admin key: `HUB_API_KEY` is set nowhere (checked in order: process environment, `~/.config/agent-hub/env`) and the bootstrap key file is missing or unreadable (for example you deleted it but did not move the key). |
| `403 forbidden` "this key has the viewer role (read-only)" / "client tokens may only post to the memory inbox" / "admin role required" | Wrong role for that action. Use an admin key. |
| `403 csrf_header_required` "send the X-Agent-Hub: 1 header on non-GET requests" | A non-GET without `Authorization` and without `X-Agent-Hub: 1`. Curl with a bearer key is exempt. |
| `403 cross_origin` | Origin does not match Host. Do not call the API from another site or tool that sets a foreign `Origin`. |
| `421 misdirected_request` "unrecognised Host header" | The Host header is not `127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>` for the **configured** `HUB_PORT`. Causes: a reverse proxy or a hostname/LAN IP (add it to `HUB_ALLOWED_HOSTS`), or an old build where `hubctl serve --port N` left the allowlist on `HUB_PORT` (fixed; `--port` now works). |
| `413 payload_too_large` | Body over the cap (1 MiB; 4 MiB import/discover). |
| `503 content_not_initialised` "content repository is not initialised; run `hubctl init`" | No content repo at `HUB_CONTENT_DIR`. Run `hubctl init`, or fix the path. |
| `floor_unavailable` "policy floor unusable (<path>): ..." (raised when the hub is constructed, so the server or `hubctl` fails at start) | `policy/floor.yaml` is missing or invalid; there is no "no floor" mode. Fix the file (it is not content). |
| `429 rate_limited` / `inbox_full` / `too_many_streams` | Inbox rate (20/min/client) or cap (200/client; promote or reject), or 8 open SSE streams. |

### Content and git

| You see | Fix |
|---|---|
| `422 content_invalid` "content has validation problems; fix or discard them before committing" | The commit gate. `hubctl validate` lists `path: message`; fix or discard. |
| `409 nothing_to_commit` | No pending changes. |
| `409 dirty_tree` "commit or discard pending changes before reverting" | Revert needs a clean tree. |
| `409 revert_failed`, `400 unknown_commit` | The revert conflicts or is a merge commit; or the SHA is not in history. |
| `503 git_missing` / `git_timeout`, `409 git_error` | `git` not installed / slow / a git command failed (detail has the first 300 characters). |
| `409 already_exists`, `400 invalid_id` | Names/ids must match `^[a-z0-9][a-z0-9._-]{0,63}$`. |
| `422 file_too_large` / `skill_too_large` / `409 snapshot_too_large` | Per-file (256 KiB), per-skill (4 MiB), snapshot (64 MiB) caps. |

### Plan and apply

| You see | Meaning and fix |
|---|---|
| `409 plan_stale` "live files or content changed since the plan was made; plan again" (also "content changed since this plan was made" and "client X is gone") | Something moved after the plan: a new commit, an edit to a live client file, or another apply. Nothing was written. Re-plan, re-review, apply. |
| `409 plan_is_preview` "this plan is a working-tree preview; commit the content and plan again" | You planned from the working tree (`source: working`). Commit, then plan without preview. |
| `422 confirm_required` "apply needs confirm: true" | API call without `confirm`; the CLI needs `--yes`. |
| A file with action `conflict` in the plan, reason "live content differs from what the hub last applied" / "file exists and was not created by the hub" / "managed slice was edited since it was applied" / "hub-created file was edited since it was applied" (apply result: `skipped_conflicts`) | The file changed outside the hub. Review the diff. Either fold the live edit into the content (edit, commit, re-plan) or adopt: `--adopt <root>:<path>`. |
| Conflict reason "live file unreadable: ..." or a merge-file parse reason | Directory, symlink, too large, or unparsable JSON/YAML. **Cannot be adopted**; fix the file. |
| **adopt_paths format.** You passed `--adopt` and the file is still skipped | Entries must be exactly `<root>:<path>` or `<client>:<root>:<path>`, e.g. `project:CLAUDE.md`, `claude-code:project:.claude/agents/mesh-doctor.md`. A non-matching entry is not an error; it just does not match (read from `planner.adopt_match`). Copy `root:path` from the plan output. |
| Plan `[BLOCKED]`, `hubctl plan` exit 2, blocked reasons | Errors on a `strict` client, hard diagnostics (`bad_root`, `adapter_missing`, `unsafe_path`, `unknown_root`, `duplicate_artifact`, `merge_mode_mismatch`, `render_failed`, `invalid_content`, `bad_artifact`) or floor violations. See next table. |
| `concern_unsupported` diagnostic "<adapter> cannot consume <concerns>" | You enabled an item kind the client cannot use (for example agents for a Hermes client, skills for opencode). An error on a strict client for directly enabled items. Disable it in the Matrix. |
| apply result `status: failed`, "...; nothing was changed" | A write failed and was rolled back from backups (permissions, disk). Fix and re-plan. |
| apply result `verify_ok: false` / `verify FAILED` | Files were written but the adapter could not confirm the client sees them (hash mismatch, or the client did not list the skill). Read `checks`. Hermes verify needs the container running. |

### Floor violations (plan blocked, `code` in the finding)

| Code | Cause | Fix |
|---|---|---|
| `floor_path_read` / `floor_path_write` | A rule allows a path overlapping a floor deny (e.g. `~/.ssh/**`) | Remove or narrow the rule. The floor cannot be overridden from content. |
| `floor_egress_wildcard` | `egress_host allow` with `*`, `*.*` or `**` | Name the host. |
| `floor_command_pattern` | `allow` on a `command` rule with an empty or catch-all pattern | Use a specific pattern. |
| `floor_tool` | A tool decision above the ceiling (`web_fetch`, `web_search`, `shell` at most `ask`) | Lower the decision. Raising a ceiling is a floor edit in git. |
| `floor_default_tool`, `floor_network_default` | Default tool decision weaker than `ask`, or network default not `deny` | Fix the profile. |
| `mcp_not_clean` | MCP server `scan_status` is not `clean` (seen live on imported servers). `clean` is bound to `scan_digest`: editing command, args, url, pinned_ref or transport resets it to `unscanned` | Run your MCP scanner on the server, then attest it with `hubctl attest-mcp NAME --yes` (the digest is an HMAC keyed by `<data_dir>/attest.key`; the hub does not run a scanner). Stdio servers also need `sandbox_profile`; http/sse servers need `egress_hosts`. |
| `floor_mcp_rule` | An allow rule for `mcp_server` or `tool:mcp` (`mcp__*`) would cover an unscanned MCP server | Narrow the rule to scanned servers. |
| `floor_violation_rendered` | The floor re-check on the RENDERED JSON/YAML permission or MCP artifact found what the abstract rules did not show (TOML and block artifacts are not inspected) | Read the finding's message and rule; fix the source item or adapter mapping. |
| `mcp_egress_not_allowed` | An MCP `egress_hosts` entry has no explicit `egress_host allow` rule | Add the rule (and the host to your egress allowlist). |
| `mcp_url_host_undeclared` | Non-loopback URL host not in `egress_hosts` | Declare it. |
| `floor_deny_missing` (warn, audit) | The live config has no deny covering a floor path | Add it to the client's config (advisory concerns are not written by the hub). |

### Reading one item, client status, misc

| You see | Meaning and fix |
|---|---|
| `404 not_rendered` "<kind> '<name>' does not render for <client>: <reason>" | The item is not enabled for that client, the client cannot consume it, or it is blocked. Check the Matrix cell and Clients, effective config. |
| `409 never_applied` "nothing has been applied to this client yet" (verify) | Expected before the first apply; verify needs a lock. Import does not count as an apply. |
| `409 client_not_committed` | The client file is in the working tree only. Commit first. |
| `422 unknown_adapter` / `503 adapter_missing` | `adapter:` names no installed adapter (`hubctl adapters`); a broken adapter module is logged and skipped. |
| `503 discover_failed` / `verify_failed` | The adapter raised while reading or verifying; check server logs. |
| Client status `edited_outside` | A lock entry exists and the live slice differs (someone edited it). `pending_changes`: the hub would change files nobody edited. `never_applied`: no real apply yet. |
| Knowledge view: `503 knowledge_unavailable` "knowledge service is not reachable" | The service is not running at `HUB_KNOWLEDGE_URL`. |
| `502 knowledge_upstream_unauthorized` "the knowledge service rejected the hub's admin key ..." | The hub has no valid knowledge admin key. Set `HUB_KNOWLEDGE_ADMIN_KEY` in `~/.config/agent-hub/env`, or make `~/.local/share/agent-knowledge/admin.key` (written by the service on first start) readable, or set `KNOWLEDGE_ADMIN_KEY_FILE`. It is never the browser's 401/403. |
| `422 unknown_scope` when creating a client key | Only `inbox:write` exists; `knowledge:read:<ns>` was removed. Issue knowledge tokens from the knowledge service. |
| `422 confirm_required` on Discard all | Discarding every pending change needs `confirm: true`; send `dry_run: true` first to list what would go. |
| `404/400 knowledge_path_denied`, `502 upstream_too_large` | Path not in `health|backends|namespaces|jobs|indexes|tokens`; response over 8 MiB. |
| knowledge service exits "refusing a non-loopback host without --allow-bind-any" | `host` in its config is not loopback; `--allow-bind-any` is for the container only. |
| knowledge `403 csrf_header_required` | Send `X-Agent-Knowledge: 1` on non-GET without a bearer. |
| `hubctl import`: "pass --all or --item kind:name" | Import needs a selection. |
| `hermes-deliver.sh`: `stage dir missing` / `refusing: no SKILL.md` / `refusing: symlink inside` / `hermes does not list skill <n>` / `FAILED for <client>` | Apply the Hermes client first; check the stage for symlinks; the last two mean the copy worked but the agent did not index it (wrong path or container not running). |
| systemd `status=226/NAMESPACE` | A `ReadWritePaths=` path does not exist. |
