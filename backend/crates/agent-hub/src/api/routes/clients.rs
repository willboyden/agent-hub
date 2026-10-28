//! api/routers/clients.py: clients, import, observed inventory, rendered/effective/verify/drift/audit, applies,
//! rollback. Owner: CORE (LOCKED).

use std::sync::Arc;

use hub_core::Problem;
use hub_core::value::PyValue;
use indexmap::IndexMap;
use serde_json::Value;

use super::bodies::{self, import_in, observed_import_in, rollback_in};
use super::{Resp, reply, reply_empty, run};
use crate::api::deps::{Errors, body_dict, body_model, q, q_int, q_required, require_admin};
use crate::api::errors::{problem, validation};
use crate::api::{AppState, Req};
use crate::services::store::page_list;

macro_rules! admin {
    ($r:expr) => {
        if let Err(p) = require_admin(&$r.principal) {
            return problem(&p);
        }
    };
}

pub async fn list_clients(s: Arc<AppState>, r: Req) -> Resp {
    let mut e = Errors::default();
    let limit = q_int(&r, "limit", 100, None, None, &mut e);
    let cursor = q(&r, "cursor");
    if !e.is_empty() {
        return validation(e.into_value());
    }
    reply(
        200,
        run(&s, move |h| Ok(page_list(h.list_clients()?, limit, cursor.as_deref()))).await,
    )
}

fn payload(m: &serde_json::Map<String, Value>) -> IndexMap<String, PyValue> {
    m.iter()
        .filter(|(k, _)| k.as_str() != "id")
        .map(|(k, v)| (k.clone(), PyValue::from_json(v)))
        .collect()
}

