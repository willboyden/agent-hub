//! Port of api.py: every route with its dependencies. EVERY route but `/health` authenticates, and each handler then
//! names the scope it needs (admin / write on ns / read on ns). As in FastAPI, a body route's JSON is decoded first
//! (`app.rs`), then the dependencies run (authenticate, then `ADMIN` / `scoped(mode)`), and only then is the body
//! validated, so a wrong token never sees a 422 that reveals the schema.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};

use hub_core::PyValue;
use hub_core::pyd::{ErrItem, Lit, Loc, Ty};
use regex::Regex;
use subtle::ConstantTimeEq;

use crate::config::Config;
use crate::db::{Db, IndexRow, Principal, hash_secret, scopes_value};
use crate::env::{self, Env};
use crate::errors::KnowledgeError;
use crate::pyjson::obj;
use crate::ratelimit::RateLimiter;
use crate::service::Service;
use crate::stores::{NS_RE, VectorStore};
use crate::telemetry::Telemetry;

static CLIENT_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$"));
static INDEX_NAME: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^[a-z0-9][a-z0-9_-]{0,63}$"));

/// `Ctx`: everything a handler needs.
pub struct Ctx {
    pub cfg: Arc<Config>,
    pub db: Arc<Db>,
    pub svc: Arc<Service>,
    pub tel: Arc<Telemetry>,
    pub limiter: RateLimiter,
    pub admin_hash: String,
    pub streams: AtomicI64,
    pub env: Env,
}

/// The request as the handlers see it.
pub struct Req {
    pub method: String,
    /// percent-decoded (`scope["path"]`)
    pub path: String,
    /// lowercase names, latin-1 values, in arrival order
    pub headers: Vec<(String, String)>,
    pub params: Vec<(String, String)>,
    pub body: Body,
    /// `request.scope["state"]["principal_id"]`, for the audit log
    pub principal_id: Option<String>,
}

impl Req {
    /// Starlette `request.headers.get(name)`: the first value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn param(&self, name: &str) -> &str {
        self.params
            .iter()
            .find(|(k, _)| k == name)
            .map_or("", |(_, v)| v.as_str())
    }
}

/// The decoded request body, as FastAPI sees it.
#[derive(Debug, Clone)]
pub enum Body {
    /// no bytes (or a route without a body)
    Missing,
    Json(PyValue),
    /// bytes that are not parsed as JSON (no or a non-JSON content type)
    Raw,
}

