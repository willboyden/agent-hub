# ADR-0005: Progressive adoption: managed vs advisory per concern, with a lock manifest

Status: accepted (decision 5) with a later refinement (Addendum C client status). Implemented.

## Context

Many setups already have authoritative sync scripts or dotfile managers that render a client's sandbox settings, Hermes
config and more. A new tool that overwrites what those own would break working setups.

## Decision

Per client and per concern (`skills, agents, instructions, mcp, permissions, memory`) a `manage` flag chooses **managed**
(the hub writes) or **advisory** (the plan shows what would change; `audit` checks the live config against the floor).
Defaults: skills, agents, instructions managed; mcp, permissions, memory advisory. A per-client **lock manifest** in the
state dir records the whole-file hash and the hub-owned slice hash of everything delivered. Apply refuses to overwrite a
file whose live slice differs from the lock (`conflict`) unless the path is explicitly adopted (`--adopt`). The hub only
deletes files it created whose hash still matches. Merge modes (`json_keys`, `yaml_keys`, `block`) leave everything the
hub does not own untouched. Import records the live state as adopted so the first plan is not a wall of conflicts.

## Consequences

* You can adopt one concern at a time and never lose hand edits silently.
* Cost: two sources of truth exist during adoption, and the state dir is critical: losing the lock turns every existing
  file into a conflict.
* Cost: floor rules for advisory concerns are reported, not enforced, so the safety of `permissions` depends on the
  client's own config until you opt in.
* Cost: agents are re-rendered, not copied, so imported agents are not byte-identical and appear as conflicts to review on first apply.
* Cost: opencode `AGENTS.md` in `own` mode becomes a generated, hub-owned file; edits there must go through the hub.
* Resolved (Addendum D): effective `manage` is the client's `manage`, else `params.manage`, else the adapter's documented default, else the hub default. An earlier build ignored the adapter defaults.
* Consequence for safety: `permissions` is advisory for all four current clients, so the floor's read-deny of the hub state dirs is audited but not delivered.

## Alternatives considered

* **Hub owns everything from day one.** Rejected: clobbers the outputs of existing sync scripts.
* **Never write, only advise.** Rejected: defeats the purpose for skills and agents, where the hub is the editor you want.
* **Three-way merge of live edits back into content.** Not built: too much implicit magic for files that steer agents; conflicts are shown and the human decides.
