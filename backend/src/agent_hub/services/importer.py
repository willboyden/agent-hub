"""Import: discover what a client natively has (adapter.discover through a FileSystem port limited to configured
roots) and write chosen items into the content WORKING TREE. Imported MCP servers are always `unscanned` (the hub does not scan
anything itself; `clean` is an operator attestation bound to a digest of the server's command/args/url/pinned_ref/transport)."""
from __future__ import annotations

import hashlib
import time
from collections.abc import Callable
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from pydantic import BaseModel

from agent_hub.adapters import AdapterRegistry
from agent_hub.adapters.base import CAPABILITIES, DiscoveredContent
from agent_hub.domain import yamlsafe
from agent_hub.domain.errors import BadRequest, Conflict, NotFound, ProblemError, Unavailable
from agent_hub.domain.ids import ENV_NAME_RE, HOST_RE, valid_id
from agent_hub.domain.models import AgentMeta, InstructionMeta, McpDoc, RuleDoc, Sidecar
from agent_hub.domain.pathsafe import PathError
from agent_hub.services.common import scrub
from agent_hub.services.content import ContentStore
from agent_hub.services.floor import Floor
from agent_hub.services.fs import discovery_fs
from agent_hub.services.organize import Organizer
from agent_hub.services.resolver import client_config
from agent_hub.services.skilleval import content_hash

if TYPE_CHECKING:
    from agent_hub.services.observe import ObserveService

IMPORT_KINDS = ("skill", "agent", "instruction", "mcp", "rule")
CONFLICT_MODES = ("skip", "rename", "replace", "link")


@dataclass
class ImportGate:
    """F8: what the evaluation gate says about a skill about to be written into the content tree.
    mode off: no gate. advisory: every row carries the gate, nothing is refused. require: `missing` and `blocked` are refused
    (`eval_required` / `eval_blocked`), a blocked skill passes only with an override reason (gate becomes `overridden`)."""
    mode: str
    check: Callable[[str], dict[str, Any]]                            # content hash -> SkillEvalService.gate()
    override_reason: str | None = None
    audit: Callable[[str, dict[str, Any]], None] | None = None        # called once a skill was written under an override

    def decide(self, files: dict[str, bytes]) -> tuple[dict[str, Any] | None, str | None]:
        if self.mode == "off":
            return None, None
        g = self.check(content_hash(files))
        info: dict[str, Any] = {"gate": g["status"], "run_id": g.get("run_id"), "verdict": g.get("verdict"),
                                "reasons": list(g.get("reasons") or [])}
        if self.mode == "require":
            if g["status"] == "missing":
                return info, "eval_required"
            if g["status"] == "blocked":
                if self.override_reason:
                    return {**info, "gate": "overridden", "override_reason": self.override_reason, "gate_status": "blocked"}, None
                return info, "eval_blocked"
        return info, None


