# ADR-0011: Evaluate before import: observe what clients hold, gate on static security findings

Status: accepted (Addendum F, F14 in `docs/ARCHITECTURE.md`). Implemented; covered by fake-runner tests, not exercised
against a real SkillEvaluator install in this repository's test suite.

## Context

A client usually holds more skills than the hub manages. A self-improving agent writes its own, a client release ships its
own, another sync tool drops some in. The hub could not see them, and a hub copy of a skill goes stale as soon as the live one
changes. Importing them is the natural next step, but an agent-written skill is untrusted text that becomes prompt content
for every client it is enabled for, and the content repo is the source of truth. Nothing new should enter it unexamined.

## Decision

1. **Observe, then compare.** `ClientAdapter.observe()` returns what the client really has (path, name, origin, files). The
   hub stores it as an untrusted snapshot and compares content hashes with its own skills (`hub_status`). The Hermes adapter
   reads a running container read-only through one `docker exec` of a constant program.
2. **Evaluate with NVIDIA SkillEvaluator**, as a main view, for hub and observed skills alike, through one FIFO queue,
   reusing a finished run whenever the same content hash was already evaluated. Every evaluation runs a static pass first
   (`--no-llm`) with an explicit check list that never includes `dependency`; the optional LLM pass comes second.
3. **Gate import on the static security result, failing closed.** The gate is `off`, `advisory` or `require`, set
   separately for the discover import (`eval_gate`, default `advisory`) and for observed import (`eval_gate_observed`,
   default `require`: observed skills may be agent-written). A skill is `blocked` on a critical or high finding from a
   security validator, when one of the six expected security validators is absent from the report, or when any security
   validator did not finish (status other than `passed` or `failed`). No usable report is `missing`. The LLM pass can add
   blocks, never clear a static finding. In `require` mode a blocked skill imports only with an override reason (10 to 500
   characters) recorded in the audit log and the skill's provenance. Observed import also refuses text with secret-shaped
   values (`secret_detected`) unless overridden.
4. **Observed import never enables by default.** Enabling an observed skill for the client it came from would deliver a
   second copy next to the original.

## Consequences

* A reviewer sees where each skill came from, what was found, and what the operator overrode. Identical content is
  evaluated once.
* The gate is a filter, not a proof. Static scanners miss things, and the hub still cannot judge whether a prompt is malicious;
  commit review stays the real control.
* Cost: evaluation runs third-party code (SkillEvaluator and the scanners it drives) as the hub's user, and the static pass of
  SkillSpector queries `api.osv.dev` for skills that declare dependencies. Offline, those skills come out `incomplete`
  and the gate blocks them (below).
* Cost, deliberate: SkillSpector often reports `incomplete` (it could not fully analyse files a skill references). That
  blocks, so such skills need a reason-bearing override before they can be imported under `require`. Treating "the scan did
  not finish" as safe would let skill text that defeats the scanner through.
* Addendum (0.2.0): the most common `incomplete` was not a scan that did not finish. SkillSpector 2.12 marks its analysis
  `partial` when a SKILL.md merely mentions a local path the skill does not bundle; SkillEvaluator 0.5.0 turns that into an
  incomplete Security Scan. On 61 real skills that was the only reason for 38 of 55 incomplete scans. For exactly that pattern
  the hub reads SkillSpector's own report of the same copy and the gate warns (`unresolved_references`, the references named)
  instead of blocking; every other partial reason still blocks (`docs/SECURITY.md` section 14).
* Cost: the queue is in memory. A hub restart drops queued jobs and records the running one as `error` (`interrupted`).
* Cost: with `require`, every new skill needs an evaluation first, which needs SkillEvaluator installed.
* Not covered: cross-skill duplication (Tier 2 runs without a catalog) and live agent runs (Tier 3).

## Alternatives considered

* **Import everything blindly.** Rejected: it moves agent-written text into the source of truth with no signal.
* **Ignore unmanaged skills.** Rejected: the hub's view of a client would stay wrong, drift could not be reported, and
  nothing could be adopted.
* **Gate on the overall verdict.** Rejected: SkillEvaluator's publication schema checks (a missing `metadata.author`, version,
  license) fail most private skills and say nothing about safety. The gate would block everything or be overridden every time.
  Those validators are excluded; unknown validators count as security (fail closed).
* **Gate on the LLM pass.** Rejected: the LLM finding verifier can downgrade findings to `info`, and the skill text it reads
  can steer it. The LLM pass may only add blocks.
* **Run SkillEvaluator's default checks.** Rejected: they include `dependency`, which runs pip-audit on the skill's
  requirements files and thereby installs packages the skill's author chose, on the host.
* **Tier 3 live agent evaluation.** Out of scope: its stock sandboxes need public network and install agent CLIs at run time.
* **Cross-skill Tier 2 with a catalog.** Deferred: a skill that is in the catalog matches itself as an exact duplicate;
  it needs leave-one-out catalogs.
