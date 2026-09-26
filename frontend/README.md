# Agent Hub frontend

The web UI for Agent Hub: organise skills, agents, instructions, MCP servers, rules and memory across many agent clients, review a plan, then apply it.

- **Zero build, zero npm dependencies.** Plain ES modules and web components served as static files; strict CSP (no inline scripts or styles, no CDN, no eval).
- **Tests:** `npm test` (Node's built-in test runner; pure logic, the API client, the markdown XSS corpus and the mock server).
- **Try it without a backend:** `node dev/mock-server.mjs` serves the UI and a contract-faithful mock API on `http://127.0.0.1:8794/` (flags: `--port`, `--empty` for a first-run state, `--trust` for the trust warning). It uses invented data only.
- **Against a real backend:** the hub serves this directory itself; nothing to build.

Layout: `js/` (views, components, pure helpers), `css/`, `i18n/en.json` (every user-visible string), `dev/` (mock server and seed data), `tests/`.
