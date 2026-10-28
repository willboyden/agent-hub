# Adding a client to Agent Hub

A client is any agent harness that reads instructions, skills, subagents, MCP servers or permissions from files. There are two ways to add
one, and neither needs a change to the core or the UI.

| Way | Use when | Effort |
|---|---|---|
| **A. Declarative spec** (`adapter: generic`) | the client is configured by plain files (markdown, JSON, YAML, TOML blocks) | one YAML file |
| **B. Rust adapter** | the client needs logic a spec cannot express (custom merge, an "ask the client" verify, staging + delivery) | one file exposing `pub fn adapter()` |

Adapters are **pure**: `render(ctx)` returns artifacts and diagnostics and does no I/O. `discover` and `verify` reach the machine only through the
injected `FileSystem` / `CommandRunner` ports, so tests run offline.

## A. Declarative spec

Create `content/clients/<id>.yaml` (a `ClientConfig` plus the `manage` flags) and put the spec under `spec:`. The UI's add-client wizard validates
it live (`POST /clients/validate-spec`) and can dry-run it against sample content.

```yaml
id: acme
adapter: generic
display_name: Acme Agent
strict: false                    # true: anything the client cannot express is an ERROR that blocks apply
roots: {project: ~/work/acme}    # named delivery roots; the spec refers to them by name
manage: {skills: true, agents: true, instructions: true, mcp: false, permissions: false, memory: false}
spec:
  version: 1
  tool_map: {read: fs.read, write: fs.write, shell: sh, web_fetch: null}   # canonical capability -> native tool; null = unsupported
  model_tiers: {fast: acme-small, deep: acme-large}                        # fast|standard|deep -> native model id
  skills:       {root: project, path: .acme/skills, layout: dir_per_skill, filename: SKILL.md}
  agents:       {root: project, path: .acme/agents, filename: "{name}.json", format: json,
                 fields: {id: name, about: description, prompt: body}, static: {kind: helper},
                 tools_field: allowedTools, tools_format: bool_map, model_field: model}
  instructions: {root: project, file: ACME.md, mode: block}
  mcp:          {root: project, file: .acme/mcp.yaml, format: yaml, key_path: tools.mcp,
                 entry: {stdio: {exec: "{argv}", environment: "{env}"}, http: {endpoint: "{url}"}}}
  permissions:  {root: project, file: .acme/policy.json, format: json, lists: {allow: rules.allow, deny: rules.deny},
                 rule_templates: {command: "shell:{match}", tool: "tool:{tool}"}}
  memory:       {root: project, path: .acme/memory, filename: "{id}.md"}
```

Every section is optional; a concern with no section is reported as unsupported (error when `strict`, warning otherwise).

### Reference

| Section | Keys (all others are errors) |
|---|---|
| top | `version` (1), `notes`, `tool_map`, `model_tiers` |
| `skills` | `root`, `path`, `layout` = `dir_per_skill` (default; `<path>/<name>/<filename>` plus supporting files) or `flat` (`filename` is a template with `{name}`; supporting files are dropped with a warning), `filename` (default `SKILL.md`) |
| `agents` | `root`, `path`, `filename` (template, needs `{name}`), `format` = `md_frontmatter` / `json` / `yaml`, `fields` (output key -> source `name`/`description`/`mode`/`body`), `static` (constant scalars), `tools_field`, `tools_format` = `csv` / `list` / `bool_map`, `model_field`, `tool_map`, `model_tiers` (override the top-level ones) |
| `instructions` | `root`, `file`, `mode` = `block` (only the hub marker region, headings kept), `own` (whole file hub-owned, `## title` headings), `concat` (whole file hub-owned, bodies joined, no headings), `marker` = `md` / `hash` |
| `mcp` | `root`, `file`, `format` = `json` / `yaml` / `toml`, `key_path` (dotted), `entry` (transport `stdio`/`http`/`sse` -> template object), `env_style` = `dollar_brace` / `env_ref` / `names_only`, `wrapper` (argv prefix for `sandbox_profile` servers) |
| `permissions` | `root`, `file`, `format` = `json` / `yaml`, `lists` (decision `allow`/`ask`/`deny` -> dotted key path), `rule_templates` (rule kind `tool`/`command`/`path_read`/`path_write`/`egress_host`/`mcp_server` -> template or list of templates) |
| `memory` | `root`, `path`, `filename` (template, needs `{id}`), `frontmatter` (bool) |

