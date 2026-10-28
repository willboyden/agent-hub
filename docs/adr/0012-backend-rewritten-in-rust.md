# ADR-0012: The backend and the knowledge service rewritten in Rust (0.3.0)

Status: accepted. Implemented in 0.3.0. Amends ADR-0002 (the adapter port is a Rust trait; a coded adapter is one Rust
file) and ADR-0007 (the hardened loopback service is a Rust server on hyper, not FastAPI); every other decision is unchanged.

## Context

The hub writes other tools' configuration and holds the policy floor, so its correctness and its supply chain matter
more than its size. The Python 0.2.x backend needed a virtualenv per component, a pinned interpreter (3.13; 3.14 lacked
wheels) and the packages of two lockfiles (49 distinct packages, development tools included), some of them native
wheels, which is why the systemd templates left `MemoryDenyWriteExecute` off as a precaution.

## Decision

Port the hub, `hubctl` and the knowledge service to one Rust workspace (`backend/`), behaviour-compatible with 0.2.x:
the same HTTP API (routes, status codes, problem bodies, headers), the same files written byte for byte (JSON, a port of
PyYAML 6.0.3's safe dumper and loader, Markdown), the same state on disk (the Rust server opens what Python wrote), the
same subprocess argv. Compatibility is proven, not assumed: every Python test has a Rust counterpart
(`backend/TEST-MAP.md`), and a differential harness (`backend/parity/`) sends the same requests to both implementations
in the same temporary layout and compares responses and the files each wrote. Deliberate differences are few, listed
in `backend/PORTING.md` and `CHANGELOG.md`; three of them fix security problems the port found in 0.2.x.

## Consequences

* One `cargo build`; the binaries need no interpreter at run time. protoc, needed only to build the Lance engine, is
  vendored.
* The dependency policy is enforced in CI: cargo-deny, cargo-audit, and a 14-day cooldown on every locked crate
  (`backend/tools/crate-age.py`; security fixes are the only exceptions).
* A coded client adapter is one Rust file (the build discovers it) instead of one Python module.
* The Python 0.2.x code is no longer in the tree; the harness rebuilds it from git (`--a python@<ref>`) when a
  comparison is needed.
* Cost: a longer first build (minutes, for the Lance engine) and a stricter language for contributors.

## Alternatives considered

* **Stay on Python.** Rejected for the interpreter and dependency surface above.
* **Port only the hub.** Rejected: the knowledge service shares the same hardening code and would keep the Python
  surface alive.
* **A rewrite without the compatibility harness.** Rejected: the files the hub writes are other tools' configuration,
  and a silent difference there is a real problem for the user.
