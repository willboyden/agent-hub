"""Import: discover what a client natively has (adapter.discover through a FileSystem port limited to configured
roots) and write chosen items into the content WORKING TREE. Imported MCP servers are always `unscanned` (the hub does not scan
anything itself; `clean` is an operator attestation bound to a digest of the server's command/args/url/pinned_ref/transport)."""
from __future__ import annotations

import hashlib
import time
from typing import Any

from pydantic import BaseModel

from agent_hub.adapters import AdapterRegistry
from agent_hub.adapters.base import CAPABILITIES, DiscoveredContent
from agent_hub.domain import yamlsafe
from agent_hub.domain.errors import BadRequest, Conflict, NotFound, Unavailable
from agent_hub.domain.ids import ENV_NAME_RE, HOST_RE, valid_id
from agent_hub.domain.models import AgentMeta, InstructionMeta, McpDoc, RuleDoc, Sidecar
from agent_hub.domain.pathsafe import PathError
from agent_hub.services.content import ContentStore
from agent_hub.services.floor import Floor
from agent_hub.services.fs import discovery_fs
from agent_hub.services.organize import Organizer
from agent_hub.services.resolver import client_config

IMPORT_KINDS = ("skill", "agent", "instruction", "mcp", "rule")
CONFLICT_MODES = ("skip", "rename", "replace", "link")


def _slug(name: str) -> str:
    from agent_hub.domain.ids import slugify
    return slugify(name)