_REFUSED = {"eval_required": "no evaluation of this exact content exists; evaluate it first (eval_gate is `require`)",
            "eval_blocked": "the evaluation did not clear this skill (security finding, or a security scan that did not finish); fix it "
                            "or give an override reason",
            "secret_detected": "the text contains secret-looking values; remove them or give an override reason"}


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
                  enable: bool = True, overrides: dict[tuple[str, str], str] | None = None,
                  gate: ImportGate | None = None) -> dict[str, Any]:
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
                    self._import_one(client_id, kind, e, overrides.get((kind, str(e["name"])), on_conflict), res, gate)
                except ProblemError as exc:
                    res["status"], res["code"], res["detail"] = "error", exc.code, exc.detail
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

    def import_observed(self, client_id: str, items: list[dict[str, Any]], on_conflict: str, enable: bool,
                        gate: ImportGate | None, observe: ObserveService) -> dict[str, Any]:
        """F8: create content skills from a client's stored observation. Never enabled unless asked: enabling an observed skill
        for the SAME client would deliver a second copy next to the original."""
        if on_conflict not in CONFLICT_MODES:
            raise BadRequest("on_conflict must be skip, rename, replace or link")
        if client_id not in self.work.names_of("clients"):
            raise NotFound(f"no such client: {client_id}")
        if observe.index(client_id) is None:
            raise NotFound("this client has not been observed yet", code="not_observed")
        results: list[dict[str, Any]] = []
        wanted: list[str] = []
        for it in items:
            key = str(it.get("key", ""))
            row: dict[str, Any] = {"key": key, "final_name": "", "status": "error", "gate": None, "detail": ""}
            try:
                ent = observe.entry(client_id, key)
                if ent is None:
                    raise NotFound("not in the stored observation of this client")
                name = str(it.get("name") or ent.get("name") or key)
                row["final_name"] = name
                e: dict[str, Any] = {"scan_secrets": True, "observed_key": key, "name": name, "description": ent.get("description", ""), "files": observe.files(client_id, key),
                                     "observed": {"client": client_id, "path": ent.get("path"), "origin": ent.get("origin"),
                                                  "content_hash": ent.get("content_hash"),
                                                  "observed_at": (observe.index(client_id) or {}).get("observed_at")}}
                inner: dict[str, Any] = {"final_name": name, "status": "error", "detail": ""}
                self._import_one(client_id, "skill", e, on_conflict, inner, gate)
                row.update({k: v for k, v in inner.items() if k not in ("kind", "name")})
                if row["status"] == "identical":
                    row["status"] = "skipped"
                    row["detail"] = "identical to the existing skill " + str(row["final_name"])
                if row["status"] in ("created", "replaced", "renamed"):
                    wanted.append(str(row["final_name"]))
            except ProblemError as exc:
                row["status"], row["code"], row["detail"] = "error", exc.code, exc.detail
            except Exception as exc:  # noqa: BLE001 - one bad item must not abort the import
                row["status"], row["detail"] = "error", type(exc).__name__
            results.append(row)
        if enable and wanted:
            self.org.enable_for_client(client_id, "skill", wanted)
        return {"client": client_id, "results": results, "enabled": bool(enable and wanted)}

    def _import_one(self, client: str, kind: str, e: dict[str, Any], on_conflict: str | None, res: dict[str, Any],
                    gate: ImportGate | None = None) -> None:
        name = str(e["name"])
        info, refusal = gate.decide(e["files"]) if (gate is not None and kind == "skill") else (None, None)
        if info is not None:
            res["gate"] = info["gate"]
            e["_eval"] = {k: v for k, v in {"run_id": info.get("run_id"), "verdict": info.get("verdict"), "gate": info["gate"],
                                            "override_reason": info.get("override_reason")}.items()
                          if v is not None or k in ("run_id", "verdict")}
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
        overrides: list[str] = []
        if info is not None and info["gate"] == "overridden":
            overrides.append("eval_blocked")
        if e.get("scan_secrets") and kind == "skill":
            hits = self._secret_files(e["files"])
            if hits and gate is not None and gate.override_reason:
                overrides.append("secret_detected")
            elif hits and refusal is None:
                refusal = "secret_detected"
                res["secret_files"] = hits[:10]
        if refusal is not None:
            res["status"], res["code"], res["detail"] = "refused", refusal, _REFUSED[refusal]
            if info and info.get("reasons") and refusal != "secret_detected":
                res["detail"] += ": " + "; ".join(str(x) for x in info["reasons"][:3])
            if refusal == "secret_detected":
                res["detail"] += " (" + ", ".join(res["secret_files"]) + ")"
            return
        outcome = "written"
        try:
            self._write(client, kind, final, e, replace=replaced)
        except BaseException as exc:
            outcome = f"failed: {type(exc).__name__}"
            raise
        else:
            res["status"] = "replaced" if replaced else ("renamed" if final != name else "created")
        finally:                                    # the override was USED even if the write then failed: always on record
            if overrides and gate is not None and gate.audit is not None:
                gate.audit(final, {**(info or {}), "overrides": overrides, "outcome": outcome, "override_reason": gate.override_reason,
                                   "content_hash": content_hash(e["files"]), "key": e.get("observed_key")})

    @staticmethod
    def _secret_files(files: dict[str, bytes]) -> list[str]:
        """Text files in which the hub's secret scrubber would redact something."""
        hits: list[str] = []
        for rel, data in sorted(files.items()):
            if b"\x00" in data[:4096]:
                continue
            text = data.decode("utf-8", "replace")
            if scrub(text) != text:
                hits.append(rel)
        return hits

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
            self.work.validate_skill_files(files)              # BEFORE anything is removed or written
            ev = e.get("_eval")
            if e.get("observed"):
                prov: dict[str, Any] = {**e["observed"], "imported_at": int(time.time())}
                side = Sidecar(source=f"observed:{client}", provenance={**prov, **({"evaluation": ev} if ev else {})})
            else:
                side = Sidecar(source=f"import:{client}", provenance={"client": client, "imported_at": int(time.time()),
                                                                      **({"evaluation": ev} if ev else {})})
            if replace and name in self.work.names("skill"):
                self.work.swap_skill_files(name, files, side)
            else:
                self.work.write_skill_files(name, files, side)
            return
        payload = dict(e["payload"])
        key = "id" if kind in ("instruction", "rule") else "name"
        payload[key] = name
        self.work.put(kind, name, payload)
