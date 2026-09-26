# ADR-0003: Edit, plan, apply, with a git commit as the approval of content

Status: accepted (decision 3). Implemented.

## Context

The hub writes files that steer sandboxed agents. Toggling something in a UI must not silently change what an agent may do.

## Decision

Edits (UI, CLI, import) change only the content **working tree**. A **commit** approves content and is refused if content
does not validate. A **plan** renders the committed tree per client and shows a redacted unified diff, diagnostics,
floor violations and conflicts. **Apply** needs `confirm: true` and a plan bound to the content tree hash; it recomputes
each client against the live files and refuses with `plan_stale` if the digest differs. A working-tree **preview plan**
exists for looking ahead and cannot be applied (`plan_is_preview`). Apply is all-or-nothing per client (stage, atomic
renames, rollback from backups), then verify.

## Consequences

* Every effect is diffable, auditable and reproducible from a commit.
* Cost: friction. A one-checkbox change is edit, commit, plan, apply. The UI shortens it (unsaved bar, preview) but does not remove it.
* Cost: two approvals in spirit (commit, then apply) but one human; this is a review habit, not separation of duties.
* Cost: `revert` reverts a **content commit**, not an apply. Undoing an apply is `hubctl rollback` (restores files while their live hash still equals the post-apply hash), or a manual copy from the backups directory.
* Cost: real-world apply failure modes (ownership, read-only mounts, editors holding files) surface as a failed, rolled-back apply.

## Alternatives considered

* **Live toggles that apply immediately.** Rejected: the whole point is containment and review (the project's principles: containment and observability first).
* **Apply straight from the working tree.** Rejected: a plan must refer to immutable content; the preview covers the wish to look ahead.
* **A separate "approve" state besides git.** Rejected as extra machinery; a commit is already an immutable record.
