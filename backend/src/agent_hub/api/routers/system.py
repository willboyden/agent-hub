"""health, adapters, policy, audit, settings, keys, metrics."""
from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Body, Query, Response
from prometheus_client import CONTENT_TYPE_LATEST, generate_latest
from pydantic import BaseModel, ConfigDict

from agent_hub import __version__
from agent_hub.api.deps import Admin, H
from agent_hub.domain.errors import NotFound

router = APIRouter()
root_router = APIRouter()


class KeyIn(BaseModel):
    model_config = ConfigDict(extra="forbid")
    name: str
    role: str = "viewer"
    client: str | None = None
    scopes: list[str] = []


@router.get("/health")
def health(hub: H) -> dict[str, Any]:
    warnings = []
    if hub.cfg.trust_loopback:
        warnings.append("HUB_TRUST_LOOPBACK is ON: any local process (including agents with a shell) is admin without a key")
    return {"status": "ok", "version": __version__, "content_initialised": hub.repo.is_repo(),
            "adapters": hub.registry.ids(), "trust_loopback": hub.cfg.trust_loopback, "warnings": warnings}


@router.get("/adapters")
def adapters(hub: H) -> dict[str, Any]:
    return {"items": hub.adapters_view(), "next_cursor": None}


@router.get("/adapters/{adapter_id}")
def adapter(adapter_id: str, hub: H) -> dict[str, Any]:
    for a in hub.adapters_view():
        if a["id"] == adapter_id:
            return a
    raise NotFound(f"no such adapter: {adapter_id}")


@router.get("/policy/floor")
def floor(hub: H) -> dict[str, Any]:
    """Read-only by design: the floor lives in the app repo and has no write route."""
    return hub.floor.describe()


@router.post("/policy/check")
def policy_check(hub: H, body: dict[str, Any] = Body(...)) -> dict[str, Any]:
    return hub.policy_check(body)


@router.get("/audit")
def audit(hub: H, _: Admin, limit: int = Query(100, ge=1, le=500), cursor: str | None = None) -> dict[str, Any]:
    return hub.audit_svc.list_page(limit, cursor)


@router.get("/settings")
def get_settings(hub: H, _: Admin) -> dict[str, Any]:
    return hub.settings()


@router.put("/settings")
def put_settings(hub: H, _: Admin, body: dict[str, Any] = Body(...)) -> dict[str, Any]:
    return hub.update_settings(body)


@router.get("/keys")
def list_keys(hub: H, _: Admin) -> dict[str, Any]:
    return {"items": hub.auth.list_keys(), "next_cursor": None}


@router.post("/keys", status_code=201)
def create_key(hub: H, _: Admin, body: KeyIn) -> dict[str, object]:
    return hub.auth.create_key(body.name, body.role, body.client, body.scopes)


@router.delete("/keys/{key_id}", status_code=204)
def delete_key(key_id: str, hub: H, _: Admin) -> Response:
    hub.auth.revoke(key_id)
    return Response(status_code=204)


@router.get("/doctor")
async def doctor(hub: H, _: Admin) -> dict[str, Any]:
    """Read-only host-exposure report; never contains secrets."""
    from starlette.concurrency import run_in_threadpool
    ks = await hub.knowledge.summary()
    return await run_in_threadpool(hub.doctor, ks)


@router.get("/doctor/summary")
async def doctor_summary(hub: H) -> dict[str, int]:
    from starlette.concurrency import run_in_threadpool
    ks = await hub.knowledge.summary()
    counts = (await run_in_threadpool(hub.doctor, ks))["counts"]
    return {"crit": counts["crit"], "warn": counts["warn"], "info": counts["info"]}


@router.get("/content/validate")
def validate(hub: H) -> dict[str, Any]:
    return hub.validate_content()


@root_router.get("/metrics", include_in_schema=False)
def metrics(hub: H) -> Response:
    return Response(generate_latest(hub.telemetry.registry), media_type=CONTENT_TYPE_LATEST)
