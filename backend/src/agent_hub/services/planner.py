"""Planner: pure. Content catalog + live files + lock -> a per-client plan. Writes nothing (the applier does).

Per file: add | change | remove | unchanged | conflict | advisory (ARCHITECTURE section 4)."""
from __future__ import annotations

import difflib
import hashlib
import json
from dataclasses import dataclass, field
from typing import Any

from agent_hub.adapters import AdapterRegistry
from agent_hub.adapters.base import Diag
from agent_hub.domain.ids import CONCERN_OF_KIND
from agent_hub.domain.models import ClientPlan, Finding, PlanFile
from agent_hub.domain.pathsafe import PathError
from agent_hub.services.common import scrub, sha256
from agent_hub.services.content import Catalog
from agent_hub.services.fs import LiveFiles
from agent_hub.services.lock import Lock, LockEntry, LockStore
from agent_hub.services.merge import Desired, Merged, merge_file
from agent_hub.services.resolver import Resolved, Resolver

# Diagnostics that make the plan meaningless: they block apply even for a non-strict client.
HARD_CODES = {"bad_root", "adapter_missing", "unsafe_path", "unknown_root", "duplicate_artifact", "merge_mode_mismatch",
              "render_failed", "invalid_content", "bad_artifact"}
MAX_DIFF = 100_000


@dataclass
class PlannedFile:
    desired: Desired
    action: str
    reason: str = ""
    merged: Merged = field(default_factory=Merged)
    live: bytes | None = None
    entry: LockEntry | None = None                     # previous lock entry
    adopted: bool = False
    final: bytes | None = None                         # whole file after apply (None: file deleted / not written)
    delete_file: bool = False
    adoptable: bool = True

    @property
    def key(self) -> str:
        return self.desired.key


@dataclass
class ClientPlanInternal:
    plan: ClientPlan
    files: list[PlannedFile]
    resolved: Resolved
    lock: Lock | None


def unified(live: bytes | None, new: bytes | None, path: str) -> str:
    a, b = live or b"", new or b""
    if b"\x00" in a or b"\x00" in b:
        return f"[binary file, {len(a)} -> {len(b)} bytes]\n"
    diff = difflib.unified_diff(a.decode("utf-8", "replace").splitlines(keepends=True),
                                b.decode("utf-8", "replace").splitlines(keepends=True),
                                "a/" + path if live is not None else "/dev/null",
                                "b/" + path if new is not None else "/dev/null")
    text = scrub("".join(diff))
    return text if len(text) <= MAX_DIFF else text[:MAX_DIFF] + "\n[diff truncated]\n"


def adopt_match(adopt: frozenset[str] | set[str], client: str, root: str, path: str) -> bool:
    return f"{root}:{path}" in adopt or f"{client}:{root}:{path}" in adopt


