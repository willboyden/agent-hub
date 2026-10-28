//! Port of app.py `create_app` plus the request pipeline that FastAPI/Starlette/uvicorn give the Python service:
//!
//! 1. `security.rs` precheck (Host 421, path 400, Origin/CSRF 403, Content-Length 413);
//! 2. Starlette routing on the percent-decoded path, first match wins (table order = `routes.knowledge.json`); a
//!    path-only match is 405, no match tries `redirect_slashes` (307 to the path with/without its trailing slash),
//!    else 404;
//! 3. for body routes, the body is read under the streaming cap (413) and JSON-decoded when the content type is JSON
//!    (`json_invalid` 422 at the decoder position; any other decode failure 400 "There was an error parsing the body");
//! 4. the route's dependencies and handler (`api.rs`);
//! 5. error handlers: `KnowledgeError` -> problem (+ `Retry-After: 30` on 429), validation -> one-string detail,
//!    HTTP errors -> `not_found` / `method_not_allowed` / `payload_too_large` / `http_error`, anything else -> 500
//!    `internal_error`;
//! 6. security headers on every response (replacing the handler's), metrics (route template or `unmatched`, method or
//!    `OTHER`), and the metadata-only audit row for mutating methods.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::body::Body as AxBody;
use bytes::Bytes;
use http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use http_body_util::BodyExt;
use hub_core::PyValue;
use regex::Regex;

use crate::api::{self, Body, Ctx, Out, Req};
use crate::config::Config;
use crate::db::{Db, hash_secret};
use crate::embed::{Embedder, RouterEmbedder};
use crate::env::{self, Env};
use crate::errors::{KnowledgeError, problem_text};
use crate::pyjson::{self, DecodeError, Style};
use crate::ratelimit::RateLimiter;
use crate::security;
use crate::service::{Job, MetricsHook, Service};
use crate::stores::Stores;
use crate::stores::qdrant::QdrantStore;
use crate::telemetry::Telemetry;

pub use crate::api::VERSION;

/// (methods, path template, Python endpoint name, has a body parameter)
pub const ROUTES: [(&[&str], &str, &str, bool); 20] = [
    (&["GET"], "/namespaces", "list_ns", false),
    (&["POST"], "/namespaces", "create_ns", true),
    (&["GET"], "/namespaces/{ns}", "get_ns", false),
    (&["PUT"], "/namespaces/{ns}", "put_ns", true),
    (&["DELETE"], "/namespaces/{ns}", "del_ns", false),
    (&["POST"], "/namespaces/{ns}/ingest", "ingest", true),
    (&["POST"], "/namespaces/{ns}/query", "query", true),
    (
        &["DELETE"],
        "/namespaces/{ns}/documents/{doc_id:path}",
        "del_doc",
        false,
    ),
    (&["GET"], "/jobs", "list_jobs", false),
    (&["GET"], "/jobs/{jid}", "job", false),
    (&["GET"], "/jobs/{jid}/stream", "job_stream", false),
    (&["GET"], "/tokens", "list_tokens", false),
    (&["POST"], "/tokens", "create_token", true),
    (&["DELETE"], "/tokens/{tid}", "delete_token", false),
    (&["GET"], "/indexes", "list_idx", false),
    (&["POST"], "/indexes", "add_idx", true),
    (&["DELETE"], "/indexes/{name}", "del_idx", false),
    (&["GET"], "/backends", "backends", false),
    (&["GET"], "/metrics", "metrics", false),
    (&["GET"], "/health", "health", false),
];

const KNOWN_METHODS: [&str; 7] = ["GET", "HEAD", "OPTIONS", "POST", "PUT", "PATCH", "DELETE"];

struct Route {
    methods: &'static [&'static str],
    template: &'static str,
    name: &'static str,
    body: bool,
    re: Regex,
    params: Vec<String>,
}

