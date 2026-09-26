"""The Hub: one facade over the content repo, resolver, planner, applier and stores. The HTTP API and `hubctl` are thin
layers over this class. Content mutations take one lock (single operator, git is not concurrency-friendly)."""
from __future__ import annotations

import contextlib
import json
import os
import shutil
import threading
from collections.abc import Iterator
from pathlib import Path
from typing import Any

from pydantic import ValidationError

from agent_hub.adapters import AdapterRegistry, load_registry
from agent_hub.adapters.base import ClientAdapter, Diag, RenderContext
from agent_hub.config import APP_ROOT, Settings
from agent_hub.domain import yamlsafe
from agent_hub.domain.errors import BadRequest, Conflict, NotFound, Unavailable, Unprocessable
from agent_hub.domain.ids import CONCERNS, KINDS, PLURAL, valid_id
from agent_hub.domain.models import ClientDoc, Finding, Issue, McpDoc, Plan, RuleDoc, issues_from
from agent_hub.domain.pathsafe import PathError, expand_root
from agent_hub.services import contentguard
from agent_hub.services.applier import Applier, ApplyResult
from agent_hub.services.applylog import ApplyLog
from agent_hub.services.attest import Attestor
from agent_hub.services.audit import AuditService, Telemetry
from agent_hub.services.auth import AuthService
from agent_hub.services.common import atomic_write, harden_tree, new_id, now, redact, sha256
from agent_hub.services.content import Catalog, ContentStore, Item
from agent_hub.services.floor import Floor, load_floor
from agent_hub.services.fs import LiveFiles, RealFS, RealRunner, discovery_fs
from agent_hub.services.gitrepo import EMPTY_TREE, GitRepo
from agent_hub.services.importer import Importer
from agent_hub.services.inbox import Inbox
from agent_hub.services.knowledge import KnowledgeProxy
from agent_hub.services.lock import LockStore
from agent_hub.services.organize import Organizer
from agent_hub.services.planner import Planner
from agent_hub.services.resolver import Resolver, client_caps, client_config, manage_flags, to_mcp
from agent_hub.services.rollback import Rollbacker
from agent_hub.services.store import Store, page_list
from agent_hub.services.targets import TargetGuard

SNAPSHOTS_KEEP = 5
_NO_DRIFT: dict[str, Any] = {"edited_outside": [], "pending_changes": 0}


