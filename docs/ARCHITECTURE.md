# Agent Hub — architecture and contract (v1)

One place to organise and control what every agent client knows and may do: shared **instructions, skills,
subagents, rules, memories, MCP servers, permissions, egress** — rendered into each client's native format —
plus a separate **knowledge plane** (vector stores, shared indexes) reached through brokers.

This is the contract for contributors. Changes are additive and recorded in an addendum at the end.

## 1. Decisions

| # | Decision | Why |
|---|---|---|
| 1 | **Source of truth = a git repo of declarative files** (`content/`, its own nested repo). The hub is a *compiler + change-controlled control plane*, never a runtime service clients query. | Containerised clients (for example Hermes Agent) often cannot reach host services; GitOps gives history, review and rollback for free. |
| 2 | **One `ClientAdapter` per client** (`adapters/base.py`, LOCKED) plus a **declarative `generic` adapter** driven by a YAML spec. | New clients (Cursor, Codex, Gemini CLI, ...) need a spec or a small adapter — no core/UI change. |
| 3 | **Edit → Plan → Apply**. UI/CLI edits change the content working tree (*pending changes*); a **commit** is the approval of content; **apply** renders and delivers to clients, only from a plan bound to the content hash. | Nothing takes effect live; every effect is diffable and auditable. |
| 4 | **Policy floor** in the app repo (`policy/floor.yaml`, not editable via UI/API). Content may only be stricter. Plan fails on violation. | Sandbox posture cannot be weakened by a toggle. |
| 5 | **Progressive adoption, no clobbering.** Per client, per concern (`skills, agents, instructions, mcp, permissions, memory`) a `manage` flag decides *managed* (hub writes) vs *advisory* (plan shows what would change; `audit` checks the live config against the floor). Defaults: skills/agents/instructions managed, mcp/permissions advisory for clients whose config another sync process owns. A lock manifest records the hash of every delivered file; apply refuses to overwrite a file whose current hash differs from the last applied one unless the path is explicitly adopted. | Existing sync scripts or tools that own a file stay authoritative until the operator opts in. |
| 6 | **Secrets never live in the hub.** MCP items carry env var *names* only. API/plan/audit output is redacted. | Standard practice for a control plane. |
| 7 | **Host-run FastAPI on 127.0.0.1:8792**, hardened loopback-service pattern (Host allowlist, same-origin Origin, `X-Agent-Hub: 1` CSRF header on non-GET, strict CSP, no CORS, role enforcement on every router, body caps, SSE caps). Knowledge service on 127.0.0.1:8795 (separate process). | Proven pattern; these controls came from two audits. |
| 8 | **Zero-build, zero-npm frontend** (ES modules + web components, no CDN). | No supply-chain surface. |
| 9 | **Knowledge plane is a separate component** with a `VectorStore` port and **two backends: Qdrant (service) and LanceDB (embedded)**. Untrusted agents reach it only through per-client scoped tokens / broker sidecars; default read-only. | Isolation; both are wanted: Qdrant for scale, LanceDB for zero-service use. |
| 10 | **Curated memory**: agents write to a per-client *inbox*; only a human promotes to `content/memory/`. | Memory poisoning defence. |

## 2. Layout and ownership (disjoint)

```
hub/
  README.md docs/ deploy/ Makefile          (docs/ARCHITECTURE.md is this file)
  policy/floor.yaml
  backend/pyproject.toml
  backend/src/agent_hub/
    adapters/base.py                        LOCKED port
    adapters/__init__.py                    registry: imports every module in this package; a module exposes ADAPTER = <class>
    adapters/{claude_code,opencode,hermes,generic}.py  + adapters/_util.py
    domain/ services/ api/ main.py config.py cli.py store...
  backend/tests/core/  backend/tests/adapters/
  frontend/
  knowledge/
  examples/clients/*.yaml  docs/ADDING-A-CLIENT.md
content repo  (NOT in this tree by default: HUB_CONTENT_DIR, default <data_dir>/content, its own git repo; see Addendum E round 2)
```
State (audit DB, plan cache, lock manifests, tokens): `~/.local/share/agent-hub/` (0700). Env file: `~/.config/agent-hub/env`.

## 3. Content model (files in `content/`)

