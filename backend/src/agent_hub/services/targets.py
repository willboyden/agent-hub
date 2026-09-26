"""Where the hub may deliver files. A client's roots and every artifact target are checked against the floor's write denies,
the hub's own state, the app's policy/backend dirs, `.git` internals and the home dir itself (a root of `~` is refused)."""
from __future__ import annotations

import os
from pathlib import Path

from agent_hub.services.floor import FloorPolicy, norm_path

_SECRET_HOME_DIRS = (".ssh", ".aws", ".config/gcloud", ".gnupg", ".config/agent-hub", ".config/agent-knowledge",
                     ".local/share/agent-hub", ".local/share/agent-knowledge")


def _real(p: str | Path) -> Path:
    return Path(os.path.realpath(p))


def _stem_dir(glob: str) -> Path | None:
    g = norm_path(glob)
    for i, ch in enumerate(g):
        if ch in "*?[":
            g = g[:i]
            break
    g = g.rstrip("/")
    return _real(g) if g.startswith("/") and len(g) > 1 else None


class TargetGuard:
    def __init__(self, policy: FloorPolicy, data_dir: Path, content_dir: Path, app_root: Path, home: Path | None = None) -> None:
        self.home = _real(home or Path.home())
        secret = [self.home / d for d in _SECRET_HOME_DIRS] + [_real(data_dir)]
        secret += [d for g in policy.deny_path_write if (d := _stem_dir(g)) is not None]
        # nothing may be delivered INTO these, and no root may CONTAIN them (a root of ~ would contain them)
        self.secret = [_real(s) for s in secret]
        # nothing may be delivered into these (but a broad project root may legitimately contain them)
        # the whole app tree except out/ (the staging area adapters may write into)
        self.inside_only = [_real(content_dir), _real(app_root)]
        self.allowed_under = [_real(app_root / "out")]

    @staticmethod
    def _within(p: Path, d: Path) -> bool:
        return p == d or d in p.parents

    def root_problem(self, root: str | Path) -> str | None:
        rp = _real(root)
        if rp == Path("/") or rp == self.home or rp in self.home.parents:
            return f"root {rp} is the filesystem root or the home directory (or an ancestor of it)"
        if ".git" in rp.parts:
            return f"root {rp} is inside a .git directory"
        for s in self.secret:
            if self._within(rp, s) or self._within(s, rp):
                return f"root {rp} overlaps the protected path {s} (floor write deny / hub state)"
        for d in self.inside_only:
            if self._within(rp, d) and not any(self._within(rp, x) for x in self.allowed_under):
                return f"root {rp} is inside the protected path {d}"
        return None

    def target_problem(self, target: str | Path, rel: str, generic: bool = False) -> str | None:
        tp = _real(target)
        segs = rel.split("/")
        if ".git" in segs or (generic and "hooks" in segs):
            return f"artifact path {rel!r} has a .git{' or hooks' if generic else ''} segment"
        if any(self._within(tp, x) for x in self.allowed_under):
            return None
        for d in [*self.secret, *self.inside_only]:
            if self._within(tp, d):
                return f"artifact target {tp} is inside the protected path {d}"
        return None

    @staticmethod
    def sensitive_target(adapter_id: str, manage: dict[str, bool], kind: str, rel: str) -> str | None:
        """Files that configure the agent's own permissions or repo: no adapter may write them unless it explicitly owns them
        (claude-code: .claude/settings.json only with manage.permissions; opencode/generic: AGENTS.md as instructions)."""
        segs = rel.split("/")
        if segs[0] == "security":
            return f"artifact path {rel!r} is under security/"
        if ".git" in segs:
            return f"artifact path {rel!r} is inside .git"
        if rel.endswith(".claude/settings.local.json"):
            return f"artifact path {rel!r} is the user's local settings file"
        if rel.endswith(".claude/settings.json") and not (adapter_id == "claude-code" and manage.get("permissions") and kind == "rule"):
            return f"artifact path {rel!r} may only be written by claude-code with manage.permissions enabled"
        if segs[-1] == "AGENTS.md" and not (adapter_id in ("opencode", "generic") and kind == "instruction" and manage.get("instructions")):
            return f"artifact path {rel!r} (AGENTS.md) may only be written as managed instructions by an adapter that declares it"
        return None