/// A handler's success.
pub enum Out {
    /// `dict` result through FastAPI's pydantic serialisation
    Json(u16, PyValue),
    /// `Response(status_code=204)`
    NoContent,
    /// `Response(text, media_type=...)`
    Text(String, &'static str),
    /// a job's SSE stream
    Sse(Arc<crate::service::Job>),
}

pub type HResult = Result<Out, KnowledgeError>;

// ------------------------------------------------------------------------------------------------ models

pub enum Ft {
    Py(Ty),
    Opt(Box<Ft>),
    List(Box<Ft>, Option<usize>),
    Model(fn() -> &'static ModelDef),
}

pub struct FieldDef {
    pub name: &'static str,
    pub ty: Ft,
    pub default: Option<fn() -> PyValue>,
}

pub struct ModelDef {
    pub fields: Vec<FieldDef>,
}

fn req_f(name: &'static str, ty: Ft) -> FieldDef {
    FieldDef {
        name,
        ty,
        default: None,
    }
}

fn opt_f(name: &'static str, ty: Ft, default: fn() -> PyValue) -> FieldDef {
    FieldDef {
        name,
        ty,
        default: Some(default),
    }
}

fn s() -> Ft {
    Ft::Py(Ty::str())
}

fn s_max(max: usize) -> Ft {
    Ft::Py(Ty::Str {
        min: None,
        max: Some(max),
        pattern: None,
    })
}

fn opt(t: Ft) -> Ft {
    Ft::Opt(Box::new(t))
}

fn int_range(ge: Option<i64>, le: Option<i64>) -> Ft {
    Ft::Py(Ty::Int { ge, le })
}

fn none() -> PyValue {
    PyValue::None
}

static NS_CREATE: LazyLock<ModelDef> = LazyLock::new(|| ModelDef {
    fields: vec![
        req_f("name", s()),
        req_f("backend", Ft::Py(Ty::Lit(&[Lit::S("qdrant"), Lit::S("lancedb")]))),
        opt_f(
            "embedding_model",
            opt(Ft::Py(Ty::Str {
                min: None,
                max: Some(128),
                pattern: Some("^[A-Za-z0-9._:/-]+$"),
            })),
            none,
        ),
        req_f("dim", Ft::Py(Ty::int())),
        opt_f("description", s_max(500), || PyValue::str("")),
    ],
});

static NS_UPDATE: LazyLock<ModelDef> = LazyLock::new(|| ModelDef {
    fields: vec![
        opt_f("description", opt(s_max(500)), none),
        opt_f("backend", opt(s()), none),
        opt_f("embedding_model", opt(s()), none),
        opt_f("dim", opt(Ft::Py(Ty::int())), none),
    ],
});

static DOC_IN: LazyLock<ModelDef> = LazyLock::new(|| ModelDef {
    fields: vec![
        opt_f("id", opt(s()), none),
        req_f("text", s()),
        opt_f("metadata", Ft::Py(Ty::dict(Ty::str(), Ty::Any)), || {
            PyValue::Dict(Vec::new())
        }),
    ],
});

static INGEST_REQ: LazyLock<ModelDef> = LazyLock::new(|| ModelDef {
    fields: vec![
        opt_f("documents", opt(Ft::List(Box::new(Ft::Model(|| &DOC_IN)), None)), none),
        opt_f("path", opt(s()), none),
        opt_f("glob", s(), || PyValue::str("**/*")),
        opt_f("chunk_size", int_range(Some(50), Some(8000)), || PyValue::Int(1000)),
        opt_f("chunk_overlap", int_range(Some(0), Some(4000)), || PyValue::Int(200)),
    ],
});

static QUERY_REQ: LazyLock<ModelDef> = LazyLock::new(|| ModelDef {
    fields: vec![
        opt_f("text", opt(s()), none),
        opt_f("vector", opt(Ft::List(Box::new(Ft::Py(Ty::Float)), Some(8192))), none),
        opt_f("k", Ft::Py(Ty::int()), || PyValue::Int(5)),
        opt_f("filter", opt(Ft::Py(Ty::dict(Ty::str(), Ty::Any))), none),
        opt_f("min_score", opt(Ft::Py(Ty::Float)), none),
        opt_f("embedding_model", opt(s()), none),
    ],
});

static SCOPE_IN: LazyLock<ModelDef> = LazyLock::new(|| ModelDef {
    fields: vec![
        req_f("ns", s()),
        opt_f("mode", Ft::Py(Ty::Lit(&[Lit::S("read"), Lit::S("write")])), || {
            PyValue::str("read")
        }),
    ],
});

static TOKEN_CREATE: LazyLock<ModelDef> = LazyLock::new(|| ModelDef {
    fields: vec![
        req_f("client", s()),
        req_f("scopes", Ft::List(Box::new(Ft::Model(|| &SCOPE_IN)), Some(64))),
        opt_f("rate_per_min", opt(int_range(Some(1), Some(6000))), none),
        opt_f("daily_queries", opt(int_range(Some(0), Some(1_000_000))), none),
        opt_f("daily_writes", opt(int_range(Some(0), Some(10_000_000))), none),
    ],
});

static INDEX_IN: LazyLock<ModelDef> = LazyLock::new(|| ModelDef {
    fields: vec![
        req_f("name", s()),
        req_f("kind", Ft::Py(Ty::Lit(&[Lit::S("graphify"), Lit::S("serena")]))),
        req_f("path", s()),
        opt_f("project", s_max(128), || PyValue::str("")),
    ],
});

fn push(errs: &mut Vec<ErrItem>, loc: &[Loc], kind: &str, msg: impl Into<String>, input: &PyValue) {
    errs.push(ErrItem {
        loc: loc.to_vec(),
        msg: msg.into(),
        kind: kind.into(),
        input: input.clone(),
    });
}

/// pydantic's lax `int`/`float` for an int beyond i64 (`PyValue::BigInt`, which `hub_core::pyd` does not take): an
/// unconstrained int accepts it (the service then answers, e.g. `invalid_k`), `ge`/`le` refuse it by its sign, and a
/// float takes it unless it overflows (`Input should be a valid number`).
fn big_int(ty: &Ty, digits: &str, v: &PyValue, loc: &[Loc], errs: &mut Vec<ErrItem>) -> Option<PyValue> {
    let negative = digits.starts_with('-');
    match ty {
        Ty::Int { ge, le } => {
            if let (Some(g), true) = (ge, negative) {
                push(
                    errs,
                    loc,
                    "greater_than_equal",
                    format!("Input should be greater than or equal to {g}"),
                    v,
                );
                return Some(PyValue::None);
            }
            if let (Some(l), false) = (le, negative) {
                push(
                    errs,
                    loc,
                    "less_than_equal",
                    format!("Input should be less than or equal to {l}"),
                    v,
                );
                return Some(PyValue::None);
            }
            Some(v.clone())
        }
        Ty::Float => match digits.parse::<f64>() {
            Ok(f) if f.is_finite() => Some(PyValue::Float(f)),
            _ => {
                push(errs, loc, "float_type", "Input should be a valid number", v);
                Some(PyValue::None)
            }
        },
        _ => None,
    }
}

fn validate_ft(ft: &Ft, v: &PyValue, loc: &mut Vec<Loc>, errs: &mut Vec<ErrItem>) -> PyValue {
    match ft {
        Ft::Py(ty) => {
            if let PyValue::BigInt(digits) = v
                && let Some(out) = big_int(ty, digits, v, loc, errs)
            {
                return out;
            }
            hub_core::pyd::validate_ty(ty, v, loc, errs)
        }
        Ft::Opt(inner) => {
            if v.is_none() {
                PyValue::None
            } else {
                validate_ft(inner, v, loc, errs)
            }
        }
        Ft::List(inner, max) => {
            let Some(items) = v.as_list() else {
                push(errs, loc, "list_type", "Input should be a valid list", v);
                return PyValue::None;
            };
            let mut mine = Vec::new();
            let mut out = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                loc.push(Loc::Index(i));
                out.push(validate_ft(inner, it, loc, &mut mine));
                loc.pop();
                if let Some(m) = max
                    && i + 1 > *m
                {
                    // pydantic-core's MaxLengthCheck: the too_long error replaces everything collected so far
                    push(
                        errs,
                        loc,
                        "too_long",
                        format!(
                            "List should have at most {m} item{} after validation, not {}",
                            if *m == 1 { "" } else { "s" },
                            items.len()
                        ),
                        v,
                    );
                    return PyValue::None;
                }
            }
            errs.extend(mine);
            PyValue::List(out)
        }
        Ft::Model(get) => validate_model(get(), v, loc, errs),
    }
}

/// A `Strict` model (`extra="forbid"`) validated with `from_attributes=True`, as FastAPI validates bodies.
fn validate_model(model: &ModelDef, v: &PyValue, loc: &mut Vec<Loc>, errs: &mut Vec<ErrItem>) -> PyValue {
    let Some(d) = v.as_dict() else {
        push(
            errs,
            loc,
            "model_attributes_type",
            "Input should be a valid dictionary or object to extract fields from",
            v,
        );
        return PyValue::None;
    };
    let mut out = Vec::new();
    for f in &model.fields {
        loc.push(Loc::Key(f.name.into()));
        match d.iter().find(|(k, _)| k.as_str() == Some(f.name)) {
            Some((_, val)) => out.push((PyValue::str(f.name), validate_ft(&f.ty, val, loc, errs))),
            None => match f.default {
                Some(def) => out.push((PyValue::str(f.name), def())),
                None => push(errs, loc, "missing", "Field required", v),
            },
        }
        loc.pop();
    }
    for (k, val) in d {
        if k.as_str().is_some_and(|s| model.fields.iter().any(|f| f.name == s)) {
            continue;
        }
        loc.push(Loc::Key(k.py_str()));
        push(errs, loc, "extra_forbidden", "Extra inputs are not permitted", val);
        loc.pop();
    }
    PyValue::Dict(out)
}

/// The 422 detail: `"; ".join(f"{loc}: {msg}")[:500]`.
pub fn detail_of(errs: &[ErrItem]) -> String {
    let text = errs
        .iter()
        .map(|e| format!("{}: {}", hub_core::pyd::loc_str(&e.loc), e.msg))
        .collect::<Vec<_>>()
        .join("; ");
    text.chars().take(500).collect()
}

pub fn validation_error(detail: String) -> KnowledgeError {
    KnowledgeError::new(422, "validation_error", detail)
}

/// FastAPI's validation of a required, non-embedded model body (`body: Model`).
pub fn body_model(body: &Body, model: &ModelDef) -> Result<PyValue, KnowledgeError> {
    let mut errs = Vec::new();
    let mut loc = vec![Loc::Key("body".into())];
    let out = match body {
        // a JSON `null` is no body at all to FastAPI
        Body::Missing | Body::Json(PyValue::None) => {
            push(&mut errs, &loc, "missing", "Field required", &PyValue::None);
            PyValue::None
        }
        Body::Json(v) => validate_model(model, v, &mut loc, &mut errs),
        Body::Raw => {
            push(
                &mut errs,
                &loc,
                "model_attributes_type",
                "Input should be a valid dictionary or object to extract fields from",
                &PyValue::None,
            );
            PyValue::None
        }
    };
    if errs.is_empty() {
        Ok(out)
    } else {
        Err(validation_error(detail_of(&errs)))
    }
}

/// Which body model a route takes (for tests and the golden comparison).
pub fn model_for(endpoint: &str) -> Option<&'static ModelDef> {
    Some(match endpoint {
        "create_ns" => &NS_CREATE,
        "put_ns" => &NS_UPDATE,
        "ingest" => &INGEST_REQ,
        "query" => &QUERY_REQ,
        "create_token" => &TOKEN_CREATE,
        "add_idx" => &INDEX_IN,
        _ => return None,
    })
}

