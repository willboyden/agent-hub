//! api/deps.py and FastAPI's request parsing: role gates (`require_admin`, `enforce_roles`), query parameter coercion
//! (pydantic lax rules, loc `["query", name]`), JSON body extraction (strict content type: only `application/json` and
//! `application/*+json` are parsed; empty body = missing; decode errors as `json_invalid` at the decoder position) and
//! the SSE connection limiter. Owner: CORE (LOCKED).

use std::sync::atomic::{AtomicI64, Ordering};

use hub_core::Problem;
use hub_core::pyd::{self, ErrItem, Loc, Model, Ty};
use hub_core::value::PyValue;
use serde_json::{Value, json};

use super::Req;
use crate::services::auth::Principal;

pub const SAFE_METHODS: [&str; 3] = ["GET", "HEAD", "OPTIONS"];

/// `require_admin`: 403 "admin role required".
pub fn require_admin(p: &Principal) -> Result<(), Problem> {
    if p.role != "admin" {
        return Err(Problem::forbidden("admin role required"));
    }
    Ok(())
}

/// `enforce_roles` (attached to every API router): a mutating route is admin-only unless the (method, path) is on the
/// list a lesser role may use.
pub fn enforce_roles(req: &Req) -> Result<(), Problem> {
    if SAFE_METHODS.contains(&req.method.as_str()) {
        return Ok(());
    }
    let p = &req.principal;
    if p.role == "admin"
        || (p.role == "viewer" && super::security::viewer_may(&req.method, &req.path))
        || (p.role == "client" && super::security::client_may(&req.method, &req.path))
    {
        return Ok(());
    }
    Err(Problem::forbidden("admin role required for this action"))
}

/// Accumulates request validation errors (query and body) like FastAPI does before answering 422.
#[derive(Default)]
pub struct Errors(pub Vec<Value>);