```
hub.yaml                    {version: 1, name}
endpoints.yaml              {router: http://127.0.0.1:4000/v1, knowledge: http://127.0.0.1:8795, ...}   # kills URL drift
instructions/<id>.md        frontmatter {id, title, order: int, applies_to?: [client ids]}  + markdown body
skills/<name>/SKILL.md      the shared standard: frontmatter {name, description} + body; extra files allowed
skills/<name>/hub.yaml      sidecar {groups: [], tags: [], notes: "", source: "", provenance: {}}  (keeps SKILL.md standard-clean)
agents/<name>.md            frontmatter {name, description, capabilities: [canonical], model_tier?, mode?, read_only?} + body
mcp/<name>.yaml             McpItem fields + {groups, tags, notes}
rules/<id>.yaml             {id, title, kind, match, decision, reason?, applies_to?: [client ids]}   # abstract Rule
memory/<id>.md              frontmatter {id, type, title} + body
inbox/<client>/<id>.md      proposed memories (never compiled)
collections/<id>.yaml       {id, title, description, icon, color, order, members: [{kind, name}]}   # multi-membership groups
clients/<id>.yaml           ClientConfig (adapters/base.py) + {manage: {skills: true, agents: true, ...}}
profiles/<client>.yaml      {collections: [ids], enable: {skills: [], agents: [], mcp: [], instructions: [], rules: [], memory: []},
                             disable: {...}}    # effective = union(collections members, enable) - disable; then floor
```
Names/ids: `^[a-z0-9][a-z0-9._-]{0,63}$`, validated with `fullmatch`. All parsing via safe YAML; unknown keys are errors.
Skill/agent bodies are untrusted text; never executed by the hub.

### Effective config
`effective(client) = resolve(profile) → enabled items → floor enforcement (rules merged; floor wins) → adapter.render`.
MCP enable requires `scan_status: clean` and the server's `egress_hosts` allowed by the profile's egress rules, else a plan
**error** (floor rule). Egress default is deny.

## 4. Plan and apply

* `plan(clients)` → `Plan{id, content_hash, clients: [{client, files: [{root, path, action: add|change|remove|unchanged|conflict|advisory,
  kind, source_ids, diff (unified, redacted)}], diagnostics, floor_violations, summary}]}`. Pure; writes nothing. Stored (in memory + state dir)
  keyed by `id`, valid only for the `content_hash` it was computed from (stale → 409 `plan_stale`).
* `apply(plan_id, confirm: true)` → for each managed file: atomic write (temp + fsync + rename in the same dir), mode set, backup of the previous
  file into `<state>/backups/<client>/<ts>/`, lock manifest update; `json_keys/yaml_keys/block` merges preserve everything not managed;
  removal only for files the lock manifest says the hub created. Then `verify` (adapter). Result recorded in audit.
  Any `error` diagnostic on a `strict` client blocks apply for that client. Apply never partially applies one client: stage all, then commit.
* `conflict` = live file hash ≠ last-applied hash (someone edited it). Needs explicit `adopt_paths` per path; otherwise skipped and reported.
* `drift(client)` = live vs rendered vs lock. `audit(client)` = check the live client config against the floor even for advisory concerns.
* Path safety for every artifact: relative, no `..`/absolute/backslash/NUL, resolved path under the named root, no symlink escape, no writes outside `roots`.

## 5. HTTP API (JSON; prefix `/api/v1`; RFC 7807 errors with `code`; lists return `{items, next_cursor}`)