pub struct App {
    pub ctx: Arc<Ctx>,
    routes: Vec<Route>,
    hosts: Vec<String>,
    /// how long a request body may take to arrive, in ms (408 after it; [`BODY_DEADLINE`] unless a test sets it)
    body_deadline_ms: std::sync::atomic::AtomicU64,
}

/// The default [`App`] body deadline, the hub's too.
pub const BODY_DEADLINE: Duration = Duration::from_secs(60);

/// What `create_app` can be given instead of the defaults (Python's `stores=` / `embedder=` keywords) and the
/// environment it reads.
#[derive(Default)]
pub struct Options {
    pub stores: Option<Stores>,
    pub embedder: Option<Arc<dyn Embedder>>,
    pub env: Option<Env>,
}

/// `default_stores(cfg)`: LanceDB always, Qdrant when `qdrant_url` is set.
pub fn default_stores(cfg: &Config, env: &Env) -> Result<Stores, String> {
    let mut stores: Stores = Vec::new();
    #[cfg(feature = "lance")]
    {
        use crate::stores::lance::{LanceOptions, LanceStore};
        let workers = usize::try_from(cfg.lance_workers).ok().filter(|w| *w > 0);
        let Some(workers) = workers else {
            return Err("max_workers must be greater than 0".into());
        };
        let s = LanceStore::new(
            &cfg.lance_path(),
            LanceOptions {
                max_scan_rows: cfg.lance_max_scan_rows,
                timeout_s: cfg.store_timeout_s,
                lock_wait_s: cfg.lance_lock_wait_s,
                workers,
                before_op: None,
            },
        )
        .map_err(|e| hub_core::py::os::error_str(&e, Some(&cfg.lance_path().to_string_lossy())))?;
        stores.push(("lancedb".into(), Arc::new(s)));
    }
    if let Some(url) = &cfg.qdrant_url {
        let allow = env::get(env, "KNOWLEDGE_ALLOW_UNAUTH_QDRANT") == "1";
        stores.push((
            "qdrant".into(),
            Arc::new(QdrantStore::new(
                url,
                cfg.qdrant_api_key_env.as_deref(),
                cfg.qdrant_vectors_on_disk,
                cfg.store_timeout_s,
                allow,
                env.clone(),
            )),
        ));
    }
    Ok(stores)
}

/// `create_app(cfg)`.
pub fn create_app(cfg: Config) -> Result<Arc<App>, String> {
    create_app_with(cfg, Options::default())
}

/// `create_app(cfg, stores=..., embedder=...)`.
pub fn create_app_with(cfg: Config, opts: Options) -> Result<Arc<App>, String> {
    let env = opts.env.unwrap_or_else(env::process);
    let path = cfg.data_dir.to_string_lossy().into_owned();
    let db = Db::open(&cfg.data_dir).map_err(|e| hub_core::py::os::error_str(&e, Some(&path)))?;
    let admin_hash = hash_secret(&db.admin_key().map_err(|e| hub_core::py::os::error_str(&e, None))?);
    let tel = Arc::new(Telemetry::new());
    let emb: Arc<dyn Embedder> = match opts.embedder {
        Some(e) => e,
        None => Arc::new(RouterEmbedder::with(
            &cfg.router_url,
            &cfg.router_api_key_env,
            cfg.embed_batch,
            4,
            60.0,
            crate::embed::tokio_sleep(),
            env.clone(),
        )),
    };
    let stores = match opts.stores {
        Some(s) => s,
        None => default_stores(&cfg, &env)?,
    };
    let t2 = tel.clone();
    let hook: MetricsHook = Arc::new(move |j: &Job| {
        let s = j.snapshot();
        t2.jobs.inc(&[&s.state]);
        #[allow(clippy::cast_precision_loss)]
        t2.chunks.inc_by(&[&j.ns], s.chunks as f64);
    });
    let cfg = Arc::new(cfg);
    let db = Arc::new(db);
    let svc = Arc::new(Service::new(cfg.clone(), db.clone(), stores, emb, Some(hook)));
    let hosts = security::allowed_hosts(&cfg);
    let ctx = Arc::new(Ctx {
        cfg,
        db,
        svc,
        tel,
        limiter: RateLimiter::default(),
        admin_hash,
        streams: std::sync::atomic::AtomicI64::new(0),
        env,
    });
    let routes = ROUTES
        .iter()
        .map(|(m, p, n, b)| {
            let (re, params) = hub_core::py::starlette::compile_path(p);
            Route {
                methods: m,
                template: p,
                name: n,
                body: *b,
                re,
                params,
            }
        })
        .collect();
    Ok(Arc::new(App {
        ctx,
        routes,
        hosts,
        body_deadline_ms: std::sync::atomic::AtomicU64::new(
            u64::try_from(BODY_DEADLINE.as_millis()).unwrap_or(u64::MAX),
        ),
    }))
}

