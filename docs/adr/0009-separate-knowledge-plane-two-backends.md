# ADR-0009: The knowledge plane is a separate component with two vector backends

Status: accepted (decision 9). Implemented.

## Context

Agents benefit from shared retrieval, but they are untrusted, Hermes cannot reach host services, host RAM is scarce, and
both Qdrant and LanceDB were wanted. Content governance (the hub) and data (vectors) have different failure
modes and sizes.

## Decision

A separate process and package (`hub/knowledge`, `agent_knowledge`, 127.0.0.1:8795) behind a `VectorStore` port with
two implementations chosen **per namespace**: Qdrant (service, HNSW on disk) and LanceDB (embedded, flat scan). The
hub only links to it through an admin-token same-origin proxy. Untrusted agents get **per-client scoped tokens, read by
default, no wildcard namespaces**, and internal-net clients reach it through a read-only broker template that injects the
token and allows only `POST /namespaces/<ns>/query` and `GET /health`. Embeddings come from the router's
`/v1/embeddings`. Metadata filters are a validated AST, never string-built.

## Consequences

* Isolation: a knowledge bug or a large corpus cannot take the policy plane down, and vice versa.
* Choice per corpus: LanceDB (one directory, lowest idle RAM, latency grows with rows) by default up to a few hundred
  thousand chunks; Qdrant for larger, shared or heavily filtered corpora (one more container and page cache).
* Cost: two backends means two code paths and behaviour that differs (filters; Qdrant's disk size is an estimate).
* Cost: the container and broker path is a template to adapt; the host-process deployment is the simpler path.
* Since the second audit: a keyed Qdrant is required by the compose template, the embeddings key is a dedicated env name that must be an embeddings-only virtual key, and stores have per-namespace locks and timeouts.
* Cost: a write-scoped token in an untrusted agent's hands is a data-poisoning risk the code cannot prevent.
* Not built: LanceDB ANN index.

## Alternatives considered

* **Qdrant only.** Both were wanted; LanceDB removes a service for small corpora.
* **LanceDB only.** Rejected: no concurrent readers or native payload filters at scale.
* **pgvector / Chroma / RAM-disk stores.** Skipped: no agent-facing store of that kind was already in place, and host RAM can be scarce.
* **Fold it into the hub process.** Rejected: different trust and resource profile; agents would be one hop from the policy plane.
