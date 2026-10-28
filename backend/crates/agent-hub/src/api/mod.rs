//! The HTTP application (main.py `create_app` + Starlette/FastAPI routing semantics).
//!
//! Routing is ordered first-match exactly like Starlette: the route table (routes/mod.rs, generated order identical to
//! backend/parity/routes.python.json) is scanned in order against the percent-decoded path; the first route whose path
//! AND method match handles the request; otherwise the first path-only match answers 405; with no match at all (only
//! possible without a frontend dir) a slash-variant redirect (307) or 404. Path parameters are `[^/]+` (or `.*` for
//! `{x:path}`), matched on the decoded path. Owner: CORE (LOCKED).

pub mod deps;
pub mod errors;
pub mod routes;
pub mod security;
pub mod static_files;

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use bytes::Bytes;
use futures::future::BoxFuture;
use http::{HeaderMap, Response};
use regex::Regex;

use crate::services::auth::Principal;
use crate::services::hub::Hub;

/// Shared state of a running server.
pub struct AppState {
    pub hub: Arc<Hub>,
    pub routes: Vec<Route>,
    pub frontend: Option<std::path::PathBuf>,
    pub knowledge_sse: deps::SseLimiter,
    /// how long a request body may take to arrive (408 after it; audit F8)
    pub body_deadline: std::time::Duration,
}

/// [`AppState::body_deadline`] of a served app.
pub const BODY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// One request, as the handlers see it (after the security middleware).
pub struct Req {
    pub method: String,
    /// percent-decoded path (Starlette `scope["path"]`)
    pub path: String,
    /// raw query string (latin-1)
    pub query: String,
    /// `parse_qsl(query, keep_blank_values=True)`
    pub query_pairs: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub params: HashMap<String, String>,
    pub principal: Principal,
    /// the listening socket's port
    pub server_port: u16,
    /// set when the handler consumed the body (FastAPI reads it only for body routes; the knowledge proxy only after
    /// its own checks): the audit row records a body only then
    pub body_used: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Req {
    /// A header value (latin-1 decoded), case-insensitive name.
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .map(|v| v.as_bytes().iter().map(|&b| char::from(b)).collect())
    }
    /// A path parameter.
    pub fn param(&self, name: &str) -> String {
        self.params.get(name).cloned().unwrap_or_default()
    }
    /// Mark the body as read (see `body_used`).
    pub fn use_body(&self) -> &Bytes {
        self.body_used.store(true, std::sync::atomic::Ordering::SeqCst);
        &self.body
    }
}

pub type Handler = fn(Arc<AppState>, Req) -> BoxFuture<'static, Response<Body>>;

/// A registered route (Python APIRoute / Route).
pub struct Route {
    pub methods: &'static [&'static str],
    /// the effective template, e.g. "/api/v1/clients/{cid}"
    pub path: &'static str,
    /// the label Python's metrics use (`route.path`, prefixed with /api/v1 for included routers)
    pub name: &'static str,
    pub handler: Handler,
    /// FastAPI reads the body before running dependencies (routes with a body parameter)
    pub reads_body: bool,
    regex: Regex,
    params: Vec<String>,
}

impl Route {
    pub fn new(
        methods: &'static [&'static str],
        path: &'static str,
        name: &'static str,
        handler: Handler,
        reads_body: bool,
    ) -> Self {
        let (regex, params) = compile_path(path);
        Self {
            methods,
            path,
            name,
            handler,
            reads_body,
            regex,
            params,
        }
    }

    /// The path parameters when the path matches.
    pub fn match_path(&self, path: &str) -> Option<HashMap<String, String>> {
        let caps = self.regex.captures(path)?;
        Some(
            self.params
                .iter()
                .filter_map(|n| caps.name(n).map(|m| (n.clone(), m.as_str().to_string())))
                .collect(),
        )
    }
}

pub use hub_core::py::starlette::compile_path;

/// Starlette's match over the ordered table.
pub enum Matched<'a> {
    Full(&'a Route, HashMap<String, String>),
    /// path matched, method did not: 405
    Partial(&'a Route),
    None,
}

pub fn find_route<'a>(routes: &'a [Route], method: &str, path: &str) -> Matched<'a> {
    let mut partial: Option<&Route> = None;
    for r in routes {
        if let Some(params) = r.match_path(path) {
            if r.methods.contains(&method) {
                return Matched::Full(r, params);
            }
            if partial.is_none() {
                partial = Some(r);
            }
        }
    }
    match partial {
        Some(r) => Matched::Partial(r),
        None => Matched::None,
    }
}

/// `parse_qsl(qs, keep_blank_values=True)` (`&`-separated, `+` = space, %-decoding with replacement).
pub fn parse_qsl(qs: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in qs.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        let dec = |s: &str| {
            percent_encoding::percent_decode_str(&s.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned()
        };
        out.push((dec(k), dec(v)));
    }
    out
}