impl App {
    /// Set the request-body deadline (tests use a short one).
    pub fn set_body_deadline(&self, d: Duration) {
        self.body_deadline_ms
            .store(u64::try_from(d.as_millis()).unwrap_or(u64::MAX), Ordering::SeqCst);
    }

    fn body_deadline(&self) -> Duration {
        Duration::from_millis(self.body_deadline_ms.load(Ordering::SeqCst))
    }

    /// The lifespan's startup warnings (logged at ERROR, names only).
    pub async fn startup_warnings(&self) -> Vec<String> {
        api::startup_warnings(&self.ctx.svc.stores, &self.ctx.cfg, &self.ctx.env).await
    }
}

// ------------------------------------------------------------------------------------------------ responses

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: AxBody,
}

fn problem_resp(status: u16, code: &str, detail: &str) -> Resp {
    let mut headers = vec![("content-type".to_string(), "application/problem+json".to_string())];
    if status == 429 {
        headers.push(("retry-after".into(), "30".into()));
    }
    Resp {
        status,
        headers,
        body: AxBody::from(problem_text(status, code, detail)),
    }
}

fn ke_resp(e: &KnowledgeError) -> Resp {
    if e.status == 500 {
        tracing::error!("unhandled error");
    }
    problem_resp(e.status, &e.code, &e.detail)
}

/// Starlette `HTTPException` through the app's `_http` handler.
fn http_error(status: u16, detail: &str) -> Resp {
    let code = match status {
        404 => "not_found",
        405 => "method_not_allowed",
        413 => "payload_too_large",
        _ => "http_error",
    };
    problem_resp(status, code, &detail.chars().take(200).collect::<String>())
}

fn json_resp(status: u16, v: &PyValue) -> Resp {
    Resp {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: AxBody::from(pyjson::dumps(v, Style::Pydantic)),
    }
}

/// `urllib.parse.quote(url, safe=":/%#?=@[]!$&'()*+,;")` (RedirectResponse's Location).
fn quote_url(s: &str) -> String {
    const SAFE: &str = ":/%#?=@[]!$&'()*+,;_.-~";
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || SAFE.as_bytes().contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ------------------------------------------------------------------------------------------------ the pipeline

struct Incoming {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: AxBody,
}

/// Records metrics (and the stream slot) when a streamed response ends, however it ends.
struct StreamGuard {
    app: Arc<App>,
    method: String,
    route: String,
    t0: Instant,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.app.ctx.streams.fetch_sub(1, Ordering::SeqCst);
        let m = metric_method(&self.method);
        self.app.ctx.tel.requests.inc(&[m, &self.route, "200"]);
        self.app
            .ctx
            .tel
            .latency
            .observe(&[m, &self.route], self.t0.elapsed().as_secs_f64());
    }
}

fn metric_method(m: &str) -> &str {
    if KNOWN_METHODS.contains(&m) { m } else { "OTHER" }
}