**How merging works.** `json`/`yaml` files use key merges (only the listed `key_path` / `lists` keys are the hub's; everything else in the file is
preserved). `toml` is delivered as a marker block (`# agent-hub:begin ... # agent-hub:end`) because there is no TOML key merge. `block` instructions
replace only the marker region, so hand-written text in the same file survives.

**Placeholders.** `{name}`, `{id}` in file names; `{command} {args} {argv} {url} {env} {env_names} {transport} {pinned_ref} {name}` in MCP entries;
`{match} {tool}` in rule templates. A value that is exactly one placeholder keeps its type (a list stays a list); inside longer text it is
substituted as text (a list there is an error); a placeholder with no value drops its key. There is **no** `str.format`, no attribute access, no
eval, no shell: an unknown placeholder, `{0.__class__}`, `{name!r}` or a stray brace is a validation error, and substituted values are never
re-scanned. Environment variables carry **names only**; values never enter the hub.

**Errors name their path**, for example `spec.skills.layoutt: unknown key`, `spec.mcp.entry.stdio.exec: unknown placeholder {shell}`,
`spec.instructions.file: unsafe relative path: ...`.

### Try it without touching a real client

In the UI, the add-client wizard validates the spec live and previews the rendered files (`POST
/api/v1/clients/validate-spec`, which a viewer key may call). In Rust tests:

```rust
use agent_hub::adapters::generic::{dry_render, validate_spec};
let problems = validate_spec(&spec);             // empty means valid; each Diag's item is the path
let result = dry_render(&spec, &sample_context)?; // artifacts + diagnostics, no I/O
```

`examples/clients/{cursor,codex-cli,gemini-cli}.yaml` are example specs; adjust them to your client's current config format and check every
path before turning on `manage`.

## B. Rust adapter

Add `backend/crates/agent-hub/src/adapters/<name>.rs` that exposes `pub fn adapter()` (one adapter per file; a second id gets its own
file). The build discovers every file in that directory (except `mod.rs`, `base.rs` and `_*.rs`) and registers it; nothing else is edited.
A constructor that returns `None` or an error is recorded in the registry's load errors (shown in Settings) and skipped, never fatal.

```rust
use std::sync::Arc;
use super::base::*;

pub struct MyAdapter;

impl ClientAdapter for MyAdapter {
    fn id(&self) -> &str { "my-client" }
    fn display_name(&self) -> &str { "My client" }
    fn docs(&self) -> &str { "What this adapter writes, and where." }
    fn caps(&self) -> AdapterCaps { todo!() }              // what the client can consume; drives the UI greying-out
    fn default_config(&self) -> ClientConfig { todo!() }   // starter roots/params for the wizard
    fn render(&self, ctx: &RenderContext) -> AdapterResult<RenderResult> { todo!() }    // PURE
    fn discover(&self, cfg: &ClientConfig, fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> { todo!() }  // read-only, for import
    fn verify(&self, cfg: &ClientConfig, expected: &[Expected], fs: &dyn FileSystem, run: &dyn CommandRunner)
        -> AdapterResult<Vec<Check>> { todo!() }           // prove the client sees it; ask it when it can answer
}

/// Registered by the build. `Some(Err("<Exception>: <message>"))` records a load error instead.
pub fn adapter() -> Option<Result<Arc<dyn ClientAdapter>, String>> {
    Some(Ok(Arc::new(MyAdapter)))
}
```

(`todo!()` marks what you fill in; the workspace lints refuse it in committed code.) Rules every adapter follows (see `_util.rs` for helpers):

