"""Skill evaluation with NVIDIA SkillEvaluator (advisory; see services/skilleval.py). Reads are cheap (stored records and a
content hash, no subprocess) except the evaluator status, whose `--version` probe is cached. Starting an evaluation is an admin
POST like every other mutation (CSRF header, audit record, 409 while another evaluation runs)."""
from __future__ import annotations

from typing import Any, Literal

from fastapi import APIRouter, Query
from pydantic import BaseModel, ConfigDict

from agent_hub.api.deps import Admin, H

router = APIRouter()


class EvaluateIn(BaseModel):
    model_config = ConfigDict(extra="forbid")
    tiers: Literal["static", "tier1", "tier1+2"] = "static"


@router.get("/skill-evaluator")
def evaluator_status(hub: H) -> dict[str, Any]:
    """Is SkillEvaluator installed, which launcher, which version; the tiers; whether an evaluation is running."""
    return hub.skilleval.status()


@router.get("/skill-evaluations")
def list_evaluations(hub: H) -> dict[str, Any]:
    """Latest evaluation per skill of the working tree with its state (current | stale | none), for the library badges."""
    return {"items": hub.skilleval.list_latest(), "running": hub.skilleval.running(), "next_cursor": None}


@router.get("/skills/{name}/evaluations")
def skill_evaluations(name: str, hub: H, limit: int = Query(20, ge=1, le=100)) -> dict[str, Any]:
    return hub.skilleval.detail(name, limit)


@router.get("/skills/{name}/evaluations/{run_id}/report")
def evaluation_report(name: str, run_id: str, hub: H) -> dict[str, Any]:
    return hub.skilleval.report(name, run_id)


@router.post("/skills/{name}/evaluate", status_code=202)
def evaluate(name: str, hub: H, who: Admin, body: EvaluateIn | None = None) -> dict[str, Any]:
    """Starts one evaluation (202). The copy and the checks happen before this returns; SkillEvaluator then runs in the
    background and the result appears under GET /skills/{name}/evaluations."""
    return hub.skilleval.start(name, body.tiers if body else "static", actor=who.name)
