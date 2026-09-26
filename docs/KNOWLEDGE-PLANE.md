# Knowledge plane (`hub/knowledge`, package `agent_knowledge`)

A separate process (127.0.0.1:8795, console script `agent-knowledge`) that stores and searches document chunks for agents.
Contract: `ARCHITECTURE.md` §7. It is **not** part of the Hub backend; the Hub only links to it.

## Architecture

```
operator/Hub --admin key--> agent-knowledge (host, 127.0.0.1:8795)
agent client --scoped token (read by default)--> [broker sidecar] --> agent-knowledge
                              |                         |
                    SQLite registry (0600)      VectorStore port  --> QdrantStore (REST)  --> Qdrant service (SSD)
                    namespaces, hashed tokens,                   \--> LanceStore (embedded) --> <lance_dir> on SSD
                    usage, index registry       Embedder port     --> RouterEmbedder --> <router>/v1/embeddings
```

* One **namespace** = one collection/table, bound to a backend, an `embedding_model` and a `dim` at creation. These are
  immutable (`PUT` refuses a change with 409); writes/queries whose vector length or model differ are refused (409).
* **Ingest** takes inline `documents` (any write token) or `{path, glob}` (admin only, allowlisted roots), chunks with
  overlap (default 1000/200 chars), embeds through the router in batches, replaces a re-ingested document's old chunks,
  and runs as a **job** (`queued -> running -> done|failed`, progress counters, SSE at `/jobs/{id}/stream`).
* **Query** takes `text` (embedded via the router) or a raw `vector`, plus `k`, `min_score` and a `filter`.
* Filter grammar (validated into an AST; never string-built): `{"field": value}`, `$eq $ne $gt $gte $lt $lte $in`,
  `$and $or $not`; field names `[A-Za-z_][A-Za-z0-9_]{0,63}`, depth <= 4, <= 32 nodes. Qdrant gets structured JSON;
  LanceDB never receives filter text (see below).
* Chunk metadata is flat scalars; `doc_id`, `chunk`, `source` are reserved and set by the service.

## Qdrant (service) vs LanceDB (embedded)

| | Qdrant | LanceDB |
|---|---|---|
| Shape | separate container, REST | in-process library, files under `lance_dir` |
| Index | HNSW, **on disk** (graph and vectors) as configured by the app | none built by this app: flat scan of the (SSD) table |
| Filtering | native payload filters | `doc_id` equality uses a column predicate (no scan); any other filter evaluates the AST in Python over `(id, meta_json)` (refused with 422 `scan_cap` above `lance_max_scan_rows`, default 200k; result cap 50k), then an `id IN (...)` prefilter; ids match a strict regex so the clause cannot carry SQL |
| Good for | > ~1M chunks, concurrent readers, filtered search at scale, an isolated blast radius | small/medium corpora, zero extra service, one directory to back up, lowest idle RAM |
| Costs | one more container, its page cache, an image to pin | search latency grows linearly with rows; broad filters are capped; single-process |

Choose **LanceDB** by default for per-project corpora up to a few hundred thousand chunks. Choose **Qdrant** when a corpus is
large, shared by several clients, or filtered heavily. Both can coexist; the backend is per namespace.

**Sizing (generic guidance).** Raw vectors are `chunks x dim x 4 bytes` (768-d: about 3 KB/chunk; 1M chunks = about 3 GB),
plus text/payload (about the chunk size) and, for Qdrant, HNSW links (roughly 100-200 B/vector at default `m`). Everything is
on disk; RAM use is page cache plus a few hundred MB of service. On a memory-constrained machine, cap the container (`mem_limit` in
the template) so the page cache does not compete with other workloads. Keep the data on an SSD path, not tmpfs. Qdrant's REST API
exposes no on-disk size, so its `disk_bytes` is flagged `disk_bytes_estimated: true`; LanceDB's is the real directory size.
Not implemented: a LanceDB ANN index (IVF-PQ); add one if a table passes a few hundred thousand rows and latency matters.

## Reaching it from an internal-net client (broker)

Sandboxed agents often sit on `--internal` container networks, and with rootless Docker/Podman a container usually cannot reach host
loopback, so a host process on 8795 is invisible to them. Use `hub/deploy/knowledge/docker-compose.broker.yml`: a **credential-broker
sidecar** (a mitmproxy reverse proxy with a small addon) on the client-facing internal network, forwarding to the knowledge service
as a container on a second internal network. The client holds a dummy key; the broker injects the real **read-only** token (env NAME
`AGENT_KNOWLEDGE_TOKEN`) and allows exactly `POST /namespaces/<ns>/query` (ns from `KNOWLEDGE_ALLOWED_NS`, body <= 64 KB) and
`GET /health`. Ingest, delete, tokens, namespaces, indexes, backends, metrics and jobs are 403 at the broker even if the token allowed them.
The embeddings endpoint for text queries should be reached through its own credential broker (not done in the template). Network names
are parameters (`AGENT_CLIENT_NETWORK`, `AGENT_KNOWLEDGE_NETWORK`); create them with `docker network create --internal <name>`.
`docker-compose.qdrant.yml` is the loopback-published Qdrant for the host-process deployment. Templates are opt-in and never applied by the app.

