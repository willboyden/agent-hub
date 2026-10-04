# Agent Hub security

Scope: the Hub (`backend/`, 127.0.0.1:8792), its content repo, its delivery into client roots, and the knowledge
plane (`knowledge/`, 127.0.0.1:8795) with its broker template. Written for a single operator on one workstation.
Multi-user use is not a design goal.

## 1. What is being protected

The hub is a **policy authority over untrusted agents**. Whoever can change hub content or apply a plan controls what
your agent clients (Claude Code, opencode, Hermes Agent, ...) are told and (for permissions, when managed) what they may do. The
assets, in order:

1. The **admin key** and the state dir (`~/.local/share/agent-hub/`: DB, keys, locks, plans, backups).
2. The **content repo** (skills, agent prompts, rules, MCP definitions): prompts for models, so writing to it is a
   prompt-injection channel into every client that enables the item. It lives **outside your project tree**, by default
   `~/.local/share/agent-hub/content` (finding N1 below), because agents can write inside the project tree.
3. The **policy floor** file (`policy/floor.yaml`).
4. The **client roots** the hub can write into (for project-root clients such as Claude Code and opencode: your project's working tree).
5. Secrets the hub must never hold (see section 7).

## 2. Trust boundaries

```
  B1  browser / curl / hubctl  --->  hub API        (bearer key; Host + Origin + CSRF header checks)
  B2  hub process              --->  client roots    (writes only under named roots, path-safe, lock-checked)
  B3  untrusted agents (a local-model client, a containerised client) <-X-> hub   (they must have NO route to the hub or its files)
  B4  content repo text        --->  agents          (untrusted text that becomes prompts; the hub never executes it)
  B5  hub                      --->  knowledge svc   (admin token, allowlisted paths, caps)
  B6  agent -> broker -> knowledge svc               (per-client READ token injected by the broker)
  B7  knowledge svc            --->  router /v1/embeddings, Qdrant   (env-var-named credentials, no redirects)
```

B3 is the one the design leans on and the one it cannot enforce by itself: it depends on the *client's* sandbox
(section 5). The rest is enforced in code.

## 3. STRIDE

| Threat | Where | Mitigation in place | Status / residual |
|---|---|---|---|
| **Spoofing** an admin | API, `hubctl` | Bearer keys (`hc_` + 256-bit, SHA-256 at rest, compared with `hmac.compare_digest`); loopback trust **off** by default; `hubctl` also needs a key; per-client tokens cannot do anything but post to their own inbox | Residual: the key file is readable by any process of the same user (section 4). |
| Spoofing via **DNS rebinding / cross-site browser** | API | Exact `Host` allowlist (421), same-origin `Origin` check on non-GET (403 `cross_origin`), `X-Agent-Hub: 1` on non-GET without a bearer (403 `csrf_header_required`), no CORS at all |  |
| **Tampering** with delivered files | Client roots | Path safety (relative only, no `..`, no NUL/backslash, no symlink components, must resolve under the root); delivery targets checked against the floor and the app dirs (H1); writes through directory fds opened with `O_NOFOLLOW` plus a compare-and-swap re-read (M4); per-file backup; all-or-nothing per client with rollback; **lock manifest** and conflict protection (section 6) |  |
| Tampering with **content** | Content repo | The repo is outside the project tree (N1); the hub refuses a repo with an unsafe `.git/config`, attributes, hooks, ownership or location and scrubs the git environment. Working-tree edits are inert until `commit`; plans render the *committed* tree (`git archive HEAD`); commit refuses invalid content; git runs with hooks off, global/system config ignored, fixed identity, timeouts | Residual: any process that can write `hub/content` can edit the working tree (it cannot get it applied without a key, but a human may commit it without reading the diff). Commits all carry one fixed author, so git history does not attribute who approved. The audit DB records the actor. |
| Tampering with the **floor** | `policy/floor.yaml` | Lives in the app repo, not content; no API writes it; `GET /policy/floor` is read-only; invalid or missing floor means the app does not start (`floor_unavailable`, fail closed) | Anyone who can write the checkout can weaken it. It is a security control: change it in git with review. |
| Tampering with **stored state** | `hub.db`, locks | DB and locks are 0600 in a 0700 dir; audit table rejects UPDATE/DELETE via the DB | A same-user process can edit the SQLite file or the lock manifests directly; the audit trail is not tamper-evident against that. |
| **Repudiation** | Audit | Every mutating API call is recorded (actor key id, role, method, path, status, redacted params); apply/commit/revert/discard also write domain events. Memory and knowledge bodies are audited as metadata only | `hubctl import` is **not** audited (it does not go through the API middleware); commit and apply from `hubctl` are, as actor `hubctl`. The log is only as trustworthy as the disk (above). |
| **Information disclosure**: secrets | API, plans, diffs, audit, logs | MCP items carry env var *names* only; responses, plans, diffs and audit params are redacted (`redact`/`scrub`: key-name and token-shape patterns); the bootstrap key path is logged, never the key; `/metrics` needs auth | Redaction is heuristic (pattern-based). A secret in an unusual shape, pasted into a skill body, can still land in a diff or a rendered file. Imports do not scan bodies for secrets. |
| Disclosure of **state dir** to agents | Agents' file tools | The floor lists `~/.config/agent-hub/**`, `~/.local/share/agent-hub/**`, the knowledge equivalents, `**/bootstrap-admin.key`, `**/admin.key`, `**/.env` as deny-read | **Not delivered by default**: `manage.permissions` is advisory unless you opt in, so the floor's read-deny is audited, not written (M5, section 4). Add the paths to each client's own sandbox or permission config (section 4) and run `hubctl doctor`. |
| **Denial of service** | API, knowledge | Body caps (1 MiB; 4 MiB import/discover), 413 also for chunked bodies; SSE connection cap and idle timeout; inbox rate limit and per-client cap; plan retention; knowledge: per-token rate limit and daily quotas, chunk and file caps, response cap on the proxy (`upstream_too_large`) | Per-file, per-skill and plan-snapshot size caps exist; there is no global quota on the content repo. |
| **Elevation of privilege** via roles | API | Roles admin / viewer / client. The middleware and `enforce_roles` (attached to every router) both check: viewer may only POST `policy/check`, `clients/validate-spec`, and knowledge namespace queries; client tokens may only POST `/memory/inbox` for their own client | A route-table test on the knowledge service proves no route is left unauthenticated. |
| Elevation via **content**: a skill or agent prompt that hijacks a client | B4 | Bodies are untrusted text, never executed by the hub; a lossy mapping is a diagnostic (error on `strict`); an agent with no tools is an error; rules pass the floor; MCP enable needs `scan_status: clean` and explicit egress | Residual, and the largest one: the hub cannot judge whether a prompt is malicious. Review diffs in Changes. Imported items come from files a model may have written (section 8). |
| Elevation via **plan replay / TOCTOU** | Apply | `apply` needs `confirm: true`; the plan is bound to the content tree hash and a per-client digest of the live files; any change gives 409 `plan_stale`; previews cannot be applied |  |

## 4. Why loopback trust is off, and the residual risk

`HUB_TRUST_LOOPBACK` defaults to **false** (`config.py`). Reason: an agent client that can run tools may run them on the
same host, and any agent with an unsandboxed shell can open a socket to `127.0.0.1:8792`. With loopback trust on, that agent
would be admin: it could edit its own profile, enable an MCP server, commit and apply. So a bearer key is required for every
API call and for `hubctl`. `GET /api/v1/health` reports `trust_loopback` and a warning when it is on, and `hubctl serve`
prints a warning.

**Residual risk M5, stated plainly.** The key is a file (`~/.local/share/agent-hub/bootstrap-admin.key`, 0600) owned by your
OS user, and `hubctl` falls back to it. Any process running as that user and able to read the file can act as admin. Loopback
trust off removes the *ambient* access (no key needed), not the *same-user* access. An unsandboxed client running as your
user can read everything you can. **The fix is a separate OS user for that client. That is a host-level change and your
decision; the hub cannot and must not make it.** A containerised client with no host mounts and an internal-only network
does not have this problem.

### What the hub does about it

* **`hubctl` prompts for the key** with a hidden prompt when no key file or environment variable exists, and
  **`hubctl retire-bootstrap-key`** verifies the key, then zero-fills and deletes the bootstrap file, so the key can live
  only in a password manager.
* **`hubctl doctor`** (and Settings, Host hardening; `GET /api/v1/doctor`, admin; `GET /api/v1/doctor/summary`, counts
  only) is a read-only report of how exposed the hub's secrets and policy are to other processes of the same user: bootstrap
  key at rest, file modes, content dir location and ownership, loopback bind and trust, whether your client sandbox templates
  deny the hub's state paths, unsandboxed clients, `ptrace_scope`, installed unit hardening, knowledge reachability, and
  advisory permission gaps. It prints exact fixes and never prints a secret. Run it on your host: it describes *your* setup,
  which this document cannot.
* Key handling: create (`hubctl init`), store the key in a password manager, then `hubctl retire-bootstrap-key`. The older
  alternative, `HUB_API_KEY` in `~/.config/agent-hub/env` (0600), still works but leaves a same-user-readable copy on disk.
  Use `viewer` keys for routine UI browsing. Never put the key in a unit file.

### Paths to deny in every client's own sandbox or permission config

The hub itself delivers no state-dir read-deny to a client unless you manage that client's `permissions` concern (advisory by
default). Add these to each client's own configuration (a Claude Code sandbox template, an srt-style sandbox config, ...):

