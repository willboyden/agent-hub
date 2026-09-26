"""Organising content in the working tree: collections (multi-membership, ordering, bulk ops), profile toggles and the
matrix (items grouped by collection x clients). Everything here edits files only; nothing reaches a client."""
from __future__ import annotations

from typing import Any

from agent_hub.adapters import AdapterRegistry
from agent_hub.domain.errors import BadRequest, Conflict, NotFound
from agent_hub.domain.ids import KINDS, PLURAL, valid_id
from agent_hub.domain.models import CollectionDoc, Member, ProfileDoc, Sidecar
from agent_hub.services.content import ContentStore, Item
from agent_hub.services.resolver import Resolver, cap_supported, client_caps, enabled_keys


class Organizer:
    def __init__(self, work: ContentStore, resolver: Resolver, registry: AdapterRegistry) -> None:
        self.work = work
        self.resolver = resolver
        self.registry = registry

    # -- collections ---------------------------------------------------------------------------
    def _require_item(self, kind: str, name: str) -> None:
        if kind not in KINDS:
            raise BadRequest(f"unknown kind: {kind}")
        if not valid_id(name) or name not in self.work.names(kind):
            raise NotFound(f"no such {kind}: {name}")

    def create_collection(self, payload: dict[str, Any]) -> CollectionDoc:
        cid = payload.get("id")
        if not isinstance(cid, str) or not valid_id(cid):
            raise BadRequest("`id` must match ^[a-z0-9][a-z0-9._-]{0,63}$", code="invalid_id")
        if cid in self.work.names_of("collections"):
            raise Conflict("collection already exists", code="already_exists")
        for m in payload.get("members", []) or []:
            if isinstance(m, dict):
                self._require_item(str(m.get("kind")), str(m.get("name")))
        return self.work.put_collection(cid, payload)

    def update_collection(self, cid: str, payload: dict[str, Any]) -> CollectionDoc:
        self.work.get_collection(cid)
        for m in payload.get("members", []) or []:
            if isinstance(m, dict):
                self._require_item(str(m.get("kind")), str(m.get("name")))
        return self.work.put_collection(cid, payload)

    def add_members(self, cid: str, members: list[Member]) -> CollectionDoc:
        coll = self.work.get_collection(cid)
        have = {(m.kind, m.name) for m in coll.members}
        for m in members:
            self._require_item(m.kind, m.name)
            if (m.kind, m.name) not in have:
                coll.members.append(m)
                have.add((m.kind, m.name))
        return self.work.put_collection(cid, coll.model_dump(mode="json"))

    def remove_member(self, cid: str, kind: str, name: str) -> CollectionDoc:
        coll = self.work.get_collection(cid)
        kept = [m for m in coll.members if not (m.kind == kind and m.name == name)]
        if len(kept) == len(coll.members):
            raise NotFound("not a member of this collection")
        coll.members = kept
        return self.work.put_collection(cid, coll.model_dump(mode="json"))

    def reorder(self, ids: list[str]) -> list[CollectionDoc]:
        known = set(self.work.names_of("collections"))
        for i in ids:
            if i not in known:
                raise NotFound(f"no such collection: {i}")
        if len(set(ids)) != len(ids):
            raise BadRequest("duplicate ids")
        out = []
        for idx, cid in enumerate(ids):
            coll = self.work.get_collection(cid)
            coll.order = (idx + 1) * 10
            out.append(self.work.put_collection(cid, coll.model_dump(mode="json")))
        return out

    def accept_suggestions(self, ids: list[str], overrides: dict[str, dict[str, Any]]) -> list[dict[str, Any]]:
        """Create suggested collections in the working tree. Suggestions are recomputed here, never trusted from the client."""
        from agent_hub.services.suggest import suggest
        if not ids or len(set(ids)) != len(ids):
            raise BadRequest("`ids` must be a non-empty list without duplicates")
        known = {s["id"]: s for s in suggest(self.work.catalog())}
        for i in ids:
            if i not in known:
                raise NotFound(f"no such suggestion: {i}")
        for k, ov in overrides.items():
            if k not in ids or not isinstance(ov, dict) or set(ov) - {"title", "description", "members"}:
                raise BadRequest("overrides may only set title, description and members for accepted ids")
        created = []
        for i in ids:
            s = known[i]
            payload = {"id": i, "title": s["title"], "description": s["description"], "icon": s["icon"], "color": s["color"],
                       "members": s["members"], **overrides.get(i, {})}
            created.append(self.create_collection(payload).model_dump(mode="json"))
        return created

    # -- bulk ------------------------------------------------------------------------------------
    def bulk(self, items: list[Member], add_to: str | None, remove_from: str | None,
             add_tags: list[str], remove_tags: list[str]) -> list[dict[str, Any]]:
        results: list[dict[str, Any]] = []
        if add_to:
            self.work.get_collection(add_to)
        if remove_from:
            self.work.get_collection(remove_from)
        for m in items:
            r: dict[str, Any] = {"kind": m.kind, "name": m.name, "ok": True, "detail": []}
            try:
                self._require_item(m.kind, m.name)
                if add_to:
                    self.add_members(add_to, [m])
                    r["detail"].append(f"added to {add_to}")
                if remove_from:
                    try:
                        self.remove_member(remove_from, m.kind, m.name)
                        r["detail"].append(f"removed from {remove_from}")
                    except NotFound:
                        r["detail"].append(f"was not in {remove_from}")
                if add_tags or remove_tags:
                    self._retag(m.kind, m.name, add_tags, remove_tags)
                    r["detail"].append("tags updated")
            except (NotFound, BadRequest, Conflict) as exc:
                r["ok"], r["error"] = False, exc.detail
            except Exception as exc:  # noqa: BLE001 - one bad item must not abort the batch
                r["ok"], r["error"] = False, f"{type(exc).__name__}"
            results.append(r)
        return results

    def _retag(self, kind: str, name: str, add: list[str], remove: list[str]) -> None:
        item = self.work.load(kind, name)
        if item.meta is None:
            raise BadRequest(f"{kind} {name} is invalid; fix it before tagging")
        for t in add:
            if not valid_id(t):
                raise BadRequest(f"invalid tag {t!r}", code="invalid_id")
        tags = [t for t in item.tags if t not in remove]
        tags += [t for t in add if t not in tags]
        if kind == "skill":
            side = (item.sidecar or Sidecar()).model_copy(update={"tags": tags})
            self.work.set_sidecar(name, side)
            return
        data = item.meta.model_dump(mode="json", exclude_none=True)
        data["tags"] = tags
        if kind in ("agent", "instruction", "memory"):
            data["body"] = item.body
        self.work.put(kind, name, data, must_exist=True)

    # -- profiles / toggles ----------------------------------------------------------------------------
    def _client_ids(self) -> set[str]:
        return set(self.work.names_of("clients"))

    def cell_state(self, cat: Any, resolved: Any, client: str, kind: str, name: str) -> dict[str, Any]:
        adapter = resolved.adapter
        if adapter is None:
            return {"state": "blocked", "reason": f"adapter {resolved.client.adapter!r} is not installed"}
        if not cap_supported(client_caps(adapter, resolved.client), kind):
            return {"state": "unsupported", "reason": f"{adapter.id} cannot consume {PLURAL[kind]}"}
        profile: ProfileDoc = cat.profiles.get(client, ProfileDoc())
        via, _ = enabled_keys(profile, cat)
        prov = via.get((kind, name), [])
        reason = self.resolver.block_reason(cat, resolved, kind, name)
        if reason:
            return {"state": "blocked", "reason": reason, "via": prov}
        if "direct" in prov:
            return {"state": "on", "via": prov}
        if prov:
            return {"state": "via_collection", "via": prov}
        return {"state": "off", "via": []}

    def toggle(self, client: str, kind: str, name: str, enabled: bool) -> dict[str, Any]:
        if client not in self._client_ids():
            raise NotFound(f"no such client: {client}")
        self._require_item(kind, name)
        cat = self.work.catalog()
        resolved = self.resolver.resolve(cat, client)
        cell = self.cell_state(cat, resolved, client, kind, name)
        prof = cat.profiles.get(client, ProfileDoc())
        plural = PLURAL[kind]
        enable: list[str] = list(getattr(prof.enable, plural))
        disable: list[str] = list(getattr(prof.disable, plural))
        via_coll = any(m.kind == kind and m.name == name for cid in prof.collections
                       if (coll := cat.collections.get(cid)) is not None for m in coll.members)   # ignores `disable`
        if enabled:
            if cell["state"] in ("unsupported", "blocked"):
                raise Conflict(f"cannot enable: {cell.get('reason', cell['state'])}", code="cell_" + cell["state"],
                               reason=cell.get("reason"))
            disable = [n for n in disable if n != name]
            if not via_coll and name not in enable:
                enable.append(name)
        else:
            enable = [n for n in enable if n != name]
            if via_coll and name not in disable:
                disable.append(name)
        setattr(prof.enable, plural, enable)
        setattr(prof.disable, plural, disable)
        self.work.put_profile(client, prof)
        cat2 = self.work.catalog()
        return {"client": client, "kind": kind, "name": name,
                **self.cell_state(cat2, self.resolver.resolve(cat2, client), client, kind, name)}

    def bulk_toggle(self, clients: list[str], items: list[Member], enabled: bool) -> list[dict[str, Any]]:
        out: list[dict[str, Any]] = []
        for c in clients:
            for m in items:
                try:
                    out.append({**self.toggle(c, m.kind, m.name, enabled), "ok": True})
                except (NotFound, Conflict, BadRequest) as exc:
                    out.append({"client": c, "kind": m.kind, "name": m.name, "ok": False, "error": exc.detail,
                                "code": exc.code})
        return out

    def enable_for_client(self, client: str, kind: str, names: list[str]) -> None:
        """Import helper: mark items enabled for a client without the blocked/unsupported gate (the plan still gates)."""
        prof = self.work.get_profile(client)
        plural = PLURAL[kind]
        lst = list(getattr(prof.enable, plural))
        for n in names:
            if n not in lst:
                lst.append(n)
        setattr(prof.enable, plural, lst)
        setattr(prof.disable, plural, [n for n in getattr(prof.disable, plural) if n not in names])
        self.work.put_profile(client, prof)

    # -- matrix ---------------------------------------------------------------------------------------
    def matrix(self) -> dict[str, Any]:
        cat = self.work.catalog()
        clients = sorted(cat.clients)
        resolved = {c: self.resolver.resolve(cat, c) for c in clients}

        def row(item: Item) -> dict[str, Any]:
            return {"kind": item.kind, "name": item.name, "title": item.title, "valid": item.valid,
                    "cells": {c: self.cell_state(cat, resolved[c], c, item.kind, item.name) for c in clients}}

        groups: list[dict[str, Any]] = []
        in_any: set[tuple[str, str]] = set()
        for coll in sorted(cat.collections.values(), key=lambda c: (c.order, c.id)):
            rows = []
            for m in coll.members:
                it = cat.items.get((m.kind, m.name))
                if it is not None:
                    rows.append(row(it))
                    in_any.add((m.kind, m.name))
            groups.append({"id": coll.id, "title": coll.title, "icon": coll.icon, "color": coll.color, "rows": rows})
        loose = [row(i) for (k, n), i in sorted(cat.items.items()) if (k, n) not in in_any]
        groups.append({"id": None, "title": "Uncollected", "icon": "", "color": "", "rows": loose})
        cols = [{"id": c, "display_name": cat.clients[c].display_name, "adapter": cat.clients[c].adapter,
                 "adapter_installed": resolved[c].adapter is not None} for c in clients]
        return {"clients": cols, "groups": groups}
