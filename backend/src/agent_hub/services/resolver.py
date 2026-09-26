"""Resolver: profile (collections + enable - disable) -> enabled items -> rules merged with the floor -> RenderContext.
Pure over a Catalog: it reads nothing and writes nothing."""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from agent_hub.adapters import AdapterRegistry
from agent_hub.adapters.base import (
    AdapterCaps,
    AgentItem,
    ClientAdapter,
    ClientConfig,
    Diag,
    InstructionItem,
    McpItem,
    MemoryItem,
    PermissionSet,
    RenderContext,
    Rule,
    SkillItem,
)
from agent_hub.domain.ids import CONCERN_OF_KIND, DEFAULT_MANAGE, KINDS, PLURAL
from agent_hub.domain.models import (
    AgentMeta,
    ClientDoc,
    Finding,
    InstructionMeta,
    McpDoc,
    MemoryMeta,
    ProfileDoc,
    RuleDoc,
    SkillMeta,
)
from agent_hub.domain.pathsafe import PathError, expand_root
from agent_hub.services.attest import Attestor
from agent_hub.services.content import Catalog, Item, meta_as
from agent_hub.services.floor import Floor
from agent_hub.services.targets import TargetGuard

CAP_OF_CONCERN = {"skills": "skills", "agents": "agents", "instructions": "instructions", "mcp": "mcp",
                  "permissions": "permissions", "memory": "memory"}


def client_config(doc: ClientDoc, adapter: ClientAdapter | None = None) -> ClientConfig:
    """ClientDoc -> ClientConfig with roots expanded (~) and validated. Raises PathError."""
    roots = {k: str(expand_root(v)) for k, v in doc.roots.items()}
    return ClientConfig(id=doc.id, adapter=doc.adapter, display_name=doc.display_name, description=doc.description,
                        roots=roots, params=doc.params, spec=doc.spec, strict=doc.strict, icon=doc.icon, color=doc.color,
                        manage=manage_flags(doc, adapter))


def _flags(raw: Any) -> dict[str, bool]:
    return {k: v for k, v in raw.items() if k in DEFAULT_MANAGE and isinstance(v, bool)} if isinstance(raw, dict) else {}


def manage_flags(doc: ClientDoc, adapter: ClientAdapter | None = None) -> dict[str, bool]:
    """Effective managed(True)/advisory(False) per concern: the client's typed `manage`, else its `params.manage`, else the
    ADAPTER's documented default (its default_config), else the hub defaults (mcp/permissions/memory advisory)."""
    adapter_default: dict[str, bool] = {}
    if adapter is not None:
        try:
            d = adapter.default_config()
            adapter_default = {**_flags(d.params.get("manage")), **_flags(d.manage)}
        except Exception:  # noqa: BLE001 - a broken default_config must not break planning
            adapter_default = {}
    return {**DEFAULT_MANAGE, **adapter_default, **_flags(doc.params.get("manage")), **_flags(doc.manage)}


def client_caps(adapter: ClientAdapter, doc: ClientDoc) -> AdapterCaps:
    """Adapter caps; for `generic` they depend on the client's spec (caps_for_spec)."""
    if adapter.id == "generic" and doc.spec:
        try:
            from agent_hub.adapters.generic import caps_for_spec
            return caps_for_spec(doc.spec)
        except Exception:  # noqa: BLE001 - fall back to the adapter's own (permissive) caps
            return adapter.caps()
    return adapter.caps()


def cap_supported(caps: AdapterCaps, kind: str) -> bool:
    return bool(getattr(caps, CAP_OF_CONCERN[CONCERN_OF_KIND[kind]]))


@dataclass
class Resolved:
    client: ClientDoc
    cfg: ClientConfig | None
    adapter: ClientAdapter | None
    manage: dict[str, bool]
    entries: dict[str, list[dict[str, Any]]] = field(default_factory=dict)       # plural -> [{name, via}]
    ctx: RenderContext | None = None
    findings: list[Finding] = field(default_factory=list)
    diagnostics: list[Diag] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    kept_rules: list[Rule] = field(default_factory=list)                          # content rules that passed the floor
    kept_sources: list[str] = field(default_factory=list)                         # "profile" (enabled directly) | "content" (via collection)
    allow_hosts: list[str] = field(default_factory=list)
    permissions: PermissionSet = field(default_factory=PermissionSet)
    mcp: list[McpItem] = field(default_factory=list)