fn field<'a>(v: &'a PyValue, name: &str) -> &'a PyValue {
    static NONE: PyValue = PyValue::None;
    v.get(name).unwrap_or(&NONE)
}

fn opt_str(v: &PyValue, name: &str) -> Option<String> {
    field(v, name).as_str().map(str::to_string)
}

/// An int field's value; an int beyond i64 saturates (it can only fail the service's range checks, as in Python).
fn int_of(v: &PyValue, name: &str) -> Option<i64> {
    match field(v, name) {
        PyValue::Int(i) => Some(*i),
        PyValue::BigInt(d) => Some(if d.starts_with('-') { i64::MIN } else { i64::MAX }),
        _ => None,
    }
}

fn float_of(v: &PyValue, name: &str) -> Option<f64> {
    match field(v, name) {
        PyValue::Float(f) => Some(*f),
        _ => None,
    }
}

// ------------------------------------------------------------------------------------------------ dependencies

fn internal<T>(_: T) -> KnowledgeError {
    KnowledgeError::internal()
}

/// `authenticate(request)`: bearer admin key (constant-time hash compare) or a token from the DB; then the rate limit.
pub fn authenticate(c: &Ctx, req: &mut Req) -> Result<Principal, KnowledgeError> {
    let auth = req.header("authorization").unwrap_or("").to_string();
    let (scheme, tok) = auth.split_once(' ').unwrap_or((auth.as_str(), ""));
    let tok = hub_core::py::text::strip(tok);
    let mut p = None;
    if scheme.to_lowercase() == "bearer" && !tok.is_empty() {
        if bool::from(hash_secret(tok).as_bytes().ct_eq(c.admin_hash.as_bytes())) {
            p = Some(Principal::admin());
        } else {
            p = c.db.token_by_secret(tok).map_err(internal)?;
        }
    }
    let Some(p) = p else {
        c.tel.denied.inc(&["unauthenticated"]);
        return Err(KnowledgeError::new(
            401,
            "unauthorized",
            "a valid bearer token is required",
        ));
    };
    req.principal_id = Some(p.id.clone());
    let limit = if p.admin || p.rate_per_min == 0 {
        c.cfg.default_rate_per_min
    } else {
        p.rate_per_min
    };
    if !c.limiter.allow(&p.id, limit) {
        c.tel.denied.inc(&["rate_limited"]);
        return Err(KnowledgeError::new(429, "rate_limited", "too many requests; slow down"));
    }
    Ok(p)
}

