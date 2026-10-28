# Agent Hub: Python → Rust port, the contract (PORTING.md)

**Status: LOCKED (phase 1, 2026-10-27, branch `rust-backend` from main `c419f1e`).** Later changes are numbered,
additive addenda at the end of this file (`A-1`, `A-2`, ...): a workstream that needs a contract change asks the lead,
the lead appends an addendum; nobody rewrites a locked section. The binding rules for every agent of the port were a
shared rules document kept outside this repository (parity, safety, gates, reporting); this file adds the Agent Hub
specifics. Python stays the reference until cutover (not part of any workstream).

**Paths after the cutover (0.3.0, addendum A-13).** The workspace moved from `rust/` to `backend/` when the Python backend
was removed; read every `rust/...` path below as `backend/...`. The Python sources this contract cites (`backend/src/...`,
`knowledge/src/...`) are in git history (`c419f1e`, `v0.2.0`); `node backend/parity/lib/python-release.mjs [ref]` exports
them with their venvs.

## 1. What is ported

| Python | Lines (src / tests) | Rust | Binary |
|---|---|---|---|
| `backend/src/agent_hub` (FastAPI app, services, adapters) | ~13 300 / 10 700 | `rust/crates/agent-hub` + `rust/crates/hub-core` | `agent-hub` (serve, `open`, `stop`) |
| `backend/src/agent_hub/cli.py` (`hubctl`) | 423 | `rust/crates/agent-hub/src/cli.rs` | `hubctl` |
| `knowledge/src/agent_knowledge` (FastAPI, LanceDB/Qdrant/memory stores) | 2 100 / 1 000 | `rust/crates/agent-knowledge` | `agent-knowledge` |

Not ported (stay as they are, see §16): `deploy/turnstone-deliver.py`, `deploy/hermes-deliver.sh`,
`deploy/knowledge/broker/addon_knowledge_broker.py` (mitmproxy addon), `frontend/` (the API contract; only the e2e
runner got the backend switch), `policy/floor.yaml` (read by both).

## 2. Decision table

