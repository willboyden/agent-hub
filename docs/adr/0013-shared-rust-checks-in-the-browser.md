# ADR-0013: Shared Rust checks in the browser

Status: accepted (2026-11-14). Implemented, not released yet. Amends ADR-0008.

## Context

Since the Rust port (ADR-0012) the server's rules live in one crate, `hub-core`. The UI copied some of them into
JavaScript, and the copies drifted:

* The editor refused to save a skill whose description was under 20 characters (`validate.js`), while the server,
  `hubctl` and the importer accept 1 to 1024 characters (`SkillMeta`, `hub-core/src/models.rs`).
* A pasted client spec was read by a 45-line YAML subset (`yaml-lite.js`): `1.5` and `null` stayed strings, `yes`
  was a string where PyYAML (and the hub) read a boolean, and anchors, block scalars and flow maps were refused.
* The config editor sent a hub env draft (which can hold the knowledge admin key) to the server for its check, after a
  JavaScript copy of the hub's dotenv grammar replaced the values of keys *named* like a secret. A secret under another
  name was sent as written, and the masked keys were never really checked.

None of this is slow; it is logic in two languages. A Rust UI framework (Leptos) would not remove it by itself and
would cost a rewrite of the UI, about 73 more crates and about 3.5 MB of WebAssembly. Linking `hub-core` does remove
it, without a framework.

## Decision

One small crate, `backend/crates/hub-wasm`, compiles `hub-core`'s own functions to WebAssembly. It has no logic of its
own: each export calls what the server calls and returns JSON text, never throws, and caps its input at 64 KiB.

| Export | What it returns |
|---|---|
| `version()` | the workspace version |
| `check_config(file, text)` | for `hub-env` and `knowledge`, exactly the body `POST /config-files/{file}/validate` answers; for `client-spec`, the YAML, model and id half plus `server_check: true` |
| `yaml_load(text)` | PyYAML's safe loader (the hub's port) with the strict loader's size, depth and node limits |
| `yaml_scalar_type(s)` | how PyYAML types a plain scalar |
| `validate_meta(kind, json)` | the item model's errors, `[{loc, msg, type}]` |

To make this possible, the settings model and the config checks that need no hub state moved into `hub-core`
(`settings.rs`, `config_check.rs`, as `knowledge_config` did in 0.4.0; `agent-hub` re-exports them), and the OS-only
parts of `hub-core` (random ids, unix file helpers, the clock, the errno table; `getrandom` and `nix`) are compiled for
native targets only.

`make web` builds the module (wasm32-unknown-unknown, `opt-level = "z"`, `panic = "abort"`, the release path
rewrites) and runs `wasm-bindgen` 0.2.129 (the CLI must equal the crate in `Cargo.lock`) into `frontend/wasm/`, which
is generated, gitignored and never committed (an unreviewable binary in a security tool). It refuses a module that
still contains a path of the build machine or is larger than 1.5 MB. `build`, `release`, `frontend-test` and `e2e`
depend on it; `clippy` and `cargo-deny` also cover the wasm32 build.

The JavaScript UI stays (ADR-0008): `js/hubwasm.js` loads the module once and wraps it. Uses:

* Settings > Config files: hub env and knowledge drafts are checked in the tab, live and on Check. An env draft is
  **never sent**: the API client refuses it, and without the module it is not checked at all. A knowledge draft falls
  back to the server route. A client file's local half is checked live; Check asks the hub for the rest.
* The client wizard reads a pasted spec with the hub's YAML loader.
* The editor's blocking errors are the item model's. The 20-character description rule is a suggestion (a warning).
* The dev mock server and the node tests load the same module from its bytes.

The server stays authoritative: it checks again on every write. If the module does not load or traps (a panic aborts),
the UI says so (the editor shows a note rather than a clean state), saving still goes through the server, and the next
use loads a fresh instance; a failed load is tried again at most every 5 seconds. A view waits at most 3 seconds for
the module, so a stalled fetch never holds it, and the client wizard's starting spec is JSON when the YAML reader is
not there, so its own default never blocks Next.

The Content-Security-Policy gains one token, `script-src 'self' 'wasm-unsafe-eval'`: the page may compile its own
same-origin module; JavaScript `eval` and inline scripts stay blocked (`backend/PORTING.md` A-19). The module imports
nothing from the page (no network, DOM or storage): it takes text and returns JSON.

## Consequences

* One copy of each rule. An env draft, secrets included, stays in the browser and every value is checked.
* ADR-0008 changes in one place: there is now one generated folder, built by cargo and `wasm-bindgen`, both pinned.
  Still no npm runtime dependency, no bundler, no CDN; the JS is still served as written.
* A developer who runs the UI or the node tests needs `make web` once (and `wasm-bindgen-cli` 0.2.129). Without it the
  node tests that need the module skip with a message and the UI degrades as above.
* About 1.2 MB of WebAssembly (about 430 KB gzipped) is fetched when a page first needs a check (the editor, the client
  wizard, Settings > Config files), then revalidated with an ETag like every static file.
* `/health` still reports `version: "0.3.0"` (a value the parity harness normalises), so the page cannot compare the
  module's version with the hub's; a glue file that lacks part of the export contract is refused instead, and every
  static file is served `no-cache`.
* The Form view's TOML parser (ported from Nemo Console) is deleted: no config file here is TOML.
* Kept in JavaScript on purpose: the Form view's span-preserving parsers (`cfg-parse.js`, including `yamlPlainType`,
  which the Form needs synchronously; a node test checks it against `yaml_scalar_type`), UX hints with no server rule,
  rendering, search, diff and i18n.

## Alternatives considered

* **Leptos for the whole UI, or for the config editor only.** Rejected for now: a rewrite (or two UI paradigms), many
  more crates and a larger module, for drift that linking `hub-core` already removes. Revisit if the API becomes typed
  (a shared crate of serde types) or the UI outgrows plain modules.
* **Golden vectors written by a Rust test and asserted by the node tests.** Catches drift without a build step, but
  keeps both copies and the need to mask env drafts before sending them.
* **Keep sending env drafts, masked.** Rejected: the masking is a heuristic on key names.