pub fn need_admin(p: &Principal) -> Result<(), KnowledgeError> {
    if p.admin {
        Ok(())
    } else {
        Err(KnowledgeError::new(
            403,
            "forbidden",
            "this operation needs the admin key",
        ))
    }
}

/// `need_scope(c, p, ns, mode)`: unknown namespace and no-scope look identical to a non-admin.
pub fn need_scope(c: &Ctx, p: &Principal, ns: &str, mode: &str) -> Result<(), KnowledgeError> {
    if !NS_RE.is_match(ns) || !p.can(ns, mode) {
        c.tel.denied.inc(&["scope"]);
        return Err(if p.can(ns, "read") {
            KnowledgeError::new(403, "forbidden", format!("token lacks {mode} scope on this namespace"))
        } else {
            KnowledgeError::new(404, "namespace_not_found", "no such namespace")
        });
    }
    Ok(())
}

/// `spend(c, p, field, n)`: the daily quota of a client token.
pub fn spend(c: &Ctx, p: &Principal, field: &str, n: i64) -> Result<(), KnowledgeError> {
    if p.admin {
        return Ok(());
    }
    let limit = if field == "queries" {
        p.daily_queries
    } else {
        p.daily_writes
    };
    if !c.db.bump(&p.id, field, n, limit).map_err(internal)? {
        c.tel.denied.inc(&["quota"]);
        return Err(KnowledgeError::new(
            429,
            "quota_exceeded",
            format!("daily {field} quota exhausted for this token"),
        ));
    }
    Ok(())
}

/// Route-level dependency.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dep {
    None,
    Admin,
    Scoped(&'static str),
}