impl Errors {
    pub fn push(&mut self, loc: &[Value], msg: &str, kind: &str) {
        self.0.push(json!({"loc": loc, "msg": msg, "type": kind}));
    }
    pub fn extend_pyd(&mut self, prefix: &[Loc], e: &pyd::ValidationError) {
        if let Value::Array(items) = e.errors_json(prefix) {
            self.0.extend(items);
        }
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// 422 problem response body errors.
    pub fn into_value(self) -> Value {
        Value::Array(self.0)
    }
}

/// The LAST value of a query parameter (Starlette `query_params.get`).
pub fn q(req: &Req, name: &str) -> Option<String> {
    req.query_pairs
        .iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

fn coerce(ty: &Ty, raw: &str, name: &str, errs: &mut Errors) -> Option<PyValue> {
    let mut items: Vec<ErrItem> = Vec::new();
    let mut loc = vec![Loc::Key("query".into()), Loc::Key(name.into())];
    let v = pyd::validate_ty(ty, &PyValue::str(raw), &mut loc, &mut items);
    if items.is_empty() {
        return Some(v);
    }
    for it in items {
        let l: Vec<Value> = it.loc.iter().map(|x| Value::String(x.to_string())).collect();
        errs.push(&l, &it.msg, &it.kind);
    }
    None
}

/// `name: int = Query(default, ge=, le=)` (or a plain `int = default`).
pub fn q_int(req: &Req, name: &str, default: i64, ge: Option<i64>, le: Option<i64>, errs: &mut Errors) -> i64 {
    match q(req, name) {
        None => default,
        Some(raw) => match coerce(&Ty::Int { ge, le }, &raw, name, errs) {
            Some(PyValue::Int(i)) => i,
            _ => default,
        },
    }
}

/// `name: bool | None = None`.
pub fn q_opt_bool(req: &Req, name: &str, errs: &mut Errors) -> Option<bool> {
    let raw = q(req, name)?;
    match coerce(&Ty::Bool, &raw, name, errs) {
        Some(PyValue::Bool(b)) => Some(b),
        _ => None,
    }
}

/// `name: str` (required query parameter): missing -> `Field required` at `["query", name]`.
pub fn q_required(req: &Req, name: &str, errs: &mut Errors) -> String {
    match q(req, name) {
        Some(v) => v,
        None => {
            errs.push(&[json!("query"), json!(name)], "Field required", "missing");
            String::new()
        }
    }
}

/// What the request body is, as FastAPI sees it.
pub enum Body {
    /// no bytes at all
    Missing,
    /// a JSON content type and valid JSON
    Json(Value),
    /// bytes that are not parsed as JSON (no or a non-JSON content type)
    Raw,
}

fn is_json_ctype(ct: &str) -> bool {
    let main = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let Some((t, sub)) = main.split_once('/') else {
        return false;
    };
    t == "application" && (sub == "json" || sub.ends_with("+json"))
}

/// The deepest `[` / `{` nesting the hub hands to its handlers (PORTING.md D-JSON-DEPTH). CPython decodes up to 9991
/// levels, but a value that deep needs more stack than a worker thread has once it is converted and walked
/// recursively (measured: 512 levels need about 1 MiB in a debug build, 1000 about 2 MiB), so a deeper body gets the
/// answer FastAPI gives a body it cannot decode at all.
pub const BODY_MAX_DEPTH: usize = 256;

/// FastAPI's answer to a body whose decoding raised anything but `JSONDecodeError` (RecursionError,
/// UnicodeDecodeError, the int digit limit): `HTTPException(400, "There was an error parsing the body")`.
pub const BODY_PARSE_ERROR: &str = "There was an error parsing the body";

/// The deepest `[`/`{` nesting of a JSON text, counted outside strings without decoding anything (any bytes).
pub(crate) fn nesting(b: &[u8]) -> usize {
    let (mut depth, mut max, mut in_str, mut esc) = (0usize, 0usize, false, false);
    for &c in b {
        if in_str {
            match (esc, c) {
                (true, _) => esc = false,
                (false, b'\\') => esc = true,
                (false, b'"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'[' | b'{' => {
                depth += 1;
                max = max.max(depth);
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

/// A body FastAPI parses as JSON: non-empty, with an `application/json` or `application/*+json` content type
/// (strict content type: no content type is not JSON).
fn is_json_body(req: &Req) -> bool {
    !req.body.is_empty() && req.header("content-type").is_some_and(|ct| is_json_ctype(&ct))
}

/// FastAPI's body read, done before any dependency runs (the pipeline calls it for every route with a body
/// parameter): `Ok(Some(value))` for a JSON body, `Ok(None)` for no or a non-JSON body; `Err` = the response FastAPI
/// gives: 422 `json_invalid` at `["body", <Python's character position>]` for a `JSONDecodeError`, else 400
/// [`BODY_PARSE_ERROR`]. Decoding is `json.loads(bytes)` exactly ([`hub_core::py::pyjson::loads_bytes`]).
/// `json.loads(raw)` for the audit log (`None` where Python raises ValueError), decoded only when the text is nested at
/// most [`BODY_MAX_DEPTH`] levels (deeper bodies are never built: PORTING.md D-JSON-DEPTH / P-5).
pub fn audit_json(raw: &[u8]) -> Option<Value> {
    if nesting(raw) > BODY_MAX_DEPTH {
        return None;
    }
    hub_core::py::pyjson::loads_bytes(raw).ok().map(|v| v.to_json())
}

pub fn decode_body(req: &Req) -> Result<Option<PyValue>, Box<axum::response::Response>> {
    use hub_core::py::pyjson::{DecodeError, loads_bytes};
    if !is_json_body(req) {
        return Ok(None);
    }
    if nesting(&req.body) > BODY_MAX_DEPTH {
        return Err(Box::new(super::errors::http_error(400, BODY_PARSE_ERROR)));
    }
    match loads_bytes(&req.body) {
        Ok(v) => Ok(Some(v)),
        Err(DecodeError::Json { pos, .. }) => {
            let mut e = Errors::default();
            e.push(&[json!("body"), json!(pos)], "JSON decode error", "json_invalid");
            Err(Box::new(super::errors::validation(e.into_value())))
        }
        Err(_) => Err(Box::new(super::errors::http_error(400, BODY_PARSE_ERROR))),
    }
}

/// Parse the body (FastAPI strict content type). A JSON `null` is a missing body (FastAPI's `body = None`). The
/// pipeline has already answered every body that does not decode ([`decode_body`]); should one reach a handler
/// anyway, it is a `json_invalid` validation error.
pub fn body(req: &Req, errs: &mut Errors) -> Body {
    if req.body.is_empty() {
        return Body::Missing;
    }
    if !is_json_body(req) {
        return Body::Raw;
    }
    match decode_body(req) {
        Ok(Some(PyValue::None)) => Body::Missing,
        Ok(Some(v)) => Body::Json(v.to_json()),
        Ok(None) => Body::Raw,
        Err(_) => {
            errs.push(&[json!("body"), json!(0)], "JSON decode error", "json_invalid");
            Body::Raw
        }
    }
}

/// A required `dict[str, Any] = Body(...)`.
pub fn body_dict(req: &Req, errs: &mut Errors) -> Option<serde_json::Map<String, Value>> {
    let before = errs.0.len();
    match body(req, errs) {
        Body::Missing => {
            errs.push(&[json!("body")], "Field required", "missing");
            None
        }
        Body::Json(Value::Object(m)) => Some(m),
        Body::Json(_) | Body::Raw => {
            if errs.0.len() == before {
                errs.push(&[json!("body")], "Input should be a valid dictionary", "dict_type");
            }
            None
        }
    }
}

/// An optional `dict[str, Any] | None = Body(None)`.
pub fn body_opt_dict(req: &Req, errs: &mut Errors) -> Option<serde_json::Map<String, Value>> {
    let before = errs.0.len();
    match body(req, errs) {
        Body::Missing | Body::Json(Value::Null) => None,
        Body::Json(Value::Object(m)) => Some(m),
        Body::Json(_) | Body::Raw => {
            if errs.0.len() == before {
                errs.push(&[json!("body")], "Input should be a valid dictionary", "dict_type");
            }
            None
        }
    }
}

/// A required pydantic model body (`body: Model`): the validated value.
pub fn body_model(req: &Req, model: &Model, errs: &mut Errors) -> Option<PyValue> {
    let before = errs.0.len();
    match body(req, errs) {
        Body::Missing => {
            errs.push(&[json!("body")], "Field required", "missing");
            None
        }
        Body::Json(v @ Value::Object(_)) => match pyd::validate(model, &PyValue::from_json(&v)) {
            Ok(x) => Some(x),
            Err(e) => {
                errs.extend_pyd(&[Loc::Key("body".into())], &e);
                None
            }
        },
        Body::Json(_) | Body::Raw => {
            if errs.0.len() == before {
                errs.push(
                    &[json!("body")],
                    "Input should be a valid dictionary or object to extract fields from",
                    "model_attributes_type",
                );
            }
            None
        }
    }
}

/// An optional model body (`body: Model | None = None`): `Ok(None)` when absent.
pub fn body_opt_model(req: &Req, model: &Model, errs: &mut Errors) -> Option<PyValue> {
    let before = errs.0.len();
    match body(req, errs) {
        Body::Missing | Body::Json(Value::Null) => None,
        Body::Json(v @ Value::Object(_)) => match pyd::validate(model, &PyValue::from_json(&v)) {
            Ok(x) => Some(x),
            Err(e) => {
                errs.extend_pyd(&[Loc::Key("body".into())], &e);
                None
            }
        },
        Body::Json(_) | Body::Raw => {
            if errs.0.len() == before {
                errs.push(
                    &[json!("body")],
                    "Input should be a valid dictionary or object to extract fields from",
                    "model_attributes_type",
                );
            }
            None
        }
    }
}

/// SseLimiter: connection cap (429 `too_many_streams` when full).
pub struct SseLimiter {
    pub limit: i64,
    active: AtomicI64,
}

impl SseLimiter {
    pub fn new(limit: i64) -> Self {
        Self {
            limit,
            active: AtomicI64::new(0),
        }
    }
    pub fn acquire(&self) -> bool {
        let mut cur = self.active.load(Ordering::SeqCst);
        loop {
            if cur >= self.limit {
                return false;
            }
            match self
                .active
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return true,
                Err(x) => cur = x,
            }
        }
    }
    pub fn release(&self) {
        let _ = self
            .active
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some((v - 1).max(0)));
    }
}
