"""clients, import, effective/verify/drift/audit."""
from __future__ import annotations

from typing import Any, Literal

from fastapi import APIRouter, Body, Response
from pydantic import BaseModel, ConfigDict

from agent_hub.api.deps import Admin, H
from agent_hub.domain.errors import BadRequest
from agent_hub.services.store import page_list

router = APIRouter()


class ImportItem(BaseModel):
    model_config = ConfigDict(extra="forbid")
    kind: str
    name: str
    on_conflict: str | None = None      # per-item override of the global default


class ImportIn(BaseModel):
    model_config = ConfigDict(extra="forbid")
    client: str
    items: list[ImportItem] | None = None       # None = everything discovered
    on_conflict: str | None = None      # skip | rename | replace | link; unset: link if equal, else skip
    enable: bool = True
    override_reason: str | None = None  # F8: lets a skill the evaluation gate blocked through (10..500 characters, audited)


class ObservedItem(BaseModel):
    model_config = ConfigDict(extra="forbid")
    key: str
    name: str | None = None


class ObservedImportIn(BaseModel):
    model_config = ConfigDict(extra="forbid")
    items: list[ObservedItem]
    on_conflict: Literal["skip", "rename", "replace", "link"] = "skip"
    enable: bool = False
    override_reason: str | None = None


@router.get("/clients")
def list_clients(hub: H, limit: int = 100, cursor: str | None = None) -> dict[str, Any]:
    return page_list(hub.list_clients(), limit, cursor)


@router.post("/clients", status_code=201)
def create_client(hub: H, _: Admin, body: dict[str, Any] = Body(...)) -> dict[str, Any]:
    cid = body.get("id")
    if not isinstance(cid, str):
        raise BadRequest("`id` is required", code="invalid_id")
    return hub.put_client(cid, {k: v for k, v in body.items() if k != "id"}, create=True)


@router.post("/clients/validate-spec")
def validate_spec(hub: H, body: dict[str, Any] = Body(...)) -> dict[str, Any]:
    return hub.validate_spec(body)


@router.get("/clients/{cid}")
def get_client(cid: str, hub: H) -> dict[str, Any]:
    return hub.client_view(cid)


@router.put("/clients/{cid}")
def put_client(cid: str, hub: H, _: Admin, body: dict[str, Any] = Body(...)) -> dict[str, Any]:
    return hub.put_client(cid, {k: v for k, v in body.items() if k != "id"}, create=False)


@router.delete("/clients/{cid}", status_code=204)
def delete_client(cid: str, hub: H, _: Admin) -> Response:
    hub.delete_client(cid)
    return Response(status_code=204)


@router.post("/clients/{cid}/discover")
def discover(cid: str, hub: H, _: Admin) -> dict[str, Any]:
    return hub.importer.discover(cid)


@router.post("/clients/{cid}/observe")
def observe(cid: str, hub: H, who: Admin) -> dict[str, Any]:
    """Read what the client really has (read-only) and store it; see GET /clients/{id}/observed."""
    return hub.observe(cid, who.name)


@router.get("/clients/{cid}/observed")
def observed(cid: str, hub: H, origin: str | None = None, hub_status: str | None = None, q: str | None = None,
             limit: int = 100, cursor: str | None = None) -> dict[str, Any]:
    return hub.observed_view(cid, origin, hub_status, q, limit, cursor)


@router.post("/clients/{cid}/observed/import")
def observed_import(cid: str, body: ObservedImportIn, hub: H, who: Admin) -> dict[str, Any]:
    if not 1 <= len(body.items) <= 500:
        raise BadRequest("send 1 to 500 items")
    return hub.import_observed(cid, [i.model_dump() for i in body.items], body.on_conflict, body.enable, body.override_reason,
                               who.name)


@router.post("/import")
def do_import(body: ImportIn, hub: H, who: Admin) -> dict[str, Any]:
    sel = None if body.items is None else [(i.kind, i.name) for i in body.items]
    ov = {(i.kind, i.name): i.on_conflict for i in (body.items or []) if i.on_conflict}
    return hub.import_items(body.client, sel, body.on_conflict, body.enable, ov, body.override_reason, who.name)


@router.get("/clients/{cid}/rendered")
def rendered(cid: str, kind: str, name: str, hub: H, source: str = "committed") -> dict[str, Any]:
    return hub.rendered(cid, kind, name, source)


class RollbackIn(BaseModel):
    model_config = ConfigDict(extra="forbid")
    apply_id: str | None = None
    confirm: bool = False
    dry_run: bool = False
    force_paths: list[str] = []


@router.get("/clients/{cid}/applies")
def applies(cid: str, hub: H) -> dict[str, Any]:
    return hub.list_applies(cid)


@router.post("/clients/{cid}/rollback")
def rollback(cid: str, body: RollbackIn, hub: H, who: Admin) -> dict[str, Any]:
    return hub.rollback(cid, body.apply_id, body.confirm, body.dry_run, body.force_paths, who.name)


@router.get("/clients/{cid}/effective")
def effective(cid: str, hub: H, source: str = "committed") -> dict[str, Any]:
    return hub.effective(cid, source)


@router.post("/clients/{cid}/verify")
def verify(cid: str, hub: H, _: Admin) -> dict[str, Any]:
    return hub.verify(cid)


@router.get("/clients/{cid}/drift")
def drift(cid: str, hub: H) -> dict[str, Any]:
    return hub.drift(cid)


@router.get("/clients/{cid}/audit")
def audit_client(cid: str, hub: H) -> dict[str, Any]:
    return hub.audit_client(cid)