/// The dependencies of a route, in FastAPI's order: authenticate (router), then the route's own.
pub fn run_deps(c: &Ctx, req: &mut Req, dep: Dep) -> Result<Principal, KnowledgeError> {
    let p = authenticate(c, req)?;
    match dep {
        Dep::Admin => need_admin(&p)?,
        Dep::Scoped(mode) => {
            let ns = req.param("ns").to_string();
            need_scope(c, &p, &ns, mode)?;
        }
        Dep::None => {}
    }
    Ok(p)
}

fn page(items: Vec<PyValue>) -> PyValue {
    obj([("items", PyValue::List(items)), ("next_cursor", PyValue::None)])
}

// ------------------------------------------------------------------------------------------------ handlers

pub async fn list_ns(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::None)?;
    let names = c.db.ns_names().map_err(internal)?;
    let mut items = Vec::new();
    for n in names {
        if p.can(&n, "read") {
            items.push(c.svc.ns_view(&n).await?);
        }
    }
    Ok(Out::Json(200, page(items)))
}

pub async fn create_ns(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Admin)?;
    let body = body_model(&req.body, &NS_CREATE)?;
    need_admin(&p)?;
    let v = c
        .svc
        .create_namespace(
            &opt_str(&body, "name").unwrap_or_default(),
            &opt_str(&body, "backend").unwrap_or_default(),
            opt_str(&body, "embedding_model").as_deref(),
            int_of(&body, "dim").unwrap_or(0),
            &opt_str(&body, "description").unwrap_or_default(),
        )
        .await?;
    Ok(Out::Json(201, v))
}

pub async fn get_ns(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Scoped("read"))?;
    let ns = req.param("ns").to_string();
    need_scope(c, &p, &ns, "read")?;
    Ok(Out::Json(200, c.svc.ns_view(&ns).await?))
}

pub async fn put_ns(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Admin)?;
    let body = body_model(&req.body, &NS_UPDATE)?;
    need_admin(&p)?;
    let ns = req.param("ns").to_string();
    let row = c.svc.ns_row(&ns)?;
    for f in ["backend", "embedding_model", "dim"] {
        let v = field(&body, f);
        let same = match (f, v) {
            (_, PyValue::None) => true,
            ("backend", PyValue::Str(s)) => *s == row.backend,
            ("embedding_model", PyValue::Str(s)) => *s == row.embedding_model,
            ("dim", PyValue::Int(i)) => *i == row.dim,
            _ => false,
        };
        if !same {
            return Err(KnowledgeError::new(
                409,
                "immutable_field",
                format!("{f} is fixed at creation; create a new namespace and re-ingest"),
            ));
        }
    }
    if let Some(d) = opt_str(&body, "description") {
        c.db.ns_set_description(&ns, &d).map_err(internal)?;
    }
    Ok(Out::Json(200, c.svc.ns_view(&ns).await?))
}

pub async fn del_ns(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Admin)?;
    need_admin(&p)?;
    c.svc.drop_namespace(req.param("ns")).await?;
    Ok(Out::NoContent)
}

pub async fn ingest(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Scoped("write"))?;
    let body = body_model(&req.body, &INGEST_REQ)?;
    let ns = req.param("ns").to_string();
    need_scope(c, &p, &ns, "write")?;
    c.svc.ns_row(&ns)?;
    let docs = field(&body, "documents");
    let path = opt_str(&body, "path");
    if docs.is_none() == path.is_none() {
        return Err(KnowledgeError::new(
            422,
            "invalid_ingest",
            "send exactly one of documents or path",
        ));
    }
    let glob = opt_str(&body, "glob").unwrap_or_default();
    let size = int_of(&body, "chunk_size").unwrap_or(1000);
    let overlap = int_of(&body, "chunk_overlap").unwrap_or(200);
    let (_job, view) = if let Some(path) = path {
        need_admin(&p)?; // reading the host filesystem is an operator action, never a client-token one
        c.svc.plan_path(&path, &glob)?; // fail fast on traversal / bad glob
        c.svc.start_job(&ns, None, Some(path), &glob, size, overlap, 0)?
    } else {
        let list = docs.as_list().unwrap_or(&[]);
        let prepared = c.svc.prepare_inline(list, size, overlap)?;
        let n: i64 = prepared
            .iter()
            .map(|d| i64::try_from(d.chunks.len()).unwrap_or(0))
            .sum();
        c.svc.reserve(&ns, n).await?; // atomic cap check + reservation, released by the job
        if let Err(e) = spend(c, &p, "writes", n) {
            c.svc.release(&ns, n);
            return Err(e);
        }
        c.svc.start_job(&ns, Some(prepared), None, "", size, overlap, n)?
    };
    Ok(Out::Json(202, obj([("job", view)])))
}

