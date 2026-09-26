"""pending changes, commit, discard, history, revert; plan and apply."""
from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Query
from pydantic import BaseModel, ConfigDict

from agent_hub.api.deps import Admin, H

router = APIRouter()


class Strict(BaseModel):
    model_config = ConfigDict(extra="forbid")


class CommitIn(Strict):
    message: str


class DiscardIn(Strict):
    paths: list[str] | None = None
    confirm: bool = False
    dry_run: bool = False


class RevertIn(Strict):
    commit: str


class PlanIn(Strict):
    clients: list[str] | None = None
    source: str = "committed"


class ApplyIn(Strict):
    plan_id: str
    confirm: bool = False
    adopt_paths: list[str] = []


@router.get("/changes")
def changes(hub: H) -> dict[str, Any]:
    return hub.changes()


@router.post("/changes/commit")
def commit(body: CommitIn, hub: H, who: Admin) -> dict[str, Any]:
    return hub.commit(body.message, who.name)


@router.post("/changes/discard")
def discard(body: DiscardIn, hub: H, who: Admin) -> dict[str, Any]:
    return hub.discard(body.paths, who.name, body.confirm, body.dry_run)


@router.get("/changes/history")
def history(hub: H, limit: int = Query(50, ge=1, le=500)) -> dict[str, Any]:
    return {"items": hub.history(limit), "next_cursor": None}


@router.post("/changes/revert")
def revert(body: RevertIn, hub: H, who: Admin) -> dict[str, Any]:
    return hub.revert(body.commit, who.name)


@router.post("/plan")
def make_plan(body: PlanIn, hub: H, _: Admin) -> dict[str, Any]:
    return hub.plan(body.clients, body.source).model_dump(mode="json")


@router.get("/plan/{plan_id}")
def get_plan(plan_id: str, hub: H) -> dict[str, Any]:
    return hub.get_plan(plan_id).model_dump(mode="json")


@router.post("/apply")
def apply(body: ApplyIn, hub: H, who: Admin) -> dict[str, Any]:
    return hub.apply(body.plan_id, body.confirm, body.adopt_paths, who.name)