pub async fn create_client(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_dict(&r, &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let Some(cid) = b.get("id").and_then(Value::as_str).map(str::to_string) else {
        return problem(&Problem::bad_request("`id` is required").code("invalid_id"));
    };
    let p = payload(&b);
    reply(201, run(&s, move |h| h.put_client(&cid, &p, true)).await)
}

pub async fn validate_spec(s: Arc<AppState>, r: Req) -> Resp {
    let mut e = Errors::default();
    let body = body_dict(&r, &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let p: IndexMap<String, PyValue> = b.iter().map(|(k, v)| (k.clone(), PyValue::from_json(v))).collect();
    reply(200, run(&s, move |h| h.validate_spec(&p)).await)
}

pub async fn get_client(s: Arc<AppState>, r: Req) -> Resp {
    let cid = r.param("cid");
    reply(200, run(&s, move |h| h.client_view(&cid)).await)
}

pub async fn put_client(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_dict(&r, &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let cid = r.param("cid");
    let p = payload(&b);
    reply(200, run(&s, move |h| h.put_client(&cid, &p, false)).await)
}

pub async fn delete_client(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let cid = r.param("cid");
    reply_empty(run(&s, move |h| h.delete_client(&cid)).await)
}

pub async fn discover(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let cid = r.param("cid");
    reply(200, run(&s, move |h| h.discover(&cid)).await)
}

pub async fn observe(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let cid = r.param("cid");
    let actor = r.principal.name.clone();
    reply(200, run(&s, move |h| h.observe(&cid, &actor)).await)
}

pub async fn observed(s: Arc<AppState>, r: Req) -> Resp {
    let mut e = Errors::default();
    let (origin, hub_status, qq) = (q(&r, "origin"), q(&r, "hub_status"), q(&r, "q"));
    let limit = q_int(&r, "limit", 100, None, None, &mut e);
    let cursor = q(&r, "cursor");
    if !e.is_empty() {
        return validation(e.into_value());
    }
    let cid = r.param("cid");
    reply(
        200,
        run(&s, move |h| {
            h.observed_view(
                &cid,
                origin.as_deref(),
                hub_status.as_deref(),
                qq.as_deref(),
                limit,
                cursor.as_deref(),
            )
        })
        .await,
    )
}

pub async fn observed_import(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, observed_import_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let items: Vec<Value> = b
        .get("items")
        .and_then(PyValue::as_list)
        .unwrap_or_default()
        .iter()
        .map(PyValue::to_json)
        .collect();
    if !(1..=500).contains(&items.len()) {
        return problem(&Problem::bad_request("send 1 to 500 items"));
    }
    let cid = r.param("cid");
    let (oc, en, why, actor) = (
        bodies::s(&b, "on_conflict"),
        bodies::b(&b, "enable"),
        bodies::os(&b, "override_reason"),
        r.principal.name.clone(),
    );
    reply(
        200,
        run(&s, move |h| {
            h.import_observed(&cid, &items, &oc, en, why.as_deref(), &actor)
        })
        .await,
    )
}

pub async fn do_import(s: Arc<AppState>, r: Req) -> Resp {
    // Admin is a dependency of the endpoint, resolved after the body is read and before it is validated.
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, import_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let items = b.get("items").and_then(PyValue::as_list).map(<[PyValue]>::to_vec);
    let sel: Option<Vec<(String, String)>> = items
        .as_ref()
        .map(|l| l.iter().map(|i| (bodies::s(i, "kind"), bodies::s(i, "name"))).collect());
    let mut ov: IndexMap<(String, String), String> = IndexMap::new();
    for i in items.unwrap_or_default() {
        if let Some(c) = bodies::os(&i, "on_conflict").filter(|c| !c.is_empty()) {
            ov.insert((bodies::s(&i, "kind"), bodies::s(&i, "name")), c);
        }
    }
    let (client, oc, en, why, actor) = (
        bodies::s(&b, "client"),
        bodies::os(&b, "on_conflict"),
        bodies::b(&b, "enable"),
        bodies::os(&b, "override_reason"),
        r.principal.name.clone(),
    );
    reply(
        200,
        run(&s, move |h| {
            h.import_items(&client, sel.as_deref(), oc.as_deref(), en, &ov, why.as_deref(), &actor)
        })
        .await,
    )
}

pub async fn rendered(s: Arc<AppState>, r: Req) -> Resp {
    let mut e = Errors::default();
    let kind = q_required(&r, "kind", &mut e);
    let name = q_required(&r, "name", &mut e);
    let source = q(&r, "source").unwrap_or_else(|| "committed".into());
    if !e.is_empty() {
        return validation(e.into_value());
    }
    let cid = r.param("cid");
    reply(200, run(&s, move |h| h.rendered(&cid, &kind, &name, &source)).await)
}

pub async fn applies(s: Arc<AppState>, r: Req) -> Resp {
    let cid = r.param("cid");
    reply(200, run(&s, move |h| h.list_applies(&cid)).await)
}

pub async fn rollback(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, rollback_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let cid = r.param("cid");
    let (aid, confirm, dry, force, actor) = (
        bodies::os(&b, "apply_id"),
        bodies::b(&b, "confirm"),
        bodies::b(&b, "dry_run"),
        bodies::l(&b, "force_paths"),
        r.principal.name.clone(),
    );
    reply(
        200,
        run(&s, move |h| {
            h.rollback(&cid, aid.as_deref(), confirm, dry, &force, &actor)
        })
        .await,
    )
}

pub async fn effective(s: Arc<AppState>, r: Req) -> Resp {
    let cid = r.param("cid");
    let source = q(&r, "source").unwrap_or_else(|| "committed".into());
    reply(200, run(&s, move |h| h.effective(&cid, &source)).await)
}

pub async fn verify(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let cid = r.param("cid");
    reply(200, run(&s, move |h| h.verify(&cid)).await)
}

pub async fn drift(s: Arc<AppState>, r: Req) -> Resp {
    let cid = r.param("cid");
    reply(200, run(&s, move |h| h.drift(&cid)).await)
}

pub async fn audit_client(s: Arc<AppState>, r: Req) -> Resp {
    let cid = r.param("cid");
    reply(200, run(&s, move |h| h.audit_client(&cid)).await)
}