pub async fn query(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Scoped("read"))?;
    let body = body_model(&req.body, &QUERY_REQ)?;
    let ns = req.param("ns").to_string();
    need_scope(c, &p, &ns, "read")?;
    spend(c, &p, "queries", 1)?;
    let vector = field(&body, "vector").as_list().map(|l| {
        l.iter()
            .map(|x| match x {
                PyValue::Float(f) => *f,
                _ => 0.0,
            })
            .collect::<Vec<f64>>()
    });
    let filter = match field(&body, "filter") {
        PyValue::None => None,
        f => Some(f.clone()),
    };
    let hits = c
        .svc
        .query(
            &ns,
            opt_str(&body, "text").as_deref(),
            vector,
            int_of(&body, "k").unwrap_or(5),
            filter.as_ref(),
            float_of(&body, "min_score"),
            opt_str(&body, "embedding_model").as_deref(),
        )
        .await?;
    c.tel.queries.inc(&[&ns]);
    let hits: Vec<PyValue> = hits
        .into_iter()
        .map(|h| {
            obj([
                ("id", h.id),
                ("score", PyValue::Float(h.score)),
                ("text", h.text),
                ("metadata", h.meta),
            ])
        })
        .collect();
    Ok(Out::Json(200, obj([("hits", PyValue::List(hits))])))
}

pub async fn del_doc(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Scoped("write"))?;
    let ns = req.param("ns").to_string();
    need_scope(c, &p, &ns, "write")?;
    let n = c.svc.delete_document(&ns, req.param("doc_id")).await?;
    Ok(Out::Json(200, obj([("deleted_chunks", PyValue::Int(n))])))
}

fn get_job(c: &Ctx, p: &Principal, jid: &str) -> Result<Arc<crate::service::Job>, KnowledgeError> {
    match c.svc.job(jid) {
        Some(j) if p.can(&j.ns, "write") => Ok(j),
        _ => Err(KnowledgeError::new(404, "job_not_found", "no such job")),
    }
}

pub async fn list_jobs(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::None)?;
    let items: Vec<PyValue> = c
        .svc
        .jobs()
        .iter()
        .rev()
        .filter(|j| p.can(&j.ns, "write"))
        .map(|j| j.view())
        .collect();
    Ok(Out::Json(200, page(items)))
}

pub async fn job(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::None)?;
    Ok(Out::Json(200, get_job(c, &p, req.param("jid"))?.view()))
}

pub async fn job_stream(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::None)?;
    let j = get_job(c, &p, req.param("jid"))?;
    if c.streams.load(Ordering::SeqCst) >= c.cfg.sse_max_streams {
        return Err(KnowledgeError::new(
            429,
            "too_many_streams",
            "too many open event streams",
        ));
    }
    c.streams.fetch_add(1, Ordering::SeqCst);
    Ok(Out::Sse(j))
}

pub async fn list_tokens(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Admin)?;
    need_admin(&p)?;
    let mut items = Vec::new();
    for (id, t) in c.db.list_tokens().map_err(internal)? {
        let (q, w) = c.db.usage(&id).map_err(internal)?;
        let PyValue::Dict(mut d) = t else { continue };
        d.push((
            PyValue::str("usage_today"),
            obj([("queries", PyValue::Int(q)), ("writes", PyValue::Int(w))]),
        ));
        items.push(PyValue::Dict(d));
    }
    Ok(Out::Json(200, page(items)))
}

pub async fn create_token(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Admin)?;
    let body = body_model(&req.body, &TOKEN_CREATE)?;
    need_admin(&p)?;
    let client = opt_str(&body, "client").unwrap_or_default();
    if !CLIENT_RE.is_match(&client) {
        return Err(KnowledgeError::new(
            422,
            "invalid_client",
            "client must match [A-Za-z0-9][A-Za-z0-9._-]{0,63}",
        ));
    }
    let mut scopes: Vec<(String, String)> = Vec::new();
    for s in field(&body, "scopes").as_list().unwrap_or(&[]) {
        let ns = opt_str(s, "ns").unwrap_or_default();
        let mode = opt_str(s, "mode").unwrap_or_else(|| "read".into());
        c.svc.ns_row(&ns)?; // must exist
        match scopes.iter_mut().find(|(n, _)| *n == ns) {
            Some((_, m)) if m == "write" => {}
            Some((_, m)) => *m = mode,
            None => scopes.push((ns, mode)),
        }
    }
    scopes.sort();
    let cfg = &c.cfg;
    let (tid, secret) =
        c.db.add_token(
            &client,
            &scopes,
            int_of(&body, "rate_per_min")
                .filter(|r| *r != 0)
                .unwrap_or(cfg.default_rate_per_min),
            int_of(&body, "daily_queries").unwrap_or(cfg.default_daily_queries),
            int_of(&body, "daily_writes").unwrap_or(cfg.default_daily_writes),
        )
        .map_err(internal)?;
    // The secret is shown exactly once; only its hash is stored.
    Ok(Out::Json(
        201,
        obj([
            ("id", PyValue::Str(tid)),
            ("client", PyValue::Str(client)),
            ("scopes", scopes_value(&scopes)),
            ("token", PyValue::Str(secret)),
        ]),
    ))
}