```
GET  /health  /adapters  /adapters/{id}                caps + default_config + docs
GET  /clients  POST /clients  GET|PUT|DELETE /clients/{id}   status: in_sync|drift|never_applied|error; caps; manage flags
POST /clients/validate-spec                             dry-run a generic spec against sample content
POST /clients/{id}/discover                             -> DiscoveredContent with per-item conflict info vs existing content
POST /import   {client, items[], on_conflict: skip|rename|replace}   writes into the content working tree
GET  /skills  GET|PUT|DELETE /skills/{name}  POST /skills/{name}/duplicate     ?q=&group=&tag=&client=&enabled=&source=&issues=&sort=
GET|PUT|DELETE /agents/{name}   /instructions/{id}   /mcp/{name}   /rules/{id}   /memory/{id}   (same list filters)
GET  /collections  POST /collections  PUT|DELETE /collections/{id}  POST /collections/{id}/members  DELETE /collections/{id}/members/{kind}/{name}
POST /collections/reorder   {ids[]}      POST /items/bulk   {items[{kind,name}], add_to_collection?, remove_from_collection?, add_tags?, remove_tags?}
GET  /profiles/{client}  PUT /profiles/{client}
GET  /matrix                                            rows (items grouped by collection) x columns (clients); cell: on|off|via_collection|unsupported|blocked(+reason)
POST /matrix/toggle {client, kind, name, enabled}       and POST /matrix/bulk {clients[], items[], enabled}   (edit profiles in the working tree)
GET  /clients/{id}/effective                            resolved items, rules after floor, MCP, egress, warnings ("what can this client do now")
GET  /changes                                           pending content changes (git status + unified diff)
POST /changes/commit {message}   POST /changes/discard {paths?}   GET /changes/history   POST /changes/revert {commit}
POST /plan {clients?}  GET /plan/{id}  POST /apply {plan_id, confirm, adopt_paths?}  POST /clients/{id}/verify  GET /clients/{id}/drift  GET /clients/{id}/audit
GET  /policy/floor  (read-only)   POST /policy/check {profile-like body} -> violations
GET  /memory/inbox  POST /memory/inbox {client, title, body, type}  (token: scope inbox:write)  POST /memory/inbox/{id}/promote|reject   (admin)
GET  /knowledge/*   proxy to the knowledge service with the admin token (browser stays same-origin)
GET  /audit  GET|PUT /settings  GET|POST|DELETE /keys
GET  /metrics (root)
```
Roles `admin` / `viewer`; client tokens (`ec_`-style random, hashed at rest) carry the scope `inbox:write` (knowledge access uses tokens issued by the knowledge service, see Addendum D); the first start writes a bootstrap admin key (0600).
Edits (PUT/DELETE/POST on content) modify the **working tree only**. Nothing reaches a client until commit + plan + apply.

## 6. CLI `hubctl`
`init` (create content repo, seed policy defaults) · `validate` · `plan [--client]` · `apply [--client] [--adopt path]` · `verify` · `drift` · `audit` ·
`import --client X [--all]` · `adapters` · `serve`. Exit codes: 0 ok, 1 error, 2 plan has errors/violations. JSON output with `--json`.

## 7. Knowledge service (127.0.0.1:8795; separate process/package `agent_knowledge`)

```
GET  /health  /backends                                 backend health: qdrant (url), lancedb (path); dims; disk
GET|POST /namespaces  GET|PUT|DELETE /namespaces/{ns}   {name, backend: qdrant|lancedb, embedding_model, dim, description, disk_bytes, count}
POST /namespaces/{ns}/ingest   {documents: [{id?, text, metadata}]} | {path, glob}  (path only under allowlisted roots)   -> job
GET  /jobs  GET /jobs/{id}   (SSE /jobs/{id}/stream)
POST /namespaces/{ns}/query    {text | vector, k, filter?, min_score?} -> hits {id, score, text, metadata}
DELETE /namespaces/{ns}/documents/{id}
GET|POST|DELETE /tokens        per-client tokens {client, scopes: [{ns, mode: read|write}]}   default read-only; hashed at rest
GET|POST|DELETE /indexes       registry of shared read-only indexes (graphify/serena output dirs): {name, kind, path, project}
```
`VectorStore` port: `ensure(ns, dim)`, `upsert`, `query`, `delete`, `stats`, `drop`. Embeddings come from the router's `/v1/embeddings`
(`endpoints.yaml`), batched, with a fake in tests. Metadata filters are validated (no injection into backend filter syntax).
Untrusted agents: read-mostly, per-token namespace scopes, query/ingest size caps, rate limits. Compose templates under `deploy/`
(Qdrant, broker sidecar for an internal-net client) are opt-in and never applied by the app.

## 8. Frontend (zero-build). UX bar — "friendly organising" is the point

Hash router, web components, dark/light, `Ctrl-K` palette, keyboard reachable, toasts, skeleton/empty/error states everywhere, i18n file, WCAG AA,
responsive from ~600 px (nav collapses to a compact sticky bar; tables scroll inside cards; the shell grid rows are `auto minmax(0,1fr)`).
First-run **checklist** ("Import what you have → Organise into collections → Review plan → Apply"). Views:

1. **Library** — all skills/agents/instructions/MCP/rules/memory. Card and dense-list toggle; fuzzy search; filters (collection, tag, client, enabled state,
   source/provenance, has-issues); sort. Multi-select with shift/ctrl-click and a floating **bulk bar** (add to collection, tag, enable/disable for clients).
