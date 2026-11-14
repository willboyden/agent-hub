# ADR-0008: Zero-build, zero-npm frontend

Status: accepted (decision 8). Implemented. Amended by ADR-0013 (shared Rust checks in the browser).

## Context

No supply-chain surface for an app that steers agents, no build step, no CDN (the CSP forbids
external origins).

## Decision

Plain ES modules and web components served by the backend from `frontend/` (`index.html`, `css/`, `js/`, `i18n/`); no
inline scripts or styles (CSS classes and CSSOM only); all strings from `i18n/en.json`; a contract-faithful
`dev/mock-server.mjs`. `package.json` exists only so Node treats `js/` as ES modules for `node --test`.

Amendment (2026-11-06): zero *runtime* npm dependencies. `package.json` also pins one devDependency, the Prettier
formatter (exact version, committed `package-lock.json`, `make fmt` / `make fmt-check`, checked in CI). It is a
development tool only: nothing of it is served (the hub serves `index.html`, `css/`, `js/`, `i18n/` and, since
ADR-0013, the generated `wasm/` only) or built into a binary, and nothing is needed to run the app.

Amendment (2026-11-14, ADR-0013): one generated folder. `make web` compiles the backend's own `hub-core` checks to a
WebAssembly module with cargo and `wasm-bindgen` (both pinned) into `frontend/wasm/` (gitignored, never committed),
which the hub serves next to `js/`. The JavaScript is still served as written: no bundler, no npm runtime dependency,
no CDN. The CSP gains `'wasm-unsafe-eval'` for that same-origin module only. The hand-rolled YAML reader for pasted
client specs (`yaml-lite.js`) is gone: the hub's own loader runs in the page instead.

## Consequences

* Nothing to install to run; nothing to audit except our own code (and, since ADR-0013, `wasm-bindgen`'s glue); fast
  start.
* Cost: hand-rolled pieces (a small YAML reader until ADR-0013, markdown renderer, fuzzy matcher, drag-and-drop with
  keyboard alternatives) that a framework would give for free, and that we must keep accessible ourselves.
* Cost: English only; WCAG AA and responsive behaviour are goals, verified only for what was viewed.
* Cost: the mock server can drift from the backend; it is not proof of behaviour.

## Alternatives considered

* **React/Vue/Svelte with a build.** Rejected: dependency and build surface for a single-operator tool.
* **Server-rendered HTML.** Not chosen: drag-and-drop, matrix toggles and unsaved-change bars are client-side
  interactions, and the JSON API had to exist for scripting anyway.
* **CDN-hosted libraries.** Forbidden by the CSP and by the egress posture.
