# ADR-0007: Host-run FastAPI on 127.0.0.1:8792, hardened loopback service, loopback trust off

Status: accepted (decision 7). Implemented.

## Context

A hardened loopback-service skeleton (Host allowlist, same-origin Origin, CSRF header, strict CSP, no CORS, role checks,
caps) came out of two audits. The hub can rewrite agent policy, which is a higher-value target. Agents with shells run on
the same host.

## Decision

Reuse the pattern (re-implemented from earlier local tools, nothing imported): a pure-ASGI middleware, roles enforced twice
(middleware and a dependency on every router), a fixed viewer/client allowlist, body caps, SSE caps, append-only audit,
no OpenAPI docs page (the CSP would forbid its CDN). The knowledge service is a separate process on 8795. **Unlike
those earlier tools, `HUB_TRUST_LOOPBACK` defaults to false** (`config.py`), and `hubctl` also requires an admin key
(`HUB_API_KEY` or the 0600 bootstrap file).

## Consequences

* An unsandboxed agent with a shell cannot call the API merely by being local.
* Cost: friction; you paste a key into the UI (kept in per-tab `sessionStorage`) and `curl` needs a header.
* Residual: a same-user process that can read `~/.local/share/agent-hub/bootstrap-admin.key` is admin. The control that
  matters is the client sandbox denying that path; whether a given client's sandbox denies that path is what `hubctl doctor` checks, and an unsandboxed client cannot be
  helped by the hub.
* Fixed since the first build (Addendum D): the Host allowlist follows the effective bind, so `hubctl serve --port N` works
 , and `ARCHITECTURE.md` Addendum A now states the false default. A non-loopback
  `HUB_HOST` is refused unless `HUB_ALLOW_NONLOOPBACK=1`.
* Recommended key handling: password manager or `HUB_API_KEY` in the 0600 env file, then delete the bootstrap file.

## Alternatives considered

* **Trust loopback** (the pattern of earlier local tools). Rejected here, precisely for the agent-with-a-shell case.
* **Unix domain socket instead of TCP.** Not built: the browser UI needs HTTP, and a socket permission model was not explored.
* **mTLS or per-request signed tokens.** Overkill for one operator and one user account; would not stop a same-user process either.
* **A shared library with the earlier tools.** Deferred until a third consumer exists.
