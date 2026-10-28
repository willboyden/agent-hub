//! api/routers/changes.py: pending changes, commit, discard, history, revert; plan and apply. Owner: CORE (LOCKED).

use std::sync::Arc;

use super::bodies::{self, apply_in, commit_in, discard_in, plan_in, revert_in};
use super::{Resp, items, reply, run};
use crate::api::deps::{Errors, body_model, q_int, require_admin};
use crate::api::errors::{problem, validation};
use crate::api::{AppState, Req};

macro_rules! admin {
    ($r:expr) => {
        if let Err(p) = require_admin(&$r.principal) {
            return problem(&p);
        }
    };
}

pub async fn changes(s: Arc<AppState>, _r: Req) -> Resp {
    reply(200, run(&s, |h| h.changes()).await)
}

pub async fn commit(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, commit_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let (msg, actor) = (bodies::s(&b, "message"), r.principal.name.clone());
    reply(200, run(&s, move |h| h.commit(&msg, &actor)).await)
}

pub async fn discard(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, discard_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let (paths, confirm, dry, actor) = (
        bodies::ol(&b, "paths"),
        bodies::b(&b, "confirm"),
        bodies::b(&b, "dry_run"),
        r.principal.name.clone(),
    );
    reply(
        200,
        run(&s, move |h| h.discard(paths.as_deref(), &actor, confirm, dry)).await,
    )
}

pub async fn history(s: Arc<AppState>, r: Req) -> Resp {
    let mut e = Errors::default();
    let limit = q_int(&r, "limit", 50, Some(1), Some(500), &mut e);
    if !e.is_empty() {
        return validation(e.into_value());
    }
    reply(200, run(&s, move |h| Ok(items(h.history(limit)?))).await)
}

pub async fn revert(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, revert_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let (commit, actor) = (bodies::s(&b, "commit"), r.principal.name.clone());
    reply(200, run(&s, move |h| h.revert(&commit, &actor)).await)
}

pub async fn make_plan(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, plan_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let (clients, source) = (bodies::ol(&b, "clients"), bodies::s(&b, "source"));
    reply(
        200,
        run(&s, move |h| {
            let p = h.plan(clients.as_deref(), &source)?;
            serde_json::to_value(p).map_err(|_| hub_core::Problem::internal())
        })
        .await,
    )
}

pub async fn get_plan(s: Arc<AppState>, r: Req) -> Resp {
    let id = r.param("plan_id");
    reply(
        200,
        run(&s, move |h| {
            let p = h.get_plan(&id)?;
            serde_json::to_value(p).map_err(|_| hub_core::Problem::internal())
        })
        .await,
    )
}

pub async fn apply(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, apply_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let (id, confirm, adopt, actor) = (
        bodies::s(&b, "plan_id"),
        bodies::b(&b, "confirm"),
        bodies::l(&b, "adopt_paths"),
        r.principal.name.clone(),
    );
    reply(200, run(&s, move |h| h.apply(&id, confirm, &adopt, &actor)).await)
}
