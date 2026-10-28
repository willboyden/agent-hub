//! api/security.py `SecurityMiddleware`, merged with Starlette's routing into one request pipeline: Host allowlist
//! (DNS rebinding, 421), same-origin Origin check (403), `X-Agent-Hub: 1` CSRF header on non-GET without a bearer
//! (403), Content-Length cap (413), authentication (401) and role gates (403) on /api/* and /metrics except
//! /api/v1/health, streaming body cap (413), strict security headers on EVERY response (+ `Cache-Control: no-store` on
//! /api/*), bounded-label metrics and an audit row for every mutating guarded call (bodies redacted; inbox/knowledge
//! bodies as metadata only). There is deliberately no CORS. Security controls S-HOST, S-ORIGIN, S-CSRF, S-AUTH,
//! S-ROLES, S-BODYCAP, S-HEADERS, S-AUDIT. Owner: CORE (LOCKED).
//!
//! Since 0.3.0 (audit F8, PORTING.md A-14): a body that ends early is a 400 and a body that does not arrive within
//! `AppState::body_deadline` a 408 (Python handed a truncated body to the handler); a panicking handler answers 500
//! and still gets its audit row; the key lookup and the audit insert (SQLite) run on the blocking pool.

use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Instant;

use futures::FutureExt as _;

use axum::body::Body;
use bytes::Bytes;
use http::{HeaderName, HeaderValue, Request, Response};
use serde_json::{Map, Value, json};

use super::errors::{http_error, problem_parts};
use super::{AppState, Matched, Req, find_route, parse_qsl};
use crate::services::auth::Principal;

pub const LOOPBACK: [&str; 3] = ["127.0.0.1", "::1", "localhost"];
pub const OPEN_PATHS: [&str; 1] = ["/api/v1/health"];
pub const SAFE: [&str; 3] = ["GET", "HEAD", "OPTIONS"];
pub const MUTATING: [&str; 4] = ["POST", "PUT", "PATCH", "DELETE"];
pub const KNOWN_METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
pub const CSRF_HEADER: &str = "x-agent-hub";
pub const INBOX: &str = "/api/v1/memory/inbox";
pub const CONTENT_PATHS: [&str; 2] = [INBOX, "/api/v1/knowledge"];
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; object-src 'none'; frame-ancestors 'none'";
pub const SECURITY_HEADERS: [(&str, &str); 6] = [
    ("content-security-policy", CSP),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("x-frame-options", "DENY"),
    ("permissions-policy", "camera=(), microphone=(), geolocation=()"),
    ("cross-origin-resource-policy", "same-origin"),
];
pub const BODY_CAP_AUDIT: usize = 64 * 1024;

/// The ONLY mutating (method, path) pairs a viewer may perform: read-only dry runs and the knowledge query playground.
pub fn viewer_may(method: &str, path: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        hub_core::re(r"^(?:/api/v1/(policy/check|clients/validate-spec|knowledge/namespaces/[A-Za-z0-9._-]+/query))$")
    });
    method == "POST" && RE.is_match(path)
}

/// Client tokens may only post to the memory inbox.
pub fn client_may(method: &str, path: &str) -> bool {
    method == "POST" && path == INBOX
}

/// `_guarded(path)`.
pub fn guarded(path: &str) -> bool {
    (path.starts_with("/api/") || path == "/metrics") && !OPEN_PATHS.contains(&path)
}

/// `urlsplit(origin).netloc` for the Origin check.
pub fn netloc(url: &str) -> &str {
    let rest = match url.find(':') {
        Some(i)
            if i > 0
                && url[..i].chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && url[..i].chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) =>
        {
            &url[i + 1..]
        }
        _ => url,
    };
    match rest.strip_prefix("//") {
        Some(r) => {
            let end = r.find(['/', '?', '#']).unwrap_or(r.len());
            &r[..end]
        }
        None => "",
    }
}

fn header_str(req: &Request<Body>, name: &str) -> Option<String> {
    req.headers()
        .get(name)
        .map(|v| v.as_bytes().iter().map(|&b| char::from(b)).collect())
}