* names/ids validated by `fullmatch`; every path through `safe_relpath` (no `..`, absolute, backslash, NUL); no absolute paths in artifacts;
* deterministic output (sort by name/id; JSON with sorted keys; fixed frontmatter key order);
* frontmatter through `render_frontmatter`; descriptions collapsed to one line (`oneline`) and size-capped;
* an agent that would end up with **no tools** is an error (several clients treat an empty list as "all tools");
* diagnostics for anything unsupported or lossy, at `error` when `cfg.strict` else `warn` (`unsupported_severity`); `info` for MCP servers without a clean scan;
* `verify` never reports `ok` from a copy alone: check hashes, and where the client has a list command, run it. If the client cannot be asked, say
  so (`ok=false`, with a detail saying it could not be checked).

## Built-in adapters and what they own

| id | Writes | Advisory by default | Notes |
|---|---|---|---|
| `claude-code` | `.claude/skills`, `.claude/agents`, `CLAUDE.md` (block, or whole file with `params.instructions_mode: own`) | `.mcp.json` (`mcpServers`), `.claude/settings.json` (`permissions.*`) | writes project-level files only; a user-level settings file that another process renders is never touched. `params.sandbox_wrapper` (argv prefix) is required for MCP servers that declare a `sandbox_profile` (fail closed) |
| `opencode` | `.opencode/agent`, `AGENTS.md` (block or `own`) | `opencode.json` (`mcp`, `permission`; `instructions` only with `params.instruction_files`) | no skills directory is known; `params.skills_via_claude_dir` is an opt-in (whether opencode reads `.claude/skills` depends on your version) |
| `hermes-agent` | a hub-owned **stage** dir (`skills/`, `AGENTS.md`, `config.*.fragment.yaml`; the `AGENTS.md` is blocked at apply, keep `instructions` off) | config fragments | one adapter for every Hermes instance; client ids are free and everything instance-specific is a param (below). Delivery is the opt-in `deploy/hermes-deliver.sh`; verify asks the running container (`hermes skills list`, whole-name match) |
| `deepseek-harness` | a hub-owned **stage** dir (`skills/`, `AGENTS.md`, `hub.cordis.patch.yml`) | `hub.cordis.patch.yml` (MCP rows) | DeepSeek Harness (dsh). Skills dsh would load unsafely are refused, not staged (below). Delivery is the opt-in `deploy/dsh-deliver.sh` into the docker volume dsh reads as `DSH_AGENTS_HOME`; verify asks the running container (`node /opt/lab/list-skills.mjs`, dsh's own discovery). A staged `AGENTS.md` is blocked at apply for now (below) |
| `generic` | whatever the spec says | per `manage` | this document |

`manage` flags live in the client file (`ClientDoc.manage`), not in the adapter. The adapters' `default_config().params["manage"]` documents the
recommended defaults.

### Hermes Agent params

```yaml
id: my-hermes
adapter: hermes-agent
roots: {stage: ~/hub-out/my-hermes}             # a directory the hub owns
params:
  compose_project: my-hermes-project            # label filter used to find the running container
  compose_service: hermes
  docker_context: rootless                      # optional: docker --context <value>
  hermes_bin: hermes                            # command inside the container
  exec_user: hermes                             # optional: docker exec --user
  disabled_toolsets: [web, search, browser]     # optional: may add to, never drop from, the built-in baseline
  source_root: ~/path/to/project                # where discover() reads the instance's real sources
  source_dirs:                                  # empty by default
    - {kind: skills, path: agent/skills}
    - {kind: instruction, path: agent/workspace-AGENTS.md}
    - {kind: config, path: agent/config.yaml}   # mcp_servers (env NAMES only) and approvals.deny are imported
```

### Delivering staged skills (`deploy/hermes-deliver.sh`)

Opt-in and not run by the hub. Required environment: `HERMES_STAGE_DIR` (the staged `skills/` directory), `HERMES_COMPOSE_FILE`
(a `docker-compose.y[a]ml` / `compose.y[a]ml`, not a symlink), `HERMES_SERVICE`, `HERMES_SKILLS_DEST` (an absolute path inside the container
for a category of its own, for example `/data/skills/hub`). Optional: `HERMES_DOCKER_CONTEXT`, `HERMES_BIN`, `HERMES_PATH_PREFIX`. The script
refuses to run when a required variable is unset, works on a private copy of the stage, refuses symlinks, extracts beside the destination and
swaps with `mv`, then asks hermes to list the skills (whole-name match) and rolls back on failure.

### DeepSeek Harness params

dsh reads user-global skills (`skills/<name>/SKILL.md`), an `AGENTS.md` and MCP rows from its agents home
(`$DSH_AGENTS_HOME`). The adapter assumes dsh runs in a container that mounts a docker volume read-only there; every name
below is an example.

```yaml
id: my-dsh
adapter: deepseek-harness
roots: {stage: ~/hub-out/my-dsh}                # a directory the hub owns
manage: {skills: true, instructions: false, mcp: false}   # instructions: see "AGENTS.md" below
params:
  container: my-dsh-1                           # the running dsh container ("CHANGEME" = not configured: verify says not checked)
  docker_context: rootless                      # optional: docker --context <value>
```

Both params must fullmatch `[A-Za-z0-9][A-Za-z0-9_.-]{0,127}`, or verify fails without running anything. Verify runs
`docker [--context <ctx>] exec <container> node /opt/lab/list-skills.mjs` (fixed; no param chooses the program). That
helper is part of your dsh image, not of the hub: it runs dsh's own skill discovery and prints one JSON line,
`{"complete": true, "skills": [{"name": ..., "source": ..., "path": ...}]}`. Every staged skill must be listed with source
`user-agents` (found in the agents home); a skill listed only from another source, such as a project's `.agents/skills`,
fails the check.

What the plan refuses (an error; the skill is not staged), because dsh inserts `SKILL.md` verbatim into the prompt between
its own `<skill_...>` tags, with no size cap, and follows symlinks:

* a `SKILL.md` or file path holding any `<skill_...>` / `</skill_...>` tag (it could fake that framing);
* a `SKILL.md` over 64 KiB, a skill folder over 4 MiB, a `SKILL.md` that is not UTF-8;
* a skill name that is not `[a-z0-9]+(-[a-z0-9]+)*`, or a frontmatter `name` missing or different from it;
* a frontmatter `description` that is missing, over 1024 characters, or holds control, format or line-separator characters.

`allowed-tools` is a warning (dsh does not read it). MCP servers become one `insert` list of `@deepseek-ai/dsh-mcp-client`
rows (`id: hub-mcp-<name>`, `transport: stdio` with `env: {}` or `streamable-http`; `sse` is unsupported). A dsh patch may
carry `!!js` tags that execute JavaScript, so the patch is written by the safe YAML dumper and a server whose values would
put `!!`, `!<` or a `%` directive line into it is refused. Agents, memory and permission rules are not rendered (dsh has
none of them as files).

**`AGENTS.md`.** The adapter renders the enabled instructions as `AGENTS.md` in the stage, but the hub's delivery guard
lets only `opencode` and `generic` write a file named `AGENTS.md`, so applying a dsh client with an instruction enabled is
blocked (`unsafe_path`, nothing written). Until that guard names this adapter, keep `instructions` off for dsh clients
(or deliver an `AGENTS.md` into the volume yourself).

### Delivering to dsh (`deploy/dsh-deliver.sh`)

Opt-in and not run by the hub. Required environment: `DSH_STAGE_DIR` (the client's stage root) and `DSH_VOLUME` (the
docker volume name, never a host path; it must exist). Optional: `DSH_DOCKER_CONTEXT`, and `DSH_CONTAINER` to ask dsh
afterwards (needs `python3` on the host to read the answer). `deploy/dsh-deliver.sh rollback` puts back the tree the last
delivery replaced. The script refuses to run when a required variable is unset, works on a private copy of the stage,
refuses symlinks, hard links and special files, re-checks skill folder names, the two size caps, `<skill_...>` tags in
`SKILL.md` and the patch's tag rule (not the frontmatter rules: those are the plan's), copies only `skills/`,
`AGENTS.md` and `hub.cordis.patch.yml`, writes through a throwaway `alpine` container (pinned digest, `--network none`),
swaps with `mv` and keeps the previous tree in `.hub-prev`, and rolls back when `DSH_CONTAINER` is set and a staged skill
is missing. Details: `docs/OPERATIONS.md` section 8c.
