"""CRUD for skills, agents, instructions, mcp, rules, memory (+ memory inbox). Edits touch the WORKING TREE only."""
from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Body, Query, Request, Response
from pydantic import BaseModel, ConfigDict

from agent_hub.api.deps import Admin, H, principal_of
from agent_hub.domain.errors import Forbidden

router = APIRouter()
inbox_router = APIRouter()


class DuplicateIn(BaseModel):
    model_config = ConfigDict(extra="forbid")
    new_name: str


class InboxIn(BaseModel):
    model_config = ConfigDict(extra="forbid")
    client: str
    title: str
    body: str
    type: str = "reference"


def _register(kind: str, plural: str) -> None:
    @router.get(f"/{plural}", name=f"list_{plural}")
    def list_(hub: H, q: str | None = None, group: str | None = None, tag: str | None = None, client: str | None = None,
              enabled: bool | None = None, source: str | None = None, issues: bool | None = None, sort: str = "name",
              limit: int = Query(100, ge=1, le=500), cursor: str | None = None) -> dict[str, Any]:
        return hub.list_items(kind, q=q, group=group, tag=tag, client=client, enabled=enabled, source=source,
                              issues=issues, sort=sort, limit=limit, cursor=cursor)

    @router.get(f"/{plural}/{{name}}", name=f"get_{kind}")
    def get_(name: str, hub: H) -> dict[str, Any]:
        return hub.item_detail(kind, name)

    @router.put(f"/{plural}/{{name}}", name=f"put_{kind}")
    def put_(name: str, hub: H, _: Admin, body: dict[str, Any] = Body(...)) -> dict[str, Any]:
        with hub.mutating():
            hub.work.put(kind, name, body)
            return hub.item_detail(kind, name)

    @router.delete(f"/{plural}/{{name}}", status_code=204, name=f"delete_{kind}")
    def delete_(name: str, hub: H, _: Admin) -> Response:
        with hub.mutating():
            hub.work.delete(kind, name)
        return Response(status_code=204)


for _kind, _plural in (("skill", "skills"), ("agent", "agents"), ("instruction", "instructions"), ("mcp", "mcp"),
                       ("rule", "rules"), ("memory", "memory")):
    _register(_kind, _plural)


@router.post("/skills/{name}/duplicate", status_code=201)
def duplicate(name: str, body: DuplicateIn, hub: H, _: Admin) -> dict[str, Any]:
    with hub.mutating():
        hub.work.duplicate_skill(name, body.new_name)
        return hub.item_detail("skill", body.new_name)


# -- memory inbox: registered FIRST in main.py so /memory/inbox is not captured by /memory/{name} ---------------------
@inbox_router.get("/memory/inbox")
def list_inbox(hub: H, client: str | None = None) -> dict[str, Any]:
    return {"items": hub.inbox.list(client), "next_cursor": None}


@inbox_router.post("/memory/inbox", status_code=201)
def add_inbox(body: InboxIn, request: Request, hub: H) -> dict[str, Any]:
    p = principal_of(request)
    if p.role == "client":
        if "inbox:write" not in p.scopes or p.client != body.client:
            raise Forbidden("this token may only write to the inbox of its own client")
    elif p.role != "admin":
        raise Forbidden("inbox:write scope or admin role required")
    with hub.mutating():
        return hub.inbox.add(body.client, body.title, body.body, body.type)


@inbox_router.post("/memory/inbox/{iid}/promote")
def promote(iid: str, hub: H, _: Admin, body: dict[str, Any] | None = Body(None)) -> dict[str, Any]:
    with hub.mutating():
        return hub.inbox.promote(iid, body)


@inbox_router.post("/memory/inbox/{iid}/reject")
def reject(iid: str, hub: H, _: Admin) -> dict[str, Any]:
    with hub.mutating():
        return hub.inbox.reject(iid)