| # | Decision | Why |
|---|---|---|
| D1 | Cargo workspace in `rust/`: `hub-core` (lib), `agent-hub` (lib + 2 bins), `agent-knowledge` (lib + bin). `default-members` = hub-core, agent-hub. | Common rules layout; agent-knowledge pulls ~500 crates (lance), so hub workstreams build without it. Gates use `--workspace`. |
| D2 | HTTP: axum 0.8 as the socket layer only; ONE fallback handler runs our own Starlette-compatible pipeline (`api/security.rs::handle`): ordered first-match routing on the percent-decoded path, 405 on a path-only match, problem+json everywhere. | matchit's static-over-param priority and axum's 405/422 differ from Starlette/FastAPI. |
| D3 | Route table is code (`api/routes/mod.rs::table()`), in Python's exact order; `tests/routes.rs` asserts equality with `parity/routes.python.json` (generated from the live app). | Order is semantics in Starlette. |
| D4 | Validation: `hub_core::pyd`, a pydantic-2.13 lax-mode emulation (messages, `type`, `loc`, order, `str(ValidationError)` incl. the 25+…+24 truncation). Content and body models are declared once (`hub_core::models`, `api/routes/bodies.rs`) and golden-tested against pydantic itself. | 422 `errors` and content `issues` are UI-visible text. |
| D5 | Python value model `hub_core::PyValue` for everything YAML-born (ordered dicts with any hashable keys, int/float/bool apart, date/datetime, bytes, set). API/JSON-born data is `serde_json::Value` with `preserve_order`. | Frontmatter keeps unknown keys verbatim; YAML 1.1 types. |
| D6 | JSON bytes: `hub_core::py::json` reproduces `json.dumps` (all separators/indent/sort_keys/ensure_ascii combos used) and pydantic `model_dump_json` (incl. pydantic's float format). Float `repr` ported exactly (`py::float`). | Files are compared byte-for-byte; HMAC/digests are over JSON text. |
| D7 | YAML: `hub_core::yaml` has a LOCKED API (`load(text, Loader::{Safe,Strict})`, `dump(value, DumpOpts)`); phase 1 ships a TEMPORARY engine on yaml-rust2's event parser with PyYAML's YAML 1.1 resolver/constructor. WS-Y replaces the engine with a port of PyYAML 6.0.3 (see §12) and removes `yaml-rust2`. | YAML emitter fidelity is the biggest byte-identity risk; isolating it lets every other workstream code against the API now. |
| D8 | TOML: parsing (`planner.toml_block_mcp_servers`) with the `toml` crate (semantic check only); the one TOML writer (`generic._toml_val`/`toml_tables`) is hand-ported in `adapters/generic.rs`. | Python uses `tomllib` (parse) and a hand-rolled emitter. |
| D9 | SQLite via rusqlite (bundled), identical DDL and migration bookkeeping; hub.db/knowledge.db written by Python open unchanged (proven by the parity harness: the Rust server runs on a Python-seeded hub.db). | Persisted-state rule. |
| D10 | Subprocesses: `std::process::Command` with explicit argv, cleared env plus exactly Python's variables, own process group where Python uses `start_new_session`, hard timeouts, capped output (§13). Never a shell. | Common rules. |
| D11 | Directory-fd file operations (safefs) with `nix` (openat/mkdirat/renameat/unlinkat/fstat); `unsafe_code = "forbid"` stays. | TOCTOU control without unsafe. |
| D12 | Prometheus text exposition hand-written (`agent-hub/src/telemetry.rs`) to match prometheus_client 0.26 byte-for-byte (`_created` gauges, `floatToGoString`, `le` first, content type `text/plain; version=1.0.0; charset=utf-8`). | Grafana boards; prometheus-client crate output differs. |
| D13 | Adapter registry discovered at BUILD time (`agent-hub/build.rs` scans `src/adapters/*.rs` except `mod.rs`, `base.rs`, `_*.rs`; each module exposes `pub fn adapter() -> Option<Result<Arc<dyn ClientAdapter>, String>>`). | Mirrors `pkgutil.iter_modules`; a new client (deepseek_harness) is ONE file. |
| D14 | Hub facade split by owner: `services/hub/{mod.rs (CORE), content_ops.rs (B1), delivery_ops.rs (B2), policy_ops.rs (F), eval_ops.rs (C)}` as separate `impl Hub` blocks. Services that need hub callbacks (SkillEvalService, ObserveService) get a `Weak<Hub>` (`Arc::new_cyclic`). | One owner per file; Python's callbacks (`guard=self.mutating`, `hub_view=self._hub_view`). |
| D15 | Blocking hub work runs in `tokio::task::spawn_blocking` (Python: sync endpoints in a thread pool); the hub lock is a re-entrant `HubLock` (Python `RLock`). | Same concurrency semantics. |
| D16 | LanceDB: the `lance` crate `=12.0.0` directly (NOT the `lancedb` crate), protoc from `protoc-bin-vendored =3.3.0` via `rust/tools/protoc` + `.cargo/config.toml [env] PROTOC`. §15. | `lancedb 0.39.0` does not compile without `remote`; lance 12.0.0 is the exact engine in Python lancedb 0.39.0. |
| D17 | Unported handlers answer `501` problem+json `{"type":"urn:agent-hub:not_ported","title":"not ported","status":501,"code":"not_ported","detail":"not ported: <module>"}` (it carries the `detail` the brief asks for, in the app's own problem shape). | Distinguishable from every Python status. |
| D18 | Test names mirror Python test names; one Rust test may cover all parametrisations. TEST-MAP.md is generated (`parity/gen/gen_test_map.mjs`) and checked (`parity/check-test-map.mjs`). | Common rules §Testing. |
| D19 | Versions: newest stable ≥14 days old (2026-10-27), preferring the versions already locked by the maintainer's harness2-workbench project (its lock file seeded `rust/Cargo.lock`). Exception: `lance`/`lance-linalg =12.0.0` (pinned to Python's engine, D16) and `protoc-bin-vendored =3.3.0`. | The project's dependency cooldown policy. |

## 3. Workspace, build, gates

```
rust/Cargo.toml            workspace (lints = harness2-workbench's; edition 2024; toolchain 1.99.1)
rust/.cargo/config.toml    PROTOC=tools/protoc (relative), build.jobs = 8
rust/.cargo/audit.toml     the same 4 advisory ignores for `cargo audit`
rust/tools/protoc          execs ~/.cargo/registry/src/*/protoc-bin-vendored-linux-<arch>-3.3.0/bin/protoc
rust/deny.toml             adapted from harness2-workbench (no wasm; 4 advisory ignores with reasons, R-LANCE-ADVISORIES)
rust/crates/hub-core       py-compat, yaml, pyd, dotenv, domain (errors, ids, pathsafe, models, common, yamlsafe)
rust/crates/agent-hub      config, api, services, adapters, cli, desktop, telemetry, server
rust/crates/agent-knowledge
rust/parity                differential harness (Node 22, zero deps), route tables, test-map tooling, golden generators
```

`export PATH=$HOME/.cargo/bin:$PATH CARGO_BUILD_JOBS=8`; in `rust/`: `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace`, `cargo deny check`,
`cargo audit` (same 4 ignores, `.cargo/audit.toml`), then `node parity/check-test-map.mjs`, `node parity/run.mjs` (+ `--app knowledge`),
`node --test ../frontend/tests/*.test.mjs`, `E2E_BACKEND=rust node ../frontend/e2e/run.mjs`.
First cold build of agent-knowledge ≈ 4 min (lance); hub crates ≈ 1 min.

Workstreams work in their own git worktree on branch
`rust-backend-<ws>` from the phase-1 commit, edit ONLY the files they own (§5), and never add a crate to a
`Cargo.toml` (every crate a workstream needs is already declared; ask for an addendum otherwise).

## 4. Workstreams

| WS | Scope | Python sources | pytest ids |
|---|---|---|---|
| **CORE** (lead) | config, server/routing/middleware/errors/static, api handlers, store, auth, audit, telemetry, desktop, hub construction + settings, adapter port + registry, hub-core primitives, dotenv, pyd, models | config.py, main.py, api/*, domain/{errors,ids,pathsafe,models}.py, services/{store,auth,audit,common}.py, desktop.py, adapters/{__init__,base}.py, parts of hub.py | 109 (71 mapped, 38 pending: test_desktop launcher flows, test_cli_registry registry/env-file, test_ops_hardening state dirs, part of test_api) |
| **WS-A** client adapters | `_util`, claude_code, generic (incl. spec validation, dry_render, TOML writer), hermes (observe program, docker argv), opencode, turnstone | adapters/{_util,claude_code,generic,hermes,opencode,turnstone}.py | 232 (+43 N/A turnstone-deliver) |
| **WS-B1** content store, organize, import | content, attest, resolver, importer, organize, inbox, suggest, `hub/content_ops.rs` | services/{content,attest,resolver,importer,organize,inbox,suggest}.py + hub.py content/client/import methods | 81 |
| **WS-B2** delivery | gitrepo, merge, planner, applier, rollback, applylog, lock, safefs, fs (RealFs, runners, discovery_fs), difflib, `hub/delivery_ops.rs` | services/{gitrepo,merge,planner,applier,rollback,applylog,lock,safefs,fs}.py + hub.py lifecycle/changes/plan/apply methods | 110 |
| **WS-F** policy & safety | floor, targets, contentguard, doctor, `hub/policy_ops.rs` | services/{floor,targets,contentguard,doctor}.py + hub.py policy_check/audit_client/doctor | 237 |
| **WS-C** evaluation, observation, knowledge client, CLI | skilleval (+SkillSpector), observe, knowledge proxy client, `hub/eval_ops.rs`, `cli.rs` (`hubctl`) | services/{skilleval,observe,knowledge}.py, cli.py, hub.py observe/eval methods | 173 |
| **WS-D** knowledge service | every file of `rust/crates/agent-knowledge` | knowledge/src/agent_knowledge/** | 77 (+15 N/A broker addon) |
| **WS-Y** YAML | the PyYAML 6.0.3 port behind the locked `hub_core::yaml` API, `yamlsafe.rs` | PyYAML SafeLoader/SafeDumper (+ domain/yamlsafe.py) | 8 (test_yamlsafe) + every byte-level YAML assertion elsewhere |

`deepseek_harness` adapter: ported for 0.3.0 from the Python files that were never committed (A-15), as ONE new file
`crates/agent-hub/src/adapters/deepseek_harness.rs` exposing `adapter()` (build.rs picks it up), owned by WS-A.

## 5. Module map (one owner per file)

CORE files are implemented in phase 1 unless marked *pending*. Every other file exists as a stub whose PUBLIC
signatures are the locked contract (bodies return `Problem::not_ported(..)` → HTTP 501).

| Rust file | Python source | Owner |
|---|---|---|
| hub-core/src/lib.rs, py/mod.rs, py/{float,json,text,fnmatch,os,starlette}.rs | json/repr/float/fnmatch/posixpath/str methods/starlette.compile_path | CORE |
| hub-core/src/value.rs | (Python object model) | CORE |
| hub-core/src/dotenv.rs | python-dotenv 1.2.3 `dotenv_values` | CORE |
| hub-core/src/pyd.rs | pydantic 2.13 lax validation | CORE |
| hub-core/src/errors.rs | domain/errors.py, api/errors.py body | CORE |
| hub-core/src/ids.rs, pathsafe.rs, common.rs | domain/ids.py, domain/pathsafe.py, services/common.py | CORE |
| hub-core/src/models.rs | domain/models.py (+ adapters/base.py CAPABILITIES) | CORE |
| hub-core/src/yaml/mod.rs (API), yaml/compat.rs (temporary engine) | PyYAML | WS-Y |
| hub-core/src/yamlsafe.rs | domain/yamlsafe.py | WS-Y |
| hub-core/tests/golden/models.json | (generated: parity/gen/gen_models_golden.py) | CORE |
| agent-hub/build.rs, src/lib.rs, src/bin/*.rs | entry points | CORE |
| agent-hub/src/config.rs | config.py | CORE |
| agent-hub/src/server.rs | main.py | CORE |
| agent-hub/src/telemetry.rs | services/audit.py Telemetry | CORE |
| agent-hub/src/desktop.rs | desktop.py | CORE (*pending: launcher flow tests*) |
| agent-hub/src/api/{mod,deps,errors,security,static_files}.rs | api/{deps,errors,security}.py, main.py | CORE |
| agent-hub/src/api/routes/{mod,bodies,system,clients,content,organize,changes,knowledge,skilleval}.rs | api/routers/*.py | CORE |
| agent-hub/assets/openapi.json | FastAPI-generated (parity/gen/dump_openapi.py) | CORE |
| agent-hub/src/adapters/{mod,base}.rs | adapters/__init__.py, adapters/base.py | CORE |
| agent-hub/src/adapters/{_util,claude_code,generic,hermes,opencode,turnstone}.rs | adapters/*.py | WS-A |
| agent-hub/src/services/mod.rs, store.rs, auth.rs, audit.rs, hub/mod.rs | services/{store,auth,audit}.py, hub.py (init, lock, settings, adapters view) | CORE |
| agent-hub/src/services/{content,attest,resolver,importer,organize,inbox,suggest}.rs, hub/content_ops.rs | same-named .py, hub.py | WS-B1 |
| agent-hub/src/services/{gitrepo,merge,planner,applier,rollback,applylog,lock,safefs,fs,difflib}.rs, hub/delivery_ops.rs | same-named .py (+ difflib), hub.py | WS-B2 |
| agent-hub/src/services/{floor,targets,contentguard,doctor}.rs, hub/policy_ops.rs | same-named .py, hub.py | WS-F |
| agent-hub/src/services/{skilleval,observe,knowledge}.rs, hub/eval_ops.rs, src/cli.rs | same-named .py, cli.py, hub.py | WS-C |
| agent-hub/tests/api_security.rs, routes.rs, golden_bodies.rs | tests/core/test_api.py (core parts) | CORE |
| agent-knowledge/** (incl. tests/, build.rs, fixtures) | knowledge/** | WS-D |
| parity/** | — | CORE (WS add corpus cases via addendum requests; see §14) |
| TEST-MAP.md | — | generated; each WS edits only its rows |
| PORTING.md, Cargo.toml (all), deny.toml, .cargo/, tools/ | — | CORE |

New Rust test files: a workstream may ADD test files under `crates/<crate>/tests/` named after the Python test file it
ports (e.g. `tests/test_floor.rs`) and unit tests inside the files it owns.

## 6. Cross-workstream contract (locked signatures)

The stubs ARE the contract; read them. The ones that cross workstreams:

- **Adapter port** (`adapters/base.rs`, CORE): `ClientAdapter` (`id, display_name, docs, caps, default_config, render,
  discover, verify, observe`), `FileSystem`, `CommandRunner`, all item/artifact/result types (field order = pydantic).
  `docs()` returns the Python class docstring `.strip()`ped (GET /adapters serves its first 4000 characters).
- **Content** (B1 → B2, C, F): `ContentStore` (`names, names_of, load, put, catalog, get_client, ...`), `Item`, `Catalog`
  (typed `ClientDoc`, `CollectionDoc`, `ProfileDoc` from `hub_core::models`), `validate_endpoints`, `kind_from_plural`.
- **Resolution** (B1 → B2, F, C): `Resolver::resolve -> Resolved`, `enabled_keys`, `client_config`, `manage_flags`,
  `client_caps`, `to_mcp`.
- **Delivery** (B2 → B1, C, F): `GitRepo` (`is_repo, head_tree, status, changes, commit, discard, history, revert,
  export_head, verify_safe`), `Planner::{plan_client, drift}`, `PlannedFile`, `ClientPlanInternal`, `merge::{Desired,
  Merged, merge_file, find_region}`, `LockStore`/`Lock`/`LockEntry`, `ApplyLog`, `Applier`, `Rollbacker`, `fs::{RealFs,
  RealRunner, StreamingRunner, LiveFiles, discovery_fs, is_secret_path}`, `safefs::*`, `difflib::unified_diff`,
  `Hub::{require_repo, check_content_safety, approved, plan, get_plan, ...}`.
- **Policy** (F → B1, B2): `Floor` (`check_rule, check_permission_set, enforce, check_skill, check_mcp,
  missing_denies, describe`), `FloorPolicy`, `load_floor` (implemented), `norm_path`/`overlaps` (implemented),
  `host_matches`, `TargetGuard::{root_problem, target_problem, sensitive_target}`, `contentguard::*`, `doctor::*`.
- **Evaluation** (C → B1, F): `SkillEvalService::{gate, current_hash, records, freshness, running, queued_subjects,
  enqueue, detail, report, status, doctor_info, find_launcher, evaluate*, start, queue, cancel, prune_observed,
  shutdown}`, `content_hash`, `is_bytecode`, `security_severity`; `ObserveService::{store, index, entry, files,
  annotated, count, summary, forget}`, `HubView`; `KnowledgeProxy::{summary, forward, stream, status}`.
- **Import gate** (B1, called with C's gate): `ImportGate { mode, check: GateCheck, override_reason, audit }`.

A workstream that finds a signature it cannot implement faithfully asks the lead for an addendum; it does not change
another owner's file.

## 7. HTTP surface

### 7.1 Routes
- Hub: 98 routes, `rust/parity/routes.python.json` (order, methods, effective path, regex, path/query/body params,
  status code, dependencies). Prefix `/api/v1`; `/metrics` only with `HUB_ENABLE_METRICS` (default on); the catch-all
  `GET /{path:path}` (SPA) only when the frontend dir exists. `tests/routes.rs` keeps the Rust table equal.
- Knowledge: 20 routes, `rust/parity/routes.knowledge.json` (no prefix, no catch-all).
- Regenerate both with `rust/parity/gen/dump_routes.py` (header of the file has the commands).

### 7.2 Request pipeline (hub; `api/security.rs::handle`, the order matters)
1. Host allowlist → 421 `misdirected_request` (`{127.0.0.1, localhost, [::1], HUB_HOST} × {HUB_PORT, socket port}` ∪
   `HUB_ALLOWED_HOSTS`, case-insensitive, port required).
2. Non-GET/HEAD/OPTIONS: `Origin` (`null`, or netloc ≠ Host) → 403 `cross_origin`; no `Authorization` and
   `X-Agent-Hub` ≠ `1` → 403 `csrf_header_required`; digit-only `Content-Length` over the path's cap → 413.
3. Guarded paths (`/api/*`, `/metrics`, not `/api/v1/health`): bearer key → principal; else loopback trust (only with
   `HUB_TRUST_LOOPBACK` and no `X-Forwarded-For`) → admin; else 401 `unauthorized`. Client role: only `POST
   /api/v1/memory/inbox`; viewer: no mutation except policy/check, clients/validate-spec, knowledge
   namespaces/<ns>/query → 403 `forbidden`.
4. Routing (Starlette first-match; 405 `method_not_allowed` "Method Not Allowed"; 404 "Not Found").
5. Body read (only routes with a body parameter, and the knowledge proxy), streaming cap → 413 `payload_too_large`
   "request body too large" (title "payload too large").
6. `enforce_roles` (every /api/v1 router) → 403 "admin role required for this action".
7. Endpoint dependencies (`require_admin` → 403 "admin role required") BEFORE query/body validation.
8. Validation → 422 `validation_error` "request validation failed" + `errors=[{loc, msg, type}]` (query and body
   errors together; strict content type: only `application/json`/`application/*+json` bodies are parsed; empty body =
   `missing` at `["body"]`; bad JSON = `json_invalid` at `["body", <pos>]`; non-object for a model =
   `model_attributes_type`, for `dict[str, Any]` = `dict_type`).
9. Handler; Python `ProblemError` → problem+json; unexpected error → 500 `internal_error` "unexpected server error".
10. Every response: the 6 security headers replace any existing ones (CSP, nosniff, no-referrer, DENY,
    permissions-policy, CORP same-origin); `/api/*` also `Cache-Control: no-store`; no CORS headers ever.
11. Metrics (`method` ∈ known set else OTHER; `route` = matched template, `/api/v1` prefixed; `unmatched`/`static`),
    then the audit row for guarded mutating requests (also denied ones): actor = key id or principal name; `params` =
    redacted query (`parse_qsl`, last value wins) + body: content paths (inbox, knowledge) as `{"bytes", "fields"}`
    metadata, JSON bodies redacted, else `[N bytes, non-json]`; the body only if the handler read it.
Problem body: `{"type": "urn:agent-hub:<code>", "title": <code with spaces>, "status", "code", "detail", **extra}`,
media type `application/problem+json`. JSON success bodies: `json.dumps(ensure_ascii=False, separators=(",", ":"))`.

### 7.3 Static frontend
`index.html` + `css/`, `js/`, `i18n/` only; `..`/symlink escapes and missing assets → 404 problem "no such file";
`/api` or `/api/...` → 404 "no such API route"; SPA fallback for extension-less paths; `Cache-Control: no-cache`,
`ETag = "md5(repr(st_mtime) + '-' + st_size)"`, `Last-Modified`, `Accept-Ranges: bytes`; matching `If-None-Match`
(weak tags accepted) → 304 with ETag + Cache-Control only. HEAD → 405 (FastAPI GET routes do not answer HEAD).

### 7.4 Knowledge service
Its own pipeline (knowledge/src/agent_knowledge/security.py:49): Host (cfg host:port + allowed_hosts) → 421; empty /
`.` / `..` / backslash / NUL segments → 400 `invalid_path`; non-safe: Origin → 403, `X-Agent-Knowledge: 1` unless
bearer → 403, body cap → 413; headers incl. `Cache-Control: no-store` on every response; audit.jsonl metadata only;
auth + scope dependencies BEFORE body validation (api.py:51/82/103); 422 detail is ONE string `"loc: msg; ..."`
capped at 500 chars; problem type `urn:agent-knowledge:<code>`; 429 carries `Retry-After: 30`. Owner WS-D.

### 7.5 SSE
Hub `GET /api/v1/knowledge/<...>/stream`: connection cap `HUB_MAX_SSE_CONNECTIONS` (429 `too_many_streams`), idle
timeout `HUB_SSE_IDLE_TIMEOUT_S` ends the stream, upstream byte cap `HUB_KNOWLEDGE_RESPONSE_CAP_BYTES`, headers
`text/event-stream; charset=utf-8`, `X-Accel-Buffering: no` (Cache-Control becomes `no-store` on /api). Knowledge
`GET /jobs/{jid}/stream`: events as Python (WS-D).

## 8. Settings, environment, config files

### 8.1 Hub (`config.rs`, pydantic-settings semantics)
Source precedence: explicit (CLI `--content-dir`) > environment > env file > default. Names case-insensitive.
Env file: `AGENT_HUB_ENV_FILE` or `~/.config/agent-hub/env` (python-dotenv syntax: `export`, quotes, escapes,
`${VAR}`/`${VAR:-default}` interpolation over env + earlier keys, `#` comments). List fields are JSON
(`["a:1"]`); a JSON error refuses to start: `error parsing value for field "<f>" from source "EnvSettingsSource"`.
A bad value refuses to start with pydantic's `str(ValidationError)` (title `Settings`).

| Setting (env `HUB_<NAME>`) | Default |
|---|---|
| host / port | `127.0.0.1` / `8792` |
| data_dir | `~/.local/share/agent-hub` |
| content_dir | `<data_dir>/content` unless set |
| policy_file | `<app root>/policy/floor.yaml` |
| frontend_dir | `<app root>/frontend` |
| git_bin / git_timeout_s / git_author_name / git_author_email | `git` / 30.0 / `Agent Hub` / `agent-hub@localhost` |
| allowed_hosts | `[]` |
| trust_loopback | false |
| body_cap_bytes / import_body_cap_bytes | 1 MiB / 4 MiB (`/api/v1/import`, `*/discover`) |
| max_sse_connections / sse_idle_timeout_s | 8 / 300.0 |
| max_content_file_bytes / max_skill_bytes / max_live_file_bytes | 256 KiB / 4 MiB / 8 MiB |
| knowledge_url / knowledge_timeout_s / knowledge_body_cap_bytes / knowledge_response_cap_bytes | `http://127.0.0.1:8795` / 30.0 / 1 MiB / 8 MiB |
| knowledge_admin_key (legacy knowledge_token) | unset (never logged; settings show `[set, N chars]`) |
| knowledge_admin_key_file (env `KNOWLEDGE_ADMIN_KEY_FILE` or `HUB_KNOWLEDGE_ADMIN_KEY_FILE`) | `~/.local/share/agent-knowledge/admin.key` |
| discover_extra_roots | `[]` |
| inbox_rate_per_min / inbox_max_per_client / plan_retention | 20 / 200 / 20 (editable in PUT /settings: 1..600 / 1..5000 / 1..200) |
| eval_gate / eval_gate_observed / eval_gate_min_tiers | `advisory` / `require` / `static` (editable enums) |
| enable_metrics | true |

Other variables: `HUB_ALLOW_NONLOOPBACK=1` (non-loopback bind), `HUB_API_KEY` (hubctl: env, else env file, else
`<data>/bootstrap-admin.key`, else hidden TTY prompt), `SKILL_EVAL_*`/`SKILLEVAL_*` (passed ONLY to SkillEvaluator),
`AGENT_HUB_APP_ROOT` (new, D-APPROOT). Stored overrides of the editable settings live in hub.db `settings`
(`json.dumps(value)`); an enum value no longer allowed falls back to the configured default.

### 8.2 Knowledge (`agent-knowledge/src/config.rs`, WS-D)
`--config FILE` or `AGENT_KNOWLEDGE_CONFIG` (YAML, `yaml.safe_load`, extra keys forbidden), all keys of
knowledge/src/agent_knowledge/config.py:16 (host, port, allowed_hosts, data_dir, lance_dir, qdrant_url,
qdrant_api_key_env, qdrant_vectors_on_disk, router_url, router_api_key_env, default_embedding_model, ingest_roots,
index_roots, caps/limits, sse_*); `KNOWLEDGE_ROUTER_URL` default for router_url; secrets by env var NAME
(`KNOWLEDGE_EMBED_API_KEY`, `KNOWLEDGE_QDRANT_API_KEY`); `KNOWLEDGE_ALLOW_UNAUTH_QDRANT=1`; `--allow-bind-any`.

## 9. Persisted state (must load as Python wrote it; same modes; atomic writes)

Data dir (0700, every subdir tightened to 0700 at start):

| Path | Format | Writer |
|---|---|---|
| `hub.db` (0600) + WAL | SQLite: `schema_migrations(name, applied_at)`, `audit` (append-only triggers), `api_keys` (sha256 hex of the secret, prefix = first 8 chars, scopes JSON), `settings(key, value JSON)` — DDL in `services/store.rs` | CORE |
| `bootstrap-admin.key` (0600) | `hc_<token_urlsafe(32)>\n` | CORE |
| `attest.key` (0600, O_EXCL) | 64 hex chars (HMAC key for MCP attestations) | B1 |
| `plans/plan_<12 hex>.json` (0600) | pydantic `model_dump_json()` of Plan (compact, pydantic floats), newest `plan_retention` kept | B2 |
| `locks/<client>.json` (0600) | `model_dump_json(indent=2)+"\n"` of Lock | B2 |
| `applies/<client>/<%Y%m%dT%H%M%SZ-plan_id>.json` (0600) | `json.dumps(rec, indent=1)` (insertion order), newest 200 | B2 |
| `backups/<client>/<%Y%m%dT%H%M%S-xxxx>/<root>/<path>` (0600), rollback: `...-rollback-xxxx` | the pre-apply bytes | B2 |
| `snapshots/<tree id>/...` | HEAD export (newest 5) | B2 |
| `skilleval/` (`.lock` flock, `.work/<run>`, `<skill>/<run id>/{run.json, static/, skillevaluator-tier1.json|md, skillevaluator-tier2.json, skillspector.json}`, `@by-hash/<sha>.json`, `@observed/<client>/...`) | per skilleval.py | C |
| `observed/<client>/{index.json, skills/<key>/...}` (0700/0600) | per observe.py | C |
| `hub.log`, `hub.pid`, `hub.lock` (0600, O_NOFOLLOW) | desktop launcher | CORE |

Content repo (default `<data>/content`, its own git repo, 0700): `hub.yaml`, `endpoints.yaml`,
`skills/<name>/{SKILL.md, hub.yaml, files...}`, `agents|instructions|memory/<id>.md` (frontmatter + body),
`mcp|rules|collections|clients|profiles/<id>.yaml`, `inbox/<client>/<id>.md` (never staged by commit),
`<dir>/.gitkeep` seeds. Files 0644 via `atomic_write` (`.hub-tmp-<12 hex>` + fsync + rename + dir fsync).
Knowledge data dir (0700): `knowledge.db` (0600; namespaces, tokens, usage, indexes), `admin.key` (`akn_admin_...`),
`audit.jsonl` (0600, append), `lance/ns_<namespace>.lance` (Lance v2 manifests).

## 10. Emitted file formats (byte-identical to Python for the same content repo)

Serializers (all in hub-core; adapters call them through `_util.rs`):

| Name | Python | Rust |
|---|---|---|
| `dump_json` | `json.dumps(doc, indent=2, sort_keys=True, ensure_ascii=False) + "\n"` | `py::json::dumps(&v, &Opts::adapter()) + "\n"` |
| merge JSON | `json.dumps(doc, indent=2, ensure_ascii=False) + "\n"` (live key order kept) | `Opts { indent: Some(2), ensure_ascii: false, .. }` |
| `dump_yaml` | `yaml.safe_dump(doc, sort_keys=True, allow_unicode=True, default_flow_style=False, width=100000)` | `yaml::dump(&v, &DumpOpts::ADAPTER)` |
| `render_frontmatter` | `"---\n" + safe_dump(fields, sort_keys=False, …, width=100000) + "---\n\n" + body.rstrip() + "\n"` | `DumpOpts::FRONTMATTER` |
| merge YAML | `safe_dump(doc, sort_keys=False, …, width=100000)` | `DumpOpts::FRONTMATTER` |
| content YAML (`yamlsafe.dump`) | `safe_dump(sort_keys=False, allow_unicode=True, default_flow_style=False, width=100)` | `DumpOpts::CONTENT` |
| `join_frontmatter` | `"---\n" + yamlsafe.dump(meta) + "---\n" + body` | `yamlsafe::join_frontmatter` |
| hub blocks | `<!-- agent-hub:begin (managed block; edits inside are overwritten) -->` / `<!-- agent-hub:end -->`, `# agent-hub:begin ...` / `# agent-hub:end` | WS-A/WS-B2 |
| TOML (generic) | `[<key_path>.<name>]` + `k = <_toml_val>` lines inside a hash block | WS-A |

Per adapter (all paths relative to the named root; merge mode in brackets):

| Adapter (id) | Files |
|---|---|
| claude-code | `.claude/skills/<name>/**` [own, verbatim bytes], `.claude/agents/<name>.md` [own, frontmatter], `CLAUDE.md` [block, or own with `instructions_mode: own`], `.claude/settings.json` [json_keys: permissions…, only with manage.permissions], `.mcp.json` [json_keys `mcpServers`] |
| opencode | `.claude/skills/**` [own] (or its own skills dir per params), `.opencode/agent/<name>.md` [own], `AGENTS.md` [block/own], `opencode.json` [json_keys] |
| hermes-agent | stage: `skills/<name>/**`, `AGENTS.md` [own], `config.mcp.fragment.yaml` [yaml_keys `mcp_servers`], `config.policy.fragment.yaml` [yaml_keys `agent.disabled_toolsets`, `approvals.deny`] |
| turnstone | stage: `skills/<name>/**`, `skills-meta.json` [own, dump_json], `instructions.md` [own], `mcp.json` [own] |
| generic | everything per spec: skills dir/filename, agents (md frontmatter / json / yaml), instructions (own/block), mcp (json/yaml keys or TOML block), permissions (json/yaml keys), memory |

Golden files: `backend/tests/adapters/golden/*.json` (Python-generated full renders) — WS-A ports them as Rust tests
that compare bytes; the parity harness compares the files an apply writes (§14).

## 11. Subprocess argv (identical argv, env, timeouts, output handling)

| # | Where | argv | env / limits |
|---|---|---|---|
| G-1 | gitrepo.run (every git call) | `[git_bin, -c core.hooksPath=/dev/null, -c commit.gpgsign=false, -c core.sshCommand=/bin/false, -c protocol.allow=never, -c diff.external=, -c core.quotepath=off, -c core.fsmonitor=false, -c core.autocrlf=false, -c core.attributesFile=/dev/null, -c safe.directory=<content_dir>, *args]`, cwd = content dir | env exactly: PATH, HOME=<content_dir>, GIT_CONFIG_GLOBAL=/dev/null, GIT_CONFIG_SYSTEM=/dev/null, GIT_CONFIG_NOSYSTEM=1, GIT_ATTR_NOSYSTEM=1, GIT_TERMINAL_PROMPT=0, GIT_LITERAL_PATHSPECS=1, GIT_OPTIONAL_LOCKS=0, LC_ALL=C, GIT_AUTHOR_NAME/EMAIL, GIT_COMMITTER_NAME/EMAIL; umask 077; timeout git_timeout_s; FileNotFoundError → 503 git_missing; timeout → 503 git_timeout; rc≠0 with check → 409 git_error "git <arg0> failed: <stderr[:300]>" |
| G-2 | args used | `init -q -b main`; `rev-parse --git-dir`; `rev-parse --verify -q HEAD`; `rev-parse HEAD^{tree}`; `rev-parse HEAD`; `status --porcelain=v1 -z --untracked-files=all`; `diff HEAD --no-color --no-ext-diff --no-textconv -- <path>`; `ls-files --error-unmatch -- <path>`; `add -A -- <known dir>`; `diff --cached --quiet`; `commit -q --no-verify -m <msg>`; `restore --source=HEAD --staged --worktree -- <paths>`; `clean -fdq -- <paths>`; `cat-file -e HEAD:<path>`; `log -n<N> --format=%H%x1f%an%x1f%at%x1f%s`; `cat-file -e <commit>^{commit}`; `revert --no-edit <commit>`; `revert --abort`; `ls-tree -r -z --full-tree HEAD`; `fsck --strict` (migrate) | |
| G-3 | gitrepo.export_head | `[git_bin, -c core.hooksPath=/dev/null, -c core.attributesFile=/dev/null, -c core.fsmonitor=false, -c core.sshCommand=/bin/false, -c protocol.allow=never, cat-file, --batch]`, stdin = `<sha>\n`… | same env; timeout 2×; output cap 64 MiB |
| E-1 | skilleval version probe | `[<launcher path>, --version]`, cwd `<state>/skilleval` | PATH(+~/.local/bin), HOME, LC_ALL=C + `SKILL_EVAL_*`/`SKILLEVAL_*`; 20 s; cached 300 s |
| E-2 | static pass | `[exe, tier1, <copy>, --no-llm, --checks schema,version,security,pii,license,code-integrity,unicode,quality,lint, -r json,markdown, -o <run>/static]` | 540 s per tier; own process group (killed on timeout/cancel); stdout/stderr tails 64 KiB |
| E-3 | LLM pass (tier1, tier1+2) | same without `--no-llm`, `-o <run>` | |
| E-4 | tier2 | `[exe, tier2, <copy>, -r, json, -o, <run>]` | |
| E-5 | SkillSpector re-check | `[<skillspector on PATH>, scan, <copy>, --format, json, --no-llm, --output, <work>/skillspector.json]` | 300 s |
| H-1 | hermes verify/observe | `[docker, (--context <ctx>), ps, --format, {{.Names}}, --filter, label=com.docker.compose.project=<p>, --filter, label=com.docker.compose.service=<s>]` | runner env PATH/HOME/LC_ALL=C; 15 s |
| H-2 | hermes verify | `[docker, (--context ..), exec, (--user <u>), <ctr>, <hermes_bin>, skills, list]` | 60 s |
| H-3 | hermes observe | `[docker, (--context ..), exec, (--user <u>), <ctr>, <python>, -c, <_OBSERVE_PROGRAM constant>, <root>]` | StreamingRunner: 120 s, stdout cap 48 MiB, stderr 1 MiB, group-killed over a cap (rc 125) |
| D-1 | deepseek-harness verify | `[docker, (--context <ctx>), exec, <container>, node, /opt/lab/list-skills.mjs]`; container and context fullmatch `[A-Za-z0-9][A-Za-z0-9_.-]{0,127}` or nothing runs (A-15) | 60 s |
| T-1 | turnstone verify | `[python3, <app root>/deploy/turnstone-deliver.py, list, --console, <url>, --token-file, <path>]` (Python: `sys.executable`; D-TURNSTONE) | 30 s |
| L-1 | desktop open | spawn `[<agent-hub binary>]` (D-LAUNCH), stdin /dev/null, stdout+stderr → hub.log, new session; `xdg-open <url>`; `notify-send -- "Agent Hub" <html-escaped msg>` (5 s) | |

RealRunner: argv only (NUL → rc 127 "invalid argv"), env PATH/HOME/LC_ALL=C, timeout ≤ 60 s (rc 124 "timed out"),
missing binary rc 127 "command not found", stdout 100 000 chars, stderr 20 000.

## 12. YAML/TOML byte-fidelity plan (WS-Y, R-YAML)

1. Port PyYAML 6.0.3's pure-Python SafeLoader path (reader, scanner, parser, composer, SafeConstructor, Resolver incl.
   the YAML 1.1 implicit regexes) and SafeDumper path (representer, serializer, emitter: `analyze_scalar`, style choice,
   `write_plain/single_quoted/double_quoted/folded/literal`, line folding at `best_width`, indentless sequences,
   `allow_unicode`, `sort_keys`) into `hub-core/src/yaml/` behind the locked API; error messages = PyYAML's
   (`problem` = first line; `full` with marks).