class Planner:
    def __init__(self, resolver: Resolver, registry: AdapterRegistry, locks: LockStore, max_live: int) -> None:
        self.resolver = resolver
        self.registry = registry
        self.locks = locks
        self.max_live = max_live

    # -- one client ----------------------------------------------------------------------------
    def plan_client(self, catalog: Catalog, client_id: str, adopt: frozenset[str] | set[str] = frozenset()) -> ClientPlanInternal:
        res = self.resolver.resolve(catalog, client_id)
        diags: list[Diag] = list(res.diagnostics)
        lock = self.locks.load(client_id)
        desired: list[Desired] = []
        if res.ctx is not None and res.adapter is not None and res.cfg is not None:
            try:
                rr = res.adapter.render(res.ctx)
            except Exception as exc:  # noqa: BLE001 - an adapter bug must not crash the hub
                diags.append(Diag(severity="error", code="render_failed", message=f"adapter raised {type(exc).__name__}"))
                rr = None
            if rr is not None:
                diags += rr.diagnostics
                desired, more = self._desired(res, rr.artifacts)
                diags += more
                res.findings += self._recheck_rendered(res, rr.artifacts)
        live = LiveFiles(res.cfg.roots if res.cfg else {}, self.max_live)

        files: list[PlannedFile] = []
        seen = {d.key for d in desired}
        # removals: lock entries the hub owns that are no longer rendered (only for concerns still managed)
        if lock is not None and res.cfg is not None:
            for key, e in sorted(lock.files.items()):
                concern = CONCERN_OF_KIND.get(e.kind, "skills")
                if key in seen or not res.manage.get(concern, False):
                    continue
                desired.append(Desired(e.root, e.path, e.kind, e.merge, e.source_ids, e.mode, e.managed_keys, None))
        groups: dict[str, list[Desired]] = {}
        for d in desired:
            groups.setdefault(d.key, []).append(d)
        for key in sorted(groups):
            files.append(self._plan_file(client_id, groups[key], live, lock, adopt, diags))

        findings = self._downgrade(res)
        cp = self._summarise(client_id, files, diags, findings, res)
        return ClientPlanInternal(cp, files, res, lock)

    def _recheck_rendered(self, res: Resolved, artifacts: list[Any]) -> list[Finding]:
        """The floor is enforced on what the adapter actually RENDERED, not just on what went in: an adapter bug (or a hostile
        spec) must not smuggle an unapproved MCP server or an allow entry for a floor-denied path/tool into a client config."""
        if res.ctx is None or res.adapter is None:
            return []
        from pathlib import Path

        from agent_hub.services.floor import norm_path
        from agent_hub.services.merge import _load_doc, get_dotted
        from agent_hub.services.resolver import client_caps
        floor = self.resolver.floor
        approved = {m.name for m in res.ctx.mcp_servers}
        caps = client_caps(res.adapter, res.client)
        native = {v for k in ("web_fetch", "web_search", "shell") if (v := caps.tool_map.get(k))}
        stems = []
        for g in floor.p.deny_path_read + floor.p.deny_path_write:
            s = norm_path(g)
            s = s[: min((s.find(c) for c in "*?[" if c in s), default=len(s))].rstrip("/")
            if len(s) > 3:
                stems.append(s)
        out: list[Finding] = []

        def bad(concern: str, item: str, msg: str) -> None:
            out.append(Finding(code="floor_violation_rendered", severity="error", concern=concern, item=item, message=msg))

        def walk(node: Any, key: str) -> None:
            if isinstance(node, dict):
                for k, v in node.items():
                    walk(v, str(k))
            elif isinstance(node, list) and "allow" in key.lower() and "disallow" not in key.lower():
                for e in node:
                    if not isinstance(e, str):
                        continue
                    low = e.replace("~/", str(Path.home()) + "/")          # compare in both the ~ and the absolute spelling
                    if e.strip() in native:
                        bad("permissions", e[:60], f"rendered config allows {e.strip()!r}, which the floor caps at 'ask'")
                    elif any(s in low or s in e for s in stems):
                        bad("permissions", e[:60], "rendered config allows an entry that touches a floor-denied path")
        for a in artifacts:
            if a.kind not in ("mcp", "rule") or not str(a.path).lower().endswith((".json", ".yaml", ".yml")):
                continue
            fmt = "json" if str(a.path).lower().endswith(".json") else "yaml"
            try:
                doc = _load_doc(bytes(a.content), fmt)
            except Exception:  # noqa: BLE001,S112 - not parseable here: the merge step reports it
                continue
            if a.kind == "mcp":
                for key in a.managed_keys or list(doc):
                    ok, val = get_dotted(doc, key)
                    if ok and isinstance(val, dict):
                        extra = sorted(set(val) - approved)
                        if extra:
                            bad("mcp", extra[0], f"rendered MCP server(s) {extra[:5]} are not in the approved (clean) set")
            else:
                walk(doc, "")
        return out

    def _desired(self, res: Resolved, artifacts: list[Any]) -> tuple[list[Desired], list[Diag]]:
        assert res.cfg is not None
        out: list[Desired] = []
        diags: list[Diag] = []
        live = LiveFiles(res.cfg.roots)
        for a in artifacts:
            concern = CONCERN_OF_KIND.get(a.kind)
            if concern is None or a.merge not in ("own", "json_keys", "yaml_keys", "block"):
                diags.append(Diag(severity="error", code="bad_artifact", message=f"artifact has unknown kind/merge: {a.kind}/{a.merge}"))
                continue
            if a.root not in res.cfg.roots:
                diags.append(Diag(severity="error", code="unknown_root", message=f"artifact targets unconfigured root {a.root!r}"))
                continue
            try:
                tgt = live.target(a.root, a.path)
            except PathError as exc:
                diags.append(Diag(severity="error", code="unsafe_path", message=f"{a.root}:{a.path[:80]!r}: {exc}"))
                continue
            guard = self.resolver.guard
            prob = guard.target_problem(tgt, a.path, generic=res.client.adapter == "generic") if guard else None
            if prob is None and res.manage.get(concern, False):          # advisory artifacts are never written, so never refused
                prob = guard.sensitive_target(res.client.adapter, res.manage, a.kind, a.path) if guard else None
            if prob:
                diags.append(Diag(severity="error", code="unsafe_path", message=prob[:200]))
                continue
            if a.mode & ~0o777:
                diags.append(Diag(severity="error", code="bad_artifact", message=f"{a.path}: setuid/setgid/sticky modes are refused"))
                continue
            out.append(Desired(a.root, a.path, a.kind, a.merge, list(a.source_ids), a.mode, list(a.managed_keys),
                               bytes(a.content), managed=res.manage.get(concern, False)))
        by_key: dict[str, list[Desired]] = {}
        for d in out:
            by_key.setdefault(d.key, []).append(d)
        clean: list[Desired] = []
        for key, ds in by_key.items():
            if len({d.merge for d in ds}) > 1:
                diags.append(Diag(severity="error", code="merge_mode_mismatch", message=f"{key}: artifacts disagree on the merge mode"))
            elif ds[0].merge in ("own", "block") and len(ds) > 1:
                diags.append(Diag(severity="error", code="duplicate_artifact", message=f"{key}: {len(ds)} artifacts for a {ds[0].merge} file"))
            else:
                clean += ds
        return clean, diags

    def _plan_file(self, client: str, ds: list[Desired], live_files: LiveFiles, lock: Lock | None,
                   adopt: frozenset[str] | set[str], diags: list[Diag]) -> PlannedFile:
        d0 = ds[0]
        entry = lock.files.get(d0.key) if lock else None
        pf = PlannedFile(d0, "unchanged", entry=entry)
        adopted = adopt_match(adopt, client, d0.root, d0.path)
        pf.adopted = adopted
        try:
            pf.live = live_files.read(d0.root, d0.path)
        except (PathError, OSError) as exc:
            pf.action, pf.reason = "conflict", f"live file unreadable: {exc}"
            pf.adoptable = False
            return pf
        m = merge_file(ds, pf.live, adopt_broken=adopted)
        pf.merged = m
        removal = all(d.content is None for d in ds)
        pf.final = m.new
        managed = all(d.managed for d in ds) if not removal else True
        if m.unparseable:
            pf.action, pf.reason = "conflict", m.unparseable
            return pf
        if not managed:
            # advisory: show what would change, never write, never lock
            pf.action = "advisory" if m.slice_live != m.slice_new or (pf.live is None and m.new is not None) else "unchanged"
            pf.reason = "concern is not managed by the hub" if pf.action == "advisory" else ""
            return pf
        if removal:
            self._plan_removal(pf, m, entry)
            return pf
        if pf.live is None:
            if m.new is None:
                pf.action = "unchanged"
            else:
                pf.action = "add"
            return pf
        if m.slice_live == m.slice_new:
            pf.action = "unchanged"
            return pf
        if m.slice_live is None:                    # merge file exists but the hub's slice is not in it yet
            pf.action = "change"
            return pf
        if entry is not None and m.slice_live == entry.slice_sha256:
            pf.action = "change"
            return pf
        pf.action = "conflict"
        pf.reason = ("live content differs from what the hub last applied" if entry is not None
                     else "file exists and was not created by the hub")
        return pf

    @staticmethod
    def _plan_removal(pf: PlannedFile, m: Merged, entry: LockEntry | None) -> None:
        d = pf.desired
        if pf.live is None or m.slice_live is None:
            pf.action, pf.reason = "unchanged", "already gone"
            return
        created = bool(entry and entry.created)
        if d.merge == "own":
            if not created:
                pf.action, pf.reason = "unchanged", "not created by the hub: released from the lock, left in place"
            elif entry is not None and m.slice_live == entry.slice_sha256:
                pf.action, pf.delete_file = "remove", True
            else:
                pf.action, pf.reason = "conflict", "hub-created file was edited since it was applied"
            return
        if entry is None or m.slice_live != entry.slice_sha256:
            pf.action, pf.reason = "conflict", "managed slice was edited since it was applied"
            return
        if m.empty_after and created:
            pf.action, pf.delete_file = "remove", True
        else:
            pf.action = "change"
            pf.reason = "removing the hub-managed slice; the rest of the file is preserved"

    @staticmethod
    def _downgrade(res: Resolved) -> list[Finding]:
        """Findings about an advisory concern are warnings: the hub is not writing there (audit still flags them)."""
        out = []
        for f in res.findings:
            if f.severity == "error" and f.concern and not res.manage.get(f.concern, False):
                f = f.model_copy(update={"severity": "warn", "message": f.message + " (advisory concern: not written)"})
            out.append(f)
        return out

    def _summarise(self, client: str, files: list[PlannedFile], diags: list[Diag], findings: list[Finding],
                   res: Resolved) -> ClientPlan:
        pfiles = []
        counts: dict[str, int] = {}
        for pf in files:
            d = pf.desired
            counts[pf.action] = counts.get(pf.action, 0) + 1
            new = None if pf.delete_file else pf.merged.new
            diff = "" if pf.action == "unchanged" else unified(pf.live, new, d.path)
            pfiles.append(PlanFile(root=d.root, path=d.path, action=pf.action, kind=d.kind, source_ids=d.source_ids,
                                   diff=diff, reason=pf.reason, merge=d.merge,
                                   managed=d.managed and pf.action != "advisory"))
        reasons: list[str] = []
        strict = res.client.strict
        for dg in diags:
            if dg.severity == "error" and (strict or dg.code in HARD_CODES):
                reasons.append(f"{dg.code}: {dg.message}"[:200])
        for f in findings:
            if f.severity == "error":
                reasons.append(f"{f.code}: {f.message}"[:200])
        digest_src = json.dumps(sorted([pf.key, pf.action, sha256(pf.desired.content or b""), pf.merged.slice_new or ""]
                                       for pf in files))
        return ClientPlan(
            client=client, files=pfiles,
            diagnostics=[dg.model_dump() for dg in diags], floor_violations=findings, summary=counts,
            blocked=bool(reasons), blocked_reasons=reasons[:20],
            digest=hashlib.sha256(digest_src.encode()).hexdigest())

    # -- drift -----------------------------------------------------------------------------------
    def drift(self, ip: ClientPlanInternal) -> dict[str, Any]:
        """live vs rendered vs lock, per file. Hashes are 12-char prefixes only, never contents.

        edited_outside: a lock entry exists and the live slice no longer matches it (someone changed or deleted it).
        pending_changes: the hub WOULD change something (render != live) although nobody edited the file."""
        rows: list[dict[str, Any]] = []
        edited: list[dict[str, str]] = []
        pending = 0
        for pf in ip.files:
            d = pf.desired
            if pf.action == "advisory":
                state = "advisory"
            elif pf.action == "unchanged":
                state = "in_sync"
            elif pf.action == "add":
                state = "never_applied" if pf.entry is None else "missing"
            elif pf.action == "change":
                state = "pending"
            elif pf.action == "remove":
                state = "orphaned"
            else:
                state = "changed_live" if pf.entry is not None else "unadopted"
            if state in ("changed_live", "missing"):
                edited.append({"root": d.root, "path": d.path, "reason": "changed" if state == "changed_live" else "missing"})
            elif state in ("pending", "never_applied", "orphaned", "unadopted"):
                pending += 1

            def pre(h: str | None) -> str | None:
                return h[:12] if h else None
            rows.append({"root": d.root, "path": d.path, "state": state, "kind": d.kind, "reason": pf.reason,
                         "expected": pre(pf.merged.slice_new), "actual": pre(pf.merged.slice_live),
                         "locked": pre(pf.entry.slice_sha256) if pf.entry else None})
        managed = [r for r in rows if r["state"] != "advisory"]
        lock = ip.lock
        if ip.plan.blocked:
            status = "error"
        elif lock is None or not (lock.applied_at or lock.adopted_at):
            status = "in_sync" if all(r["state"] == "in_sync" for r in managed) else "never_applied"
        elif edited:
            status = "edited_outside"
        elif pending:
            status = "pending_changes"
        else:
            status = "in_sync"
        return {"client": ip.plan.client, "status": status, "files": rows,
                "drift": {"edited_outside": edited, "pending_changes": pending}}