```
~/.config/agent-hub          ~/.local/share/agent-hub
~/.config/agent-knowledge    ~/.local/share/agent-knowledge
**/bootstrap-admin.key       **/admin.key       **/.env
write-deny: **/hub/policy/**
```

A client with no sandbox (some run unsandboxed) cannot apply them: for it this list is documentation, not protection.

## 5. The policy floor

`policy/floor.yaml` (app repo, version 1). The planner turns each violation into a structured finding and **blocks apply
for that client**. Content may only be stricter. What it forbids:

* Allowing reads that overlap the deny-read list (`~/.ssh`, `~/.aws`, `~/.config/gcloud`, `~/.gnupg`, `~/.netrc`,
  browser profiles, keyrings, the hub and knowledge config/state dirs, `bootstrap-admin.key`, `admin.key`, any `.env`)
  and writes that overlap the deny-write list (`~/.ssh`, `~/.aws`, `~/.config/gcloud`, `~/.gnupg`, `~/.netrc`).
  Overlap is a deliberately over-approximate glob comparison (fail closed), not a proof.
* A catch-all `egress_host allow` (`*`, `*.*`, `**`). Egress default must stay `deny`.
* `allow` on a `command` rule with an empty or catch-all pattern (`*`, `**`, `*.*`, `?*`, `.*`). It does not analyse what
  a specific allowed command can do: `allow git *` passes.