2. **Collections** — sidebar tree of user-defined groups with icon/colour; **drag-and-drop** items onto a collection (and reorder collections);
   an item may be in several collections; collection page shows members, description, and "enable for clients" in one click; create wizard.
3. **Matrix** — items (grouped by collection, collapsible) × clients. Tri-state cells: on / off / via-collection / unsupported / blocked (with the reason as tooltip).
   Click to toggle, column/row/collection headers toggle all, an **unsaved-changes bar** (undo, "Review plan"). This is the central enable/disable surface.
4. **Editor** — skill (name/description form + markdown editor with preview + file tree for extra files + frontmatter validation), agent (capability chips with the
   per-client native-tool mapping preview, model tier), MCP (catalog card with scan badge, egress hosts, per-client enable blocked until clean with the reason),
   rules (guided, floor-locked rows shown with a lock and why), instructions (ordered list, drag to reorder). A **"rendered for <client>"** tab shows the exact artifact.
5. **Clients** — cards with status, effective config ("what can this client do right now"), verify/drift/audit buttons, **add-client wizard** (pick an adapter or paste a
   generic spec with live validation and a dry-run render), manage-flags per concern with the diff shown before adopting management.
6. **Import wizard** — choose client → discovered items with checkboxes and conflict resolution (skip/rename/replace) → preview → import.
7. **Changes / Plan / Apply** — pending changes as a diff with commit-message box (the content approval); plan review per client with collapsible per-file unified diffs,
   diagnostics, floor violations; explicit Apply confirm; result with verify checks; history with revert.
8. **Memory inbox** — cards to edit/promote/reject. 9. **Knowledge** — namespaces, backend health, ingest form, **query playground**, tokens per client with scopes, indexes registry.
10. **Settings / audit**.
No inline scripts/styles (CSS classes and CSSOM only), all strings via i18n, charts/bars with text alternatives, a `dev/mock-server.mjs` that is contract-faithful.

## 9. Extension model (other clients)
Add a client by (a) `content/clients/<id>.yaml` with `adapter: generic` and a `spec:` (skills root + layout + filename, agent format + frontmatter/tool map,
instructions target + merge mode, mcp file + format + key path, permissions mapping, model tiers) — validated live in the UI — or (b) a Python module in
`agent_hub/adapters/` exposing `ADAPTER`. `docs/ADDING-A-CLIENT.md` documents both with a worked example. Adapters declare `caps()`, so unsupported concerns are
greyed out, and lossy mappings emit diagnostics (error when the client is `strict`).

## 10. Quality bar (all agents)
Python 3.13 via `uv`; `ruff` + `mypy --strict` clean; `pytest` green **offline** (fake filesystem, fake runner, fake embeddings, fake HTTP); coverage >= 85 % on the
planner/applier, floor, path safety, and adapters. No `shell=True`, every subprocess has a timeout, safe YAML only, identifiers by `fullmatch`. No secret in
logs/responses/plans. Comments explain why. Anything touching real files/Docker/git gets a self-test in a temp directory. Never write into a real client location during tests.

## Addendum A (core backend; additive only, nothing above changed)

Recorded by the core-backend slice. Everything here is implemented in `backend/src/agent_hub/`.

**Config.** `HUB_*` environment variables (`HUB_CONTENT_DIR`, `HUB_DATA_DIR`, `HUB_PORT`, `HUB_KNOWLEDGE_URL`, `HUB_KNOWLEDGE_TOKEN`, ...), optional
`~/.config/agent-hub/env` (override with `AGENT_HUB_ENV_FILE`); the real environment wins. `HUB_TRUST_LOOPBACK` (default FALSE: every request needs a bearer key; turning it on lets any local process be admin, `/health` then warns; same trade-off as Engine
Console: a loopback peer without an `Authorization` header is admin, but still needs `X-Agent-Hub: 1` on non-GET). (Turn it on only if you accept that.)

**Content schema additions.** Optional `tags: []` in agent/instruction/rule/memory frontmatter (bulk tagging works for every kind). `SKILL.md` frontmatter accepts
`name`/`description` are the only validated keys; every other frontmatter key is preserved verbatim (superseded from the first version, which listed `license`, `allowed-tools`, ... and rejected the rest). `hubctl init` seeds
`hub.yaml`, `endpoints.yaml`, `.gitkeep` in each content dir, and makes the first commit. Inbox files: `inbox/<client>/<id>.md`, frontmatter `{id,type,title,client,created}`.