pub async fn delete_token(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Admin)?;
    need_admin(&p)?;
    if !c.db.drop_token(req.param("tid")).map_err(internal)? {
        return Err(KnowledgeError::new(404, "token_not_found", "no such token"));
    }
    Ok(Out::NoContent)
}

/// `st_mtime` as CPython computes it (`sec + nsec * 1e-9`).
fn mtime(m: &std::fs::Metadata) -> f64 {
    use std::os::unix::fs::MetadataExt;
    #[allow(clippy::cast_precision_loss)]
    let (s, ns) = (m.mtime() as f64, m.mtime_nsec() as f64);
    s + ns * 1e-9
}

fn idx_view(row: &IndexRow, admin: bool) -> PyValue {
    let path = Path::new(&row.path);
    let mut out = vec![
        (PyValue::str("name"), PyValue::str(row.name.as_str())),
        (PyValue::str("kind"), PyValue::str(row.kind.as_str())),
        (PyValue::str("project"), PyValue::str(row.project.as_str())),
        (PyValue::str("exists"), PyValue::Bool(path.is_dir())),
    ];
    if admin {
        out.push((PyValue::str("path"), PyValue::str(row.path.as_str())));
        out.push((
            PyValue::str("updated"),
            std::fs::metadata(path).map_or(PyValue::None, |m| PyValue::Float(mtime(&m))),
        ));
    }
    PyValue::Dict(out)
}

pub async fn list_idx(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Admin)?;
    let rows = c.db.indexes().map_err(internal)?;
    Ok(Out::Json(
        200,
        page(rows.iter().map(|r| idx_view(r, p.admin)).collect()),
    ))
}

/// `Path(p).resolve(strict=True)`; `Err(None)` = OSError (path_not_found), `Err(Some(()))` = ValueError (a 500).
fn resolve_strict(p: &str) -> Result<PathBuf, Option<()>> {
    if p.contains('\0') {
        return Err(Some(()));
    }
    std::fs::canonicalize(if p.is_empty() { "." } else { p }).map_err(|_| None)
}

pub async fn add_idx(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Admin)?;
    let body = body_model(&req.body, &INDEX_IN)?;
    need_admin(&p)?;
    let name = opt_str(&body, "name").unwrap_or_default();
    if !INDEX_NAME.is_match(&name) {
        return Err(KnowledgeError::new(
            422,
            "invalid_name",
            "name must match [a-z0-9][a-z0-9_-]{0,63}",
        ));
    }
    let path = opt_str(&body, "path").unwrap_or_default();
    let real = match resolve_strict(&path) {
        Ok(r) => r,
        Err(Some(())) => return Err(KnowledgeError::internal()),
        Err(None) => return Err(KnowledgeError::new(422, "path_not_found", "path does not exist")),
    };
    let under = c.cfg.index_roots.iter().any(|r| {
        let rr = hub_core::py::os::realpath(r);
        real == rr || real.starts_with(&rr)
    });
    if !path.starts_with('/') || !real.is_dir() || !under {
        return Err(KnowledgeError::new(
            403,
            "path_not_allowed",
            "index path must be a directory under a configured index root",
        ));
    }
    if c.db.index(&name).map_err(internal)?.is_some() {
        return Err(KnowledgeError::new(409, "index_exists", "index already registered"));
    }
    let row = IndexRow {
        name: name.clone(),
        kind: opt_str(&body, "kind").unwrap_or_default(),
        path: real.to_string_lossy().into_owned(),
        project: opt_str(&body, "project").unwrap_or_default(),
    };
    c.db.index_insert(&row).map_err(internal)?;
    let stored =
        c.db.index(&name)
            .map_err(internal)?
            .ok_or_else(KnowledgeError::internal)?;
    Ok(Out::Json(201, idx_view(&stored, true)))
}

pub async fn del_idx(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Admin)?;
    need_admin(&p)?;
    if c.db.index_delete(req.param("name")).map_err(internal)? == 0 {
        return Err(KnowledgeError::new(404, "index_not_found", "no such index"));
    }
    Ok(Out::NoContent)
}