fn sse_body(max_seconds: i64, job: Arc<Job>, guard: StreamGuard) -> AxBody {
    let max = Duration::from_secs(u64::try_from(max_seconds.max(0)).unwrap_or(0));
    let stream = async_stream::stream! {
        let _guard = guard;
        let deadline = Instant::now() + max;
        let mut last = String::new();
        while Instant::now() < deadline {
            let (view, state) = job.view_and_state();
            let snap = pyjson::dumps(&view, Style::Std);
            if snap != last {
                yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(format!("event: progress\ndata: {snap}\n\n")));
                last = snap;
            }
            if state == "done" || state == "failed" {
                let end = pyjson::dumps(&pyjson::obj([("state", PyValue::Str(state))]), Style::Std);
                yield Ok(Bytes::from(format!("event: end\ndata: {end}\n\n")));
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    AxBody::from_stream(stream)
}

/// After a response that left the request body unread (a precheck refusal, a route without a body, the streaming
/// cap), keep reading and discarding it in the background like uvicorn does, so the client can finish its upload and
/// read the answer instead of hitting a reset. Bounded: 256 MiB or 60 s, then the connection is dropped.
fn drain_later(body: AxBody) {
    tokio::spawn(async move {
        let mut body = body;
        let mut seen: u64 = 0;
        let _ = tokio::time::timeout(Duration::from_secs(60), async {
            while let Some(Ok(frame)) = body.frame().await {
                if let Ok(d) = frame.into_data() {
                    seen += d.len() as u64;
                    if seen > 256 << 20 {
                        break;
                    }
                }
            }
        })
        .await;
    });
}

/// Why a body could not be read.
enum BodyError {
    /// over the cap (413; the rest is drained by `drain_later`)
    TooLarge,
    /// the stream failed before its end: never handed on as if it were the whole body (400)
    Incomplete,
    /// not complete within the deadline (408)
    TooSlow,
}

/// Reads a body under the cap within `deadline`. `total` counts what was received. Since 0.3.0 a body that breaks off
/// is a 400 and a slow one a 408, as on the hub (Python handed a truncated body on).
async fn read_capped(body: &mut AxBody, cap: i64, total: &mut u64, deadline: Duration) -> Result<Bytes, BodyError> {
    let cap = u64::try_from(cap.max(0)).unwrap_or(0);
    let read = async {
        let mut buf = bytes::BytesMut::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| BodyError::Incomplete)?;
            if let Ok(data) = frame.into_data() {
                *total += data.len() as u64;
                if *total > cap {
                    return Err(BodyError::TooLarge);
                }
                buf.extend_from_slice(&data);
            }
        }
        Ok(buf.freeze())
    };
    tokio::time::timeout(deadline, read)
        .await
        .unwrap_or(Err(BodyError::TooSlow))
}

fn is_json_ctype(ct: &str) -> bool {
    let main = ct.split(';').next().unwrap_or("").trim().to_lowercase();
    let Some((t, sub)) = main.split_once('/') else {
        return false;
    };
    if main.matches('/').count() != 1 {
        return false;
    }
    t == "application" && (sub == "json" || sub.ends_with("+json"))
}

/// FastAPI's body step (before any dependency runs): `Ok(Body)`, a 422 `json_invalid` at the decoder position, or
/// the 400 any other decoding failure gives.
pub fn decode_body(bytes: &[u8], ctype: Option<&str>) -> Result<Body, KnowledgeError> {
    if bytes.is_empty() {
        return Ok(Body::Missing);
    }
    let Some(ct) = ctype.filter(|c| !c.is_empty()) else {
        return Ok(Body::Raw);
    };
    if !is_json_ctype(ct) {
        return Ok(Body::Raw);
    }
    match pyjson::loads_bytes(bytes) {
        Ok(v) => Ok(Body::Json(v)),
        Err(DecodeError::Json { pos, .. }) => Err(api::validation_error(format!("body.{pos}: JSON decode error"))),
        Err(_) => Err(KnowledgeError::new(
            400,
            "http_error",
            "There was an error parsing the body",
        )),
    }
}