fn allowed_hosts(state: &AppState, server_port: u16) -> Vec<String> {
    let cfg = state.hub.cfg();
    let mut ports = vec![cfg.port];
    if !ports.contains(&i64::from(server_port)) {
        ports.push(i64::from(server_port));
    }
    let mut hosts = Vec::new();
    for p in &ports {
        for h in ["127.0.0.1", "localhost", "[::1]", cfg.host.as_str()] {
            hosts.push(format!("{h}:{p}").to_lowercase());
        }
    }
    hosts.extend(cfg.allowed_hosts.iter().map(|h| h.to_lowercase()));
    hosts
}

fn cap_for(state: &AppState, path: &str) -> usize {
    let cfg = state.hub.cfg();
    let c = if path == "/api/v1/import" || path.ends_with("/discover") {
        cfg.import_body_cap_bytes
    } else if path.starts_with("/api/v1/knowledge") {
        cfg.knowledge_body_cap_bytes
    } else {
        cfg.body_cap_bytes
    };
    usize::try_from(c).unwrap_or(0)
}

type Deny = (u16, &'static str, String);

fn precheck(state: &AppState, req: &Request<Body>, method: &str, path: &str, server_port: u16) -> Option<Deny> {
    let host = header_str(req, "host").unwrap_or_default().to_lowercase();
    if !allowed_hosts(state, server_port).contains(&host) {
        return Some((421, "misdirected_request", "unrecognised Host header".into()));
    }
    if !SAFE.contains(&method) {
        if let Some(origin) = header_str(req, "origin")
            && (origin == "null" || netloc(&origin).to_lowercase() != host)
        {
            return Some((403, "cross_origin", "cross-origin requests are not allowed".into()));
        }
        if req.headers().get("authorization").is_none() && header_str(req, CSRF_HEADER).as_deref() != Some("1") {
            return Some((
                403,
                "csrf_header_required",
                "send the X-Agent-Hub: 1 header on non-GET requests".into(),
            ));
        }
        if let Some(cl) = header_str(req, "content-length")
            && !cl.is_empty()
            && cl.chars().all(|c| c.is_ascii_digit())
            && cl.parse::<u128>().map_or(true, |n| n > cap_for(state, path) as u128)
        {
            return Some((413, "payload_too_large", "request body too large".into()));
        }
    }
    None
}

/// The caller's principal from its `Authorization` header (and whether `X-Forwarded-For` was sent). The bearer lookup
/// reads (and updates `last_used` in) SQLite, so it runs on the blocking pool.
async fn principal(
    state: &Arc<AppState>,
    auth: Option<String>,
    forwarded: bool,
    peer: SocketAddr,
) -> Option<Principal> {
    if let Some(auth) = auth.filter(|a| !a.is_empty()) {
        let (scheme, tok) = auth.split_once(' ').unwrap_or((auth.as_str(), ""));
        let tok = hub_core::py::text::strip(tok).to_string();
        if !scheme.eq_ignore_ascii_case("bearer") || tok.is_empty() {
            return None;
        }
        let st = state.clone();
        return tokio::task::spawn_blocking(move || st.hub.auth.authenticate(&tok))
            .await
            .unwrap_or(None);
    }
    let cfg = state.hub.cfg();
    let host = peer.ip().to_string();
    if cfg.trust_loopback && LOOPBACK.contains(&host.as_str()) && !forwarded {
        return Some(Principal::loopback());
    }
    None
}

/// Why a body could not be read.
enum BodyError {
    /// over the cap (Python raises BodyTooLarge at the first chunk over it)
    TooLarge,
    /// the stream failed before its end (the client went away, a bad chunk): never handed on as if complete
    Incomplete,
    /// not complete within the deadline
    TooSlow,
}

/// Read the whole body up to `cap` within `deadline`.
async fn read_body(body: Body, cap: usize, deadline: std::time::Duration) -> Result<Bytes, BodyError> {
    let read = async {
        let mut stream = body.into_data_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
            let c = chunk.map_err(|_| BodyError::Incomplete)?;
            if buf.len() + c.len() > cap {
                return Err(BodyError::TooLarge);
            }
            buf.extend_from_slice(&c);
        }
        Ok(Bytes::from(buf))
    };
    tokio::time::timeout(deadline, read)
        .await
        .unwrap_or(Err(BodyError::TooSlow))
}