* Tool decisions above the floor's ceiling: `web_fetch` and `web_search` at most `ask` (default `deny`); `shell` at most
  `ask`. Default tool decision weaker than `ask`.
* Enabling an MCP server whose `scan_status` is not `clean`; an MCP egress host with no explicit `egress_host allow`
  rule (or one that is denied); a non-loopback URL host not declared in the server's `egress_hosts`.
* Floor deny rules are always merged into the rendered permission set and cannot be removed by content.
* Since the internal review (section 12): paths are normalised before comparison, catch-all command/egress allows are
  detected by probe strings as well as literal patterns, MCP `clean` is digest-bound, allow rules that would cover an
  unscanned MCP server are `floor_mcp_rule` violations, and the floor is re-checked on the rendered artifacts
  (`floor_violation_rendered`).

For advisory concerns (`mcp`, `permissions` by default) findings are downgraded to `warn` in the plan (they do not
block, because the hub is not writing them), but `hubctl audit` / `GET /clients/{id}/audit` still reports them as errors
against the live config.

## 6. Delivery safety: paths, locks, conflicts

* **Path safety** (`domain/pathsafe.py`): every artifact path is relative with no `..`, empty or `.` segment, no NUL, no
  backslash, no control characters, at most 512 characters; the resolved target must stay under the named root; no
  component below the root may be a symlink (the root itself may be). Client roots must be absolute (or `~`) and free of
  `..`. Targets that are directories, symlinks or too large are conflicts that **cannot be adopted**.