async fn call(name: &str, c: &Ctx, req: &mut Req) -> api::HResult {
    match name {
        "list_ns" => api::list_ns(c, req).await,
        "create_ns" => api::create_ns(c, req).await,
        "get_ns" => api::get_ns(c, req).await,
        "put_ns" => api::put_ns(c, req).await,
        "del_ns" => api::del_ns(c, req).await,
        "ingest" => api::ingest(c, req).await,
        "query" => api::query(c, req).await,
        "del_doc" => api::del_doc(c, req).await,
        "list_jobs" => api::list_jobs(c, req).await,
        "job" => api::job(c, req).await,
        "job_stream" => api::job_stream(c, req).await,
        "list_tokens" => api::list_tokens(c, req).await,
        "create_token" => api::create_token(c, req).await,
        "delete_token" => api::delete_token(c, req).await,
        "list_idx" => api::list_idx(c, req).await,
        "add_idx" => api::add_idx(c, req).await,
        "del_idx" => api::del_idx(c, req).await,
        "backends" => api::backends(c, req).await,
        "metrics" => api::metrics(c, req).await,
        "health" => api::health(c, req).await,
        _ => Err(KnowledgeError::internal()),
    }
}

/// One request through the whole pipeline.
async fn pipeline(app: Arc<App>, mut inc: Incoming) -> Response<AxBody> {
    let t0 = Instant::now();
    let mut body_in = std::mem::replace(&mut inc.body, AxBody::empty());
    let resp = pipeline_inner(app, inc, &mut body_in, t0).await;
    drain_later(body_in);
    resp
}

async fn pipeline_inner(app: Arc<App>, inc: Incoming, body_in: &mut AxBody, t0: Instant) -> Response<AxBody> {
    let c = app.ctx.clone();
    // an exception in the middleware itself (urlsplit's ValueError): ServerErrorMiddleware's 500, nothing recorded
    let Ok(deny) = security::precheck(&c.cfg, &app.hosts, &inc.method, &inc.path, &inc.headers) else {
        return finish(problem_resp(500, "internal_error", "internal error"));
    };
    let mut route_label: Option<&'static str> = None;
    let mut principal: Option<String> = None;
    let mut total: u64 = 0;
    let resp = match deny {
        Some((status, code, detail)) => problem_resp(status, code, detail),
        None => {
            let mut full = None;
            let mut partial = None;
            for r in &app.routes {
                if let Some(caps) = r.re.captures(&inc.path) {
                    if r.methods.contains(&inc.method.as_str()) {
                        full = Some((r, caps));
                        break;
                    }
                    if partial.is_none() {
                        partial = Some(r);
                    }
                }
            }
            if let Some((r, caps)) = full {
                route_label = Some(r.template);
                let params: Vec<(String, String)> = r
                    .params
                    .iter()
                    .map(|p| (p.clone(), caps.name(p).map_or("", |m| m.as_str()).to_string()))
                    .collect();
                let body = if r.body {
                    match read_capped(body_in, c.cfg.body_cap_bytes, &mut total, app.body_deadline()).await {
                        Err(BodyError::TooLarge) => Err(http_error(413, "request body too large")),
                        Err(BodyError::Incomplete) => Err(problem_resp(
                            400,
                            "body_incomplete",
                            "the request body ended before it was complete",
                        )),
                        Err(BodyError::TooSlow) => Err(problem_resp(
                            408,
                            "body_timeout",
                            "the request body did not arrive in time",
                        )),
                        Ok(bytes) => {
                            let ct = inc
                                .headers
                                .iter()
                                .find(|(k, _)| k == "content-type")
                                .map(|(_, v)| v.as_str());
                            decode_body(&bytes, ct).map_err(|e| ke_resp(&e))
                        }
                    }
                } else {
                    Ok(Body::Missing)
                };
                match body {
                    Err(resp) => resp,
                    Ok(body) => {
                        let mut req = Req {
                            method: inc.method.clone(),
                            path: inc.path.clone(),
                            headers: inc.headers.clone(),
                            params,
                            body,
                            principal_id: None,
                        };
                        let out = call(r.name, &c, &mut req).await;
                        principal = req.principal_id.take();
                        match out {
                            Ok(Out::Json(status, v)) => json_resp(status, &v),
                            Ok(Out::NoContent) => Resp {
                                status: 204,
                                headers: Vec::new(),
                                body: AxBody::empty(),
                            },
                            Ok(Out::Text(text, ctype)) => Resp {
                                status: 200,
                                headers: vec![("content-type".into(), ctype.into())],
                                body: AxBody::from(text),
                            },
                            Ok(Out::Sse(job)) => {
                                let guard = StreamGuard {
                                    app: app.clone(),
                                    method: inc.method.clone(),
                                    route: r.template.to_string(),
                                    t0,
                                };
                                let resp = Resp {
                                    status: 200,
                                    headers: vec![("content-type".into(), "text/event-stream; charset=utf-8".into())],
                                    body: sse_body(c.cfg.sse_max_seconds, job, guard),
                                };
                                return finish(resp); // metrics are recorded when the stream ends
                            }
                            Err(e) => ke_resp(&e),
                        }
                    }
                }
            } else if let Some(r) = partial {
                route_label = Some(r.template);
                http_error(405, "Method Not Allowed")
            } else if let Some(resp) = redirect_slashes(&app, &inc) {
                resp
            } else {
                http_error(404, "Not Found")
            }
        }
    };
    let status = resp.status;
    let route = route_label.unwrap_or("unmatched");
    let m = metric_method(&inc.method);
    c.tel.requests.inc(&[m, route, &status.to_string()]);
    c.tel.latency.observe(&[m, route], t0.elapsed().as_secs_f64());
    if security::MUTATING.contains(&inc.method.as_str()) {
        security::audit(
            &c.cfg.data_dir,
            &inc.method,
            &inc.path,
            status,
            principal.as_deref().unwrap_or("-"),
            total,
        );
    }
    finish(resp)
}