## Security model (untrusted agents: read-mostly)

* Loopback bind only (refuses otherwise unless `--allow-bind-any`, meant for the container). Host allowlist (421), same-origin
  `Origin`, `X-Agent-Knowledge: 1` on non-GET for callers without a bearer, strict CSP/headers, no CORS, `no-store`, body cap (413,
  also for chunked bodies), RFC 7807 errors that never echo input, no OpenAPI/docs endpoints.
* **Every** route (except `/health`, which returns only `ok`+version) is behind bearer auth; each handler then names its scope.
  Admin key: bootstrap file `<data_dir>/admin.key` (0600). Client tokens: `akn_<id>_<256-bit>`, only the SHA-256 is stored, shown once,
  scoped `{ns, mode}` with **read as the default**; no wildcard namespaces. Non-admins get 404 (not 403) for namespaces they hold no
  scope on. Dropping a namespace strips its grants so a re-created name inherits nothing.
* Admin-only: namespace create/update/drop, `path` ingest, tokens, indexes, `/backends`, `/metrics`.
* Store abuse limits: LanceDB work runs on a bounded thread pool with a PER-NAMESPACE lock (a busy namespace answers 429 `store_busy` after `lance_lock_wait_s`; a slow call answers 429 `store_timeout` after `store_timeout_s`); filters are capped at `max_filter_nodes` (default 16) and `k` at `max_k`. A timed-out worker cannot be killed, so it finishes in the background.
* Router key: the service uses a DEDICATED env var, `KNOWLEDGE_EMBED_API_KEY` (config `router_api_key_env`), which must hold an **embeddings-only virtual key issued by the router, never the master key**. `/backends` and the startup log warn if the name is `LITELLM_API_KEY`/`LITELLM_MASTER_KEY` or contains `MASTER` ; additionally at startup it warns if the embed key's VALUE equals `LITELLM_MASTER_KEY`/`LITELLM_API_KEY` in the environment (names only are logged, never values).
* Qdrant key: Qdrant must run with `QDRANT__SERVICE__API_KEY` (compose template requires an env file), and the service sends it from env NAME `KNOWLEDGE_QDRANT_API_KEY`; otherwise any local process could bypass token scopes. The backend is REFUSED (503 `qdrant_unauthenticated`, reason shown in `/backends`, logged at startup, re-probed every 30 s) if Qdrant answers without a key, unless `KNOWLEDGE_ALLOW_UNAUTH_QDRANT=1` is set. Generate: `openssl rand -hex 32` into two 0600 files outside the repo (see the compose template).
* Write jobs and deletes are serialised per namespace; the chunk cap is checked-and-reserved atomically (inline and path ingest alike). Paths with empty, `.`/`..` or backslash segments are rejected 400.
* Admin scope is required for `/tokens`, `/indexes`, `/backends`, `/metrics` (route-table test).
* Limits: per-token sliding-window rate limit, per-token daily query and chunk-write quotas, per-namespace chunk cap, doc/metadata/`k`/query-length caps, job file/byte caps.
* Path ingest: absolute path resolved (symlinks followed) and required under a configured root; glob is character-restricted and cannot contain `..`;
  each file is re-resolved and skipped if it escapes the root; `.env*`, `.ssh`-style secret directories, `.git`, binaries and > 2 MB files are skipped.
* Secrets: router/Qdrant keys are env var **names** in config; values are never logged, echoed or stored (`/backends` shows `[set]`/`[unset]`);
  the router client does not follow redirects, so the bearer cannot be replayed to another host.
* Audit log (`audit.jsonl`, 0600): mutating calls, metadata only, never bodies.
* Authorisation and scope checks run as route dependencies before body validation; a route-table-walking test proves invalid bodies get 401/403/404, never 422.
  Memory poisoning of a namespace by a write token is a data risk, not a code one: give untrusted agents read tokens; ingest through a human-run job.

## Tests and configuration notes

* Tests that need a real LanceDB or Qdrant are marked `live` and skipped unless `AK_LIVE=1`:
  `AK_LIVE=1 AK_LIVE_QDRANT_URL=http://127.0.0.1:<port> uv run pytest -m live` (use a throwaway Qdrant).
* The embedding model alias and its dimension must match what your router returns (for example a 768-dimension model).
* The compose files parse with `docker compose config`; the Dockerfile and broker are templates to adapt to your environment.
* Not implemented: a LanceDB ANN index (IVF-PQ); add one if a table passes a few hundred thousand rows and latency matters.