* **Lock manifest** (`<state>/locks/<client>.json`, 0600): per delivered file the whole-file SHA-256, the hub-owned slice
  hash (whole file, managed keys, or marker region), the merge mode, managed keys and `created`. The hub deletes a file
  only if `created` is true and its hash still matches; merge slices are removed without touching the rest of the file.
* **Conflict protection**: a file whose live slice differs from the lock, or that has a slice but no lock entry, or that
  cannot be parsed for a key merge, is a `conflict`. Apply skips it and reports it unless you pass `--adopt root:path`
  (or `client:root:path`).
* **Apply is all-or-nothing per client**: stage everything (temp files, backups) before touching any live file; on any
  error, roll back from the backups. The plan is recomputed against the live files immediately before writing. Temp dirs.
* **Merge modes** preserve what is not the hub's: `json_keys`/`yaml_keys` replace only the managed keys; `block` replaces
  only the region between `agent-hub:begin/end` markers.
* Never written by design: a client's own sandbox/permission template that another tool renders and that the model is denied access to.
* **Rollback**: each apply is recorded under `<state>/applies/`; `hubctl rollback` restores files only while the live hash still equals the post-apply hash, through the same dirfd writes, and reports edited files as conflicts.
* Backups go to `<state>/backups/<client>/<timestamp>/` (0600). They can contain the previous content of client files.

## 7. Secret handling

Secrets never live in the hub. MCP items carry environment variable **names** (`env_names`, validated as identifiers);
adapters render references by name (or names only), never values. API responses, plans, diffs and audit params
are redacted. The bootstrap key is written 0600 from the first byte and shown only as a file path. The knowledge proxy
token (`HUB_KNOWLEDGE_TOKEN`) is never logged or returned (`[set, N chars]` / `[unset]`). Unit files are world-readable
via `systemctl show`, so keep secrets in the 0600 env file (`~/.config/agent-hub/env`), never in a unit.
Redaction is pattern-based, so a secret pasted into a skill body may not be caught (section 3).

## 8. Memory-inbox poisoning defence

A model that can write "memories" that are later fed to every client is a poisoning channel. So:

* Agents (with a `client` token holding `inbox:write` for that same client) may only POST proposals to
  `/api/v1/memory/inbox`. Proposals are files under `content/inbox/<client>/`, **never compiled** into any client.
* Only an admin can promote (into `content/memory/`, working tree, still needing commit + plan + apply) or reject.
* Limits: body at most 16 KiB, title at most 200 characters, 20 proposals per minute per client, 200 per client
  (`inbox_full`, 429). Inbox writes are audited as metadata (size, field names), not text.
* Memory is an **advisory** concern by default, so even a promoted memory reaches no client until you set
  `manage.memory` for it.

## 9. Token scopes