class Hub:
    def __init__(self, cfg: Settings, registry: AdapterRegistry | None = None, runner: RealRunner | None = None,
                 knowledge_transport: Any = None) -> None:
        self.cfg = cfg
        self.registry = registry if registry is not None else load_registry()
        policy = load_floor(cfg.policy_file)
        # the hub's own state (keys, DB, backups, locks) is off-limits to every managed client, wherever data_dir points
        for lst in (policy.deny_path_read, policy.deny_path_write):
            g = f"{cfg.data_dir}/**"
            if g not in lst:
                lst.append(g)
        self.floor = Floor(policy)
        cfg.data_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        cfg.data_dir.chmod(0o700)
        for sub_dir in ("plans", "backups", "snapshots", "locks", "applies"):
            (cfg.data_dir / sub_dir).mkdir(mode=0o700, exist_ok=True)
        harden_tree(cfg.data_dir)                                   # existing state dirs are tightened on every start
        self.store = Store(cfg.db_path)
        self.audit_svc = AuditService(self.store)
        self.auth = AuthService(self.store)
        self.telemetry = Telemetry()
        self.repo = GitRepo(cfg.content_dir, cfg.git_bin, cfg.git_author_name, cfg.git_author_email, cfg.git_timeout_s)
        self.attestor = Attestor(cfg.data_dir / "attest.key")
        self.work = ContentStore(cfg.content_dir, max_file=cfg.max_content_file_bytes, max_skill=cfg.max_skill_bytes,
                                 attestor=self.attestor)
        self.locks = LockStore(cfg.data_dir / "locks")
        self.guard = TargetGuard(policy, cfg.data_dir, cfg.content_dir, APP_ROOT)
        self.resolver = Resolver(self.floor, self.registry, self.guard, self.attestor)
        self.planner = Planner(self.resolver, self.registry, self.locks, cfg.max_live_file_bytes)
        self.runner = runner or RealRunner()
        self.applies = ApplyLog(cfg.data_dir / "applies")
        self.applier = Applier(self.planner, self.locks, cfg.data_dir / "backups", self.runner, self.applies)
        self.rollbacker = Rollbacker(self.applier, self.applies, self.locks)
        self.organizer = Organizer(self.work, self.resolver, self.registry)
        self.importer = Importer(self.work, self.registry, self.floor, self.organizer, cfg.discover_extra_roots)
        self.inbox = Inbox(self.work, cfg.inbox_rate_per_min, cfg.inbox_max_per_client)
        self.knowledge = KnowledgeProxy(cfg, knowledge_transport)
        self.lock = threading.RLock()
        self._plans: dict[str, Plan] = {}

    # -- lifecycle --------------------------------------------------------------------------------
    def init_content(self) -> dict[str, Any]:
        """`hubctl init`: create the nested content repo, seed it, make the first commit."""
        with self.lock:
            contentguard.check_location(self.cfg.content_dir, self._client_roots(), APP_ROOT)
            fresh = not self.cfg.content_dir.exists()
            self.cfg.content_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
            if fresh:
                self.cfg.content_dir.chmod(0o700)
            self.repo.init()
            self.check_content_safety()
            created = self.work.seed()
            committed = None
            if created and (self.repo.status()):
                committed = self.repo.commit("init: seed content repository")
            return {"content_dir": str(self.cfg.content_dir), "seeded": created, "commit": committed}

    @contextlib.contextmanager
    def mutating(self) -> Iterator[None]:
        """Serialise content mutations and refuse them when the content repo does not exist yet."""
        with self.lock:
            self.require_repo()
            yield

    def import_items(self, client: str, selection: list[tuple[str, str]] | None, on_conflict: str | None,
                     enable: bool = True, overrides: dict[tuple[str, str], str] | None = None) -> dict[str, Any]:
        """Import into the working tree, then record what the client has RIGHT NOW as the last-applied state, so the first
        plan is a normal change/unchanged (with a diff) instead of a wall of conflicts."""
        with self.mutating():
            out = self.importer.do_import(client, selection, on_conflict, enable, overrides)
            out["adopted_live"] = self._adopt_live(client)
            return out

    def _adopt_live(self, client: str) -> int:
        from agent_hub.services.lock import Lock, LockEntry
        cat = self.work.catalog()
        if client not in cat.clients:
            return 0
        ip = self.planner.plan_client(cat, client)
        lock = ip.lock or Lock(client=client)
        n = 0
        for pf in ip.files:
            d = pf.desired
            if (pf.key in lock.files or not d.managed or d.content is None or pf.live is None or pf.merged.unparseable
                    or not pf.adoptable or pf.merged.slice_live is None):
                continue
            lock.files[pf.key] = LockEntry(root=d.root, path=d.path, kind=d.kind, sha256=sha256(pf.live),
                                           slice_sha256=pf.merged.slice_live, merge=d.merge, managed_keys=d.managed_keys,
                                           created=False, source_ids=d.source_ids, mode=d.mode)
            n += 1
        if n:
            lock.adopted_at = now()
            self.locks.save(lock, touch=False)
        return n

    def _client_roots(self) -> list[str]:
        roots: list[str] = []
        for cid in self.work.names_of("clients"):
            try:
                for r in self.work.get_client(cid).roots.values():
                    rp = expand_root(r)
                    if self.guard.root_problem(rp) is None:     # a refused root never receives files, so it cannot expose the repo
                        roots.append(str(rp))
            except Exception:  # noqa: BLE001,S112 - an unreadable client cannot make the check weaker, only skip it
                continue
        return roots

    def check_content_safety(self) -> None:
        contentguard.check_location(self.cfg.content_dir, self._client_roots(), APP_ROOT)
        self.repo.verify_safe(force=True)

    def migrate_content(self, dest: str, yes: bool) -> dict[str, Any]:
        """Move the content repo to a private location outside the source tree. The SOURCE is validated only as "a git repo owned
        by me with a real .git directory" (it may be exactly what the safety gate refuses: group-writable, inside a client root,
        with a planted config). No git command ever runs against the source; it is moved with plain file operations. Findings are
        shown first; `yes` waives them, and they are stripped from the copy. Git (fsck) runs only on the sanitised target."""
        src = self.cfg.content_dir
        target = Path(dest).expanduser().absolute()
        if src.is_symlink() or not (src / ".git").is_dir() or (src / ".git").is_symlink():
            raise BadRequest("the configured content directory is not a git repository with a real .git directory",
                             code="content_not_initialised")
        if src.lstat().st_uid != os.getuid() or (src / ".git").lstat().st_uid != os.getuid():
            raise BadRequest("the content repository is not owned by the current user; refusing to migrate it")
        if target.exists() or target.is_symlink():
            raise Conflict(f"target {target} already exists; refusing to overwrite", code="target_exists")
        if target == src or src in target.parents or target in src.parents:
            raise BadRequest("target must be a different, non-nested location")
        contentguard.check_location(target, self._client_roots(), APP_ROOT)          # the TARGET must be a safe place
        findings = contentguard.hostile_findings(src)
        plan: dict[str, Any] = {"from": str(src), "to": str(target), "findings": [findings] if findings else []}
        if not yes:
            return {**plan, "dry_run": True}
        with self.lock:
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            tmp = target.parent / f".migrate-{new_id()}"
            try:
                shutil.copytree(src, tmp, symlinks=True)            # plain copy: modes and mtimes preserved
                plan["sanitized"] = contentguard.sanitize_git(tmp)
                contentguard.tighten(tmp)
                contentguard.check_ownership(tmp)
                contentguard.verify_git_metadata(tmp)               # only now may git run, and only on the target
                new = GitRepo(tmp, self.cfg.git_bin, self.cfg.git_author_name, self.cfg.git_author_email, self.cfg.git_timeout_s)
                new.run("fsck", "--strict")
                if new.head() != contentguard.plain_head(src):
                    raise Conflict("copied repository does not have the same HEAD", code="migrate_verify_failed")
                os.rename(tmp, target)
            except BaseException:
                shutil.rmtree(tmp, ignore_errors=True)
                raise
            shutil.rmtree(src)
        return {**plan, "dry_run": False, "head": GitRepo(target, self.cfg.git_bin, "n", "e@x", 30).head()}

    def require_repo(self) -> None:
        self.check_content_safety()
        if not self.repo.is_repo():
            raise Unavailable("content repository is not initialised; run `hubctl init`", code="content_not_initialised")

    def close(self) -> None:
        self.store.close()

    # -- approved snapshot -------------------------------------------------------------------------
    def approved(self) -> tuple[str, ContentStore]:
        """(content_hash, read-only store) of the COMMITTED content: what plans render from."""
        self.require_repo()
        tree = self.repo.head_tree()
        root = self.cfg.data_dir / "snapshots"
        dest = root / tree
        if not dest.is_dir():
            root.mkdir(parents=True, exist_ok=True, mode=0o700)
            tmp = root / f".tmp-{new_id()}"
            try:
                self.repo.export_head(tmp)
                os.replace(tmp, dest)
            except OSError:
                shutil.rmtree(tmp, ignore_errors=True)
                if not dest.is_dir():
                    raise
            harden_tree(dest)
            self._prune_snapshots(root, keep=tree)
        return tree, ContentStore(dest, max_file=self.cfg.max_content_file_bytes, max_skill=self.cfg.max_skill_bytes)

    @staticmethod
    def _prune_snapshots(root: Path, keep: str) -> None:
        dirs = sorted((d for d in root.iterdir() if d.is_dir() and not d.name.startswith(".") and d.name != keep),
                      key=lambda d: d.stat().st_mtime, reverse=True)
        for d in dirs[SNAPSHOTS_KEEP - 1:]:
            shutil.rmtree(d, ignore_errors=True)

    # -- listing with filters --------------------------------------------------------------------------
    def _enabled_map(self, cat: Catalog) -> dict[tuple[str, str], list[str]]:
        from agent_hub.services.resolver import enabled_keys
        out: dict[tuple[str, str], list[str]] = {}
        for cid in cat.clients:
            via, _ = enabled_keys(cat.profiles.get(cid) or _empty_profile(), cat)
            for k in via:
                out.setdefault(k, []).append(cid)
        return out

    def item_summary(self, item: Item, cat: Catalog, enabled: dict[tuple[str, str], list[str]]) -> dict[str, Any]:
        colls = sorted(c.id for c in cat.collections.values() if any(m.kind == item.kind and m.name == item.name for m in c.members))
        return {"kind": item.kind, "name": item.name, "title": item.title, "description": item.description[:300],
                "tags": item.tags, "groups": item.groups, "collections": colls, "source": item.source,
                "valid": item.valid, "issues": [i.model_dump() for i in item.issues],
                "updated": item.updated, "enabled_for": sorted(enabled.get((item.kind, item.name), []))}

    def list_items(self, kind: str, *, q: str | None = None, group: str | None = None, tag: str | None = None,
                   client: str | None = None, enabled: bool | None = None, source: str | None = None,
                   issues: bool | None = None, sort: str = "name", limit: int = 100, cursor: str | None = None) -> dict[str, Any]:
        cat = self.work.catalog()
        emap = self._enabled_map(cat)
        rows = [self.item_summary(i, cat, emap) for i in cat.of_kind(kind)]
        if q:
            ql = q.lower()
            rows = [r for r in rows if ql in r["name"].lower() or ql in r["title"].lower()
                    or ql in r["description"].lower() or any(ql in t for t in r["tags"])]
        if group:
            rows = [r for r in rows if group in r["collections"] or group in r["groups"]]
        if tag:
            rows = [r for r in rows if tag in r["tags"]]
        if source:
            rows = [r for r in rows if r["source"].startswith(source)]
        if issues is not None:
            rows = [r for r in rows if bool(r["issues"]) == issues]
        if client:
            if client not in cat.clients:
                raise NotFound(f"no such client: {client}")
            on = [r for r in rows if client in r["enabled_for"]]
            rows = on if enabled is not False and enabled is not None else \
                ([r for r in rows if client not in r["enabled_for"]] if enabled is False else rows)
        elif enabled is not None:
            rows = [r for r in rows if bool(r["enabled_for"]) == enabled]
        if sort == "updated":
            rows.sort(key=lambda r: (-r["updated"], r["name"]))
        elif sort == "title":
            rows.sort(key=lambda r: (r["title"].lower(), r["name"]))
        elif sort == "name":
            rows.sort(key=lambda r: r["name"])
        else:
            raise BadRequest("sort must be name, title or updated")
        return page_list(rows, limit, cursor)

    def item_detail(self, kind: str, name: str) -> dict[str, Any]:
        item = self.work.load(kind, name)
        cat = self.work.catalog()
        out = self.item_summary(item, cat, self._enabled_map(cat))
        if item.meta is not None:
            out["meta"] = item.meta.model_dump(mode="json", exclude_none=True, by_alias=True)
        out["body"] = item.body
        if kind == "skill":
            files = []
            texts: dict[str, str | None] = {}
            for rel, blob in sorted(item.files.items()):
                binary = b"\x00" in blob[:4096]
                files.append({"path": rel, "size": len(blob), "binary": binary})
                texts[rel] = None if binary else blob.decode("utf-8", "replace")
            out["file_list"], out["files"] = files, texts
            if item.sidecar:
                out["sidecar"] = item.sidecar.model_dump(mode="json")
        return out

    # -- changes ---------------------------------------------------------------------------------------
    def changes(self) -> dict[str, Any]:
        self.require_repo()
        ch = self.repo.changes()
        return {"content_hash": self.repo.head_tree(), "count": len(ch),
                "items": [{"path": c.path, "status": c.status, "diff": _scrub_diff(c.diff)} for c in ch]}

    def commit(self, message: str, actor: str) -> dict[str, Any]:
        with self.lock:
            self.require_repo()
            cat = self.work.catalog()
            if cat.problems:
                raise Unprocessable("content has validation problems; fix or discard them before committing",
                                    errors=[p.model_dump() for p in cat.problems[:50]], code="content_invalid")
            sha = self.repo.commit(message)
            strays = self.repo.strays()
            self.audit_svc.event(actor, "content.commit", {"commit": sha[:12], "strays": len(strays)})
            return {"commit": sha, "content_hash": self.repo.head_tree(), "strays": strays}

    def discard(self, paths: list[str] | None, actor: str, confirm: bool = False, dry_run: bool = False) -> dict[str, Any]:
        """Discarding EVERYTHING (no paths) is destructive: it needs confirm=true, and dry_run lists what it would delete."""
        with self.lock:
            self.require_repo()
            if dry_run:
                return {"dry_run": True, "would_discard": self.repo.discard(paths, dry_run=True)}
            if not paths and confirm is not True:
                raise Unprocessable("discarding all pending changes needs confirm: true (use dry_run: true to list them first)",
                                    code="confirm_required", would_discard=self.repo.discard(None, dry_run=True))
            done = self.repo.discard(paths)
            self.audit_svc.event(actor, "content.discard", {"paths": done[:50]})
            return {"discarded": done}

    def revert(self, commit: str, actor: str) -> dict[str, Any]:
        with self.lock:
            self.require_repo()
            sha = self.repo.revert(commit)
            self.audit_svc.event(actor, "content.revert", {"reverted": commit[:12], "commit": sha[:12]})
            return {"commit": sha, "content_hash": self.repo.head_tree()}

    def history(self, limit: int = 50) -> list[dict[str, object]]:
        self.require_repo()
        return self.repo.history(limit)

    # -- clients ---------------------------------------------------------------------------------------
    def _adapter_for(self, doc: ClientDoc) -> ClientAdapter:
        a = self.registry.get(doc.adapter)
        if a is None:
            raise Unprocessable(f"adapter {doc.adapter!r} is not installed", code="unknown_adapter")
        return a

    def put_client(self, cid: str, payload: dict[str, Any], create: bool) -> dict[str, Any]:
        payload = self._spec_body(payload)
        with self.lock:
            exists = cid in self.work.names_of("clients")
            if create and exists:
                raise Conflict("client already exists", code="already_exists")
            if not create and not exists:
                raise NotFound(f"no such client: {cid}")
            try:
                doc = ClientDoc.model_validate({**payload, "id": cid})
            except ValidationError as exc:
                raise Unprocessable("client failed validation", errors=[i.model_dump() for i in issues_from(exc)]) from exc
            self._adapter_for(doc)
            try:
                for rp in client_config(doc).roots.values():
                    if (prob := self.guard.root_problem(rp)) is not None:
                        raise Unprocessable(f"invalid roots: {prob}", code="root_not_allowed")
            except PathError as exc:
                raise Unprocessable(f"invalid roots: {exc}") from exc
            self.work.put_client(cid, doc.model_dump(mode="json", exclude_none=True))
            return self.client_view(cid)

    def client_view(self, cid: str) -> dict[str, Any]:
        doc = self.work.get_client(cid)
        adapter = self.registry.get(doc.adapter)
        status = self.client_status(cid)
        return {**doc.model_dump(mode="json", exclude_none=True), "manage_effective": manage_flags(doc, adapter),
                "adapter_installed": adapter is not None, "caps": client_caps(adapter, doc).model_dump() if adapter else None,
                "status": status["status"], "drift": status["drift"], "committed": status["committed"], **self._lock_info(cid)}

    def _lock_info(self, cid: str) -> dict[str, Any]:
        lk = self.locks.load(cid)
        return {"adopted_at": (lk.adopted_at or None) if lk else None,
                "last_applied_at": (lk.applied_at or None) if lk else None, "last_applied_plan": (lk.plan_id or None) if lk else None,
                "last_verified_at": lk.verified_at if lk else None, "last_verified_ok": lk.verified_ok if lk else None}

    def list_clients(self) -> list[dict[str, Any]]:
        return [self.client_view(c) for c in self.work.names_of("clients")]

    def client_status(self, cid: str) -> dict[str, Any]:
        try:
            _, store = self.approved()
            cat = store.catalog()
        except Unavailable:
            return {"status": "error", "drift": _NO_DRIFT, "committed": False}
        if cid not in cat.clients:
            return {"status": "never_applied", "drift": _NO_DRIFT, "committed": False}
        try:
            ip = self.planner.plan_client(cat, cid)
            d = self.planner.drift(ip)
            return {"status": d["status"], "drift": d["drift"], "committed": True}
        except Exception:  # noqa: BLE001 - a status probe must never 500 the client list
            return {"status": "error", "drift": _NO_DRIFT, "committed": True}

    def delete_client(self, cid: str) -> None:
        with self.lock:
            self.work.delete_client(cid)

    def _approved_client(self, cid: str) -> tuple[str, Catalog]:
        tree, store = self.approved()
        cat = store.catalog()
        if cid not in cat.clients:
            raise Conflict("client is not in committed content yet; commit it first", code="client_not_committed")
        return tree, cat

    def effective(self, cid: str, source: str = "committed") -> dict[str, Any]:
        if source == "working":
            cat = self.work.catalog()
            if cid not in cat.clients:
                raise NotFound(f"no such client: {cid}")
        elif source == "committed":
            _, cat = self._approved_client(cid)
        else:
            raise BadRequest("source must be committed or working")
        r = self.resolver.resolve(cat, cid)
        return {
            "client": cid, "source": source, "adapter": r.client.adapter, "adapter_installed": r.adapter is not None,
            "caps": client_caps(r.adapter, r.client).model_dump() if r.adapter else None, "manage": r.manage, "items": r.entries,
            "rules": self._rules_with_source(r),
            "default_tool_decision": r.permissions.default_tool_decision, "network_default": r.permissions.network_default,
            "mcp": [{"name": m.name, "transport": m.transport, "egress_hosts": m.egress_hosts,
                     "scan_status": m.scan_status} for m in r.mcp],
            "egress": {"default": r.permissions.network_default, "allow_hosts": r.allow_hosts},
            "findings": [f.model_dump() for f in r.findings],
            "diagnostics": [d.model_dump() for d in r.diagnostics], "warnings": r.warnings,
        }

    @staticmethod
    def _rules_with_source(r: Any) -> list[dict[str, Any]]:
        rules = r.permissions.rules
        n_floor = len(rules) - len(r.kept_rules)
        return [{**x.model_dump(), "source": "floor" if i < n_floor else r.kept_sources[i - n_floor]}
                for i, x in enumerate(rules)]

    def rendered(self, cid: str, kind: str, name: str, source: str = "committed") -> dict[str, Any]:
        """The artifacts one item contributes for one client, from the same render the planner uses."""
        from agent_hub.services.common import scrub
        if kind not in KINDS:
            raise BadRequest("unknown kind")
        if source == "working":
            cat = self.work.catalog()
            if cid not in cat.clients:
                raise NotFound(f"no such client: {cid}")
        elif source == "committed":
            _, cat = self._approved_client(cid)
        else:
            raise BadRequest("source must be committed or working")
        res = self.resolver.resolve(cat, cid)
        if res.adapter is None or res.ctx is None:
            raise Unavailable("adapter not available for this client", code="adapter_missing")
        if not any(e["name"] == name for e in res.entries.get(PLURAL[kind], [])):
            why = self.resolver.block_reason(cat, res, kind, name) or "it is not enabled for this client (profile or collection)"
            raise NotFound(f"{kind} {name!r} does not render for {cid}: {why}", code="not_rendered")
        try:
            rr = res.adapter.render(res.ctx)
        except Exception as exc:  # noqa: BLE001
            raise Unavailable(f"adapter render failed ({type(exc).__name__})", code="render_failed") from exc
        arts = []
        for a in rr.artifacts:
            if name not in a.source_ids:
                continue
            text = a.content.decode("utf-8", "replace")
            arts.append({"root": a.root, "path": a.path, "merge": a.merge, "kind": a.kind, "source_ids": a.source_ids,
                         "managed_keys": a.managed_keys, "size": len(a.content),
                         "content": scrub(text[:100_000]), "truncated": len(text) > 100_000})
        return {"client": cid, "kind": kind, "name": name, "source": source, "artifacts": arts,
                "diagnostics": [d.model_dump() for d in rr.diagnostics if d.item in (None, name)]}

    def _spec_body(self, body: dict[str, Any]) -> dict[str, Any]:
        """Accept `spec_yaml` (safe YAML, 64 KB) in place of a `spec` object, so the browser needs no YAML parser."""
        if "spec_yaml" not in body:
            return body
        if body.get("spec") is not None:
            raise Unprocessable("send either `spec` or `spec_yaml`, not both")
        text = body["spec_yaml"]
        if not isinstance(text, str):
            raise Unprocessable("`spec_yaml` must be a string")
        try:
            spec = yamlsafe.load(text, max_bytes=64 * 1024)
        except yamlsafe.YamlError as exc:
            raise Unprocessable(f"spec_yaml: {exc}", code="invalid_yaml") from exc
        if not isinstance(spec, dict):
            raise Unprocessable("spec_yaml must be a mapping")
        return {**{k: v for k, v in body.items() if k != "spec_yaml"}, "spec": spec}

    # -- plan / apply ---------------------------------------------------------------------------------
    def plan(self, clients: list[str] | None, source: str = "committed") -> Plan:
        if source == "working":
            self.require_repo()
            tree, cat = "preview", self.work.catalog()
        elif source == "committed":
            tree, store = self.approved()
            cat = store.catalog()
        else:
            raise BadRequest("source must be committed or working")
        ids = clients if clients else sorted(cat.clients)
        for c in ids:
            if c not in cat.clients:
                raise NotFound(f"client {c!r} is not in {source} content")
        plan = Plan(id=new_id("plan_"), content_hash=tree, created_at=now(),
                    clients=[self.planner.plan_client(cat, c).plan for c in ids],
                    pending_changes=len(self.repo.status()), preview=source == "working")
        self._remember(plan)
        self.telemetry.plans.inc()
        return plan

    def _remember(self, plan: Plan) -> None:
        self._plans[plan.id] = plan
        d = self.cfg.data_dir / "plans"
        atomic_write(d / f"{plan.id}.json", plan.model_dump_json().encode(), 0o600, dir_mode=0o700)
        for old in sorted(d.glob("plan_*.json"), key=lambda p: p.stat().st_mtime, reverse=True)[self.cfg.plan_retention:]:
            with contextlib.suppress(OSError):
                old.unlink()
                self._plans.pop(old.stem, None)

    def get_plan(self, plan_id: str) -> Plan:
        if plan_id in self._plans:
            return self._plans[plan_id]
        if not plan_id.startswith("plan_") or not plan_id[5:].isalnum():
            raise NotFound("no such plan")
        try:
            return Plan.model_validate_json((self.cfg.data_dir / "plans" / f"{plan_id}.json").read_text())
        except (OSError, ValueError) as exc:
            raise NotFound("no such plan") from exc

    def apply(self, plan_id: str, confirm: bool, adopt_paths: list[str], actor: str) -> dict[str, Any]:
        if confirm is not True:
            raise Unprocessable("apply needs confirm: true", code="confirm_required")
        with self.lock:
            plan = self.get_plan(plan_id)
            if plan.preview:
                raise Conflict("this plan is a working-tree preview; commit the content and plan again", code="plan_is_preview")
            tree, store = self.approved()
            if tree != plan.content_hash:
                raise Conflict("content changed since this plan was made; plan again", code="plan_stale")
            cat = store.catalog()
            adopt = frozenset(adopt_paths)
            for cp in plan.clients:                                    # pre-pass: refuse before writing anything
                if cp.client not in cat.clients:
                    raise Conflict(f"client {cp.client} is gone", code="plan_stale")
                if self.planner.plan_client(cat, cp.client).plan.digest != cp.digest:
                    raise Conflict("live files changed since this plan was made; plan again", code="plan_stale")
            results: list[ApplyResult] = []
            for cp in plan.clients:
                r = self.applier.apply_client(cat, tree, cp.client, cp.digest, adopt, plan.id)
                results.append(r)
                lk = self.locks.load(cp.client)
                if lk is not None and r.status in ("applied", "nothing"):
                    lk.plan_id = plan.id
                    if r.verify_ok is not None:
                        lk.verified_at, lk.verified_ok = now(), r.verify_ok
                    self.locks.save(lk, touch=False)
                self.telemetry.applies.labels(r.status).inc()
                self.audit_svc.event(actor, "apply", {"plan": plan.id, "client": cp.client, "status": r.status,
                                                      "written": [w["path"] for w in r.written][:50],
                                                      "removed": [w["path"] for w in r.removed][:50],
                                                      "conflicts": len(r.skipped_conflicts),
                                                      "verify_ok": r.verify_ok})
            return {"plan_id": plan.id, "content_hash": tree, "results": [r.model_dump() for r in results]}

    def doctor(self, knowledge: dict[str, Any] | None = None, fs: Any = None, env: Any = None, home: Path | None = None) -> dict[str, Any]:
        """Read-only host-exposure report (see services/doctor.py). Everything host-facing is injectable for tests."""
        import os as _os

        from agent_hub.services.doctor import DoctorContext, RealDoctorFS, run_doctor
        clients: list[dict[str, Any]] = []
        try:
            cat = self.work.catalog()
            for cid, doc in cat.clients.items():
                clients.append({"id": cid, "adapter": doc.adapter, "roots": dict(doc.roots), "params": dict(doc.params),
                                "manage": manage_flags(doc, self.registry.get(doc.adapter))})
        except Exception:  # noqa: BLE001 - an unreadable content dir is itself reported by the content-dir check
            clients = []

        def gaps(cid: str) -> list[str] | None:
            try:
                cat2 = self.work.catalog()
                res = self.resolver.resolve(cat2, cid)
                if res.adapter is None or res.cfg is None:
                    return None
                dc = res.adapter.discover(res.cfg, discovery_fs(res.cfg.roots, res.cfg.params, self.cfg.discover_extra_roots))
                if dc.permissions is None:
                    return None
                return [f"deny {(f.rule or '').replace(':', ' ', 1)}" for f in self.floor.missing_denies(dc.permissions)]
            except Exception:  # noqa: BLE001
                return None
        ctx = DoctorContext(cfg=self.cfg, fs=fs or RealDoctorFS(), env=env if env is not None else dict(_os.environ),
                            home=home or Path.home(), app_root=APP_ROOT, uid=_os.getuid(), clients=clients, knowledge=knowledge,
                            permission_gaps=gaps, floor_read=list(self.floor.p.deny_path_read),
                            floor_write=list(self.floor.p.deny_path_write))
        return run_doctor(ctx)

    def list_applies(self, cid: str) -> dict[str, Any]:
        if cid not in self.work.names_of("clients"):
            raise NotFound(f"no such client: {cid}")
        return {"items": [self.applies.summary(r) for r in self.applies.list(cid)], "next_cursor": None}

    def rollback(self, cid: str, apply_id: str | None, confirm: bool, dry_run: bool, force_paths: list[str], actor: str) -> dict[str, Any]:
        """Undo an apply (default: the latest). Files edited since are skipped and reported unless named in force_paths."""
        if not dry_run and confirm is not True:
            raise Unprocessable("rollback needs confirm: true (use dry_run: true to preview the diff)", code="confirm_required")
        with self.lock:
            _, cat = self._approved_client(cid)
            res = self.resolver.resolve(cat, cid)
            if res.cfg is None or any(d.code == "bad_root" for d in res.diagnostics):
                raise Conflict("the client's roots are not usable; cannot roll back", code="bad_root")
            out = self.rollbacker.run(res.cfg, cid, apply_id, dry_run, set(force_paths))
            if not dry_run:
                self.audit_svc.event(actor, "rollback", {"client": cid, "apply": out.get("apply_id"), "status": out["status"],
                                                         "files": [c["path"] for c in out["changed"]][:50],
                                                         "conflicts": len(out["conflicts"])})
                if out["status"] in ("rolled_back", "partially_rolled_back"):
                    try:
                        out["verify"] = self.verify(cid)
                    except (Conflict, Unavailable) as exc:
                        out["verify"] = {"ok": None, "detail": exc.detail}
            return out

    def drift(self, cid: str) -> dict[str, Any]:
        _, cat = self._approved_client(cid)
        return self.planner.drift(self.planner.plan_client(cat, cid))

    def verify(self, cid: str) -> dict[str, Any]:
        _, cat = self._approved_client(cid)
        res = self.resolver.resolve(cat, cid)
        lock = self.locks.load(cid)
        if lock is None:
            raise Conflict("nothing has been applied to this client yet", code="never_applied")
        if res.adapter is None or res.cfg is None:
            raise Unavailable("adapter not installed", code="adapter_missing")
        from agent_hub.adapters.base import Expected
        expected = [Expected(root=e.root, path=e.path, sha256=e.sha256 if e.merge == "own" else e.slice_sha256, kind=e.kind,
                             source_ids=e.source_ids,
                            merge=e.merge, managed_keys=e.managed_keys)
                    for e in lock.files.values()]
        try:
            checks = res.adapter.verify(res.cfg, expected, RealFS(list(res.cfg.roots.values())), self.runner)
        except Exception as exc:  # noqa: BLE001
            raise Unavailable(f"adapter verify failed ({type(exc).__name__})", code="verify_failed") from exc
        ok = all(c.ok for c in checks)
        lock.verified_at, lock.verified_ok = now(), ok
        self.locks.save(lock, touch=False)
        return {"client": cid, "ok": ok, "checks": [c.model_dump() for c in checks]}

    def audit_client(self, cid: str) -> dict[str, Any]:
        """The live client config against the floor, including advisory concerns the hub does not write."""
        _, cat = self._approved_client(cid)
        res = self.resolver.resolve(cat, cid)
        if res.adapter is None or res.cfg is None:
            raise Unavailable("adapter not installed", code="adapter_missing")
        try:
            dc = res.adapter.discover(res.cfg, discovery_fs(res.cfg.roots, res.cfg.params, self.cfg.discover_extra_roots))
        except Exception as exc:  # noqa: BLE001
            raise Unavailable(f"adapter discover failed ({type(exc).__name__})", code="discover_failed") from exc
        findings: list[Finding] = []
        if dc.permissions is not None:
            findings += self.floor.check_permission_set(dc.permissions)
            findings += self.floor.missing_denies(dc.permissions)
        else:
            findings.append(Finding(code="permissions_unreadable", severity="info", concern="permissions",
                                    message="the adapter reports no permissions for this client"))
        for m in dc.mcp_servers:
            hub_item = cat.items.get(("mcp", m.name))
            if hub_item is None or not hub_item.valid:
                findings.append(Finding(code="mcp_not_in_hub", severity="error", item=m.name, concern="mcp",
                                        message=f"live MCP server {m.name!r} is not a scanned item in the hub catalog"))
                continue
            hub_m = to_mcp(hub_item, self.attestor)
            live_m = hub_m.model_copy(update={"egress_hosts": sorted({*hub_m.egress_hosts, *m.egress_hosts})})
            findings += self.floor.check_mcp(live_m, res.kept_rules)
        return {"client": cid, "ok": not any(f.severity == "error" for f in findings),
                "findings": [f.model_dump() for f in findings], "manage": res.manage}

    # -- policy / spec dry runs ----------------------------------------------------------------------------
    def policy_check(self, body: dict[str, Any]) -> dict[str, Any]:
        findings: list[Finding] = []
        allowed = set(body) - {"rules", "mcp"}
        if allowed:
            raise Unprocessable(f"unknown keys: {sorted(allowed)}")
        rules = []
        for i, r in enumerate(body.get("rules", []) or []):
            try:
                doc = RuleDoc.model_validate({"id": f"check-{i}", "title": "check", **r})
            except ValidationError as exc:
                raise Unprocessable("bad rule", errors=[x.model_dump() for x in issues_from(exc, f"rules.{i}")]) from exc
            from agent_hub.adapters.base import Rule
            rule = Rule(kind=doc.kind, match=doc.match, decision=doc.decision)
            rules.append(rule)
            if (f := self.floor.check_rule(rule, "permissions", doc.id)) is not None:
                findings.append(f)
        for i, m in enumerate(body.get("mcp", []) or []):
            try:
                mdoc = McpDoc.model_validate(m)
            except ValidationError as exc:
                raise Unprocessable("bad mcp", errors=[x.model_dump() for x in issues_from(exc, f"mcp.{i}")]) from exc
            from agent_hub.adapters.base import McpItem
            findings += self.floor.check_mcp(McpItem.model_validate(mdoc.model_dump(include=set(McpItem.model_fields))), rules)
        return {"ok": not any(f.severity == "error" for f in findings), "violations": [f.model_dump() for f in findings]}

    def validate_spec(self, body: dict[str, Any]) -> dict[str, Any]:
        """Dry-run a generic spec against sample content. Pure: renders, never touches a filesystem."""
        body = self._spec_body(body)
        adapter = self.registry.get("generic")
        if adapter is None:
            raise Unavailable("the generic adapter is not installed", code="adapter_missing")
        spec = body.get("spec")
        if not isinstance(spec, dict):
            raise Unprocessable("`spec` must be a mapping")
        roots = {k: str(v) for k, v in (body.get("roots") or {"home": "/tmp/agent-hub-dry-run"}).items()}  # noqa: S108
        try:
            doc = ClientDoc.model_validate({"id": "dry-run", "adapter": "generic", "display_name": "Dry run",
                                            "roots": roots, "spec": spec, "strict": bool(body.get("strict", False))})
            cfg = client_config(doc)
        except (ValidationError, PathError) as exc:
            raise Unprocessable("spec failed validation", errors=[i.model_dump() for i in issues_from(exc)]
                                if isinstance(exc, ValidationError) else [{"message": str(exc)}]) from exc
        ctx = self._sample_context(cfg)
        try:
            from agent_hub.adapters import generic as g
            spec_diags = g.validate_spec(spec)
            if any(d.severity == "error" for d in spec_diags):
                return {"ok": False, "artifacts": [], "diagnostics": [d.model_dump() for d in spec_diags]}
            rr = g.dry_render(spec, ctx)
        except ImportError:
            rr = adapter.render(ctx)
        except Exception as exc:  # noqa: BLE001
            return {"ok": False, "artifacts": [], "diagnostics": [Diag(severity="error", code="render_failed",
                                                                        message=f"{type(exc).__name__}: {str(exc)[:200]}").model_dump()]}
        arts = []
        for a in rr.artifacts:
            text = a.content.decode("utf-8", "replace")
            arts.append({"root": a.root, "path": a.path, "kind": a.kind, "merge": a.merge, "size": len(a.content),
                         "managed_keys": a.managed_keys, "preview": redact(text[:4000]) if False else _scrub_diff(text[:4000])})
        return {"ok": not any(d.severity == "error" for d in rr.diagnostics), "artifacts": arts,
                "diagnostics": [d.model_dump() for d in rr.diagnostics]}

    def _sample_context(self, cfg: Any) -> RenderContext:
        from agent_hub.adapters.base import AgentItem, InstructionItem, McpItem, SkillItem
        skills = [SkillItem(name="sample-skill", description="A sample skill", files={
            "SKILL.md": b"---\nname: sample-skill\ndescription: A sample skill\n---\nDo the sample thing.\n"})]
        agents = [AgentItem(name="sample-agent", description="A sample agent", capabilities=["read", "shell"],
                            model_tier="standard", body="You are a sample agent.")]
        instr = [InstructionItem(id="sample-rules", title="Sample rules", order=10, body="Be careful.")]
        mcp = [McpItem(name="sample-mcp", transport="stdio", command="sample-mcp", scan_status="clean")]
        return RenderContext(client=cfg, skills=skills, agents=agents, instructions=instr, mcp_servers=mcp,
                             permissions=self.floor.enforce([]).permissions, endpoints=self.work_endpoints())

    def work_endpoints(self) -> dict[str, str]:
        with contextlib.suppress(Exception):
            return self.work.endpoints()
        return {}

    # -- settings ---------------------------------------------------------------------------------------
    EDITABLE = {"plan_retention": (1, 200), "inbox_max_per_client": (1, 5000), "inbox_rate_per_min": (1, 600)}

    def settings(self) -> dict[str, Any]:
        eff = {k: self._setting(k) for k in self.EDITABLE}
        return {"editable": eff,
                "info": {"content_dir": str(self.cfg.content_dir), "data_dir": str(self.cfg.data_dir),
                         "policy_file": str(self.cfg.policy_file), "host": self.cfg.host, "port": self.cfg.port,
                         "knowledge": self.knowledge.status(), "adapters": self.registry.ids(),
                         "adapter_load_errors": self.registry.load_errors, "trust_loopback": self.cfg.trust_loopback}}

    def _setting(self, key: str) -> int:
        row = self.store.one("SELECT value FROM settings WHERE key=?", (key,))
        return int(json.loads(row["value"])) if row else int(getattr(self.cfg, key))

    def update_settings(self, patch: dict[str, Any]) -> dict[str, Any]:
        unknown = set(patch) - set(self.EDITABLE)
        if unknown:
            raise Unprocessable(f"unknown or read-only settings: {sorted(unknown)}")
        for k, v in patch.items():
            lo, hi = self.EDITABLE[k]
            if not isinstance(v, int) or isinstance(v, bool) or not lo <= v <= hi:
                raise Unprocessable(f"{k} must be an integer in {lo}..{hi}")
            self.store.execute("INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                               (k, json.dumps(v)))
            setattr(self.cfg, k, v)
            if k == "inbox_max_per_client":
                self.inbox.max_per_client = v
            if k == "inbox_rate_per_min":
                self.inbox.rate = v
        return self.settings()

    # -- misc -----------------------------------------------------------------------------------------
    def validate_content(self) -> dict[str, Any]:
        cat = self.work.catalog()
        return {"ok": not cat.problems, "problems": [p.model_dump() for p in cat.problems]}

    def adapters_view(self) -> list[dict[str, Any]]:
        return [self._adapter_view(a) for a in self.registry.all()]

    @staticmethod
    def _adapter_view(a: ClientAdapter) -> dict[str, Any]:
        return {"id": a.id, "display_name": a.display_name, "caps": a.caps().model_dump(),
                "default_config": a.default_config().model_dump(), "docs": (type(a).__doc__ or "").strip()[:4000]}

    def sha(self, data: bytes) -> str:
        return sha256(data)


def _empty_profile() -> Any:
    from agent_hub.domain.models import ProfileDoc
    return ProfileDoc()


def _scrub_diff(text: str) -> str:
    from agent_hub.services.common import scrub
    return scrub(text)


__all__ = ["CONCERNS", "EMPTY_TREE", "Hub", "Issue", "KINDS", "LiveFiles", "PLURAL", "valid_id", "yamlsafe"]