def enabled_keys(profile: ProfileDoc, catalog: Catalog) -> tuple[dict[tuple[str, str], list[str]], list[str]]:
    """(key -> provenance list, warnings). Provenance is 'direct' and/or 'collection:<id>'."""
    via: dict[tuple[str, str], list[str]] = {}
    warnings: list[str] = []
    for cid in profile.collections:
        coll = catalog.collections.get(cid)
        if coll is None:
            warnings.append(f"profile references missing collection {cid!r}")
            continue
        for m in coll.members:
            via.setdefault((m.kind, m.name), []).append(f"collection:{cid}")
    for kind in KINDS:
        for name in getattr(profile.enable, PLURAL[kind]):
            via.setdefault((kind, name), []).append("direct")
    for kind in KINDS:
        for name in getattr(profile.disable, PLURAL[kind]):
            via.pop((kind, name), None)
    return via, warnings


def to_mcp(item: Item, attestor: Attestor | None) -> McpItem:
    """The McpItem the floor and adapters see. `clean` is only honoured while the attested digest matches the current fields:
    any edit to command/args/url/pinned_ref/transport puts the server back to `unscanned`."""
    d = item.meta.model_dump(include=set(McpItem.model_fields)) if item.meta else {}
    if d.get("scan_status") == "clean":
        doc = meta_as(item, McpDoc)
        if attestor is None or not attestor.valid(doc.scan_digest, doc.command, doc.args, doc.url, doc.pinned_ref, doc.transport):
            d["scan_status"] = "unscanned"
    return McpItem.model_validate(d)


