//! api/routers/skilleval.py: SkillEvaluator status, the Evaluations view, the queue, per-skill routes.
//! Owner: CORE (LOCKED).

use std::sync::Arc;

use super::bodies::{self, evaluate_in, run_in};
use super::{Resp, reply, run};
use crate::api::deps::{Errors, body_model, body_opt_model, q, q_int, require_admin};
use crate::api::errors::{problem, validation};
use crate::api::{AppState, Req};

macro_rules! admin {
    ($r:expr) => {
        if let Err(p) = require_admin(&$r.principal) {
            return problem(&p);
        }
    };
}

pub async fn evaluator_status(s: Arc<AppState>, _r: Req) -> Resp {
    reply(200, run(&s, |h| h.skilleval.status()).await)
}

pub async fn list_evaluations(s: Arc<AppState>, r: Req) -> Resp {
    let mut e = Errors::default();
    let source = q(&r, "source").unwrap_or_else(|| "hub".into());
    let (state, verdict, origin, gate, qq) = (
        q(&r, "state"),
        q(&r, "verdict"),
        q(&r, "origin"),
        q(&r, "gate"),
        q(&r, "q"),
    );
    let limit = q_int(&r, "limit", 100, Some(1), Some(500), &mut e);
    let cursor = q(&r, "cursor");
    if !e.is_empty() {
        return validation(e.into_value());
    }
    reply(
        200,
        run(&s, move |h| {
            h.eval_list(
                &source,
                state.as_deref(),
                verdict.as_deref(),
                origin.as_deref(),
                gate.as_deref(),
                qq.as_deref(),
                limit,
                cursor.as_deref(),
            )
        })
        .await,
    )
}

pub async fn run_evaluations(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, run_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let (subjects, select, source, tiers, force, actor) = (
        bodies::ol(&b, "subjects"),
        bodies::os(&b, "select"),
        bodies::s(&b, "source"),
        bodies::s(&b, "tiers"),
        bodies::b(&b, "force"),
        r.principal.name.clone(),
    );
    reply(
        200,
        run(&s, move |h| {
            h.eval_run(subjects.as_deref(), select.as_deref(), &source, &tiers, force, &actor)
        })
        .await,
    )
}

pub async fn evaluation_queue(s: Arc<AppState>, _r: Req) -> Resp {
    reply(200, run(&s, |h| h.skilleval.queue()).await)
}

pub async fn cancel_queue(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    reply(200, run(&s, |h| h.skilleval.cancel()).await)
}

pub async fn evaluation_detail(s: Arc<AppState>, r: Req) -> Resp {
    let subject = r.param("subject");
    reply(200, run(&s, move |h| h.eval_detail(&subject)).await)
}

pub async fn subject_report(s: Arc<AppState>, r: Req) -> Resp {
    let (subject, run_id) = (r.param("subject"), r.param("run_id"));
    reply(200, run(&s, move |h| h.skilleval.report(&subject, &run_id)).await)
}

pub async fn skill_evaluations(s: Arc<AppState>, r: Req) -> Resp {
    let mut e = Errors::default();
    let limit = q_int(&r, "limit", 20, Some(1), Some(100), &mut e);
    if !e.is_empty() {
        return validation(e.into_value());
    }
    let name = r.param("name");
    reply(
        200,
        run(&s, move |h| {
            h.skilleval.detail(&name, usize::try_from(limit).unwrap_or(20))
        })
        .await,
    )
}

pub async fn evaluation_report(s: Arc<AppState>, r: Req) -> Resp {
    let (name, run_id) = (r.param("name"), r.param("run_id"));
    reply(200, run(&s, move |h| h.skilleval.report(&name, &run_id)).await)
}

pub async fn evaluate(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_opt_model(&r, evaluate_in(), &mut e);
    if !e.is_empty() {
        return validation(e.into_value());
    }
    let tiers = body.map_or_else(|| "static".to_string(), |b| bodies::s(&b, "tiers"));
    let (name, actor) = (r.param("name"), r.principal.name.clone());
    reply(202, run(&s, move |h| h.skilleval.start(&name, &tiers, &actor)).await)
}
