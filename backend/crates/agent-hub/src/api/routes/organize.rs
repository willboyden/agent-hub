//! api/routers/organize.py: collections, suggestions, bulk ops, profiles, matrix. Owner: CORE (LOCKED).

use std::sync::Arc;

use hub_core::value::PyValue;
use indexmap::IndexMap;
use serde_json::{Value, json};

use super::bodies::{self, accept_in, bulk_in, bulk_toggle_in, members_in, reorder_in, toggle_in};
use super::{Resp, items, reply, reply_empty, run};
use crate::api::deps::{Errors, body_dict, body_model, q, q_int, require_admin};
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

pub async fn list_collections(s: Arc<AppState>, r: Req) -> Resp {
    let mut e = Errors::default();
    let limit = q_int(&r, "limit", 100, None, None, &mut e);
    let cursor = q(&r, "cursor");
    if !e.is_empty() {
        return validation(e.into_value());
    }
    reply(
        200,
        run(&s, move |h| {
            Ok(page_list(h.list_collections()?, limit, cursor.as_deref()))
        })
        .await,
    )
}

pub async fn suggestions(s: Arc<AppState>, _r: Req) -> Resp {
    reply(200, run(&s, |h| Ok(items(h.suggestions()?))).await)
}

pub async fn accept_suggestions(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, accept_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let ids = bodies::l(&b, "ids");
    let overrides: IndexMap<String, PyValue> = b
        .get("overrides")
        .and_then(PyValue::as_dict)
        .unwrap_or_default()
        .iter()
        .map(|(k, v)| (k.py_str(), v.clone()))
        .collect();
    reply(
        201,
        run(&s, move |h| Ok(items(h.accept_suggestions(&ids, &overrides)?))).await,
    )
}

pub async fn create_collection(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_dict(&r, &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let v = PyValue::from_json(&Value::Object(b));
    reply(201, run(&s, move |h| h.create_collection(&v)).await)
}

pub async fn reorder(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, reorder_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let ids = bodies::l(&b, "ids");
    reply(200, run(&s, move |h| Ok(items(h.reorder_collections(&ids)?))).await)
}

pub async fn put_collection(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_dict(&r, &mut e);
    let Some(mut b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let cid = r.param("cid");
    b.insert("id".into(), Value::String(cid.clone()));
    let v = PyValue::from_json(&Value::Object(b));
    reply(200, run(&s, move |h| h.update_collection(&cid, &v)).await)
}

pub async fn delete_collection(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let cid = r.param("cid");
    reply_empty(run(&s, move |h| h.delete_collection(&cid)).await)
}

pub async fn add_members(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, members_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let (cid, members) = (r.param("cid"), bodies::members(&b, "members"));
    reply(200, run(&s, move |h| h.add_members(&cid, &members)).await)
}

pub async fn remove_member(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let (cid, kind, name) = (r.param("cid"), r.param("kind"), r.param("name"));
    reply(200, run(&s, move |h| h.remove_member(&cid, &kind, &name)).await)
}

pub async fn bulk(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, bulk_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let (its, add, rem, at, rt) = (
        bodies::members(&b, "items"),
        bodies::os(&b, "add_to_collection"),
        bodies::os(&b, "remove_from_collection"),
        bodies::l(&b, "add_tags"),
        bodies::l(&b, "remove_tags"),
    );
    reply(
        200,
        run(&s, move |h| {
            let res = h.bulk(&its, add.as_deref(), rem.as_deref(), &at, &rt)?;
            let ok = res
                .iter()
                .all(|x| x.get("ok").and_then(Value::as_bool).unwrap_or(false));
            Ok(json!({"results": res, "ok": ok}))
        })
        .await,
    )
}

pub async fn get_profile(s: Arc<AppState>, r: Req) -> Resp {
    let client = r.param("client");
    reply(200, run(&s, move |h| h.get_profile(&client)).await)
}

pub async fn put_profile(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_dict(&r, &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let client = r.param("client");
    let v = PyValue::from_json(&Value::Object(b));
    reply(200, run(&s, move |h| h.put_profile(&client, &v)).await)
}

pub async fn matrix(s: Arc<AppState>, _r: Req) -> Resp {
    reply(200, run(&s, |h| h.matrix()).await)
}

pub async fn toggle(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, toggle_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let (c, k, n, en) = (
        bodies::s(&b, "client"),
        bodies::s(&b, "kind"),
        bodies::s(&b, "name"),
        bodies::b(&b, "enabled"),
    );
    reply(200, run(&s, move |h| h.toggle(&c, &k, &n, en)).await)
}

pub async fn bulk_toggle(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, bulk_toggle_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let (clients, its, en) = (
        bodies::l(&b, "clients"),
        bodies::members(&b, "items"),
        bodies::b(&b, "enabled"),
    );
    reply(
        200,
        run(&s, move |h| {
            let res = h.bulk_toggle(&clients, &its, en)?;
            let ok = res
                .iter()
                .all(|x| x.get("ok").and_then(Value::as_bool).unwrap_or(false));
            Ok(json!({"results": res, "ok": ok}))
        })
        .await,
    )
}
