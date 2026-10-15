"""Skill evaluation with NVIDIA SkillEvaluator (services/skilleval.py) as a main view: every skill the hub has and every skill a
client really has (observed), queued one at a time, with a gate that import can require. Reads are cheap (stored records and a
content hash, no subprocess) except the evaluator status, whose `--version` probe is cached. Starting evaluations is an admin
POST like every other mutation (CSRF header, audit record). The old per-skill routes stay and map to `hub:<name>`."""
from __future__ import annotations

from typing import Any, Literal

from fastapi import APIRouter, Query
from pydantic import BaseModel, ConfigDict

from agent_hub.api.deps import Admin, H

router = APIRouter()
Tiers = Literal["static", "tier1", "tier1+2"]


class EvaluateIn(BaseModel):
    model_config = ConfigDict(extra="forbid")
    tiers: Tiers = "static"


class RunIn(BaseModel):
    model_config = ConfigDict(extra="forbid")
    subjects: list[str] | None = None
    select: Literal["none_or_stale", "all"] | None = None
    source: str = "hub"                 # all | hub | <client>; used with `select` only
    tiers: Tiers = "static"
    force: bool = False


@router.get("/skill-evaluator")
def evaluator_status(hub: H) -> dict[str, Any]:
    """Is SkillEvaluator installed, which launcher, which version; the tiers; whether an evaluation is running."""
    return hub.skilleval.status()


@router.get("/skill-evaluations")
def list_evaluations(hub: H, source: str = "hub", state: str | None = None, verdict: str | None = None,
                     origin: str | None = None, gate: str | None = None, q: str | None = None,
                     limit: int = Query(100, ge=1, le=500), cursor: str | None = None) -> dict[str, Any]:
    """One row per skill (source=hub, the default: the working tree; a client id: its observed skills; all: both)."""
    return hub.eval_list(source, state, verdict, origin, gate, q, limit, cursor)


@router.post("/skill-evaluations/run")
def run_evaluations(body: RunIn, hub: H, who: Admin) -> dict[str, Any]:
    """Queue evaluations: explicit `subjects` or a `select`ion (exactly one). Returns {queued, skipped}."""
    return hub.eval_run(body.subjects, body.select, body.source, body.tiers, body.force, who.name)


@router.get("/skill-evaluations/queue")
def evaluation_queue(hub: H) -> dict[str, Any]:
    return hub.skilleval.queue()


@router.post("/skill-evaluations/queue/cancel")
def cancel_queue(hub: H, _: Admin) -> dict[str, Any]:
    """Clear the queue and kill the running evaluation."""
    return hub.skilleval.cancel()


@router.get("/skill-evaluations/{subject}")
def evaluation_detail(subject: str, hub: H) -> dict[str, Any]:
    return hub.eval_detail(subject)


@router.get("/skill-evaluations/{subject}/runs/{run_id}/report")
def subject_report(subject: str, run_id: str, hub: H) -> dict[str, Any]:
    return hub.skilleval.report(subject, run_id)


@router.get("/skills/{name}/evaluations")
def skill_evaluations(name: str, hub: H, limit: int = Query(20, ge=1, le=100)) -> dict[str, Any]:
    return hub.skilleval.detail(name, limit)


@router.get("/skills/{name}/evaluations/{run_id}/report")
def evaluation_report(name: str, run_id: str, hub: H) -> dict[str, Any]:
    return hub.skilleval.report(name, run_id)


@router.post("/skills/{name}/evaluate", status_code=202)
def evaluate(name: str, hub: H, who: Admin, body: EvaluateIn | None = None) -> dict[str, Any]:
    """Queues one forced evaluation (202, `status` queued or running). The result appears under GET /skills/{name}/evaluations."""
    return hub.skilleval.start(name, body.tiers if body else "static", actor=who.name)