**Plan / apply semantics.**
* Plans render from a snapshot of the COMMITTED tree (`git archive HEAD`), never the working tree. `content_hash` = the tree id of `HEAD` (empty tree when no commit).
  A plan carries a per-client `digest` (files + actions + desired hashes); `apply` recomputes against the live files and returns 409 `plan_stale` if the digest or
  content hash differs. `pending_changes` on the plan says how many uncommitted edits are NOT included.
* `adopt_paths` entries are `"<root>:<path>"` or `"<client>:<root>:<path>"`. Lock manifest: `<state>/locks/<client>.json` (0600) with per-file `sha256`,
  `slice_sha256` (the hub-owned part: whole file / managed keys / marker region), `merge`, `managed_keys`, `created`. A file is only ever deleted when `created` is
  true and its hash still matches the lock; merge slices are removed without touching the rest of the file.
* Conflict rule (all merge modes): live slice != lock slice, or a slice exists but there is no lock entry, or the file cannot be parsed for a key merge (or has a begin
  marker without an end marker). Unreadable targets (directory, symlink, too large) are conflicts that cannot be adopted.
* `manage` defaults: see Addendum D for the effective resolution (typed `manage`, then `params.manage`, then the adapter's default_config, then skills/agents/instructions managed and mcp/permissions/memory advisory). Floor findings about an advisory concern are downgraded to `warn`
  (they do not block), but `GET /clients/{id}/audit` still reports them as errors.
* `block` artifacts already contain their markers (`_util.wrap_block`); the applier replaces only the marker region (md `<!-- agent-hub:begin ... -->` or `#` style).
* `Expected.sha256` handed to `verify` is the hash of the intended whole file after the merge; `Expected.merge/managed_keys` are passed too.

**API additions.** `GET /content/validate`; `POST /skills/{name}/duplicate {new_name}`; `GET /clients/{id}/effective?source=committed|working` (default committed);
`POST /import` accepts `items` omitted (= everything) and `enable` (default true: imported items are enabled for that client); `POST /keys` accepts
`role: client` with `client` + `scopes`; `POST /memory/inbox` is allowed for admin or a client token holding `inbox:write` for that same client (viewers: 403).
Viewer-permitted POSTs (read-only dry runs): `/policy/check`, `/clients/validate-spec`, `/knowledge/namespaces/{ns}/query`. `PUT /settings` edits only
`plan_retention`, `inbox_max_per_client`, `inbox_rate_per_min`. Knowledge proxy: GET/POST (and admin-only DELETE, Addendum B), first path segment in `health|backends|namespaces|jobs|indexes|tokens`,
`GET .../stream` proxied as SSE under the connection cap.

**Proposals (not implemented).** (1) [done in Addendum B] admin-only `DELETE` through the knowledge proxy.
(2) Floor `tools.<name>.max` is a hard ceiling; if an operator ever needs `web_fetch: allow` it must be a floor edit in git, by design.
(3) [superseded] import now records the live files as the last-applied baseline, so the first plan after an import shows normal changes, not conflicts.

## Addendum B (frontend-driven backend additions)

* `GET /clients/{id}/rendered?kind=&name=&source=committed|working`: the artifacts one item contributes (root, path, merge, kind, source_ids, managed_keys, redacted
  `content`), from the planner's own render; 404 `not_rendered` with a reason when the item is not enabled for the client.
* Knowledge proxy now allows `DELETE` (admin only; same path allowlist and caps; DELETE bodies are not forwarded).
* `GET /clients/{id}/effective`: every rule has `source: floor|content|profile` (`profile` = enabled directly, `content` = via a collection).
* Client list/detail: `last_applied_at`, `last_applied_plan`, `last_verified_at`, `last_verified_ok` (stored in the lock manifest; `verify` updates them).
* `POST /clients` and `POST /clients/validate-spec` accept `spec_yaml` (safe YAML, 64 KB, no anchors); sending both `spec` and `spec_yaml` is a 422.
* `POST /plan {clients?, source: committed|working}`: `working` renders the working tree without committing; the plan has `preview: true`, `content_hash: "preview"`,
  and `apply` refuses it with 409 `plan_is_preview`.

## Addendum C (real-content gaps)

* **Client status.** Client list/detail: `adopted_at` (import adopted live files), `last_applied_at` (real applies only; null until the first), `status` in
  `in_sync|edited_outside|pending_changes|never_applied|error`, and `drift: {edited_outside: [{root,path,reason: changed|missing}], pending_changes: n}`.
  `edited_outside` = a lock entry exists and the live slice differs from it; `pending_changes` = the hub would change files nobody edited. The old `drift` status is gone.
* **`GET /clients/{id}/drift`** returns the same structure plus per-file `expected` (rendered), `actual` (live) and `locked` slice hashes, 12-char prefixes only.
* **`GET /collections/suggestions`** (deterministic: common name prefixes of 1-2 hyphen tokens, shared `source`, shared tags, a keyword table; 3..40 members; existing
  collection ids skipped; identical member sets are not repeated) and **`POST /collections/suggestions/accept {ids, overrides?}`** (admin; suggestions are recomputed server-side).
* **Import conflicts.** `discover` items carry `conflict: null | {existing_source, equal, differences[]}`. `POST /import` takes `on_conflict` globally and per item
  (`skip|rename|replace|link`); unset means `link` when equal, else `skip` with `reason: "differs"` and `differences`. `link` enables the existing item for this
  client (status `linked`) and never overwrites. `hubctl import --on-conflict link` and the same default.

## Addendum D (quickstart fixes)

1. **Host allowlist follows the effective bind.** It is derived from the configured port (`HUB_PORT`, `hubctl serve --port N`) and the listening socket's own port
   (127.0.0.1, localhost, [::1], the bind host) plus `HUB_ALLOWED_HOSTS`; it no longer stays on 8792.
2. **Effective `manage` per concern** = the client's typed `manage[concern]`, else `params.manage[concern]`, else the ADAPTER's documented default (its
   `default_config`), else the hub default (skills/agents/instructions managed; mcp/permissions/memory advisory). The client detail shows the result as `manage_effective`.
3. **Token scopes.** The hub's only client-token scope is `inbox:write`; `knowledge:read:<ns>` is removed (422 `unknown_scope`). Knowledge access uses tokens issued by the knowledge service.
4. **`HUB_API_KEY`** for `hubctl` is read from the environment, else from `~/.config/agent-hub/env`, else the 0600 bootstrap file (the real environment wins; never printed).
5. Addendum A corrected: `HUB_TRUST_LOOPBACK` defaults to false; superseded SKILL.md-key, knowledge-DELETE and import-conflict statements are marked in place.

## Addendum E (audit fixes)

* **Delivery targets (H1).** Client roots and artifact targets are refused (`bad_root` / `unsafe_path`) when they overlap the floor's `deny_path_write` stems,
  the hub state dir, credential dirs, contain or are the home dir or `/`, sit inside `.git`, or (targets only) inside the content dir, `policy/` or `backend/`;
  generic specs may not target `hooks` segments. A project root that merely CONTAINS the app is allowed.
* **MCP gate (H2).** `clean` is bound to `scan_digest` (sha256 of command/args/url/pinned_ref/transport); editing those fields returns the server to
  `unscanned`. Stdio servers need `sandbox_profile`; http/sse servers need `egress_hosts` (loopback exempt). Allow rules for `mcp_server` / `tool:mcp`
  (`mcp__*`) that would cover an unscanned server are floor violations (`floor_mcp_rule`).
* **Skills (M2).** `allowed-tools` with unrestricted `Bash`/`*`/`mcp__*` or a `hooks` key is an error (the skill is withheld, bytes never rewritten); other pre-approvals warn.
* **Floor (M3).** Paths are normalised (`~`, `.`, `..`, `//`), catch-all command/egress allows are detected by probe strings (`command_allow_probes`,
  `egress.forbid_allow_probes` in floor.yaml), and the floor is re-checked on the RENDERED artifacts (`floor_violation_rendered`).
* **Applier (M4).** Writes go through directory fds opened component-by-component with `O_NOFOLLOW`; before replacing or deleting, the live file is re-read and
  compared with what was planned (a mismatch aborts the client, rolls back, and reports `conflict`).
* **Verify (I1).** `Expected.sha256` for json_keys/yaml_keys/block artifacts is the SLICE digest with `merge`/`managed_keys` set; `RealFS.size(path)` added.
* **Ops (L2-L5).** Non-loopback `HUB_HOST` is refused unless `HUB_ALLOW_NONLOOPBACK=1`; state dirs are 0700 and tightened at start; viewers cannot read knowledge
  `tokens|indexes|backends`; git runs with literal pathspecs, no textconv and no attributes file, snapshots come from `ls-tree`+`cat-file` (not `git archive`),
  `commit` stages only known content paths and returns `strays`, and discarding everything needs `confirm: true` (`dry_run: true` lists it).

**Addendum E, round 2 (re-audit and rollback).**
* **Content repo is the approval boundary (N1).** `content_dir` now defaults to `<data_dir>/content` (0700), outside the project tree. The hub refuses to run
  (`content_dir_unsafe`, with the fix) when the content dir is inside a client root or the app tree, not owned by the current user, group/world-writable, or when
  `.git/config` has keys outside a small allowlist, `.git/info/attributes` or any `.gitattributes` is non-empty, or `.git/hooks` holds non-sample files. Git runs with
  umask 077, a scrubbed environment, `core.sshCommand=/bin/false`, `protocol.allow=never`, `diff.external=`. `hubctl migrate-content --to PATH [--yes]` moves an existing repo
  (refuses an existing target, preserves modes then strips group/world write, `git fsck --strict`, same HEAD, atomic rename, removes the old copy, no leftovers).
* **Skills / MCP rules (N2, N1b).** `allowed-tools` is split on commas and whitespace outside parentheses; `Bash`, `Bash(*)`, `Bash(*:*)`, `Bash(:*)`, `*`, `mcp__*` are errors; a
  named MCP tool needs an enabled, clean server. Wildcard MCP allow rules are always errors; a named allow is valid only for an enabled clean server.
* **Targets (N4).** The whole app tree except `out/` is protected; no adapter may write `.claude/settings.local.json`, `security/**` or `.git/**` paths; `.claude/settings.json` only for
  claude-code with `manage.permissions`, `AGENTS.md` only as managed instructions by opencode/generic. Advisory artifacts are never refused (they are never written).
* **Launchers (N6).** `git *`, `node *`, `npx *`, `uv run *`, `find *`, `curl *`, ... are warnings (`floor_command_pattern`, rule kept); TLD wildcards (`*.com`, `*.io`, ...) are errors.
* **Attestation (HMAC).** `scan_digest` is HMAC-SHA256 keyed by `<data_dir>/attest.key` (0600, created on first use). Old unkeyed digests read as `unscanned`.
  `hubctl attest-mcp NAME --yes` re-binds `clean` to the current fields after YOU ran your MCP scanner (it does not run one).
* **Rollback.** Every apply is recorded (`<state>/applies/<client>/<apply_id>.json`: files, backups, pre/post hashes, previous lock entries; `apply_id` = UTC timestamp + plan id).
  `GET /clients/{id}/applies`, `POST /clients/{id}/rollback {apply_id?, confirm, dry_run?, force_paths?}`, `hubctl rollback --client X [--apply-id] [--dry-run] [--yes]
  [--force-paths ROOT:PATH]`, `hubctl applies`. Files are restored (or deleted if the apply created them) only while the live hash still equals the post-apply hash, through the
  same dirfd/CAS writes, all-or-nothing; edited files are reported as conflicts and skipped (`partially_rolled_back`) unless forced; an older apply is refused (`rollback_not_latest`)
  if any of its files changed since; a second rollback reports `already_rolled_back`.

**Addendum E, round 3 (residual host exposure).**
* `hubctl` asks for the admin key with a hidden prompt (`getpass`) when there is no `HUB_API_KEY` (environment or env file) and no readable bootstrap file and stdin is a TTY;
  without a TTY it stays an error. `hubctl retire-bootstrap-key` (TTY only) verifies the key against the stored hash, then zero-fills and unlinks the bootstrap file; it is
  idempotent and refuses a symlinked path or a key that does not verify.
* `hubctl doctor [--json]`, `GET /api/v1/doctor` (admin) and `GET /api/v1/doctor/summary` (`{crit,warn,info}`, any authenticated role): a read-only report with findings
  `{id, severity: ok|info|warn|crit, title, detail, fix}` and `status: ok|attention|action_needed`. Checks: bootstrap key at rest, state modes, content dir location/ownership,
  loopback trust and bind, the Claude Code sandbox template and rendered config deny entries (four state paths and `**/hub/policy/**`), sandbox `allowedDomains`, opencode's unsandboxed
  exposure, `ptrace_scope`, installed systemd user units' `ReadWritePaths`, knowledge reachability/auth, and advisory-permission gaps. It never prints secrets; filesystem and env are injectable.