fn decode_path(raw: &str) -> String {
    percent_encoding::percent_decode_str(raw)
        .decode_utf8_lossy()
        .into_owned()
}

fn finish_headers(resp: &mut Response<Body>, path: &str) {
    let h = resp.headers_mut();
    for (n, _) in SECURITY_HEADERS {
        h.remove(n);
    }
    if path.starts_with("/api/") {
        h.remove("cache-control");
    }
    for (n, v) in SECURITY_HEADERS {
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(n.as_bytes()), HeaderValue::from_str(v)) {
            h.append(n, v);
        }
    }
    if path.starts_with("/api/") {
        h.append(http::header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
}

/// The whole pipeline for one request.
pub async fn handle(state: Arc<AppState>, peer: SocketAddr, server_port: u16, req: Request<Body>) -> Response<Body> {
    let t0 = Instant::now();
    let method = req.method().as_str().to_string();
    let raw_path = req.uri().path().to_string();
    let path = decode_path(&raw_path);
    let query = req.uri().query().unwrap_or("").to_string();
    let mut princ = Principal::public();
    let mut deny = precheck(&state, &req, &method, &path, server_port);
    if deny.is_none() && guarded(&path) {
        let auth = header_str(&req, "authorization");
        let forwarded = req.headers().get("x-forwarded-for").is_some();
        match principal(&state, auth, forwarded, peer).await {
            None => deny = Some((401, "unauthorized", "a valid bearer API key is required".into())),
            Some(p) if p.role == "client" && !client_may(&method, &path) => {
                deny = Some((
                    403,
                    "forbidden",
                    "client tokens may only post to the memory inbox".into(),
                ));
                princ = p;
            }
            Some(p) if p.role == "viewer" && MUTATING.contains(&method.as_str()) && !viewer_may(&method, &path) => {
                deny = Some((403, "forbidden", "this key has the viewer role (read-only)".into()));
                princ = p;
            }
            Some(p) => princ = p,
        }
    }

    let headers = req.headers().clone();
    let cap = cap_for(&state, &path);
    let mut route_label: Option<String> = None;
    let mut body_seen: Option<Bytes> = None;
    let mut body_flag: Option<Arc<std::sync::atomic::AtomicBool>> = None;
    let mut resp: Response<Body> = if let Some((status, code, detail)) = &deny {
        problem_parts(*status, code, &code.replace('_', " "), detail)
    } else {
        match find_route(&state.routes, &method, &path) {
            Matched::Full(route, params) => {
                route_label = Some(super::routes::metric_label(route.name, &path));
                let body = if route.reads_body || route.path == "/api/v1/knowledge/{path:path}" {
                    read_body(req.into_body(), cap, state.body_deadline).await
                } else {
                    Ok(Bytes::new())
                };
                if let Err(e) = body {
                    match e {
                        BodyError::TooLarge => {
                            problem_parts(413, "payload_too_large", "payload too large", "request body too large")
                        }
                        BodyError::Incomplete => problem_parts(
                            400,
                            "body_incomplete",
                            "bad request",
                            "the request body ended before it was complete",
                        ),
                        BodyError::TooSlow => problem_parts(
                            408,
                            "body_timeout",
                            "request timeout",
                            "the request body did not arrive in time",
                        ),
                    }
                } else {
                    let body = body.unwrap_or_default();
                    let used = Arc::new(std::sync::atomic::AtomicBool::new(route.reads_body));
                    body_seen = Some(body.clone());
                    body_flag = Some(used.clone());
                    let r = Req {
                        method: method.clone(),
                        path: path.clone(),
                        query: query.clone(),
                        query_pairs: parse_qsl(&query),
                        headers: headers.clone(),
                        body,
                        params,
                        principal: princ.clone(),
                        server_port,
                        body_used: used,
                    };
                    // FastAPI reads (and decodes) the body before any dependency; then enforce_roles, the dependency
                    // every API router carries
                    let api_router = route.path.starts_with("/api/v1/") && route.path != "/api/v1/openapi.json";
                    let decoded = if route.reads_body {
                        super::deps::decode_body(&r).map(|_| ())
                    } else {
                        Ok(())
                    };
                    match (decoded, super::deps::enforce_roles(&r)) {
                        (Err(resp), _) => *resp,
                        (Ok(()), Err(p)) if api_router => super::errors::problem(&p),
                        // a panic in a handler is a 500 (and still audited below), never a dropped connection
                        _ => AssertUnwindSafe((route.handler)(state.clone(), r))
                            .catch_unwind()
                            .await
                            .unwrap_or_else(|_| {
                                tracing::error!("request handler panicked");
                                super::errors::problem(&hub_core::Problem::internal())
                            }),
                    }
                }
            }
            Matched::Partial(route) => {
                route_label = Some(super::routes::metric_label(route.name, &path));
                http_error(405, "Method Not Allowed")
            }
            Matched::None => http_error(404, "Not Found"),
        }
    };
    let status = resp.status().as_u16();
    finish_headers(&mut resp, &path);

    // metrics: method from a fixed set, route = matched template (never the raw path)
    let mlabel = if KNOWN_METHODS.contains(&method.as_str()) {
        method.as_str()
    } else {
        "OTHER"
    };
    let route = route_label.unwrap_or_else(|| {
        if guarded(&path) {
            "unmatched".into()
        } else {
            "static".into()
        }
    });
    let hub = &state.hub;
    hub.telemetry.requests.inc(&[mlabel, &route, &status.to_string()]);
    hub.telemetry
        .latency
        .observe(&[mlabel, &route], t0.elapsed().as_secs_f64());
    if guarded(&path) && MUTATING.contains(&method.as_str()) {
        let used = body_flag.is_some_and(|f| f.load(std::sync::atomic::Ordering::SeqCst));
        let raw = if used {
            body_seen.unwrap_or_default()
        } else {
            Bytes::new()
        };
        let (st, m, p) = (state.clone(), method.clone(), path.clone());
        let _ = tokio::task::spawn_blocking(move || audit(&st, &princ, &m, &p, status, &query, &headers, &raw)).await;
    }
    resp
}

#[allow(clippy::too_many_arguments)]
fn audit(
    state: &AppState,
    p: &Principal,
    method: &str,
    path: &str,
    status: u16,
    query: &str,
    headers: &http::HeaderMap,
    raw_full: &[u8],
) {
    let size = raw_full.len();
    let raw = &raw_full[..size.min(BODY_CAP_AUDIT)];
    let mut params = Map::new();
    if !query.is_empty() {
        let latin: String = query.bytes().map(char::from).collect();
        let mut q = Map::new();
        for (k, v) in parse_qsl(&latin) {
            q.insert(k, Value::String(v));
        }
        params.insert("query".into(), hub_core::common::redact(&Value::Object(q)));
    }
    let ctype = headers
        .get("content-type")
        .map(|v| String::from_utf8_lossy(v.as_bytes()).to_string())
        .unwrap_or_default();
    if CONTENT_PATHS.iter().any(|c| path.starts_with(c)) {
        let mut meta = Map::new();
        meta.insert("bytes".into(), json!(size));
        if !raw.is_empty()
            && let Some(Value::Object(doc)) = super::deps::audit_json(raw)
        {
            let mut keys: Vec<&String> = doc.keys().collect();
            keys.sort();
            meta.insert("fields".into(), json!(keys.into_iter().take(30).collect::<Vec<_>>()));
        }
        params.insert("body".into(), Value::Object(meta));
    } else if !raw.is_empty() && ctype.contains("json") {
        let v =
            super::deps::audit_json(raw).map_or_else(|| json!("[unparseable json]"), |v| hub_core::common::redact(&v));
        params.insert("body".into(), v);
    } else if !raw.is_empty() {
        params.insert("body".into(), json!(format!("[{size} bytes, non-json]")));
    }
    let actor = p.key_id.clone().unwrap_or_else(|| p.name.clone());
    if state
        .hub
        .audit_svc
        .record(&actor, &p.role, method, path, status, &Value::Object(params))
        .is_err()
    {
        tracing::error!("audit write failed");
    }
}