/// Starlette's `redirect_slashes`: the other form of the path matches some route -> 307 there.
fn redirect_slashes(app: &App, inc: &Incoming) -> Option<Resp> {
    if inc.path == "/" {
        return None;
    }
    let other = if inc.path.ends_with('/') {
        inc.path.trim_end_matches('/').to_string()
    } else {
        format!("{}/", inc.path)
    };
    if !app.routes.iter().any(|r| r.re.is_match(&other)) {
        return None;
    }
    let host = inc.headers.iter().find(|(k, _)| k == "host").map(|(_, v)| v.clone());
    let mut url = match host {
        Some(h) => format!("http://{h}{other}"),
        None => other,
    };
    if !inc.query.is_empty() {
        url.push('?');
        url.push_str(&inc.query);
    }
    Some(Resp {
        status: 307,
        headers: vec![("location".into(), quote_url(&url))],
        body: AxBody::empty(),
    })
}

/// The security headers replace whatever the handler set; no CORS headers are ever sent.
fn finish(mut resp: Resp) -> Response<AxBody> {
    resp.headers
        .retain(|(k, _)| !security::SECURITY_HEADERS.iter().any(|(n, _)| n == k));
    for (k, v) in security::SECURITY_HEADERS {
        resp.headers.push((k.into(), v.into()));
    }
    let mut r = Response::new(resp.body);
    *r.status_mut() = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    for (k, v) in resp.headers {
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v)) {
            r.headers_mut().append(n, v);
        }
    }
    r
}

/// uvicorn's `scope["path"]`: `urllib.parse.unquote` of the raw path (UTF-8, invalid sequences replaced).
pub fn decode_path(raw: &str) -> String {
    percent_encoding::percent_decode_str(raw)
        .decode_utf8_lossy()
        .into_owned()
}

