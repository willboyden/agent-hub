"""collections, bulk ops, profiles, matrix."""
from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Body, Response
from pydantic import BaseModel, ConfigDict

from agent_hub.api.deps import Admin, H
from agent_hub.domain.models import Kind, Member
from agent_hub.services.store import page_list

router = APIRouter()


class Strict(BaseModel):
    model_config = ConfigDict(extra="forbid")


class MembersIn(Strict):
    members: list[Member]


class ReorderIn(Strict):
    ids: list[str]


class BulkIn(Strict):
    items: list[Member]
    add_to_collection: str | None = None
    remove_from_collection: str | None = None
    add_tags: list[str] = []
    remove_tags: list[str] = []


class ToggleIn(Strict):
    client: str
    kind: Kind
    name: str
    enabled: bool


class BulkToggleIn(Strict):
    clients: list[str]
    items: list[Member]
    enabled: bool


@router.get("/collections")
def list_collections(hub: H, limit: int = 100, cursor: str | None = None) -> dict[str, Any]:
    return page_list([c.model_dump(mode="json") for c in hub.work.list_collections()], limit, cursor)


class AcceptIn(Strict):
    ids: list[str]
    overrides: dict[str, dict[str, Any]] = {}


@router.get("/collections/suggestions")
def suggestions(hub: H) -> dict[str, Any]:
    from agent_hub.services.suggest import suggest
    return {"items": suggest(hub.work.catalog()), "next_cursor": None}


@router.post("/collections/suggestions/accept", status_code=201)
def accept_suggestions(body: AcceptIn, hub: H, _: Admin) -> dict[str, Any]:
    with hub.mutating():
        return {"items": hub.organizer.accept_suggestions(body.ids, body.overrides), "next_cursor": None}


@router.post("/collections", status_code=201)
def create_collection(hub: H, _: Admin, body: dict[str, Any] = Body(...)) -> dict[str, Any]:
    with hub.mutating():
        return hub.organizer.create_collection(body).model_dump(mode="json")


@router.post("/collections/reorder")
def reorder(body: ReorderIn, hub: H, _: Admin) -> dict[str, Any]:
    with hub.mutating():
        return {"items": [c.model_dump(mode="json") for c in hub.organizer.reorder(body.ids)], "next_cursor": None}


@router.put("/collections/{cid}")
def put_collection(cid: str, hub: H, _: Admin, body: dict[str, Any] = Body(...)) -> dict[str, Any]:
    with hub.mutating():
        return hub.organizer.update_collection(cid, {**body, "id": cid}).model_dump(mode="json")


@router.delete("/collections/{cid}", status_code=204)
def delete_collection(cid: str, hub: H, _: Admin) -> Response:
    with hub.mutating():
        hub.work.delete_collection(cid)
    return Response(status_code=204)


@router.post("/collections/{cid}/members")
def add_members(cid: str, body: MembersIn, hub: H, _: Admin) -> dict[str, Any]:
    with hub.mutating():
        return hub.organizer.add_members(cid, body.members).model_dump(mode="json")


@router.delete("/collections/{cid}/members/{kind}/{name}")
def remove_member(cid: str, kind: str, name: str, hub: H, _: Admin) -> dict[str, Any]:
    with hub.mutating():
        return hub.organizer.remove_member(cid, kind, name).model_dump(mode="json")


@router.post("/items/bulk")
def bulk(body: BulkIn, hub: H, _: Admin) -> dict[str, Any]:
    with hub.mutating():
        res = hub.organizer.bulk(body.items, body.add_to_collection, body.remove_from_collection, body.add_tags,
                                 body.remove_tags)
    return {"results": res, "ok": all(r["ok"] for r in res)}


@router.get("/profiles/{client}")
def get_profile(client: str, hub: H) -> dict[str, Any]:
    return hub.work.get_profile(client).model_dump(mode="json")


@router.put("/profiles/{client}")
def put_profile(client: str, hub: H, _: Admin, body: dict[str, Any] = Body(...)) -> dict[str, Any]:
    with hub.mutating():
        if client not in hub.work.names_of("clients"):
            from agent_hub.domain.errors import NotFound
            raise NotFound(f"no such client: {client}")
        return hub.work.put_profile(client, body).model_dump(mode="json")


@router.get("/matrix")
def matrix(hub: H) -> dict[str, Any]:
    return hub.organizer.matrix()


@router.post("/matrix/toggle")
def toggle(body: ToggleIn, hub: H, _: Admin) -> dict[str, Any]:
    with hub.mutating():
        return hub.organizer.toggle(body.client, body.kind, body.name, body.enabled)


@router.post("/matrix/bulk")
def bulk_toggle(body: BulkToggleIn, hub: H, _: Admin) -> dict[str, Any]:
    with hub.mutating():
        res = hub.organizer.bulk_toggle(body.clients, body.items, body.enabled)
    return {"results": res, "ok": all(r["ok"] for r in res)}