/// `key_env_warnings(name)`.
pub fn key_env_warnings(name: &str) -> Vec<String> {
    let n = name.to_uppercase();
    if n == "LITELLM_API_KEY" || n == "LITELLM_MASTER_KEY" || n.contains("MASTER") {
        return vec![format!(
            "embedder key env {name} looks like the router MASTER key; use an embeddings-only virtual key in \
             KNOWLEDGE_EMBED_API_KEY"
        )];
    }
    Vec::new()
}

/// `startup_warnings(stores, cfg)`: names only in messages, never values.
pub async fn startup_warnings(stores: &[(String, Arc<dyn VectorStore>)], cfg: &Config, env: &Env) -> Vec<String> {
    let name = &cfg.router_api_key_env;
    let mut out = key_env_warnings(name);
    // Value check: is the embed key the router master key in disguise?
    let mine = env::get(env, name);
    for other in ["LITELLM_MASTER_KEY", "LITELLM_API_KEY"] {
        let theirs = env::get(env, other);
        if !mine.is_empty()
            && !theirs.is_empty()
            && other != name
            && bool::from(mine.as_bytes().ct_eq(theirs.as_bytes()))
        {
            out.push(format!(
                "SECURITY: {name} holds the SAME value as {other} (the router master key). Issue an embeddings-only \
                 virtual key and use that instead."
            ));
        }
    }
    if let Some((_, q)) = stores.iter().find(|(n, _)| n == "qdrant")
        && let Some(why) = q.refusal().await
        && !why.is_empty()
    {
        out.push(format!("qdrant backend REFUSED: {why}"));
    }
    out
}

/// `shutil.disk_usage(path).free`: `statvfs(path)`'s f_bavail * f_frsize (an OSError is an unexpected error).
pub fn disk_free(path: &Path) -> Result<i64, KnowledgeError> {
    let st = nix::sys::statvfs::statvfs(path).map_err(|_| KnowledgeError::internal())?;
    let free = u128::from(st.blocks_available()) * u128::from(st.fragment_size());
    Ok(i64::try_from(free).unwrap_or(i64::MAX))
}

pub async fn backends(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Admin)?;
    need_admin(&p)?;
    let mut health = Vec::new();
    for (n, s) in &c.svc.stores {
        health.push((PyValue::str(n.as_str()), s.health().await));
    }
    let mut nss = Vec::new();
    for n in c.db.ns_names().map_err(internal)? {
        let v = c.svc.ns_view(&n).await?;
        let keep = ["name", "backend", "embedding_model", "dim", "count", "disk_bytes"];
        nss.push(PyValue::Dict(
            keep.iter()
                .map(|k| (PyValue::str(*k), v.get(k).cloned().unwrap_or(PyValue::None)))
                .collect(),
        ));
    }
    let free = disk_free(&c.cfg.data_dir)?;
    let warnings = startup_warnings(&c.svc.stores, &c.cfg, &c.env).await;
    let key_set = !env::get(&c.env, &c.cfg.router_api_key_env).is_empty();
    Ok(Out::Json(
        200,
        obj([
            (
                "warnings",
                PyValue::List(warnings.into_iter().map(PyValue::Str).collect()),
            ),
            ("backends", PyValue::Dict(health)),
            ("namespaces", PyValue::List(nss)),
            ("data_dir_free_bytes", PyValue::Int(free)),
            (
                "embedder",
                obj([
                    ("router_url", PyValue::str(c.cfg.router_url.as_str())),
                    ("api_key_env", PyValue::str(c.cfg.router_api_key_env.as_str())),
                    ("api_key", PyValue::str(if key_set { "[set]" } else { "[unset]" })),
                ]),
            ),
        ]),
    ))
}

pub async fn metrics(c: &Ctx, req: &mut Req) -> HResult {
    let p = run_deps(c, req, Dep::Admin)?;
    need_admin(&p)?;
    Ok(Out::Text(c.tel.render(), "text/plain; version=0.0.4; charset=utf-8"))
}

pub const VERSION: &str = "0.3.0";

pub async fn health(_c: &Ctx, _req: &mut Req) -> HResult {
    Ok(Out::Json(
        200,
        obj([("ok", PyValue::Bool(true)), ("version", PyValue::str(VERSION))]),
    ))
}

#[cfg(test)]
mod disk_free_tests {
    use super::disk_free;

    #[test]
    fn disk_free_is_statvfs_free_space_and_refuses_a_missing_path() -> Result<(), Box<dyn std::error::Error>> {
        let d = tempfile::tempdir()?;
        assert!(disk_free(d.path()).map_err(|_| "disk_free failed")? > 0);
        assert!(disk_free(&d.path().join("missing")).is_err()); // shutil.disk_usage raises OSError
        Ok(())
    }
}