/// Handles one HTTP request (the axum fallback; also used in-process by the tests).
pub async fn handle(app: Arc<App>, req: Request<AxBody>) -> Response<AxBody> {
    let (parts, body) = req.into_parts();
    let headers = parts
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_ascii_lowercase(),
                v.as_bytes().iter().map(|&b| char::from(b)).collect(),
            )
        })
        .collect();
    let inc = Incoming {
        method: parts.method.as_str().to_string(),
        path: decode_path(parts.uri.path()),
        query: parts.uri.query().unwrap_or("").to_string(),
        headers,
        body,
    };
    pipeline(app, inc).await
}

/// Connection limits of the served knowledge service, the same as the hub's (`agent_hub::server::ServeLimits`).
#[derive(Clone, Copy, Debug)]
pub struct ServeLimits {
    /// time a client has to send a request's headers; hyper starts this clock whenever it waits for a request head,
    /// so it also closes a keep-alive connection that stays idle this long
    pub header_read_timeout: Duration,
    /// open connections at most; more wait in the kernel's listen backlog until one closes
    pub max_connections: usize,
    /// how long a shutdown waits for open connections (in-flight requests and streams) before closing them
    pub shutdown_grace: Duration,
}

impl Default for ServeLimits {
    fn default() -> Self {
        Self {
            header_read_timeout: Duration::from_secs(30),
            max_connections: 256,
            shutdown_grace: Duration::from_secs(10),
        }
    }
}

/// SIGINT or SIGTERM.
async fn shutdown_signal() {
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        () = term => {},
    }
}

/// Serve until SIGINT/SIGTERM (startup warnings are logged first, like the lifespan).
pub async fn serve(app: Arc<App>, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    serve_with(app, listener, ServeLimits::default(), shutdown_signal()).await
}

/// Serve with explicit limits until `shutdown` completes: HTTP/1.1 (keep-alive) on each accepted connection, every
/// request through [`handle`]. On shutdown no new connection is accepted and open ones get `shutdown_grace` to finish.
pub async fn serve_with(
    app: Arc<App>,
    listener: tokio::net::TcpListener,
    limits: ServeLimits,
    shutdown: impl std::future::Future<Output = ()>,
) -> std::io::Result<()> {
    for w in app.startup_warnings().await {
        tracing::error!("startup: {w}");
        #[allow(clippy::print_stderr)]
        {
            eprintln!("ERROR:agent_knowledge.app:startup: {w}");
        }
    }
    let permits = Arc::new(tokio::sync::Semaphore::new(limits.max_connections.max(1)));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut conns = tokio::task::JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        let permit = tokio::select! {
            () = &mut shutdown => break,
            p = permits.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => break,
            },
        };
        let accepted = tokio::select! {
            () = &mut shutdown => break,
            a = listener.accept() => a,
        };
        let Ok((stream, _peer)) = accepted else {
            // EMFILE and friends: back off instead of spinning
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let (app, mut stop) = (app.clone(), stop_rx.clone());
        let header_timeout = limits.header_read_timeout;
        conns.spawn(async move {
            let _permit = permit;
            let svc = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                let app = app.clone();
                async move { Ok::<_, std::convert::Infallible>(handle(app, req.map(AxBody::new)).await) }
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(header_timeout)
                .keep_alive(true)
                .serve_connection(hyper_util::rt::TokioIo::new(stream), svc);
            tokio::pin!(conn);
            tokio::select! {
                _ = conn.as_mut() => {},
                () = async { let _ = stop.wait_for(|s| *s).await; } => {
                    conn.as_mut().graceful_shutdown();
                    let _ = conn.await;
                }
            }
        });
        while conns.try_join_next().is_some() {} // reap finished connections
    }
    let _ = stop_tx.send(true);
    let drain = async { while conns.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(limits.shutdown_grace, drain).await;
    conns.abort_all();
    Ok(())
}
