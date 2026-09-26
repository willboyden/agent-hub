# ADR-0008: Zero-build, zero-npm frontend

Status: accepted (decision 8). Implemented.

## Context

No supply-chain surface for an app that steers agents, no build step, no CDN (the CSP forbids
external origins).

## Decision

Plain ES modules and web components served by the backend from `frontend/` (`index.html`, `css/`, `js/`, `i18n/`); no
inline scripts or styles (CSS classes and CSSOM only); all strings from `i18n/en.json`; a contract-faithful
`dev/mock-server.mjs`. `package.json` exists only so Node treats `js/` as ES modules for `node --test`.

## Consequences

* Nothing to install to run; nothing to audit except our own code; fast start.
* Cost: hand-rolled pieces (a small YAML reader, markdown renderer, fuzzy matcher, drag-and-drop with keyboard
  alternatives) that a framework would give for free, and that we must keep accessible ourselves.
* Cost: English only; WCAG AA and responsive behaviour are goals, verified only for what was viewed.
* Cost: the mock server can drift from the backend; it is not proof of behaviour.

## Alternatives considered

* **React/Vue/Svelte with a build.** Rejected: dependency and build surface for a single-operator tool.
* **Server-rendered HTML.** Not chosen: drag-and-drop, matrix toggles and unsaved-change bars are client-side
  interactions, and the JSON API had to exist for scripting anyway.
* **CDN-hosted libraries.** Forbidden by the CSP and by the egress posture.
