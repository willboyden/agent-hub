//! api/routers/system.py: health, adapters, policy, audit, settings, keys, doctor, validate, metrics (+ the OpenAPI
//! document FastAPI serves). Owner: CORE (LOCKED).

use std::sync::Arc;

use hub_core::Problem;
use hub_core::value::PyValue;
use serde_json::{Value, json};

use super::bodies::{self, key_in};
use super::{Resp, items, reply, reply_empty, run};
use crate::api::deps::{Errors, body_dict, body_model, q, q_int, require_admin};
use crate::api::errors::{problem, raw, validation};
use crate::api::{AppState, Req};
use crate::telemetry::CONTENT_TYPE;

pub const VERSION: &str = "0.3.0";

/// The OpenAPI document FastAPI generates, captured from the Python app (assets/openapi.json, regenerate with
/// backend/parity/gen/dump_openapi.py). Behind auth like every /api path.
pub async fn openapi(_s: Arc<AppState>, _r: Req) -> Resp {
    let v: Value = serde_json::from_str(include_str!("../../../assets/openapi.json")).unwrap_or(Value::Null);
    crate::api::errors::json(200, &v)
}

pub async fn health(s: Arc<AppState>, _r: Req) -> Resp {
    reply(
        200,
        run(&s, |h| {
            let cfg = h.cfg();
            let mut warnings = Vec::new();
            if cfg.trust_loopback {
                warnings.push("HUB_TRUST_LOOPBACK is ON: any local process (including agents with a shell) is admin without a key");
            }
            Ok(json!({
                "status": "ok", "version": VERSION, "content_initialised": h.repo.is_repo()?,
                "adapters": h.registry.ids(), "trust_loopback": cfg.trust_loopback, "warnings": warnings,
            }))
        })
        .await,
    )
}

pub async fn adapters(s: Arc<AppState>, _r: Req) -> Resp {
    reply(200, run(&s, |h| Ok(items(h.adapters_view()))).await)
}

pub async fn adapter(s: Arc<AppState>, r: Req) -> Resp {
    let id = r.param("adapter_id");
    reply(
        200,
        run(&s, move |h| {
            h.adapters_view()
                .into_iter()
                .find(|a| a.get("id").and_then(Value::as_str) == Some(id.as_str()))
                .ok_or_else(|| Problem::not_found(format!("no such adapter: {id}")))
        })
        .await,
    )
}

pub async fn floor(s: Arc<AppState>, _r: Req) -> Resp {
    reply(200, run(&s, |h| Ok(h.floor.describe())).await)
}

pub async fn policy_check(s: Arc<AppState>, r: Req) -> Resp {
    let mut e = Errors::default();
    let body = body_dict(&r, &mut e);
    if !e.is_empty() {
        return validation(e.into_value());
    }
    let body: indexmap::IndexMap<String, PyValue> = body
        .unwrap_or_default()
        .iter()
        .map(|(k, v)| (k.clone(), PyValue::from_json(v)))
        .collect();
    reply(200, run(&s, move |h| h.policy_check(&body)).await)
}

pub async fn audit(s: Arc<AppState>, r: Req) -> Resp {
    if let Err(p) = require_admin(&r.principal) {
        return problem(&p);
    }
    let mut e = Errors::default();
    let limit = q_int(&r, "limit", 100, Some(1), Some(500), &mut e);
    let cursor = q(&r, "cursor");
    if !e.is_empty() {
        return validation(e.into_value());
    }
    reply(
        200,
        run(&s, move |h| h.audit_svc.list_page(limit, cursor.as_deref())).await,
    )
}

pub async fn get_settings(s: Arc<AppState>, r: Req) -> Resp {
    if let Err(p) = require_admin(&r.principal) {
        return problem(&p);
    }
    reply(200, run(&s, |h| Ok(h.settings())).await)
}

pub async fn put_settings(s: Arc<AppState>, r: Req) -> Resp {
    if let Err(p) = require_admin(&r.principal) {
        return problem(&p);
    }
    let mut e = Errors::default();
    let body = body_dict(&r, &mut e);
    if !e.is_empty() {
        return validation(e.into_value());
    }
    let body = body.unwrap_or_default();
    reply(200, run(&s, move |h| h.update_settings(&body)).await)
}

pub async fn list_keys(s: Arc<AppState>, r: Req) -> Resp {
    if let Err(p) = require_admin(&r.principal) {
        return problem(&p);
    }
    reply(200, run(&s, |h| Ok(items(h.auth.list_keys()?))).await)
}

pub async fn create_key(s: Arc<AppState>, r: Req) -> Resp {
    if let Err(p) = require_admin(&r.principal) {
        return problem(&p);
    }
    let mut e = Errors::default();
    let body = body_model(&r, key_in(), &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let (name, role, client, scopes) = (
        bodies::s(&b, "name"),
        bodies::s(&b, "role"),
        bodies::os(&b, "client"),
        bodies::l(&b, "scopes"),
    );
    reply(
        201,
        run(&s, move |h| h.auth.create_key(&name, &role, client.as_deref(), &scopes)).await,
    )
}

pub async fn delete_key(s: Arc<AppState>, r: Req) -> Resp {
    if let Err(p) = require_admin(&r.principal) {
        return problem(&p);
    }
    let id = r.param("key_id");
    reply_empty(run(&s, move |h| h.auth.revoke(&id)).await)
}

pub async fn doctor(s: Arc<AppState>, r: Req) -> Resp {
    if let Err(p) = require_admin(&r.principal) {
        return problem(&p);
    }
    let ks = s.hub.knowledge.summary().await;
    reply(200, run(&s, move |h| h.doctor(Some(&ks))).await)
}

pub async fn doctor_summary(s: Arc<AppState>, _r: Req) -> Resp {
    let ks = s.hub.knowledge.summary().await;
    reply(
        200,
        run(&s, move |h| {
            let rep = h.doctor(Some(&ks))?;
            let c = &rep["counts"];
            Ok(json!({"crit": c["crit"], "warn": c["warn"], "info": c["info"]}))
        })
        .await,
    )
}

pub async fn validate(s: Arc<AppState>, _r: Req) -> Resp {
    reply(200, run(&s, |h| h.validate_content()).await)
}

pub async fn metrics(s: Arc<AppState>, _r: Req) -> Resp {
    raw(200, CONTENT_TYPE, s.hub.telemetry.render().into_bytes())
}