class Importer:
    def __init__(self, work: ContentStore, registry: AdapterRegistry, floor: Floor, organizer: Organizer,
                 extra_roots: list[str]) -> None:
        self.work = work
        self.registry = registry
        self.floor = floor
        self.org = organizer
        self.extra_roots = extra_roots

    # -- discover ------------------------------------------------------------------------------------
    def _discover_raw(self, client_id: str) -> DiscoveredContent:
        if client_id not in self.work.names_of("clients"):
            raise NotFound(f"no such client: {client_id}")
        doc = self.work.get_client(client_id)
        adapter = self.registry.get(doc.adapter)
        if adapter is None:
            raise Unavailable(f"adapter {doc.adapter!r} is not installed", code="adapter_missing")
        try:
            cfg = client_config(doc)
        except PathError as exc:
            raise BadRequest(f"client roots invalid: {exc}") from exc
        fs = discovery_fs(cfg.roots, cfg.params, self.extra_roots)
        try:
            return adapter.discover(cfg, fs)
        except Exception as exc:  # noqa: BLE001
            raise Unavailable(f"adapter discover failed ({type(exc).__name__})", code="discover_failed") from exc

    def _entries(self, client: str, dc: DiscoveredContent) -> dict[str, list[dict[str, Any]]]:
        """kind -> [{name, payload (doc fields), files?}] normalised for both listing and importing."""
        out: dict[str, list[dict[str, Any]]] = {k: [] for k in IMPORT_KINDS}
        for s in dc.skills:
            out["skill"].append({"name": s.name, "description": s.description, "files": s.files})
        for a in dc.agents:
            caps = [c for c in a.capabilities if c in CAPABILITIES]
            payload = {"name": a.name, "description": a.description or a.name, "capabilities": caps,
                       "model_tier": a.model_tier, "mode": a.mode, "read_only": a.read_only, "body": a.body}
            out["agent"].append({"name": a.name, "description": a.description, "payload": payload})
        for i in dc.instructions:
            out["instruction"].append({"name": i.id, "description": i.title,
                                       "payload": {"id": i.id, "title": i.title, "order": i.order, "body": i.body}})
        for m in dc.mcp_servers:
            payload = {"name": m.name, "transport": m.transport, "command": m.command, "args": m.args, "url": m.url,
                       "env_names": [n for n in m.env_names if ENV_NAME_RE.fullmatch(n)],
                       "sandbox_profile": m.sandbox_profile,
                       "egress_hosts": [h for h in m.egress_hosts if HOST_RE.fullmatch(h)],
                       "scan_status": "unscanned", "pinned_ref": m.pinned_ref,
                       "tags": ["imported"]}
            payload = {k: v for k, v in payload.items() if v is not None}
            out["mcp"].append({"name": m.name, "description": m.command or m.url or "", "payload": payload})
        if dc.permissions is not None:
            for r in dc.permissions.rules:
                rid = "imp-" + _slug(client)[:20] + "-" + hashlib.sha256(f"{r.kind}|{r.match}|{r.decision}".encode()).hexdigest()[:8]
                payload = {"id": rid, "title": f"{r.kind} {r.match}"[:120], "kind": r.kind, "match": r.match,
                           "decision": r.decision, "applies_to": [client]}
                if r.reason:
                    payload["reason"] = r.reason[:300]
                out["rule"].append({"name": rid, "description": f"{r.decision} {r.kind} {r.match}", "payload": payload,
                                    "rule": r})
        return out

    def _conflict(self, kind: str, entry: dict[str, Any]) -> dict[str, Any] | None:
        """None when nothing by that name exists, else {existing_source, equal, differences} over canonical fields."""
        name = entry["name"]
        slug = _slug(name) if not valid_id(name) else name
        if not slug or slug not in self.work.names(kind):
            return None
        existing = self.work.load(kind, slug)
        diffs: list[str] = []
        if kind == "skill":
            mine = self._prepare_skill(slug, entry["files"])
            for path in sorted(set(mine) | set(existing.files)):
                if path not in existing.files:
                    diffs.append(f"{path} (only in this client)")
                elif path not in mine:
                    diffs.append(f"{path} (only in the hub)")
                elif hashlib.sha256(mine[path]).digest() != hashlib.sha256(existing.files[path]).digest():
                    diffs.append(path)
        elif existing.meta is None:
            diffs.append("existing item is invalid")
        else:
            p = dict(entry["payload"])
            body = p.pop("body", None)
            cur = existing.meta.model_dump(mode="json", exclude_none=True)
            diffs += [k for k, v in p.items() if k not in ("tags", "applies_to") and cur.get(k) != v]
            if body is not None and kind in ("agent", "instruction") and existing.body != body:
                diffs.append("body")
        return {"existing_source": existing.source or "", "equal": not diffs, "differences": diffs}

    def discover(self, client_id: str) -> dict[str, Any]:
        dc = self._discover_raw(client_id)
        entries = self._entries(client_id, dc)
        items: dict[str, list[dict[str, Any]]] = {}
        for kind, lst in entries.items():
            items[kind] = []
            for e in lst:
                row: dict[str, Any] = {"kind": kind, "name": e["name"], "description": e.get("description", ""),
                                       "conflict": self._conflict(kind, e)}
                if kind == "skill":
                    row["files"] = sorted(e["files"])
                if kind == "rule":
                    f = self.floor.check_rule(e["rule"], "permissions", e["name"])
                    if f and f.severity == "error":
                        row["floor_violation"] = f.message
                items[kind].append(row)
        return {"client": client_id, "items": items, "notes": dc.notes, "suggested_params": dc.suggested_params}

    # -- import ---------------------------------------------------------------------------------------
    def do_import(self, client_id: str, selection: list[tuple[str, str]] | None, on_conflict: str | None,
                  enable: bool = True, overrides: dict[tuple[str, str], str] | None = None) -> dict[str, Any]:
        """on_conflict: skip | rename | replace | link, globally and/or per item via `overrides`. Unspecified: an EQUAL existing
        item is linked (enabled for this client), a DIFFERING one is skipped with the differences reported."""
        overrides = overrides or {}
        for v in [on_conflict, *overrides.values()]:
            if v is not None and v not in CONFLICT_MODES:
                raise BadRequest("on_conflict must be skip, rename, replace or link")
        dc = self._discover_raw(client_id)
        entries = self._entries(client_id, dc)
        wanted = None if selection is None else set(selection)
        results: list[dict[str, Any]] = []
        enabled: dict[str, list[str]] = {}
        linked: dict[str, list[str]] = {}
        for kind in IMPORT_KINDS:
            for e in entries[kind]:
                if wanted is not None and (kind, e["name"]) not in wanted:
                    continue
                e["notes"] = [n for n in dc.notes if str(e["name"]) in n]
                res: dict[str, Any] = {"kind": kind, "name": e["name"], "final_name": e["name"], "status": "error", "detail": ""}
                try:
                    self._import_one(client_id, kind, e, overrides.get((kind, str(e["name"])), on_conflict), res)
                except (BadRequest, Conflict, NotFound) as exc:
                    res["status"], res["detail"] = "error", exc.detail
                except Exception as exc:  # noqa: BLE001 - one bad item must not abort the import
                    res["status"], res["detail"] = "error", f"{type(exc).__name__}"
                results.append(res)
                if res["status"] in ("created", "replaced", "renamed"):
                    enabled.setdefault(kind, []).append(str(res["final_name"]))
                elif res["status"] == "linked":
                    linked.setdefault(kind, []).append(str(res["final_name"]))
        if wanted is not None:
            found = {(r["kind"], r["name"]) for r in results}
            for kind, name in sorted(wanted - found):
                results.append({"kind": kind, "name": name, "final_name": name, "status": "error",
                                "detail": "not found in the client's discovered content"})
        self._merge_params(client_id, dc.suggested_params)
        for kind in sorted(set(enabled) | set(linked)):
            names = [*(enabled.get(kind, []) if enable else []), *linked.get(kind, [])]     # linking IS enabling
            if names:
                self.org.enable_for_client(client_id, kind, names)
        return {"client": client_id, "results": results}

    def _merge_params(self, client_id: str, suggested: dict[str, Any]) -> None:
        """Adapter-suggested client params (e.g. instructions_mode) fill in keys the operator has not set."""
        if not suggested:
            return
        doc = self.work.get_client(client_id)
        merged = {**suggested, **doc.params}
        if merged != doc.params:
            self.work.put_client(client_id, {**doc.model_dump(mode="json", exclude_none=True), "params": merged})

    def _import_one(self, client: str, kind: str, e: dict[str, Any], on_conflict: str | None, res: dict[str, Any]) -> None:
        name = str(e["name"])
        final = name if valid_id(name) else _slug(name)
        if not final:
            raise BadRequest("name cannot be turned into a valid id")
        if final != name:
            res["detail"] = f"renamed from {name!r} to satisfy the id rules"
        if kind == "rule":
            f = self.floor.check_rule(e["rule"], "permissions", name)
            if f and f.severity == "error":
                res["status"], res["detail"] = "skipped", f"violates the floor: {f.message}"
                return
        conflict = self._conflict(kind, e)
        replaced = False
        if conflict is not None:
            res["final_name"] = final
            if conflict["equal"]:
                if on_conflict in (None, "link"):
                    res["status"], res["detail"] = "linked", "identical to the existing item: enabled it for this client"
                else:
                    res["status"] = "identical"
                return
            res["differences"] = conflict["differences"]
            if on_conflict is None or on_conflict == "skip":
                res["status"], res["reason"] = "skipped", "differs"
                res["detail"] = "exists with different content: " + ", ".join(conflict["differences"][:8])
                return
            if on_conflict == "link":
                res["status"], res["detail"] = "linked", "enabled the existing canonical item (this client's version was not imported)"
                return
            if on_conflict == "rename":
                n = 2
                while f"{final[:58]}-{n}" in self.work.names(kind):
                    n += 1
                    if n > 99:
                        raise Conflict("could not find a free name")
                final = f"{final[:58]}-{n}"
            else:
                replaced = True
        res["final_name"] = final
        if kind != "skill":
            models: dict[str, type[BaseModel]] = {"agent": AgentMeta, "instruction": InstructionMeta, "mcp": McpDoc, "rule": RuleDoc}
            allowed = set(models[kind].model_fields) | {"body"}
            dropped = sorted(k for k in e["payload"] if k not in allowed)
            if dropped:                                            # unknown keys never fail an import: reported per item
                e["payload"] = {k: v for k, v in e["payload"].items() if k in allowed}
                res["warnings"] = [f"dropped unknown keys: {', '.join(dropped)}"]
        res["warnings"] = [*res.get("warnings", []), *[n for n in e.get("notes", [])]]
        self._write(client, kind, final, e, replace=replaced)
        res["status"] = "replaced" if replaced else ("renamed" if final != name else "created")

    @staticmethod
    def _prepare_skill(name: str, src: dict[str, bytes]) -> dict[str, bytes]:
        """Files as they will be stored: verbatim, except SKILL.md's `name` is forced to the (possibly renamed) directory name."""
        files = dict(src)
        if "SKILL.md" in files:
            try:
                meta, body = yamlsafe.split_frontmatter(files["SKILL.md"].decode("utf-8"))
                if meta.get("name") != name:
                    meta["name"] = name
                    files["SKILL.md"] = yamlsafe.join_frontmatter(meta, body).encode("utf-8")
            except (UnicodeDecodeError, yamlsafe.YamlError):
                pass                                               # left verbatim; the item shows its issues
        return files

    def _write(self, client: str, kind: str, name: str, e: dict[str, Any], replace: bool) -> None:
        if kind == "skill":
            files = self._prepare_skill(name, e["files"])
            if "SKILL.md" not in files:
                raise BadRequest("skill has no SKILL.md")
            if replace and name in self.work.names("skill"):
                self.work.remove_skill_dir(name)
            side = Sidecar(source=f"import:{client}", provenance={"client": client, "imported_at": int(time.time())})
            self.work.write_skill_files(name, files, side)
            return
        payload = dict(e["payload"])
        key = "id" if kind in ("instruction", "rule") else "name"
        payload[key] = name
        self.work.put(kind, name, payload)