2. Golden corpus generated by Python: `rust/parity/gen/` gets a `gen_yaml_golden.py` producing
   `hub-core/tests/golden/yaml_{load,dump}.json` (inputs: every content fixture of the backend tests, the policy floor,
   adapter golden documents, hostile inputs (aliases, anchors, dup keys, tags, deep nesting, huge docs), and a generated
   matrix of scalars: every YAML 1.1 implicit type, strings needing quotes, unicode, control chars, long lines at widths
   100 and 100000, multi-line strings, keys of every type). Rust tests compare bytes (dump) and values+errors (load).
3. proptest round-trip: `load(dump(v)) == v` for generated PyValues.
4. TOML: the generic adapter's writer is ported by WS-A with golden output; `toml` crate parsing for the planner check.
5. Acceptance: `yaml-rust2` removed from Cargo.toml (addendum), all golden cases equal, parity harness shows no
   content-file or client-file byte differences.

## 13. Metrics

Hub (`GET /metrics`, admin): `agent_hub_http_requests_total{method,route,status}` counter,
`agent_hub_http_request_seconds{method,route}` histogram (buckets .005…10), `agent_hub_applies_total{status}`,
`agent_hub_plans_total`, each with its `_created` gauge — implemented (`telemetry.rs`; apply/plan counters are
incremented by B2). Knowledge (`GET /metrics`, admin): `agent_knowledge_requests_total{method,route,status}`,
`agent_knowledge_request_seconds{method,route}`, `agent_knowledge_queries_total{ns}`,
`agent_knowledge_chunks_ingested_total{ns}`, `agent_knowledge_jobs_total{state}`, `agent_knowledge_denied_total{reason}`,
content type `text/plain; version=0.0.4` (WS-D).

## 14. Security controls (Python → Rust → test)

