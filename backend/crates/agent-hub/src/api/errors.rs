//! api/errors.py: RFC 7807 problem+json for every error path (stable `code`), never echoing request bodies, plus the
//! JSON success responses (Starlette `JSONResponse`: `json.dumps(ensure_ascii=False, separators=(",", ":"))`).
//! Owner: CORE (LOCKED).

use axum::body::Body;
use http::{HeaderValue, Response, StatusCode, header};
use hub_core::Problem;
use hub_core::py::json as pyjson;
use serde_json::Value;

pub const PROBLEM: &str = "application/problem+json";

/// Starlette JSONResponse rendering.
pub fn render_json(v: &Value) -> String {
    pyjson::dumps(
        v,
        &pyjson::Opts {
            indent: None,
            sort_keys: false,
            ensure_ascii: false,
            separators: Some((",", ":")),
        },
    )
}

fn response(status: u16, ctype: &str, body: Vec<u8>) -> Response<Body> {
    let len = body.len();
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
        r.headers_mut().insert(header::CONTENT_LENGTH, v);
    }
    if let Ok(v) = HeaderValue::from_str(ctype) {
        r.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    r
}

/// `problem(status, code, title, detail, **extra)` from a [`Problem`].
pub fn problem(p: &Problem) -> Response<Body> {
    response(p.status, PROBLEM, render_json(&p.body()).into_bytes())
}

/// A problem with an explicit title (the security middleware uses `code.replace("_", " ")`; HTTP errors likewise).
pub fn problem_parts(status: u16, code: &str, title: &str, detail: &str) -> Response<Body> {
    let mut p = Problem::new(status, code, detail);
    p.title = Some(title.into());
    problem(&p)
}

/// `JSONResponse(value, status_code)`.
pub fn json(status: u16, v: &Value) -> Response<Body> {
    response(status, "application/json", render_json(v).into_bytes())
}

/// `Response(status_code=204)` (no body, no content-type).
pub fn no_content() -> Response<Body> {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::NO_CONTENT;
    r
}

/// Starlette `HTTPException` rendered by the hub's handler: 404 not_found, 405 method_not_allowed, 413, 401, 403.
pub fn http_error(status: u16, detail: &str) -> Response<Body> {
    let code = match status {
        404 => "not_found",
        413 => "payload_too_large",
        405 => "method_not_allowed",
        401 => "unauthorized",
        403 => "forbidden",
        _ => "http_error",
    };
    problem_parts(status, code, &code.replace('_', " "), detail)
}

/// FastAPI `RequestValidationError` -> 422 `validation_error` with `errors=[{loc, msg, type}]` (no input/ctx).
pub fn validation(errors: Value) -> Response<Body> {
    problem(&Problem::new(422, "validation_error", "request validation failed").with("errors", errors))
}

/// Plain bytes with a content type (knowledge proxy passthrough, metrics).
pub fn raw(status: u16, ctype: &str, body: Vec<u8>) -> Response<Body> {
    // Starlette's init_headers: a text/* media type without a charset gets "; charset=utf-8"
    if ctype.starts_with("text/") && !ctype.to_lowercase().contains("charset=") {
        return response(status, &format!("{ctype}; charset=utf-8"), body);
    }
    response(status, ctype, body)
}