class Resolver:
    def __init__(self, floor: Floor, registry: AdapterRegistry, guard: TargetGuard | None = None,
                 attestor: Attestor | None = None) -> None:
        self.attestor = attestor
        self.floor = floor
        self.registry = registry
        self.guard = guard

    def resolve(self, catalog: Catalog, client_id: str) -> Resolved:
        doc = catalog.clients[client_id]
        adapter = self.registry.get(doc.adapter)
        res = Resolved(client=doc, cfg=None, adapter=adapter, manage=manage_flags(doc, adapter))
        try:
            res.cfg = client_config(doc, adapter)
        except PathError as exc:
            res.diagnostics.append(Diag(severity="error", code="bad_root", message=f"client roots invalid: {exc}"))
            return res
        if self.guard is not None:
            for rname, rpath in res.cfg.roots.items():
                prob = self.guard.root_problem(rpath)
                if prob:
                    res.diagnostics.append(Diag(severity="error", code="bad_root", message=f"root {rname!r}: {prob}"))
                    return res
        if adapter is None:
            res.diagnostics.append(Diag(severity="error", code="adapter_missing",
                                        message=f"adapter {doc.adapter!r} is not installed"))
            return res
        caps = client_caps(adapter, doc)
        profile = catalog.profiles.get(client_id, ProfileDoc())
        via, warnings = enabled_keys(profile, catalog)
        res.warnings += warnings
        direct = {(k, n) for k in KINDS for n in getattr(profile.enable, PLURAL[k])}

        chosen: dict[str, list[Item]] = {k: [] for k in KINDS}
        for (kind, name), provenance in sorted(via.items()):
            item = catalog.items.get((kind, name))
            if item is None:
                res.warnings.append(f"{kind} {name!r} is enabled but does not exist")
                continue
            if not item.valid:
                res.diagnostics.append(Diag(severity="error", code="invalid_content", item=f"{kind}:{name}",
                                            message="; ".join(f"{i.path}: {i.message}" for i in item.issues)[:300]))
                continue
            if item.applies_to and client_id not in item.applies_to:
                res.warnings.append(f"{kind} {name!r} is not meant for this client (applies_to)")
                continue
            if not cap_supported(caps, kind):
                sev = "error" if (doc.strict and (kind, name) in direct) else "info" if (kind, name) not in direct else "warn"
                res.diagnostics.append(Diag(severity=sev, code="concern_unsupported", item=f"{kind}:{name}",
                                            message=f"{adapter.id} cannot consume {PLURAL[kind]}"))
                continue
            chosen[kind].append(item)
            res.entries.setdefault(PLURAL[kind], []).append({"name": name, "via": provenance})

        # rules -> floor
        content_rules = []
        for i in chosen["rule"]:
            rm = meta_as(i, RuleDoc)
            content_rules.append((i.name, Rule(kind=rm.kind, match=rm.match, decision=rm.decision, reason=rm.reason)))
        mcp_clean = {i.name: to_mcp(i, self.attestor).scan_status == "clean" for i in chosen["mcp"]}
        fr = self.floor.enforce(content_rules, mcp_clean)
        res.findings += fr.findings
        res.permissions = fr.permissions
        res.allow_hosts = fr.allow_hosts
        kept = fr.kept
        res.kept_rules = kept
        prov = {e["name"]: e["via"] for e in res.entries.get("rules", [])}
        res.kept_sources = ["profile" if "direct" in prov.get(rid, []) else "content" for rid in fr.kept_ids]
        if not caps.permissions:
            res.warnings.append(f"{adapter.id} cannot express permissions: the floor cannot be enforced natively")

        # mcp -> floor
        mcp_ok: list[McpItem] = []
        for it in chosen["mcp"]:
            m = to_mcp(it, self.attestor)
            fs = self.floor.check_mcp(m, kept)
            if fs:
                res.findings += fs
            else:
                mcp_ok.append(m)
        res.mcp = mcp_ok

        instr = sorted(((i, meta_as(i, InstructionMeta)) for i in chosen["instruction"]), key=lambda t: (t[1].order, t[0].name))
        agents = [(i, meta_as(i, AgentMeta)) for i in chosen["agent"]]
        skills = []
        for i in chosen["skill"]:
            sm = meta_as(i, SkillMeta)
            fs = self.floor.check_skill(i.name, sm.model_extra or {}, {m.name for m in mcp_ok})
            res.findings += fs
            if not any(f.severity == "error" for f in fs):           # an offending skill is withheld, never rewritten
                skills.append((i, sm))
        mems = [(i, meta_as(i, MemoryMeta)) for i in chosen["memory"]]
        res.ctx = RenderContext(
            client=res.cfg,
            skills=[SkillItem(name=i.name, description=m.description, files=dict(i.files), groups=i.groups, tags=i.tags)
                    for i, m in skills],
            agents=[AgentItem(name=i.name, description=m.description, capabilities=list(m.capabilities),
                              model_tier=m.model_tier, mode=m.mode, read_only=m.read_only, body=i.body)
                    for i, m in agents],
            instructions=[InstructionItem(id=i.name, title=m.title, order=m.order, body=i.body) for i, m in instr],
            mcp_servers=mcp_ok,
            memory=[MemoryItem(id=i.name, type=m.type, title=m.title, body=i.body) for i, m in mems],
            permissions=fr.permissions,
            endpoints=dict(catalog.endpoints),
        )
        return res

    def block_reason(self, catalog: Catalog, resolved: Resolved, kind: str, name: str) -> str | None:
        """Why enabling this item for the client would be blocked (matrix cell 'blocked'), else None."""
        item = catalog.items.get((kind, name))
        if item is None:
            return "item does not exist"
        if not item.valid:
            return "content is invalid: " + "; ".join(i.message for i in item.issues)[:200]
        if item.applies_to and resolved.client.id not in item.applies_to:
            return f"applies_to limits it to {', '.join(item.applies_to)}"
        if kind == "mcp":
            fs = self.floor.check_mcp(to_mcp(item, self.attestor), resolved.kept_rules)
            if fs:
                return fs[0].message
        if kind == "rule" and item.meta is not None:
            rm = meta_as(item, RuleDoc)
            f = self.floor.check_rule(Rule(kind=rm.kind, match=rm.match, decision=rm.decision), "permissions", name)
            if f and f.severity == "error":
                return f.message
        return None