| Id | Control | Python | Rust | Test |
|---|---|---|---|---|
| S-HOST | Host allowlist (DNS rebinding) | api/security.py:97 | api/security.rs `precheck` | api_security.rs test_host_allowlist_421, test_host_allowlist_follows_the_effective_port, test_allowed_hosts_setting |
| S-ORIGIN | same-origin Origin | api/security.py:97 | api/security.rs `precheck`, `netloc` | test_cross_origin_and_null_origin_403 |
| S-CSRF | X-Agent-Hub on non-GET without bearer | api/security.py:97 | api/security.rs `precheck` | test_csrf_header_required_without_bearer |
| S-AUTH | bearer keys, sha256 + constant-time compare, loopback trust off/never via proxy headers | api/security.py:78, services/auth.py:77 | api/security.rs `principal`, services/auth.rs `authenticate` | test_loopback_trust_..., test_keys_are_hashed_and_secret_shown_once, test_no_key_is_401_on_every_route_but_health, test_unauthenticated_and_bad_bearer_cannot_mutate_anything |
| S-ROLES | viewer/client gates + enforce_roles + require_admin | api/security.py:46, api/deps.py:31/42 | api/security.rs `viewer_may`/`client_may`, api/deps.rs | test_route_enumeration_every_mutating_route_requires_admin, test_client_token_cannot_reach_anything_but_inbox, test_viewer_cannot_read_keys_audit_settings |
| S-BODYCAP | Content-Length + streaming caps | api/security.py:97/150 | api/security.rs `precheck`, `read_body` | test_body_caps |
| S-HEADERS | CSP & co on every response, no CORS, no-store on /api | api/security.py:35 | api/security.rs `finish_headers` | test_security_headers_..., test_no_cors |
| S-AUDIT | append-only audit, redaction, metadata-only private bodies | api/security.py:191, services/store.py:15, services/common.py:35/40 | api/security.rs `audit`, store.rs, hub-core common.rs | test_audit_is_append_only_and_redacted, common.rs scrub_and_redact |
| S-ERRORS | problem+json, never echo input | api/errors.py | api/errors.rs, api/deps.rs | test_errors_are_problem_json_and_never_echo_input, golden_bodies |
| S-STATIC | static allowlist, no escape, revalidation | main.py:74/92 | api/static_files.rs | test_static_allowlist_and_spa_fallback, test_static_symlink_escape, test_static_files_are_revalidated_on_every_load |
| S-BIND | loopback-only bind | config.py:69 | config.rs `check_bind` | config.rs test_check_bind |
| S-SECRETFILES | 0600 key files, 0700 dirs, harden_tree, O_NOFOLLOW | services/common.py:55/96/110, services/auth.py:85 | hub-core common.rs, auth.rs | test_bootstrap_admin_key_is_0600_and_works, common.rs atomic_write_modes |
| S-PATH | relpath rules, resolve_under, expand_root | domain/pathsafe.py:18/35/57 | hub-core pathsafe.rs | hub-core/tests/pathsafe.rs (11 tests + proptest) |
| S-YAML | safe loader, no anchors/aliases/dup keys, caps | domain/yamlsafe.py:16/35/50 | hub-core yamlsafe.rs, yaml/ | yamlsafe.rs caps_and_frontmatter, compat.rs strict_refuses_... (WS-Y: test_yamlsafe.py) |
| S-FLOOR | policy floor (fail closed, overlaps, probes, MCP rules, skills frontmatter) | services/floor.py:45/69/108/149/170/218/241/270 | services/floor.rs (`load_floor`, `norm_path`, `overlaps` implemented) | WS-F: test_floor.py, test_floor_hardening.py, test_audit_round2.py |
| S-TARGETS | delivery targets, sensitive files | services/targets.py:45/59/72 | services/targets.rs (stub refuses everything) | WS-F: test_targets.py, test_audit_round2.py |
| S-CONTENTGUARD | content dir location/owner/mode, git metadata allowlist, sanitize | services/contentguard.py:28/52/85/118 | services/contentguard.rs | WS-F: test_content_guard.py |
| S-GIT | git argv/env hardening, literal pathspecs, known paths only, object-db export | services/gitrepo.py:54/64/163/221/275 | services/gitrepo.rs | WS-B2: test_git.py, test_ops_hardening.py |
| S-SAFEFS | dir-fd O_NOFOLLOW ops, compare-and-swap commit, rollback | services/safefs.py:14/38, services/applier.py:156/177 | services/safefs.rs, applier.rs | WS-B2: test_toctou.py, test_apply.py |
| S-FS / S-RUNNER | allowed roots, invisible secrets, runners (no shell, timeouts, caps, group kill) | services/fs.py:23/30/72/94/188 | services/fs.rs | WS-B2: test_discovery_fs.py, test_audit_fixes streaming tests |
| S-ATTEST | MCP scan attestation HMAC, key 0600 | services/attest.py:27/48 | services/attest.rs | WS-B1: test_mcp_gate.py, test_audit_round2 attestation tests |
| S-IMPORT | secret-looking files refused, evaluation gate, override audit | services/importer.py:46/361, services/hub.py:139 | importer.rs, hub/content_ops.rs | WS-B1: test_audit_fixes.py, test_content.py |
| S-INBOX | per-client rate/cap, client token scope | services/inbox.py:30, api/routers/content.py | inbox.rs, api/routes/content.rs | WS-B1 + test_client_token_scopes |
| S-KPROXY | knowledge proxy allowlist, key file checks, 502 for upstream 401/403 | services/knowledge.py:23/39/113 | services/knowledge.rs | WS-C: test_api.py knowledge tests |
| S-EVAL-ARGV | SAFE_CHECKS on every tier1 argv, launcher bare names, env passthrough only SKILL_EVAL_* | services/skilleval.py:1341/661/672 | services/skilleval.rs | WS-C: test_skilleval.py |
| S-EVAL-COPY | bytecode + sidecar excluded from copies/imports (c419f1e), caps, flock | services/skilleval.py:296/507 | services/skilleval.rs | WS-C: test_skilleval.py, test_observed_eval.py |
| S-OBSERVE | observation stored 0700/0600, untrusted names/paths skipped, incomplete keeps previous | services/observe.py:97, adapters/hermes.py:310 | observe.rs, adapters/hermes.rs | WS-C/WS-A: test_hermes_observe.py, test_audit_fixes.py |
| S-ADAPTER-PATHS | safe_relpath, marker injection, capped reads | adapters/_util.py:49/176/380 | adapters/_util.rs | WS-A: test_util.py, test_markers.py, test_hardening.py |
| S-CLI-AUTH | hubctl needs an admin key; retire-bootstrap-key TTY-only, verify first | cli.py:179/196 | cli.rs | WS-C: test_cli_registry.py |
| S-LAUNCH | loopback-only launcher, owner checks, never signal a stranger | desktop.py:55/133/185 | desktop.rs | desktop.rs tests (CORE pending: launcher flows) |
| S-K-* | knowledge Host/path/CSRF/caps, auth+scope before validation, path-ingest roots, SQL-safe ids in Lance filters, master-key warnings, non-loopback refusal | knowledge security.py:49/115, api.py:51/82/103, paths.py:22/45, stores/lance.py:24/84, app.py:52, main.py:20 | agent-knowledge/** | WS-D |

## 15. Knowledge: LanceDB verdict

- Python `lancedb 0.39.0` (knowledge/uv.lock) bundles Lance **12.0.0** (strings in its `_lancedb.abi3.so`).
- Rust crate `lancedb 0.39.0` (→ lance 12.0.0) **does not compile** with `default-features = false`:
  `src/job.rs:112` uses `Error::Http`, which only exists with feature `remote` (reqwest/tonic/oauth2 stack). Not used.
- The `lance` crate **=12.0.0** with `default-features = false` (no aws/azure/gcp/huggingface; no openssl/native-tls in
  the tree) **builds** here with `PROTOC` pointing at `protoc-bin-vendored 3.3.0`'s binary (libprotoc 36.2) via
  `rust/tools/protoc`; cold build ≈ 3.5 min at 8 jobs.
- Compatibility proven both ways (2026-10-27, scratch probe): Rust read a table written by the Python LanceStore
  (ensure, 2 upserts, delete: count 4, scan of id/text/meta_json, cosine kNN with Python's ordering and distances,
  prefiltered kNN); Rust then merge-inserted, deleted and created a new table, and the Python LanceStore read all of it
  back (query, stats, doc_id filter). Kept as a test: `agent-knowledge/tests/lance_compat.rs` on the Python-written
  fixture `tests/fixtures/lance-py-lancedb-0.39.0` (generator `rust/parity/gen/mklance.py`).
- WS-D implements LanceStore on `lance::Dataset` (open/write/scan/nearest+distance_metric(Cosine)/prefilter+filter/
  MergeInsertBuilder/delete/count_rows; drop = remove the `.lance` dir like lancedb's local drop).
- Qdrant: plain REST over reqwest (rustls). Embeddings: OpenAI-compatible HTTP (`router_url`), key from the env var
  named in the config.

## 16. Out-of-repo consumers and how the cutover keeps them working

| Consumer | Uses | Plan |
|---|---|---|
| `deploy/agent-hub.service` (template) | `ExecStart=<checkout>/backend/.venv/bin/agent-hub`, HUB_* env, EnvironmentFile ~/.config/agent-hub/env | cutover: point ExecStart at the Rust `agent-hub`; same env/env file (D-APPROOT if the binary is moved) |
| `deploy/agent-knowledge.service` | `.venv/bin/agent-knowledge --config ~/.config/agent-knowledge/config.yaml` | same flags in Rust |
| `deploy/agent-hub.desktop` + `make install-desktop` | `@BIN@ open` / `@BIN@ stop` | same subcommands (`agent-hub open|stop`) |
| `Makefile` (run, test, e2e), `.githooks/pre-push`, `scripts/e2e-gate.sh`, CI | Python venvs | cutover task (not phase 1); e2e has `E2E_BACKEND=rust` now |
| `deploy/turnstone-deliver.py` (stdlib Python, run by hand; called by the turnstone adapter's verify) | no agent_hub import | stays Python; Rust adapter runs it with `python3` (D-TURNSTONE) |
| `deploy/hermes-deliver.sh`, `deploy/dsh-deliver.sh` (bash, run by hand; never run by the hub) | the stage dirs the hub writes | unchanged (byte-identical stages); `tests/test_hermes.rs` and `tests/test_dsh_deliver.rs` run them with a fake `docker` (A-15) |
| `deploy/knowledge/Dockerfile` + compose files | builds the PYTHON knowledge service image | cutover: new Dockerfile for the Rust binary (WS-D documents; not phase 1) |
| the maintainer's own scripts and clients outside this repo (a drift check, a health check, a skill evaluator, a Turnstone setup script, an agent sandbox's settings) | read the content layout (`content/skills`), write `~/.config/agent-hub/turnstone.token` (0600), deny the hub and knowledge state dirs | unchanged; a hubctl path printed as advice changes at cutover (edited outside this repo) |
None of them imports `agent_hub`/`agent_knowledge` or runs `hubctl` (checked 2026-10-27).

## 17. Parity harness (`rust/parity/`)

`node rust/parity/run.mjs [--app hub|knowledge] [--a python] [--b rust] [--only <regex>] [--json f] [--dump f]`.
Each side runs ALONE in the same temp layout (`$TMPDIR/agent-hub-parity-*`: `home/`, `home/project*`, `data/`) on the
same port, seeded by the PYTHON `hubctl init`, with PATH = `parity/fake-bin` (fake skillevaluator, skillspector, docker,
xdg-open, notify-send) + /usr/bin:/bin (real git), a fake knowledge upstream (`lib/fake-knowledge.mjs`), and
`AGENT_HUB_ENV_FILE` pointing nowhere. Corpus: `corpus/hub.mjs` (248 cases, all 98 routes: valid, invalid, auth
failures and the create → commit → plan → apply → verify → rollback → discard → revert scenario, inbox, import/observe,
evaluations, knowledge proxy incl. SSE, static/ETag), `corpus/knowledge.mjs` (43 cases, all 20 routes). Compared:
status, the security/caching headers, normalised JSON bodies, metrics text, and FILE SNAPSHOTS after state-changing
cases (client trees under HOME and the content repo byte-for-byte; locks/applies/plans as normalised JSON).
Normalisation rules N1–N9 are listed in `lib/normalize.mjs` (secrets, ids, clocks, git ids, ETag/dates, metric
latencies, openapi operationId, knowledge ids, Lance disk bytes and free disk space). Route coverage fails the run if any Python route has
no case. Exit 0 = only `not_ported` differences.

Phase-1 results (2026-10-27): hub python-vs-python 0 diffs / 0 uncovered; knowledge python-vs-python 0 / 0;
hub python-vs-rust (248 cases): 92 equal, 147 not ported (content_ops 84, delivery_ops 25, policy_ops 10, knowledge 9,
eval_ops 8, skilleval 7, gitrepo 4), 9 other differences, all from stubs (empty adapter registry in /settings and /adapters, knowledge summary
stub, knowledge SSE stub, audit EVENT rows and metrics statuses of unported routes). Every HTTP audit row of the
corpus is identical (actor, role, method, path, status, redacted params). Knowledge python-vs-rust (43 cases): 35 not ported,
8 differences (middleware not ported: Host 421, invalid-path 400, `Cache-Control: no-store`, metrics, SSE).

## 18. Deviations (deliberate; each needs a reason)

| Id | Deviation | Reason |
|---|---|---|
| D-APPROOT | `AGENT_HUB_APP_ROOT` env var (new) overrides the app root; default = the checkout the binary was built from | Python derives it from its source file location; a binary has none at run time |
| D-LAUNCH | the launcher spawns `[<agent-hub binary>]` instead of `[python, -I, -m, agent_hub.main]`; `stop` therefore refuses (never signals) a hub the Python launcher started | the process identity check compares argv |
| D-TURNSTONE | turnstone verify runs `python3 deploy/turnstone-deliver.py ...` (PATH lookup) instead of `sys.executable` | no Python interpreter of our own after cutover; the script is stdlib-only |
| D-BODY | the Rust pipeline reads a body route's body before routing-level checks only for body routes and the knowledge proxy (as FastAPI); a chunked body over the cap on the knowledge proxy is refused 413 even when its admin-only check would have answered 403 | streaming bodies are read before the proxy handler runs |
| D-REPR | `repr()` of a str treats code points unassigned in Unicode 15.1 as printable (no category tables in std) | only affects error text containing unassigned code points |
| D-YAML (temporary) | until WS-Y lands: YAML error texts and some emitter decisions differ from PyYAML; aliases in non-strict loads resolve to None | phase-1 engine on yaml-rust2 (D7) |
| D-CLI-USAGE | `hubctl`/`agent-knowledge` usage/help/parse-error TEXT comes from clap, not argparse (exit codes match: 2 for usage errors) | stdout formats of commands are compared; argparse's help layout is not an API |
| D-HEADERS | no `server: uvicorn` / `date` difference is compared; Rust sends `date` only | not part of any contract |
| D-RANGE (pending) | Range requests on static files are not implemented yet (Starlette answers 206) | the UI never sends Range; CORE follow-up |
| D-NOTPORTED | 501 `not_ported` problem (D17) | phase-1 only |

Python bugs found: none confirmed so far (record them here, keep Python behaviour unless security, tell the lead).

## 19. Risks

| Id | Risk | Mitigation |
|---|---|---|
| R-YAML | PyYAML emitter/scanner byte fidelity (quoting, folding, error text) | WS-Y full port + Python golden corpus (§12) |
| R-PYD | pydantic message/coercion fidelity for every model and type used | `pyd` golden test (all current cases pass); each owner adds golden cases for its models via gen_models_golden.py |
| R-DIFFLIB | `difflib.unified_diff` (SequenceMatcher autojunk heuristics) used in plan/changes diffs | WS-B2 ports SequenceMatcher exactly; golden diffs from Python |
| R-FLOATS | Python float repr vs pydantic float JSON in timestamps/plans | `py::float` (both formats, probe-tested) |
| R-FNMATCH | floor decisions depend on fnmatch semantics | exact port (`py::fnmatch`) + WS-F tests |
| R-OSERROR | messages embed `type(exc).__name__` / `str(exc)` of OSErrors | `py::os::{error_name,error_str}` |
| R-UNICODE | `str.isspace/splitlines/lower/isalnum` differences | `py::text` helpers; use them, not Rust's |
| R-LANCE-ADVISORIES | zstd 0.13 / zstd-safe 7.3 advisories (RUSTSEC-2026-0391/0398/0403 + unsound 0396..0406) pinned by arrow-ipc 58 / lance 12; `paste` unmaintained | ignored with reasons in deny.toml (review 2027-01-31): local, self-written Lance files only; revisit with lance 13 |
| R-OPENAPI | FastAPI's OpenAPI document served from a captured snapshot | regenerate with dump_openapi.py whenever routes/models change |
| R-CONCURRENCY | sync-in-threadpool + RLock semantics | spawn_blocking + HubLock; WS tests for concurrent applies/evaluations |
| R-TIME | `time.strftime`/`gmtime` formats in ids | use `time` crate with the exact formats listed in §9 |

## 20. Not verified in phase 1

- e2e on the Rust backend: the switch exists and the default (Python) passes; `E2E_BACKEND=rust` stops at the harness's
  `hubctl init` ("not_ported: not ported: cli", WS-C) and `GET /api/v1/health` is 501 until gitrepo (WS-B2) lands.
- `hubctl` commands (parser only), desktop `open`/`stop` flows end to end, Range requests, knowledge middleware.
- YAML emitter bytes beyond the shapes in compat.rs tests (WS-Y).

## Addenda

### A-1 (2026-10-28, CORE): adapter constructor errors
`Ctor`'s `Err(msg)` (adapters/mod.rs) is `"<ExceptionName>: <message>"`, the text Python records as
`f"{type(exc).__name__}: {str(exc)[:200]}"`; the registry truncates `<message>` to 200 characters. A constructor that
cannot build its adapter returns e.g. `Some(Err("TypeError: ADAPTER is not a ClientAdapter".into()))`. `discover()` also
skips `base` and `_*` names itself, like `discover_package()`.

### A-2 (2026-10-28, CORE): static Range requests are ported; TEST-MAP is edited row by row
- D-RANGE is resolved: `static_files::file_response` answers `Range` like Starlette 1.7's FileResponse (one range →
  206 + `Content-Range`; several → merged, 206 `multipart/byteranges` with a random 26-hex boundary; malformed → 400
  text/plain; start past the end → 416 + `Content-Range: bytes */<size>`; a stale `If-Range` or more than 100 ranges →
  the whole file; `If-None-Match` is still checked first). Parser golden cases from Starlette:
  `parity/gen/gen_range_golden.py` → `crates/agent-hub/tests/golden/ranges.json` (tests/static_ranges.rs).
  `hub_core::py::text::int_str` is Python's `int()` (Unicode decimal digits, `_`, sign; saturating at i128).
- Parity: 11 `static.range-*` cases; `content-range` is a compared header; rule N10 normalises the multipart boundary.
- TEST-MAP.md is no longer regenerated: rows are edited in place (`parity/gen/set_test_map_rows.py <mapping.json>` for
  the lead; workstreams edit their rows by hand). The per-owner table at the top is refreshed only at integration;
  `node parity/check-test-map.mjs` prints the live counts.

### A-3 (2026-10-28, CORE): desktop launcher seam, launcher deviations, state-dir and bind tests
- `desktop.rs` routes everything external through `trait Launcher` (check, start_background, open_browser, notify,
  pid_cmdline, terminate, server_argv, say, poll/start/stop timeouts) with `System` as the real one;
  `open_hub_with` / `stop_hub_with` / `run_with` take a launcher (and `run_with` a settings loader). `HubProcess` stands
  for `subprocess.Popen` (`pid`, `poll`, `stop` = `_stop_child`). `start_background_argv(cfg, argv, rotate_bytes)` is
  the testable form of `start_background`. `check` / `listen_uids` return `Result` so Python's ValueError text
  (`invalid literal for int() with base 16: 'zz'`) is reported, not swallowed; `int(x, 16)` keeps Python's `0x` prefix
  and `_` rules. `hub_url` follows Python 3.13's `ipaddress` (`::ffff:127.0.0.1` and `::1%lo` are loopback; one `[`/`]`
  pair is removed, not all).
- `probe` reads HTTP the way httpx + h11 do (answers recorded from Python's own `probe()` in tests/test_desktop.rs):
  status line and header syntax errors, a closed or silent connection, an incomplete Content-Length or chunked body,
  any Transfer-Encoding but `chunked` → down; 1xx (not 101) skipped; the status code is ignored; 204/304 → foreign; a
  response head still incomplete with more than 100 KiB buffered (httpcore's `MAX_INCOMPLETE_EVENT_SIZE`, read in 64 KiB
  steps) → down.
- New deviations:
  - **D-LAUNCH (extended)**: the hub gets its own process group, not its own session (`setsid()` in the child needs
    `pre_exec`, i.e. unsafe code). Its stdin is /dev/null and its output goes to hub.log, so it outlives the terminal
    that ran `open` either way.
  - **D-PROBE**: `Accept-Encoding: identity` is sent (httpx offers gzip/deflate); a hub-shaped body in UTF-16/32 or with
    NaN/Infinity literals is `foreign` (Python: `hub`).
  - **D-PIDFILE**: `hub.pid` holding a non-decimal "digit" (`str.isdigit()` but not a decimal, e.g. `²`) is reported
    as "does not hold a pid" (Python: `invalid literal for int()`); a `/proc/net/tcp` row without `:` in its address is
    reported as "list index out of range" (Python: an uncaught IndexError traceback).
- **Python bug P-1 (kept out of Rust, reported to the lead)**: Python's probe applies its 2 s total deadline only
  between body chunks; while the response head arrives only the 2 s per-read timeout applies, so a local process that
  squats the port and drips its head (up to 100 KiB, one byte every < 2 s) can hold `agent-hub open` for about two days
  (102,400 reads of up to 2 s each). Rust applies
  the deadline to the whole exchange (`foreign` when it passes during the head).
- Tests that wait for other workstreams (remove the ignore when the dependency is merged):
  `test_ops_hardening.rs::test_serve_and_main_refuse_a_public_bind_hubctl_serve` (WS-C `hubctl serve`),
  `::test_state_directories_are_private` (WS-B2 init/commit/plan/apply, WS-B1 put_client/content store),
  `::test_viewers_cannot_read_credential_listing_knowledge_paths` (WS-C proxy `forward`). Its router half (viewer 403
  before anything is forwarded) already runs as `viewers_get_403_on_credential_listing_knowledge_paths`.
- `Settings` has a hand-written `Debug` that leaves out `knowledge_token` and `knowledge_admin_key` (pydantic
  `repr=False`); the derived one printed both.

### A-4 (2026-10-28, CORE): injectable runners
- `HubOptions` gains `runner` and `observe_runner: Option<Arc<dyn CommandRunner>>` (Python's `Hub(runner=,
  observe_runner=)`); `None` = the real `RealRunner` / `StreamingRunner::new(120.0, 48 MiB, 1 MiB)`. The `Hub` fields
  `runner` and `observe_runner` are now `Arc<dyn CommandRunner>` (call `.run(argv, timeout)` through the trait).
  Build `HubOptions` with `..HubOptions::default()`.
- **WS-B2 change (applied by WS-B2 after `git merge rust-backend`)**: `services/applier.rs`
  `Applier::new(planner: Arc<Planner>, locks: Arc<LockStore>, backups_dir: &Path, runner: Arc<dyn CommandRunner>,
  log: Arc<ApplyLog>) -> Self`, and the field `pub runner: Arc<dyn CommandRunner>` (was `Arc<RealRunner>` in both).
  Until then `Hub::new` hands the applier its own `RealRunner`; at integration the lead changes that one argument in
  `services/hub/mod.rs` to `runner.clone()` (marked `A-4` there).

### A-5 (2026-10-28, integration of WS-C at 13427a0)
- WS-C is merged: skilleval, observe, the knowledge proxy client, `hub/eval_ops.rs` and `hubctl` (cli.rs). Its tests
  share `tests/wsc_support/mod.rs`; 38 of them wait for WS-B1 / WS-B2 / WS-F (`pending WS-` ignores).
- `api/errors.rs raw()` appends `; charset=utf-8` to a `text/*` media type without a charset, as Starlette's
  `init_headers` does (the knowledge proxy passes text bodies through it).
- nix's `term` feature is on: `hubctl`'s hidden key prompt switches echo off in-process (termios `TCSAFLUSH`, restored
  afterwards) like `getpass.unix_getpass`, instead of running `stty`.
- `services::hub::default_observe_runner()` builds the observe `StreamingRunner` (120 s, 48 MiB); tests check its caps
  there because the hub's runner fields are trait objects since A-4.
- Deviations:
  - **D-SSE-ERRORS**: the knowledge stream proxy checks the upstream before the response starts, so failures come back
    as problem responses; Python answers 200 and then aborts the stream.
  - **D-CLI-ITEM**: `hubctl import --item x` (no `:`) prints an error and exits 1; Python dies with a traceback
    (also exit 1).
  - **D-EVAL-JSON**: a SkillEvaluator/SkillSpector report with NaN, Infinity or nesting deeper than 128 levels cannot be
    read, so the verdict is `error` (Python's json module accepts them).
  - **D-KHEALTH-CAP**: the knowledge health summary reads the `backends` body up to `knowledge_response_cap_bytes`;
    Python reads it uncapped.
  - **D-SURROGATES**: names that are not valid UTF-8 (Python would carry them as lone surrogates) are skipped.
- **Python bug P-2 (kept)**: in skilleval's run bookkeeping, when writing `run.json` fails, the `finally` block stops at
  that failure and skips the cleanup after it. The Rust port keeps Python's order and behaviour.

### A-6 (2026-10-28, integration of WS-B2 at 5989e47)
- WS-B2 is merged: gitrepo, safefs, lock, applylog, merge, planner, applier, rollback, difflib, fs and
  `hub/delivery_ops.rs`. A-4 is wired: `Hub::new` passes the injected `runner` to `Applier::new`, whose signature is
  `Applier::new(planner, locks, backups_dir: &Path, runner: Arc<dyn CommandRunner>, log)`.
- **git's umask**: nix's `sched` feature is on. `gitrepo::with_private_umask(f)` runs `f` on a scoped thread that calls
  `unshare(CLONE_FS)` and then `umask(0o077)`; git is spawned there (every `run` and `export_head`), so its files are
  0600/0700 like Python's `subprocess.run(..., umask=0o077)` and no other thread's umask changes. Proven by
  `gitrepo::tests::git_children_get_umask_077_and_no_other_thread_changes` (child file 0600; caller's umask, read from
  /proc/thread-self/status, unchanged).
- **One approval helper**: `Hub::approved_client` (delivery_ops.rs) is the shared way to read a client's committed
  config; WS-B1 and WS-F call it instead of writing their own.
- **New public items** (WS-B2-owned, usable by other workstreams):
  planner `plan_resolved`; applier `apply_with`, `run_ops`, `ApplyHooks`, `Op`, `OpError`, `OpHook`, `OpView`,
  `prune_dirs`, `set_test_hooks`, `utc_stamp`; merge `load_doc`, `loads`, `dumps`, `Doc`, `DocError`, `Opts`, `canon`,
  `class`, `get_dotted`, `set_dotted`, `del_dotted`, `merge`; safefs `PyOsError`, `os_err`, `py_error`, `rmdir_at`,
  `unlink_at`; fs `capture`, `Captured`, `path_env`, `path_io_error`, `py_os_error`, `returncode`; difflib
  `SequenceMatcher` (`new`, `with_autojunk`, `find_longest_match`, `get_matching_blocks`, `get_opcodes`,
  `get_grouped_opcodes`), `Opcode`, `Tag`; gitrepo `GitRepo::env`, `GitRepo::is_known`, `with_private_umask`;
  applylog `dump_record`, `truthy`; hub `approved_client`.
- `merge::find_region` returns BYTE offsets into the text (Python: code-point offsets; both slice the same region).
- Deviations:
  - **D-B2-UMASK**: only when the kernel refuses `unshare(CLONE_FS)` (a seccomp sandbox) does git inherit the process
    umask; the process umask is never changed.
  - **D-B2-SESSION**: the streaming runner's children get their own process group (`process_group(0)`), not their own
    session (Python `start_new_session=True`), as in D-LAUNCH; group kill works the same.
  - **D-B2-FIFO**: `safefs::read_at` opens with `O_NONBLOCK`, so a FIFO swapped in at a target is refused at once.
    **Python bug P-3** (security, local DoS; Python unchanged): Python opens it blocking, so the apply hangs while it
    holds the hub lock and every mutating request waits behind it.
  - **D-B2-TOML**: the planner's TOML check parses with the `toml` crate (TOML 1.1) where Python uses `tomllib` (TOML
    1.0), until a port of CPython's tomllib is available (one is being written for a sibling project; it can be shared
    later).
  - **D-B2-JSON**: a lone `\uD800` escape in a live JSON file is a conflict (unparseable); Python returns a 500.
- Ignores: 5 waiting tests now pass and lost their ignore; 11 that were labelled WS-B2/WS-C now stop at
  `services.contentguard` (WS-F, called by `init_content`) and are relabelled `pending WS-F: ...`.

### A-7 (2026-10-28, integration of WS-Y at a214dd6)
- WS-Y is merged: `hub_core::yaml` is an exact port of PyYAML 6.0.3's SafeLoader and SafeDumper (reader, scanner,
  parser, composer, constructor, resolver, representer, serializer, emitter), checked against PyYAML on the golden
  corpus (`parity/gen/gen_yaml_golden.py`), property tests and a live-oracle run. The phase-1 engine
  (`yaml/compat.rs`) and the `yaml-rust2` dependency are gone (workspace and hub-core Cargo.toml, Cargo.lock).
  **D-YAML (temporary) is superseded** by the deviations below.
- API additions (locked API extended, nothing removed): `yaml::Mark` (`pub`) and `YamlError.problem_mark:
  Option<Mark>`; `YamlError::python_exception() -> Option<&'static str>` (the Python exception a non-YAML failure would
  be); `yamlsafe::LoadError` (`Yaml(YamlError)` | `Python { exc_type, message }`), `yamlsafe::load_exact` and
  `yamlsafe::split_frontmatter_exact`, which keep those Python exceptions apart.
- **Pending for WS-B1** (relayed by the lead): content loading and `validate-spec` use `load_exact` and answer
  `LoadError::Python` with a 500, as Python lets those exceptions escape. Applied at integration if B1 lands first.
- Deviations (replace D-YAML):
  - **D-YAML-PYEXC**: `yamlsafe::load` / `split_frontmatter` return Python's escaping exceptions (`ValueError` for
    `2024-02-30`, `KeyError` for `!!bool maybe`, `RecursionError`, ...) as `YamlError(str(exc))`; the `_exact` variants
    keep the exception type.
  - **D-YAML-RECURSION**: composing stops at Python's measured recursion depth for a direct call (492 levels safe,
    328 strict); Python called from deeper in a server stack fails a few levels earlier.
  - **D-YAML-RECURSIVE**: a container that contains itself (a recursive alias in a non-strict load) cannot be built as
    a PyValue tree and is refused.
  - **D-YAML-ALIAS-EXPANSION**: aliases are copied, not shared, so a non-strict load is refused beyond 1,000,000
    materialised values (Python shares the objects and loads it).
  - **D-YAML-TUPLE**: `!!omap` / `!!pairs` items (Python 2-tuples) are 2-item lists.
  - **D-YAML-SURROGATE**: a `\uD800`-style escape (a lone surrogate in Python) becomes U+FFFD.
  - **D-YAML-HUGEINT**: an int of more than 4300 digits raises Python's `ValueError` at load time instead of at
    first use.
  - Dumper notes: it never emits anchors/aliases (a PyValue tree has no shared objects); a `!!set` is written in the
    PyValue's order (Python's set order depends on string hashing and is not stable between runs anyway); any depth
    can be dumped (Python's representer would hit its recursion limit).
- `py::float` fix (CORE bug, found by WS-Y): the shortest digits now break exact ties to EVEN like CPython's dtoa and
  pydantic-core (`-1133092563735992.25` -> `...992.2`; it was `...992.3`). This affects every `repr`, `json.dumps` and
  `model_dump_json` float. Golden: 3000 values incl. 1900 exact ties, from `parity/gen/gen_float_golden.py`
  (`hub-core/tests/float_golden.rs`); the YAML dumper now calls `py::float::repr`.
- `py::pyunicode` (moved from `yaml/`, generated by `parity/gen/gen_unicode_tables.py` from CPython 3.13 / Unicode
  15.1): `py::text::is_printable` and `py::text::decimal` use its tables, so `repr()` is exact. **D-REPR is closed.**

### A-8 (2026-10-28, integration of WS-F at fc4db07)
- WS-F is merged: policy floor, delivery targets, content-dir guard, doctor and `hub/policy_ops.rs`. With WS-B2 and WS-C
  this makes `/health` report `content_initialised` and `hubctl init` get past the content guard.
- `Hub::migrate_content_as(dest, yes, uid)` (delivery_ops.rs, the seam WS-F asked for): `migrate_content` calls it
  with `getuid()`; tests pass another uid where Python monkeypatches `os.getuid` (foreign source -> 400).
- Ignores: every `pending` label now names only what is still missing. WS-F is dropped from all labels (101
  relabelled, mostly to `pending WS-B1`, since they stop at `services.content`), and 2 that pass were un-ignored
  (`test_git.rs::test_empty_repo_hash_and_nothing_to_commit`, `test_skilleval.rs::test_doctor_reports_the_evaluator_as_info`).
- Parity: `run.mjs` has an explicit allow-list of deliberate differences (`ALLOWED`, by case id: `headers` = status
  and body must still be equal; `all` = the whole case). Allowed cases are reported as "allowed deviations", never as
  equal. New CORE cases `core.body.deep-json-{100,200,5000}`.
- Deviations:
  - **D-500-HEADERS**: an unexpected error's 500 carries the six security headers and `Cache-Control: no-store` like
    every other response; Python's 500 comes from Starlette's outer `ServerErrorMiddleware`, outside the security
    middleware, so it has none. The body is identical (parity: headers-only allowance on `wsf.policy.*-500` and
    `core.body.deep-json-100`).
  - **D-JSON-DEPTH**: a request body nested more than 128 levels deep is refused by serde_json's recursion limit
    (422 `json_invalid` at `["body", <pos>]`); Python's `json` parses it (to about 9,998 levels) and the handler
    answers (a 500 for `/policy/check`). Parity cases `core.body.deep-json-200/5000` (allowed).
  - **D-UNICODE-CASE**: lowercasing (hostnames, git config keys, doctor text) uses Rust's `to_lowercase`, whose
    Unicode tables are newer than Python 3.13's 15.1: it differs from `str.lower()` on 55 code points, all letters
    added in Unicode 16 or later (e.g. U+1C89, U+A7CB, U+10D50). Measured over every code point.
  - **D-PLAIN-HEAD**: `contentguard::plain_head` returns None for a `.git/HEAD` (or ref file) that is not UTF-8, so
    `migrate-content` reports a HEAD mismatch; Python raises UnicodeDecodeError (an unexpected error).
- Open, for WS-A (relayed by the lead): `toml_tables` and `wrap_block` do not exist in WS-A's stubs yet.

### Python bugs found during the port (consolidated; Python is not changed by the port)
| Id | Severity | Bug | Rust |
|---|---|---|---|
| P-1 | low (local DoS) | desktop `probe`: the 2 s total deadline does not cover the response head, so a local process squatting the port can hold `agent-hub open` for about two days (A-3) | deadline over the whole exchange |
| P-2 | low | skilleval: when writing `run.json` fails, the `finally` block stops at that failure and skips the cleanup after it (A-5) | kept as Python |
| P-3 | medium (local DoS) | `safefs.read_at` opens a target blocking: a FIFO swapped in at a target hangs the apply while it holds the hub lock (A-6) | `O_NONBLOCK`, refused at once (D-B2-FIFO) |
| SB-1 | **high** | `sanitize_git` follows symlinks during `hubctl migrate-content --yes` (the step the doctor recommends for hostile repos): a symlinked `.git/hooks` makes it delete / rmtree files in the link's target; a symlinked `.git/config`, `.git/info/attributes` or `.gitattributes` gets its target overwritten | never writes or deletes through a symlink; removes the link and reports it |
| SB-2 | medium (secret disclosure) | `verify_git_metadata` reads a symlinked `.git/config` and echoes the key text (before `=`) of up to six lines of the link's target in the error, e.g. of a private key file | refuses a symlinked config without reading it |

### A-9 (2026-10-28, integration of WS-B1 at 5201976; crate cooldown gate)
- WS-B1 is merged: content store, attest, resolver, importer, organize, inbox, suggest and `hub/content_ops.rs`; content
  YAML goes through `yamlsafe::load_exact` / `split_frontmatter_exact` (A-7's pending item is done).
- **WS-B1 public additions**: the `*_x` convention: every locked `ContentStore` / service method that Python lets raise
  several exception types has an `*_x` twin returning `content::Exc` (`Problem` | `Validation` | `Other { name, msg }`,
  `XResult<T>`), e.g. `load_x`, `put_x`, `catalog_x`, `put_client_x`, `put_collection_x`, `put_profile_x`,
  `write_skill_files_x`, `swap_skill_files_x`, `validate_skill_files_x`, `seed_x`, `yaml_load_x`,
  `split_frontmatter_x`; resolver `resolve_x`, `to_mcp_x`, `block_reason_x`, `pathlib_str`; attest `digest_x`,
  `mcp_blob_x`, `valid_x`; importer `discover_x`, `do_import_x`, `import_observed_from`, `ObservedSource`; organize
  `*_x` twins and `cell_state`; inbox `add_x`, `list_x`, `promote_x`, `reject_x`; suggest `MIN_MEMBERS`,
  `MAX_MEMBERS`, `colour`, `title`; content helpers (`dump_model`, `dump_model_json`, `default_sidecar`,
  `sidecar_json`, `meta_*`, `dget`/`dset`/`dpop`, `rglob`, `st_mtime`, `universal_newlines`, `b64decode_strict`, ...);
  hub `client_status_x`, `item_summary`, `work_endpoints`.
- Behaviour notes: `Resolver::block_reason` (locked, returns `Option<String>`) reports a floor error as "not blocked";
  WS-B1's callers use `block_reason_x`, which propagates it like Python. `Hub::client_status` (locked, never fails)
  turns every error into status `error`; `client_status_x` does that only for `Unavailable` (no repo, git missing) and
  propagates other problems (an unsafe content dir), as Python does.
- CORE `pyd` (requested by WS-B1, golden-tested against pydantic 2.13.5 on YAML inputs in `golden/models.json`):
  `dump_json(exclude_none=true)` drops None-valued extras of `Extra::Allow` models too (WS-B1's workaround in
  `content::dump_model` is gone); a non-str key in model input is skipped by an `ignore` model and is `invalid_key`
  "Keys should be strings" for `forbid` / `allow` models, located at the key itself (`Loc::Index` / new
  `Loc::Int(i64)` for an int, `Loc::Key(str(key))` otherwise); a dict key's `loc` uses the same rule; `str(exc)` shows
  a location item containing `.` in backticks, as pydantic does.
- CLI: `hubctl rollback` without `--yes` exits 1 with `error: confirm_required: ...` through `cli::run_with` (the
  test called `cli::dispatch`, which returns the problem to its caller); `test_rollback_http_and_cli` now checks
  stdout/stderr like Python's.
- WS-C fixtures: five client roots that were the temp root (which holds the hub's state dir, so TargetGuard rightly
  refuses it) are `<tmp>/client-home` (`wsc_support::client_home`), as in Python's `home` fixture.
- `services/mod.rs`: the unused `np!` macro is gone.
- Deviations: **D-INBOX-CLIENT** (Python security bug **SB-3**, below): a `client` query that is not an id lists
  nothing; **D-500-HEADERS** (A-8) also covers `b1.skills.put-files-not-dict` (headers-only allowance).
- Pending WS-A: `resolver::generic_caps_for_spec` -> `adapters::generic::caps_for_spec` (applied with A-10).
- **Crate cooldown gate** (`tools/crate-age.py` + `tools/crate-age-exceptions.toml`, shared with a sibling project,
  stdlib Python, queries only the crates.io API, caches in `target/`): every crates.io package in Cargo.lock must be
  at least 14 days old; a younger version is allowed only as the smallest release that fixes a RustSec advisory. On
  2026-10-28 the lock had 44 younger packages: all were downgraded (`cargo update --precise`, the icu/zerovec family
  with temporary exact pins), except the security fixes now listed as exceptions: rustls 0.23.46, rustls-webpki
  0.103.16, webpki-root-certs 1.0.10 (the tool's suggestion 1.0.9 is the vulnerable one), ctutils 0.4.4
  (RUSTSEC-2026-0385) and zstd-sys 2.1.2 (RUSTSEC-2026-0410). Run it with the other gates:
  `python3 -I tools/crate-age.py` (exit 0).

| Id | Severity | Bug | Rust |
|---|---|---|---|
| SB-3 | medium (information disclosure) | `GET /api/v1/memory/inbox?client=<not an id>` joins the raw value onto `content/inbox`, so any key holder (a viewer) can list the `*.md` names of any directory (`../instructions`, `/abs/dir`); parity case `core.inbox.client-traversal` | a non-id client lists nothing (D-INBOX-CLIENT) |

### A-10 (2026-10-28, integration of WS-A at b851423)
- WS-A is merged: `_util`, claude-code, opencode, generic (spec validation, `dry_render`, the TOML writer), hermes
  (docker argv, the observe program) and turnstone adapters. **A-8's open item is closed**: `generic::toml_tables` /
  `toml_val` and `_util::wrap_block` exist. `resolver::generic_caps_for_spec` now calls
  `adapters::generic::caps_for_spec(spec).ok()` (A-9's pending item).
- **WS-A public API** (beyond the locked `adapter()` constructors): `_util`: the marker constants (`MD_BEGIN/END`,
  `HASH_BEGIN/END`), size caps (`MAX_*`), `safe_relpath`, `UnsafePath`, `MarkerInjection`, `looks_like_marker`,
  `wrap_block`, `extract_block`, `replace_block`, `find_region`, `render_frontmatter`, `parse_frontmatter`,
  `Frontmatter`, `dump_json`, `dump_json_py`, `dump_yaml`, `canon_dumps`, `canon_digest`, `slice_digest`,
  `json_loads`, `json_loads_raw`, `PyJson`, `JStr`, `merge_keys`, `get_dotted`/`set_dotted`/`del_dotted`,
  `read_capped`, `read_text`, `read_tree`, `listdir_sorted`, `discover_skills`, `skill_artifacts`,
  `instruction_artifact`, `render_instructions`, `sorted_instructions`, `effective_caps`, `ordered_caps`,
  `unknown_caps`, `unsupported_severity`, `verify_expected`, `check_description`, `valid_name`, `diag`,
  `sandbox_prefix`/`sandbox_missing`, `py_sorted_by_key`, `to_py`, ...; claude_code: `ClaudeCodeAdapter`, `TOOL_MAP`,
  `MODEL_TIERS`, `DEFAULT_MANAGE`, `parse_rule`, `render_permissions`, `cmd_pattern`; generic: `GenericSpecAdapter`,
  `Spec` and its `*Spec` parts, `parse_spec`, `validate_spec`, `caps_for_spec`, `dry_render`, `subst`, `toml_tables`,
  `toml_val`; hermes: `HermesAdapter`, `OBSERVE_PROGRAM`, `parse_observation`, `listed_names`, `BASELINE_DISABLED`,
  limits; opencode: `OpenCodeAdapter`; turnstone: `TurnstoneAdapter`, `default_deliver_script`,
  `default_token_dir`, `console_problem`, `token_file_problem`, `undelivered_resources`, limits.
- CORE `pyd` (one implementation for WS-A, WS-B1 and WS-D; golden-tested against pydantic 2.13.5, 97 cases):
  `pub fn lax_str` (bytes accepted as UTF-8 str, `string_unicode` otherwise), `pub fn lax_bool` (ints 0/1; another
  int `bool_parsing`, an int beyond i64 `bool_type`; floats 0/1; another integral float within i64 `bool_parsing`;
  other floats, NaN, infinity `bool_type`; yes/no words from str or UTF-8 bytes), `pub fn key_loc` (a dict key's `loc`:
  str as itself, int/bool as an int, anything else `repr()`); `Ty::Int` keeps ints beyond i64 (`PyValue::BigInt`,
  bounds by sign) and answers `int_parsing_size` for an integral float beyond i64; `Ty::Float` converts a big int
  (`float_type` beyond f64). WS-A's local copies in `generic.rs` are gone (it calls these).
- Deviations: **D-ADAPTER-JSON-DEPTH**: `_util::json_loads` builds a PyValue tree and refuses documents nested more than
  1000 levels (Python's scanner accepts up to 9998; client config files are never that deep). **D-TURNSTONE-NFKC**: a
  non-ASCII Turnstone console host is refused as "must be a loopback address" instead of Python's NFKC "not a valid
  URL". **D-ADAPTER-SURROGATE**: a JSON lone surrogate escape (`\ud800`) is read as U+FFFD with a flag
  (`JStr.lone_surrogate`) that hermes uses to refuse such names, where Python carries the surrogate.
- **Python bug P-4 (low)**: the generic adapter's flat-skill and memory `filename` templates are not path-checked when a
  spec is validated (`validate_spec` accepts `../x.md`); only rendering refuses them (`UnsafePath`). Rust keeps this.
- Ignores: the 12 tests that waited for WS-A pass and lost their ignore; **no `#[ignore]` is left in the tree**.

### A-11 (2026-10-28, integration of WS-D at 3d5fc71; one CPython JSON decoder)
- WS-D is merged: the knowledge service (`agent-knowledge`: security middleware, API, config, SQLite registry, chunking,
  filters, rate limits, embeddings over HTTP, Lance / Qdrant (REST) / memory stores, jobs and SSE, telemetry). With it
  every Agent Hub workstream is merged. `api::disk_free` uses `nix::sys::statvfs` (f_bavail * f_frsize, like
  `shutil.disk_usage`) instead of running `stat`; agent-knowledge depends on the workspace `nix`.
- **`hub_core::py::pyjson`** (moved from agent-knowledge, re-exported there as `agent_knowledge::pyjson`) is the
  CPython-exact `json.loads` (encoding detection, `surrogatepass`, the C scanner's error positions, NaN/Infinity, big
  ints, the 4300-digit limit, duplicate keys, depth limit 9991 measured on the running server) and `json.dumps` in the
  three flavours the services write. Its parser is iterative.
- **Hub request bodies** now follow FastAPI's body read exactly, BEFORE any dependency (this corrects §7.2 steps 5-8):
  a JSON body (strict content type) is decoded with `pyjson::loads_bytes`; a `JSONDecodeError` answers 422
  `validation_error` with only `[{"loc": ["body", <Python's character position>], "msg": "JSON decode error", "type":
  "json_invalid"}]`; any other decoding failure (RecursionError, UnicodeDecodeError, the digit limit) answers 400
  `http_error` "There was an error parsing the body"; a JSON `null` body is a missing body. Then `enforce_roles`,
  `require_admin` and validation run as before (`deps::decode_body`, called by the pipeline). Parity cases
  `core.body.*` (null, trailing comma, bad escape, NaN, a position after a non-ASCII character, depth 10000; a viewer's
  broken JSON is a 403 because the role gate is in the middleware, before the body is read) and
  `tests/test_body_decode.rs` (invalid UTF-8, the D-JSON-DEPTH band).
- **D-JSON-DEPTH (narrowed)**: the hub hands its handlers bodies nested at most 256 levels deep
  (`deps::BODY_MAX_DEPTH`; a non-decoding pre-scan); deeper bodies get FastAPI's 400 "There was an error parsing the
  body". Python decodes up to 9991 levels, so bodies between 257 and 9991 levels differ (Python runs the handler).
  Reason, measured: converting and dropping such a value recursively overflows a 2 MiB worker stack (debug: 512
  levels need about 1 MiB, 1000 about 2 MiB; at 9990 even the release build aborts), which would crash the process.
  The knowledge service keeps 9991: its bodies are only type-checked and dropped (1 MiB measured) and no recursive
  consumer sees deep values (filters stop at depth 4, metadata is scalars only). **D-BODY-NAN-BIGINT**: NaN/Infinity
  reach hub handlers as `null`, and ints beyond i64 as floats (`serde_json::Value` cannot hold them).
- JSON decoders not merged into `pyjson` (different contracts; candidates for the audit): `adapters::_util::json_loads`
  (keeps a lone-surrogate flag hermes needs, raw key order; value tree capped at 1000 levels, D-ADAPTER-JSON-DEPTH),
  `services::merge::loads` (live client files; stops at 990 levels, Python's frame limit: **D-MERGE-JSON-DEPTH**, CPython's
  C scanner allows about 9998), `services::doctor::json_loads` (golden-tested at the CLI's 9998), `services::skilleval`
  (serde for SkillEvaluator's own reports, D-EVAL-JSON).
- §7.4 (knowledge pipeline) additions: the JSON body is decoded BEFORE authentication (FastAPI reads the body before
  the `authenticate` dependency), so a broken body is a 422 even without a key; a JSON `null` body is a missing body;
  nesting beyond 9991 levels is a RecursionError answered 400; a body over the cap is answered 413 and the rest of the
  upload is drained in the background so the client reads the 413 instead of a connection reset.
- §17 (parity) knowledge: python-vs-rust 114 cases, 114 equal, 20/20 routes (WS-D's run). §20: the knowledge middleware
  is ported (drop it from "not verified").
- Knowledge deviations: **D-K-SURROGATE** (a lone UTF-16 surrogate in JSON becomes U+FFFD; positions unaffected);
  **D-500-HEADERS** applies to the knowledge service too (its unexpected-error 500s carry the security headers and
  `no-store`); **D-K-TRANSPORT** (httpx transport exception class names are derived from reqwest's error kinds:
  `ConnectError`, `ConnectTimeout`, `ReadTimeout`, `ReadError`, else `RemoteProtocolError`; httpx's rarer classes such
  as `WriteTimeout` or `ProxyError` are not distinguished); **D-K-STARTUP** (a startup failure prints the message a
  Python traceback ends with and exits 1, without the traceback); **D-K-HASHTOKENS** (the offline hash embedder
  tokenises `\w+` as `_` or Rust's Unicode alphanumerics after `to_lowercase`; Python's `re` `\w` and `str.lower()`
  differ on a few exotic code points, so such text hashes differently).
- §16 cutover note: `deploy/knowledge/Dockerfile` builds the Python service; for the Rust binary it needs the vendored
  protoc (`rust/tools/protoc`, `PROTOC`) at build time and no virtualenv at run time. Do this at cutover.
- Audit: the redacted body in an audit row is decoded like Python's (`deps::audit_json`: `pyjson`, then `redact`, which
  truncates at depth 8 as Python does) for bodies nested at most 256 levels; deeper bodies are recorded as
  `"[unparseable json]"` (Python records the first 8 levels, or nothing at all beyond its limit: P-5).
- Metrics: FastAPI's OpenAPI document is a plain Starlette `Route` (it does not set `scope["route"]`), so its requests are
  counted under route `unmatched`, as in Python (`routes::metric_label`).
- **Python bug P-5 (low, audit gap)**: the security middleware's audit hook decodes the body with `json.loads` and
  catches only `ValueError`; a body nested beyond the recursion limit raises `RecursionError`, which escapes the
  middleware's `finally`, so no audit row is written. Such requests fail with 400 anyway (FastAPI hits the same
  limit); only the record of the failed attempt is lost. Rust records it. Parity: `core.body.deep-json-10000` runs
  after `audit.final` / `metrics.final` for that reason.
- Parity after all merges (2026-10-28): hub python-vs-python 403/403 equal; python-vs-rust 403 cases, 397 equal,
  6 allowed (D-500-HEADERS, headers only), 0 differences, 98/98 routes. Knowledge python-vs-rust 114 cases, 114 equal
  (`wsd.body-over-cap` is allow-listed because Python's own answer varies between runs), 20/20 routes. TEST-MAP:
  1027 mapped, 58 N/A, 0 pending. e2e (`E2E_BACKEND=rust`): all 59 pages/tabs rendered, no error state, no console
  error.

### A-12 (2026-10-28, YAML follow-up)
- `hub_core::yaml::YamlError.problem` holds the first line of `str(exc)`, which is what Python's `yamlsafe` uses
  (`str(exc).splitlines()[0]`) and what WS-Y's goldens compare (`parity/gen/yaml_codec.py`); it is NOT PyYAML's
  `exc.problem` attribute (for a marked error the first line is usually the `context`: the two coincide in 99 of the
  468 errors of `golden/yaml_load.json`). No Agent Hub code, Python or Rust, reads PyYAML's `exc.problem`, so nothing
  changes; the field is documented as such.
- D-YAML-TUPLE and D-YAML-RECURSIVE stay: an arena-based loader (written for a sibling project) builds tuples and shared
  recursive objects, but `PyValue` (a tree, no tuple variant) cannot carry either, so adopting it would mean changing
  the value model every workstream uses. Not worth it for `!!omap`/`!!pairs` and recursive aliases, which content
  YAML never uses (the strict loader refuses aliases outright).

### A-13 (2026-10-28, cutover to 0.3.0)
- The Python `backend/` and `knowledge/` packages are removed; the Cargo workspace moved from `rust/` to `backend/`, so
  the e2e gate's tree record (`backend/ frontend/ policy/`, `scripts/e2e-gate.sh`) covers all server code including the
  knowledge crate. The compile-time app root (three ancestors of a crate) is unchanged, so the binaries still find
  `policy/`, `frontend/` and `deploy/` in the checkout they were built from (or `AGENT_HUB_APP_ROOT`).
- Version 0.3.0 (`[workspace.package]`, the hub's and the knowledge service's `VERSION`, the captured OpenAPI document).
- Parity harness: `python` now means the Python backend and knowledge service of a git ref, default `LAST_PYTHON_REF` =
  `c419f1e` (the code the port was verified against; `python@v0.2.0` or any ref also works). `lib/python-release.mjs`
  exports that ref's `backend/ knowledge/ policy/ deploy/` with `git archive` (no worktree) into
  `backend/target/python-release/<commit>/` and runs `uv sync --frozen --python 3.13` there (no interpreter download
  unless `UV_PYTHON_DOWNLOADS` allows it). Both sides get THIS checkout's `frontend/` and `policy/floor.yaml`
  (`HUB_FRONTEND_DIR`, `HUB_POLICY_FILE`) so an older ref's copies cannot skew a comparison. Seeding uses the Python side
  being compared. New normalisation rule **N11**: a semver string under a `version` key (0.3.0 vs 0.2.0).
- Golden generators run with that environment: `PYREL=$(node backend/parity/lib/python-release.mjs)`, then
  `"$PYREL/backend/.venv/bin/python" -I backend/parity/gen/<script>` from the repo root (each script's docstring).
- Tests that read the Python tree: the Python adapter goldens moved to
  `crates/hub-core/tests/golden/python-adapter-golden/` (the YAML round-trip test keeps running); the live PyYAML oracle
  (`yaml_props.rs`) and the Python half of `lance_compat.rs` run when `PYREL` names a prepared release, and say they did
  not otherwise.
- `frontend/e2e/run.mjs` runs the Rust binaries only (`backend/target/debug`, or `E2E_RUST_BIN_DIR`); `scripts/e2e-gate.sh
  run` builds them first. Makefile, CI, the systemd templates, the desktop launcher and the knowledge Dockerfile use the
  binaries (README, docs/OPERATIONS.md, CHANGELOG 0.3.0). The adapter registry is unchanged: a new client
  (`deepseek_harness`, pending its upstream Python adapter) is still one file in `crates/agent-hub/src/adapters/`.
- Gates in the cutover tree before the commit (2026-10-28): `cargo fmt --check`, clippy `-D warnings`, `cargo test
  --workspace` 948 passed / 0 failed; with `PYREL` set, the live PyYAML oracle (10,500 checks equal) and both halves of
  `lance_compat.rs` pass; `cargo deny check` and `cargo audit` pass; TEST-MAP 1027 mapped, 58 N/A, 0 pending; frontend
  180/180; e2e 59 pages/tabs, 65 static files. Parity: hub python (c419f1e) vs rust 403 cases, 397 equal, 6 allowed
  (D-500-HEADERS), 0 differences; `--a python@v0.2.0` the same (v0.2.0..c419f1e changes only the bytecode exclusion,
  which the corpus does not reach); hub python-vs-python and rust-vs-rust 403/403; knowledge python vs rust 114 cases,
  0 differences (`wsd.body-over-cap` is equal or allowed depending on the run: Python-vs-Python itself differed on it in
  5 of 8 runs today, as the allow-list says). crate-age: HTTP 429 from crates.io on the first try, passed on the retry.

### A-14 (2026-10-28, independent audit before 0.3.0)
An independent audit of the Rust backend (SHIP-WITH-FIXES; nothing critical or high) found the items below. Each fix has
a test that fails without it (checked by reverting the fix where the test can run without it); tests are named after the
finding (`tests/test_audit_0_3_0.rs` unless stated).
- **F1 (medium), D-B2-FIFO extended.** One reader for paths someone else can write: `services::fs::open_regular` /
  `read_regular` open with `O_NONBLOCK` (and `O_NOFOLLOW` unless a caller follows links on purpose), `fstat` the opened
  descriptor, require a regular file (a directory is EISDIR, Python's message) and read at most a cap. Used by `RealFs`
  (discovery and every adapter's verify, which runs under the hub lock), `LiveFiles` (plan, rollback), the doctor, the
  untracked-file diff and the backup reads. Python opened these in blocking mode, so a FIFO wedged the hub.
- **F2 (medium, publication).** The YAML corpus captured from the Python test suite held the build machine's user name
  and paths; `parity/gen/yaml_captured.json` now uses `/tmp/pytest-of-golden/pytest-0/...` and `/home/golden/agent-hub`,
  and the four YAML goldens were regenerated from it by PyYAML (`gen_yaml_golden.py`, Python release c419f1e). Porting
  notes and config comments name no local path, host or sibling checkout. `config::app_root` no longer compiles in the
  build path: it is found from the executable (`app_root_for_exe`: the directory above the nearest `backend/target`),
  `AGENT_HUB_APP_ROOT` still wins (D-APPROOT). `make release` rewrites source paths with `--remap-path-prefix` (cargo's
  `trim-paths` is not stable on 1.99.1) and fails if the checkout or home path is still in a binary.
- **F3 (medium-low), SB-3.** `f3_inbox_list_treats_client_as_an_id_not_a_path` and
  `f3_inbox_route_lists_nothing_for_a_path_shaped_client` (viewer key; `../instructions` and an absolute dir holding a
  copy of a real inbox item). The parity allow entry `core.inbox.client-traversal` now carries `rust:` (the answer the
  Rust side must give), checked on every run that includes Rust.
- **F4 (medium-low).** `fs::capture` takes a stderr cap as well as a stdout cap; `RealRunner` (adapter verify, including
  Hermes' `docker exec ... hermes skills list` into a container the hub does not trust) caps both streams while reading
  at 4 bytes per returned character plus one, so the decoded, truncated text is what Python returned. git's stderr is
  capped at 1 MiB.
- **F5 (low).** `migrate_content`'s copy refuses special files before opening them (Python's `SpecialFileError` refused a
  FIFO; the Rust copy blocked on it under the hub lock); regular files are copied through `open_regular`.
- **F6 (low-medium), D-TURNSTONE changed.** Turnstone verify runs `HUB_PYTHON` (the hub's process environment; an absolute
  path to an executable file), else `/usr/bin/python3` or `/bin/python3`; never a PATH lookup, where a user-writable
  directory such as `~/.local/bin` often comes first. A refused interpreter is a failed check and nothing runs.
- **F7 (low).** The knowledge SSE slot guard is taken right after `acquire()`: a client that goes away while the upstream
  has not answered no longer leaks the slot (`test_api_wsc.rs`).
- **F8 (low).** Request pipeline: a body stream that fails part-way is a 400 `body_incomplete` (it was handed on as a
  shorter body), a body slower than `AppState::body_deadline` (60 s) a 408 `body_timeout`; a handler panic is caught
  (500 problem, security headers, audit row); the bearer lookup (SQLite, updates `last_used`) and the audit insert run
  on the blocking pool. Server: `server::serve_with` replaces `axum::serve` with an HTTP/1.1 accept loop on hyper
  (`ServeLimits`: header-read timeout 30 s, which hyper also applies to an idle keep-alive connection; at most 256 open
  connections, more wait in the listen backlog; a 10 s grace on shutdown). Knowledge client: one call (send and the
  whole response) may take at most 4 x `HUB_KNOWLEDGE_TIMEOUT_S` (at most 10 min) on top of the per-operation timeouts
  (`test_api_wsc.rs`, an upstream that trickles a byte every 100 ms); an infinite `HUB_KNOWLEDGE_TIMEOUT_S` (pydantic
  accepts `inf`) no longer panics. Python (uvicorn) had idle keep-alive limits but none of the others.
- **F9 (low).** Tests that assumed `HOME` under `/home` pass an explicit home or re-run themselves with one
  (see the test files); `cargo deny --locked check` in the Makefile and CI (every other cargo call already had
  `--locked`; `fmt` and `audit` do not resolve dependencies).
- **F10 (low), D-BIND (new).** `config::is_loopback_host`: exactly `localhost` or an IP address that is loopback. Python
  (and the port until now) accepted any host starting with `127.`, so a name such as `127.example.org` passed as
  loopback. Used by `check_bind` and the doctor.
- **F11 (low), known limitation, not changed in 0.3.0.** The MCP scan attestation (HMAC over `[command, args, url,
  pinned_ref, transport]`) does not cover `env_names`, `sandbox_profile` or `egress_hosts`, so those can change on an
  attested server without invalidating its `clean` status (the change is still a reviewed content commit and the floor
  still checks egress). Widening the HMAC would invalidate every existing attestation; follow-up for a later release
  (CHANGELOG 0.3.0 "Known limitations", docs/SECURITY.md).
- **F12 (low), tightening.** `TargetGuard` protects `.bash_profile`, `.bash_login`, `.zprofile`, `.zshenv`,
  `.config/fish`, `.config/environment.d`, `.config/git`, `.gitconfig`, `.git-credentials`, `.docker`, `.kube`,
  `.npmrc`, `.pypirc` like `.bashrc`; the floor's `deny_path_read` adds `~/.docker/**`, `~/.kube/**`,
  `~/.git-credentials`, `~/.config/gh/**`, `~/.npmrc`. Rendered permissions therefore gain deny entries: the first plan
  after the upgrade shows them. The B1 hub golden was regenerated by Python with this floor (`gen_hub_golden.py` and
  `gen_content_golden.py` now read the checkout's `policy/floor.yaml`, not the Python release's copy).
- **F13 (low), D-HERMES-NAMES (new).** Hermes verify applies observe's plain-name rules to `compose_project`,
  `compose_service`, `docker_context`, `exec_user`, `hermes_bin` (no leading `-`) and the container name `docker ps`
  printed; a refused value runs nothing further.

### A-15 (2026-10-28, the DeepSeek Harness adapter for 0.3.0)
- **Reference.** The adapter was first written in Python after c419f1e and never released (uncommitted files:
  `adapters/deepseek_harness.py`, `tests/adapters/test_deepseek_harness.py`, `test_dsh_deliver.py`,
  `tests/adapters/golden/deepseek_harness_full.json`, `deploy/dsh-deliver.sh`; doc edits to README, CHANGELOG and four
  docs). It was checked in a scratch export of c419f1e plus those five files (`uv sync --frozen`): no TODOs or stubs;
  its two test files pass (125 tests); the full backend suite there gives 1114 passed, 4 failed, and the same 4 fail on
  a pristine export of c419f1e at the same place (they depend on HOME's location, not on the adapter).
- **Port.** `adapters/deepseek_harness.rs`, byte-identical renders and discovers. Public API: `DeepSeekHarnessAdapter`,
  `ID`, `DEFAULT_MANAGE`, `PLACEHOLDER`, `PATCH_FILE`, `MCP_CLIENT`, `HUB_SOURCE`, `LIST_SKILLS`, `MAX_SKILL_MD`,
  `MAX_SKILL_DIR`, `MAX_DESCRIPTION`, `MAX_LIST_OUTPUT`, `DSH_NAME_PATTERN`, `default_deliver_script`, `yaml_tag_risk`,
  `parse_listing`; `hermes::is_format` is now `pub` (the Cf table the description check shares). Python's `re` is
  spelled out where Rust's differs: `\s` includes `\x1c-\x1f`, and the IGNORECASE `skill_` in the framing rule also
  matches U+017F, U+212A, U+0130 and U+0131. The container's listing is parsed with `_util::json_loads_raw` (CPython's
  scanner depth, iterative drop). No deviation.
- **Goldens.** `parity/gen/gen_dsh_golden.py` (the `gen_adapter_golden.py` pattern; `PYREL=<tree>`, run from the repo
  root) writes `crates/agent-hub/tests/golden/deepseek_harness.json`: 14 renders, 2 discovers, 47 verify runs through a
  scripted runner (checks, argv and timeouts), the tag rule, metadata and constants.
- **Tests.** `tests/test_deepseek_harness.rs` (48: every pytest by name plus the golden slices) and
  `tests/test_dsh_deliver.rs` (18: the bash script run for real with a `/bin/sh` fake `docker`; every tool by system
  path, so the script's `python3` is `/usr/bin/python3`, where the pytest put the venv's first on PATH). TEST-MAP: the
  125 new pytest ids are WS-A rows, all mapped (1210 ids, 1152 mapped, 58 N/A, 0 pending).
- **`deploy/dsh-deliver.sh`** is added byte-identical. The adapter never runs it, nor any interpreter: verify runs only
  argv D-1, and `docker` is found on PATH exactly as for hermes (H-1..H-3) and in Python (the parity harness's fake
  `docker` relies on that). The script's header still says to keep its limits equal to `deepseek_harness.py`;
  `test_limits_and_rules_match_the_adapter` compares them with the Rust constants instead.
- **Parity.** `lib/python-release.mjs` takes an absolute tree path (`--a python@/abs/tree`: used as it is, needs its
  `backend/.venv`; nothing is synced or written there). `lib/servers.mjs` also creates `home/dsh-stage` and
  `home/dsh-stage2` (the hub never creates a client root). 28 `dsh.*` cases in `corpus/hub.mjs`: content with a framing
  tag and a tagged MCP server, a working-tree plan with the refusals, two applies, rendered, drift, verify (the fake
  docker echoes the exact argv), a hostile container/context that never reaches docker, discover, observe, audit.
  `run.mjs`: when exactly one side's `/adapters` lacks `deepseek-harness`, that entry is dropped from both sides'
  adapter lists before comparing and the `dsh.*` cases are reported as allowed, so the default reference (c419f1e) stays
  usable; those cases are only proven against a tree with the adapter. Results (2026-10-28): the scratch tree vs rust
  431 cases, 425 equal, 6 allowed (D-500-HEADERS), 0 differences; the same tree against itself 431/431; c419f1e vs rust
  403 equal, 28 allowed (6 D-500-HEADERS, 22 `dsh.*`), 0 differences.
- **Open question for the maintainer (kept as in Python, not fixed here): `AGENTS.md` is blocked at apply.**
  `targets::sensitive_target` (Python `services/targets.py`) lets only `opencode` and `generic` write a file named
  `AGENTS.md`. The adapter renders the enabled instructions as `AGENTS.md` in its stage (and `manage.instructions`
  defaults to true), so applying a dsh client with any instruction enabled is blocked (`unsafe_path`, nothing written);
  parity case `dsh.apply.confirm` shows it on both sides. The Python tests use a fake filesystem and never
  reach the applier, and their docs described `AGENTS.md` as delivered; the merged docs say to keep `instructions` off.
  `hermes-agent` has the same block at c419f1e (`sensitive_target("hermes-agent", {"instructions": True},
  "instruction", "AGENTS.md")` returns it), while docs/FEATURES.md lists its `AGENTS.md` as implemented. Adding the
  stage adapters to that guard is a security-rule change, so it is left to the maintainer (follow-up after 0.3.0).

### A-16 (2026-10-28, merge of A-15 into the cutover)
- `rust-dsh` (A-15) merged into `rust-cutover` after A-14; the only conflicts were the appended addenda and the CHANGELOG
  head (both kept). A-15's wording about its reference was made neutral (no checkout or session names).
- Parity: the 22 `dsh.*` cases that a reference without the adapter (the default, c419f1e) can only allow are now also
  checked on the Rust side, against `parity/expect/rust-dsh.json`. `run.mjs --a python@<tree with the adapter>
  --write-rust-expect` writes that record only when the tree and Rust agree on every `dsh.*` case and nothing differs.
  The record is normalised so it outlives the run: the harness's temp dir becomes `<TMP>` (a kept file text gets its
  `size` and `sha256` recomputed) and a plan's 64-hex `digest` becomes `<DIGEST>`; the two sides of one run are still
  compared unchanged. A changed recorded status was reported as a difference (checked by hand). Results after the
  merge: against the tree with the adapter 431 cases, 425 equal, 6 allowed (D-500-HEADERS), 0 differences (the record
  was written from this run); against c419f1e 403 equal, 28 allowed (6 D-500-HEADERS, 22 `dsh.*` with their Rust
  answers checked), 0 differences; rust vs rust 431/431, 0 differences.
- `targets_only_opencode_and_generic_may_write_agents_md` pins the `AGENTS.md` rule A-15 left as it was; FEATURES now
  marks `hermes-agent` Partial for the same reason (its `AGENTS.md` is blocked at apply, as in 0.2.x).

### A-17 (2026-10-28, release follow-ups)
- The knowledge service's server gets the hub's limits (A-14 F8): `agent_knowledge::app::serve_with` with `ServeLimits`
  (header/idle timeout 30 s, at most 256 connections, 10 s shutdown grace) replaces `axum::serve`; a body that breaks
  off is a 400 `body_incomplete` and one slower than `App::set_body_deadline` (60 s by default) a 408 `body_timeout`
  (Python handed a truncated body on and had no body deadline). The over-cap drain (D-K-OVERCAP-DRAIN) is unchanged.
  Tests in `tests/security_controls.rs`. Knowledge parity stays 114 cases, 0 differences (`wsd.body-over-cap` equal or
  allowed depending on the Python side, as before).
- crate-age: the rustls 0.23.46 exception (A-9) was removed, the version is 14 days old now; the gate passes with 4
  exceptions, 0 too new, 0 stale.
