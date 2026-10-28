//! api/routers/content.py: CRUD for skills, agents, instructions, mcp, rules, memory (working tree only) and the memory
//! inbox. Owner: CORE (LOCKED).

use std::sync::Arc;

use hub_core::Problem;
use hub_core::value::PyValue;

use super::bodies::{self, duplicate_in, inbox_in};
use super::{Resp, items, reply, reply_empty, run};
use crate::api::deps::{Errors, body_dict, body_model, body_opt_dict, q, q_int, q_opt_bool, require_admin};
use crate::api::errors::{problem, validation};
use crate::api::{AppState, Req};
use crate::services::hub::content_ops::ListFilter;

macro_rules! admin {
    ($r:expr) => {
        if let Err(p) = require_admin(&$r.principal) {
            return problem(&p);
        }
    };
}

async fn list_kind(kind: &'static str, s: Arc<AppState>, r: Req) -> Resp {
    let mut e = Errors::default();
    let f = ListFilter {
        q: q(&r, "q"),
        group: q(&r, "group"),
        tag: q(&r, "tag"),
        client: q(&r, "client"),
        enabled: q_opt_bool(&r, "enabled", &mut e),
        source: q(&r, "source"),
        issues: q_opt_bool(&r, "issues", &mut e),
        sort: q(&r, "sort").unwrap_or_else(|| "name".into()),
        limit: q_int(&r, "limit", 100, Some(1), Some(500), &mut e),
        cursor: q(&r, "cursor"),
    };
    if !e.is_empty() {
        return validation(e.into_value());
    }
    reply(200, run(&s, move |h| h.list_items(kind, &f)).await)
}

async fn get_kind(kind: &'static str, s: Arc<AppState>, r: Req) -> Resp {
    let name = r.param("name");
    reply(200, run(&s, move |h| h.item_detail(kind, &name)).await)
}

async fn put_kind(kind: &'static str, s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_dict(&r, &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let name = r.param("name");
    let payload = PyValue::from_json(&serde_json::Value::Object(b));
    reply(200, run(&s, move |h| h.put_item(kind, &name, &payload)).await)
}

async fn delete_kind(kind: &'static str, s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let name = r.param("name");
    reply_empty(run(&s, move |h| h.delete_item(kind, &name)).await)
}

macro_rules! kind_routes {
    ($kind:literal, $list:ident, $get:ident, $put:ident, $del:ident) => {
        pub async fn $list(s: Arc<AppState>, r: Req) -> Resp {
            list_kind($kind, s, r).await
        }
        pub async fn $get(s: Arc<AppState>, r: Req) -> Resp {
            get_kind($kind, s, r).await
        }
        pub async fn $put(s: Arc<AppState>, r: Req) -> Resp {
            put_kind($kind, s, r).await
        }
        pub async fn $del(s: Arc<AppState>, r: Req) -> Resp {
            delete_kind($kind, s, r).await
        }
    };
}

kind_routes!("skill", list_skills, get_skill, put_skill, delete_skill);
kind_routes!("agent", list_agents, get_agent, put_agent, delete_agent);
kind_routes!(
    "instruction",
    list_instructions,
    get_instruction,
    put_instruction,
    delete_instruction
);
kind_routes!("mcp", list_mcp, get_mcp, put_mcp, delete_mcp);
kind_routes!("rule", list_rules, get_rule, put_rule, delete_rule);
kind_routes!("memory", list_memory, get_memory, put_memory, delete_memory);

pub async fn duplicate(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_model(&r, duplicate_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let (name, new_name) = (r.param("name"), bodies::s(&b, "new_name"));
    reply(201, run(&s, move |h| h.duplicate_skill(&name, &new_name)).await)
}

pub async fn list_inbox(s: Arc<AppState>, r: Req) -> Resp {
    let client = q(&r, "client");
    reply(200, run(&s, move |h| Ok(items(h.inbox_list(client.as_deref())?))).await)
}

pub async fn add_inbox(s: Arc<AppState>, r: Req) -> Resp {
    let mut e = Errors::default();
    let body = body_model(&r, inbox_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let p = &r.principal;
    let client = bodies::s(&b, "client");
    if p.role == "client" {
        if !p.scopes.iter().any(|x| x == "inbox:write") || p.client.as_deref() != Some(client.as_str()) {
            return problem(&Problem::forbidden(
                "this token may only write to the inbox of its own client",
            ));
        }
    } else if p.role != "admin" {
        return problem(&Problem::forbidden("inbox:write scope or admin role required"));
    }
    let (title, text, mtype) = (bodies::s(&b, "title"), bodies::s(&b, "body"), bodies::s(&b, "type"));
    reply(201, run(&s, move |h| h.inbox_add(&client, &title, &text, &mtype)).await)
}

pub async fn promote(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_opt_dict(&r, &mut e);
    if !e.is_empty() {
        return validation(e.into_value());
    }
    let iid = r.param("iid");
    let edits = body.map(|m| PyValue::from_json(&serde_json::Value::Object(m)));
    reply(200, run(&s, move |h| h.inbox_promote(&iid, edits.as_ref())).await)
}

pub async fn reject(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let iid = r.param("iid");
    reply(200, run(&s, move |h| h.inbox_reject(&iid)).await)
}