**Hub keys** (`POST /api/v1/keys`): `admin`; `viewer` (all reads plus the read-only dry runs; it cannot read knowledge
`tokens`, `indexes` or `backends`); `client` with a `client` id and the single scope `inbox:write`, which lets it POST to
the inbox of its own client and nothing else. The former `knowledge:read:<ns>` scope was removed (422 `unknown_scope`);
knowledge access uses tokens issued by the knowledge service, below. Keys are shown once, listed by prefix, revocable.
The hub talks to the knowledge service with an admin key from `HUB_KNOWLEDGE_ADMIN_KEY` (legacy name
`HUB_KNOWLEDGE_TOKEN`), else from the 0600 file the knowledge service writes (`KNOWLEDGE_ADMIN_KEY_FILE`, default
`~/.local/share/agent-knowledge/admin.key`). If that service refuses the key the hub answers 502
`knowledge_upstream_unauthorized` (a hub configuration problem, never the browser's 401).

**Knowledge tokens** (`agent-knowledge`, separate service): the admin key is `<data_dir>/admin.key` (0600); client
tokens are `akn_<id>_<256-bit>`, stored as SHA-256, shown once, scoped to `{namespace, mode}` with **read as the
default** and no wildcard namespaces. Non-admins get 404 for a namespace they hold no scope on (no probing to enumerate).
Dropping a namespace strips its grants. Admin-only: namespace create/update/drop, path ingest, tokens, indexes,
`/backends`, `/metrics`. Limits: per-token rate, daily query and chunk-write quotas, per-namespace chunk cap, size caps.
Path ingest resolves symlinks and requires the path under a configured root; secret-looking files (`.env*`, `.ssh`-style
dirs, `.git`, binaries, files over 2 MB) are skipped.

**Broker template** (`deploy/knowledge/`): the client holds a dummy key; the broker injects the real read-only token and
allows exactly `POST /namespaces/<ns>/query` for namespaces in `KNOWLEDGE_ALLOWED_NS` (body up to 64 KB) and `GET
/health`; everything else is 403 at the broker. A write-scoped token given to an untrusted agent is a data-poisoning risk the code cannot
prevent: give agents read tokens and ingest through a human-run job.

## 10. Read-only delivery caveats (not achieved)

The original design goal was that delivered content be read-only to the client or re-rendered on every launch. **This is not
implemented.**

* **A sandboxed Claude Code client can usually write project `.claude/skills` and `.claude/agents`.** The hub writes those files as the operator
  (0644 by default), and a sandboxed model with write access to the project tree can rewrite them. The hub notices on the
  next plan/verify (`edited_outside`, or a `conflict` at apply, which is skipped unless adopted), but does not prevent the
  edit, and a model-planted skill stays live until someone looks. Do not run a blanket `--adopt`.
* **Hermes volumes are model-writable.** `deploy/hermes-deliver.sh` wipes and rewrites `/opt/data/skills/hub` when *you*
  run it; between runs the agent can change those files, and nothing re-renders at container launch.
* **Import reads live client files**, including anything a model wrote there, into the content working tree. Review the
  import diff before committing; the floor and the commit gate are the defence.
* A client with no sandbox (see section 4) can also edit its own delivered files, and its delivery root is a working tree you edit too.

## 11. Knowledge-plane and template caveats

* The knowledge Dockerfile and broker compose are templates: review them before use (`docs/OPERATIONS.md`).
* `docker-compose.qdrant.yml` now requires an env file holding `QDRANT__SERVICE__API_KEY`, and the service sends the same
  key from the env NAME `KNOWLEDGE_QDRANT_API_KEY`; the service probes Qdrant without a key at start and warns if it
  answers. Keyless requests to a keyed Qdrant are refused. Loopback publish is still the outer
  control; the key stops another local process bypassing token scopes.
* The embeddings key uses a dedicated env NAME `KNOWLEDGE_EMBED_API_KEY`, which must hold an embeddings-only router
  virtual key, never the master key. The service only warns on suspicious NAMES (`LITELLM_API_KEY`, `MASTER`); it cannot
  tell what the value is. The router for text queries must still be reached through a credential broker; the template
  does not wire that.

## 12. Hardening history (internal review)

Findings from the project's own AI-assisted and self-review passes, and what was done about them, from `ARCHITECTURE.md`
Addendum E and the code. This is **not** an independent or third-party audit. The IDs are kept only as stable references.

| ID | Finding | What was fixed |
|---|---|---|
| H1 | Delivery targets vs the floor | Client roots and artifact targets are refused (`bad_root` / `unsafe_path`) when they overlap the floor's `deny_path_write` stems, the hub state dir or credential dirs, are or contain the home dir or `/`, sit inside `.git`, or (targets only) sit inside the content dir, `policy/` or `backend/`. Generic specs may not target `hooks` path segments. |
| H2 | Self-attested MCP scan status | `clean` is bound to `scan_digest` (sha256 of command, args, url, pinned_ref, transport); editing those fields returns the server to `unscanned`. Stdio servers need `sandbox_profile`; http/sse servers need `egress_hosts` (loopback exempt). Allow rules for `mcp_server` or `tool:mcp` (`mcp__*`) that would cover an unscanned server are `floor_mcp_rule` violations. |
| M1 | Marker injection | A body line that looks like a hub block marker makes the adapter refuse that artifact (an error diagnostic, no artifact), so content cannot end or forge the managed region. The adapter and the merge code share test fixtures so they cannot drift. |
| M2 | Skill frontmatter | `allowed-tools` with unrestricted `Bash`, `*` or `mcp__*`, or any `hooks` key, is an error and the skill is withheld (its bytes are never rewritten); other pre-approvals warn. |
| M3 | Floor pattern strength | Paths normalised (`~`, `.`, `..`, `//`); catch-all detection by probe strings (`command_allow_probes`, `egress.forbid_allow_probes` in `floor.yaml`); the floor is re-checked on the **rendered** artifacts (`floor_violation_rendered`). |
| M4 | TOCTOU in the applier | Writes go through directory fds opened component-by-component with `O_NOFOLLOW`; before replacing or deleting, the live file is re-read and compared with what the plan saw; a mismatch aborts that client, rolls back and reports `conflict`. |
| M5 | Key file readable by same-user processes | **Not fixed; residual.** See section 4 for guidance and the exact deny paths. |
| M6 | Discovery and YAML/size caps | Discovery has per-file, per-total and per-skill file-count caps (512 KiB per file, 16 MiB total, 400 files per skill); live files over 8 MiB are conflicts; descriptions over 8192 characters are refused; YAML has size, depth and node caps and no anchors. |
| M7 | Knowledge service | Dedicated embeddings key name, keyed Qdrant, per-namespace store locks and timeouts (`store_busy`, `store_timeout`), filter node cap, admin-only `/tokens`, `/indexes`, `/backends`, `/metrics`. See `KNOWLEDGE-PLANE.md`. |
| L1-L5 | Assorted | Verify hashes the managed **slice** for json/yaml/block artifacts (I1); a non-loopback `HUB_HOST` is refused unless `HUB_ALLOW_NONLOOPBACK=1`; state dirs are 0700 and tightened at start; viewers cannot read knowledge `tokens|indexes|backends`; git runs with literal pathspecs, no textconv, no attributes file; plan snapshots come from `ls-tree` + `cat-file`, not `git archive`; `commit` stages only known content paths and reports `strays`; discarding everything needs `confirm: true` (`dry_run: true` lists it first). |

### Re-verification pass

The subcommands and defaults named here are present in the code (`hubctl --help` lists `migrate-content`, `attest-mcp`, `rollback`, `applies`, `doctor`, `retire-bootstrap-key`).

| ID | Severity | Finding | Fix |
|---|---|---|---|
| N1 | High | The content repo sat inside the project tree. Agents can write inside the project tree: a planted `.git/config` (`core.fsmonitor`, `core.sshCommand`, aliases) would run commands as the hub user, and commits could be forged. | The default location is `<data_dir>/content` (`~/.local/share/agent-hub/content`), outside the project tree. The hub refuses a content repo with an unsafe `.git/config`, attributes, hooks, ownership or location (`content_dir_unsafe`) and runs git with a scrubbed environment. `hubctl migrate-content --to PATH` moves an existing repo.  |
| N1b | | Wildcard MCP allow rules | A wildcard MCP allow is always a floor error; a named allow is accepted only for an enabled, clean server.  |
| N2 | | Skill `allowed-tools` splitting | The frontmatter value is split on commas and whitespace outside parentheses before the unrestricted-`Bash`/`*`/`mcp__*` check, so packing several tools into one string cannot slip past it.  |
| N4 | | Protected delivery targets | The whole app tree **except `out/`** is a protected target; no adapter may write `.claude/settings.local.json`, `.git/**` and similar.  |
| N6 | | Catch-all probes too narrow | Broader command and egress catch-all probes in `floor.yaml`: launchers such as `git *`, `node *`, `uv run *` are warnings; TLD wildcards such as `*.com` are errors.  |
| (H2 follow-up) | | `scan_digest` was a plain hash anyone could recompute | `scan_digest` is an **HMAC keyed by `<data_dir>/attest.key`**; `hubctl attest-mcp NAME --yes` re-binds `clean` after you ran your MCP scanner (it does not run one).  |
| (knowledge) | | Knowledge service hardening | Per-namespace service lock; capacity limits on path ingest; the service refuses an **unauthenticated Qdrant** unless `KNOWLEDGE_ALLOW_UNAUTH_QDRANT=1`; embeddings-key name checks.  |
| (rollback) | | No way back from a bad apply | Every apply is recorded; `hubctl rollback` / `POST /clients/{id}/rollback` restore files only while the live hash still equals the post-apply hash (edited files are reported as conflicts and skipped).  |

`attest.key` sits in the state dir and has the same same-user-readable limit as the admin key (M5): whoever can read it can
forge an attestation.

**Remaining limits:**

1. The rendered-output floor re-check covers **JSON and YAML** permission and MCP artifacts only. **TOML and `block`
   artifacts are not inspected.**
2. A project root that **contains** the app dirs is allowed (for example a project root that is a checkout containing the hub).
   Targets inside the app tree are refused except `out/` (N4). Other paths inside a project root are not refused, and a
   project-root client means the applier can write `AGENTS.md`, `CLAUDE.md` and `.claude/` there by design.
3. A **tiny window remains** between the compare-and-swap read and the rename. An attacker who wins that race can still
   have their bytes replaced or replace ours; it needs local write access to the client root.

## 13. Residual risks, summary

### Mitigated (with how)

* Ambient loopback access by any local process: `HUB_TRUST_LOOPBACK` is off; every call, and `hubctl`, needs a key.
* Content repo under agent control, plan replay, delivery-target abuse, marker injection, MCP self-attestation and the
  other audit findings in section 12 (fixed).
* Cross-site and DNS-rebinding access to the API (Host, Origin and CSRF checks).
* Exposure of the hub's state to a *sandboxed* client: only if that client's own sandbox denies the paths in section 4.
  Run `hubctl doctor` to see whether it does on your host.

### Reduced (what the operator does)

* The admin key on disk: store it in a password manager and run `hubctl retire-bootstrap-key` .
  Until you do, `hubctl doctor` reports the bootstrap file as present.
* The floor's state-dir deny in each client's own config: add the paths yourself (section 4); the hub does not deliver them
  unless you manage that client's `permissions` concern.
* A key saved by the browser's password manager lives in the browser profile. Without a primary password, any same-user
  process can decrypt it, which is the same exposure as the bootstrap key file; set one. Browsers also fill a saved login
  into any page on the same origin (scheme, host and port), and Firefox does so on page load by default. A process that
  serves the hub's address while the hub is down, including another user's, then receives the key without you pasting
  it. The desktop launcher's port-owner check does not cover a tab you open yourself. To require a click, turn automatic
  fill off (Firefox: `signon.autofillForms` = false in `about:config`), or keep the key in a separate password manager.
* Content is prompts; the hub cannot detect a malicious skill. Human diff review in Changes is the control.
* Delivered files are not read-only to clients (section 10); drift is detected, not prevented.
* Redaction and path-glob overlap are heuristics.

### Inherent (not fixable inside the hub)

* **An unsandboxed client is a same-user process.** Anything it runs can read everything your user can, including any key
  you can read, and act as you. The fix is a separate OS user for that client: a host-level change that is your decision and
  that the hub must not make.
* A same-user process can also ptrace or read the memory of other same-user processes. `kernel.yama.ptrace_scope=1` only
  limits ptrace to descendants; it does not stop reading files or other same-user access paths.
* The audit trail and git history are not tamper-evident against a same-user attacker, and commits share one identity.
* The desktop launcher (`agent-hub open`) refuses a port held by another user, but a same-user process that binds the
  hub's port first and copies its health answer gets the key you paste into that tab.
* The hub runs from its checkout: `make run`, `hubctl` and the desktop launcher all execute the code and virtualenv in that
  directory as your user. A client whose sandbox may write there (for example, one whose writable area is a parent
  project directory that contains the checkout) can change the hub's code, and the next start runs that change with
  access to everything the hub protects. The trusted desktop icon makes such starts routine. Keep the checkout outside
  every governed client's writable paths, or deny writes to it in each client's sandbox.
* The section 12 limits (TOML/block artifacts uninspected, app-containing roots allowed, CAS-to-rename window).
