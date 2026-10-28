//! Port of config.py: an optional YAML file (`--config` or AGENT_KNOWLEDGE_CONFIG, `yaml.safe_load`), every key
//! defaulted and validated like the pydantic model (`extra="forbid"`, errors in field order, `str(ValidationError)` as
//! the startup error). Secrets are referenced by env var NAME only; KNOWLEDGE_ROUTER_URL is the default router_url.
//! Paths are normalised like `pathlib.Path` (`a//b/./` -> `a/b`); `data_dir` is `expanduser()`ed, nothing else is.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use hub_core::PyValue;
use hub_core::pyd::{self, ErrItem, Loc, Ty, ValidationError};
use regex::Regex;

use crate::env::{self, Env};

static ENV_NAME: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^[A-Za-z_][A-Za-z0-9_]{0,63}$"));
static URL: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^https?://[A-Za-z0-9._-]+(:\d+)?(/[A-Za-z0-9._/-]*)?$"));

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub host: String,
    pub port: i64,
    pub allowed_hosts: Vec<String>,
    pub data_dir: PathBuf,
    /// defaults to `<data_dir>/lance`; point at an SSD path
    pub lance_dir: Option<PathBuf>,
    /// e.g. http://127.0.0.1:6333 ; None disables the qdrant backend
    pub qdrant_url: Option<String>,
    /// env var NAME; Qdrant must enforce the same key
    pub qdrant_api_key_env: Option<String>,
    pub qdrant_vectors_on_disk: bool,
    pub router_url: String,
    /// embeddings-only router virtual key, NEVER the master key
    pub router_api_key_env: String,
    pub default_embedding_model: String,
    /// allowlist for `path` ingest (admin only)
    pub ingest_roots: Vec<PathBuf>,
    /// allowlist for the shared-index registry
    pub index_roots: Vec<PathBuf>,
    pub body_cap_bytes: i64,
    pub max_docs_per_request: i64,
    pub max_doc_chars: i64,
    pub max_files_per_job: i64,
    pub max_job_bytes: i64,
    pub max_chunks_per_namespace: i64,
    pub max_k: i64,
    pub max_filter_nodes: i64,
    /// filtered scans over larger tables are refused (422); doc_id filters are exempt
    pub lance_max_scan_rows: i64,
    /// per store call; on expiry the caller gets 429, not a stall
    pub store_timeout_s: f64,
    pub lance_lock_wait_s: f64,
    pub lance_workers: i64,
    pub max_query_chars: i64,
    pub max_jobs_kept: i64,
    pub embed_batch: i64,
    pub default_rate_per_min: i64,
    pub default_daily_queries: i64,
    /// chunks upserted per day per write token
    pub default_daily_writes: i64,
    pub sse_max_seconds: i64,
    pub sse_max_streams: i64,
}

impl Config {
    /// The model defaults; `home` is what `Path("~").expanduser()` gives at import time.
    pub fn defaults(home: &str) -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8795,
            allowed_hosts: Vec::new(),
            data_dir: py_path(&hub_core::py::os::expanduser("~/.local/share/agent-knowledge", home)),
            lance_dir: None,
            qdrant_url: None,
            qdrant_api_key_env: Some("KNOWLEDGE_QDRANT_API_KEY".into()),
            qdrant_vectors_on_disk: true,
            router_url: "http://127.0.0.1:4000".into(),
            router_api_key_env: "KNOWLEDGE_EMBED_API_KEY".into(),
            default_embedding_model: "text-embedding-3-small".into(),
            ingest_roots: Vec::new(),
            index_roots: Vec::new(),
            body_cap_bytes: 4 * 1024 * 1024,
            max_docs_per_request: 200,
            max_doc_chars: 500_000,
            max_files_per_job: 2000,
            max_job_bytes: 200 * 1024 * 1024,
            max_chunks_per_namespace: 2_000_000,
            max_k: 100,
            max_filter_nodes: 16,
            lance_max_scan_rows: 200_000,
            store_timeout_s: 20.0,
            lance_lock_wait_s: 5.0,
            lance_workers: 4,
            max_query_chars: 8000,
            max_jobs_kept: 200,
            embed_batch: 64,
            default_rate_per_min: 120,
            default_daily_queries: 5000,
            default_daily_writes: 20000,
            sse_max_seconds: 3600,
            sse_max_streams: 8,
        }
    }

    /// Defaults with `data_dir` set (tests and the harness).
    pub fn with_data_dir(data_dir: &Path) -> Self {
        let mut c = Self::defaults("/");
        c.data_dir = data_dir.to_path_buf();
        c
    }

    /// `cfg.lance_path`.
    pub fn lance_path(&self) -> PathBuf {
        self.lance_dir.clone().unwrap_or_else(|| self.data_dir.join("lance"))
    }
}

/// `str(pathlib.PurePosixPath(s))`: duplicate slashes and `.` components collapse, a trailing slash goes, exactly two
/// leading slashes are kept, `..` stays.
pub fn py_path(s: &str) -> PathBuf {
    let root = if s.starts_with("//") && !s.starts_with("///") {
        "//"
    } else if s.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    let joined = format!("{root}{}", parts.join("/"));
    PathBuf::from(if joined.is_empty() { ".".to_string() } else { joined })
}

// ------------------------------------------------------------------------------------------------ validation

enum Kind {
    Ty(Ty),
    Path,
    OptPath,
    PathList,
}

fn validate_path(v: &PyValue, loc: &[Loc], errs: &mut Vec<ErrItem>) -> Option<PathBuf> {
    match v {
        PyValue::Str(s) => Some(py_path(s)),
        _ => {
            errs.push(ErrItem {
                loc: loc.to_vec(),
                msg: "Input is not a valid path for <class 'pathlib._local.Path'>".into(),
                kind: "path_type".into(),
                input: v.clone(),
            });
            None
        }
    }
}

fn env_name_check(v: &PyValue) -> Result<(), String> {
    match v {
        PyValue::Str(s) if !ENV_NAME.is_match(s) => Err("must be an environment variable NAME, not a value".into()),
        _ => Ok(()),
    }
}

fn url_check(v: &PyValue) -> Result<(), String> {
    match v {
        PyValue::Str(s) if !URL.is_match(s) => Err("must be a plain http(s) URL without credentials or query".into()),
        _ => Ok(()),
    }
}

/// The declared fields in order: (name, kind, after-validator).
fn fields() -> Vec<(&'static str, Kind, Option<pyd::Check>)> {
    let int = || Kind::Ty(Ty::int());
    let float = || Kind::Ty(Ty::Float);
    vec![
        ("host", Kind::Ty(Ty::str()), None),
        ("port", int(), None),
        ("allowed_hosts", Kind::Ty(Ty::list(Ty::str())), None),
        ("data_dir", Kind::Path, None),
        ("lance_dir", Kind::OptPath, None),
        ("qdrant_url", Kind::Ty(Ty::opt(Ty::str())), Some(url_check)),
        ("qdrant_api_key_env", Kind::Ty(Ty::opt(Ty::str())), Some(env_name_check)),
        ("qdrant_vectors_on_disk", Kind::Ty(Ty::Bool), None),
        ("router_url", Kind::Ty(Ty::str()), Some(url_check)),
        ("router_api_key_env", Kind::Ty(Ty::str()), Some(env_name_check)),
        ("default_embedding_model", Kind::Ty(Ty::str()), None),
        ("ingest_roots", Kind::PathList, None),
        ("index_roots", Kind::PathList, None),
        ("body_cap_bytes", int(), None),
        ("max_docs_per_request", int(), None),
        ("max_doc_chars", int(), None),
        ("max_files_per_job", int(), None),
        ("max_job_bytes", int(), None),
        ("max_chunks_per_namespace", int(), None),
        ("max_k", int(), None),
        ("max_filter_nodes", int(), None),
        ("lance_max_scan_rows", int(), None),
        ("store_timeout_s", float(), None),
        ("lance_lock_wait_s", float(), None),
        ("lance_workers", int(), None),
        ("max_query_chars", int(), None),
        ("max_jobs_kept", int(), None),
        ("embed_batch", int(), None),
        ("default_rate_per_min", int(), None),
        ("default_daily_queries", int(), None),
        ("default_daily_writes", int(), None),
        ("sse_max_seconds", int(), None),
        ("sse_max_streams", int(), None),
    ]
}

enum Val {
    Py(PyValue),
    Path(PathBuf),
    OptPath(Option<PathBuf>),
    Paths(Vec<PathBuf>),
}

fn int_of(v: &PyValue) -> i64 {
    match v {
        PyValue::Int(i) => *i,
        _ => 0,
    }
}

fn float_of(v: &PyValue) -> f64 {
    match v {
        PyValue::Float(f) => *f,
        _ => 0.0,
    }
}

fn str_of(v: &PyValue) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn opt_str_of(v: &PyValue) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// `Config.model_validate(raw)` over the defaults (`home` for the data_dir default).
pub fn validate(raw: &PyValue, home: &str) -> Result<Config, ValidationError> {
    let mut cfg = Config::defaults(home);
    let empty: Vec<(PyValue, PyValue)> = Vec::new();
    let items = raw.as_dict().unwrap_or(&empty);
    let mut errs: Vec<ErrItem> = Vec::new();
    let mut vals: Vec<(&'static str, Val)> = Vec::new();
    let decl = fields();
    for (name, kind, check) in &decl {
        let Some((_, v)) = items.iter().find(|(k, _)| k.as_str() == Some(*name)) else {
            continue;
        };
        let mut loc = vec![Loc::Key((*name).into())];
        let before = errs.len();
        let out = match kind {
            Kind::Ty(ty) => Val::Py(pyd::validate_ty(ty, v, &mut loc, &mut errs)),
            Kind::Path => match validate_path(v, &loc, &mut errs) {
                Some(p) => Val::Path(p),
                None => continue,
            },
            Kind::OptPath => {
                if v.is_none() {
                    Val::OptPath(None)
                } else {
                    match validate_path(v, &loc, &mut errs) {
                        Some(p) => Val::OptPath(Some(p)),
                        None => continue,
                    }
                }
            }
            Kind::PathList => {
                let (PyValue::List(list) | PyValue::Set(list)) = v else {
                    errs.push(ErrItem {
                        loc: loc.clone(),
                        msg: "Input should be a valid list".into(),
                        kind: "list_type".into(),
                        input: v.clone(),
                    });
                    continue;
                };
                let mut out = Vec::new();
                for (i, it) in list.iter().enumerate() {
                    loc.push(Loc::Index(i));
                    if let Some(p) = validate_path(it, &loc, &mut errs) {
                        out.push(p);
                    }
                    loc.pop();
                }
                Val::Paths(out)
            }
        };
        if errs.len() != before {
            continue;
        }
        if let (Some(check), Val::Py(pv)) = (check, &out)
            && let Err(m) = check(pv)
        {
            errs.push(ErrItem {
                loc: loc.clone(),
                msg: format!("Value error, {m}"),
                kind: "value_error".into(),
                input: pv.clone(),
            });
            continue;
        }
        vals.push((name, out));
    }
    for (k, v) in items {
        if k.as_str().is_some_and(|s| decl.iter().any(|(n, _, _)| *n == s)) {
            continue;
        }
        errs.push(ErrItem {
            loc: vec![Loc::Key(k.py_str())],
            msg: "Extra inputs are not permitted".into(),
            kind: "extra_forbidden".into(),
            input: v.clone(),
        });
    }
    if !errs.is_empty() {
        return Err(ValidationError {
            title: "Config".into(),
            errors: errs,
        });
    }
    for (name, v) in vals {
        match (name, v) {
            ("data_dir", Val::Path(p)) => cfg.data_dir = p,
            ("lance_dir", Val::OptPath(p)) => cfg.lance_dir = p,
            ("ingest_roots", Val::Paths(p)) => cfg.ingest_roots = p,
            ("index_roots", Val::Paths(p)) => cfg.index_roots = p,
            (n, Val::Py(v)) => set_py(&mut cfg, n, &v),
            _ => {}
        }
    }
    Ok(cfg)
}

fn set_py(cfg: &mut Config, name: &str, v: &PyValue) {
    match name {
        "host" => cfg.host = str_of(v),
        "port" => cfg.port = int_of(v),
        "allowed_hosts" => cfg.allowed_hosts = v.as_list().unwrap_or(&[]).iter().map(str_of).collect(),
        // the `_url` validator returns `v.rstrip("/")`
        "qdrant_url" => cfg.qdrant_url = opt_str_of(v).map(|s| s.trim_end_matches('/').to_string()),
        "qdrant_api_key_env" => cfg.qdrant_api_key_env = opt_str_of(v),
        "qdrant_vectors_on_disk" => cfg.qdrant_vectors_on_disk = matches!(v, PyValue::Bool(true)),
        "router_url" => cfg.router_url = str_of(v).trim_end_matches('/').to_string(),
        "router_api_key_env" => cfg.router_api_key_env = str_of(v),
        "default_embedding_model" => cfg.default_embedding_model = str_of(v),
        "body_cap_bytes" => cfg.body_cap_bytes = int_of(v),
        "max_docs_per_request" => cfg.max_docs_per_request = int_of(v),
        "max_doc_chars" => cfg.max_doc_chars = int_of(v),
        "max_files_per_job" => cfg.max_files_per_job = int_of(v),
        "max_job_bytes" => cfg.max_job_bytes = int_of(v),
        "max_chunks_per_namespace" => cfg.max_chunks_per_namespace = int_of(v),
        "max_k" => cfg.max_k = int_of(v),
        "max_filter_nodes" => cfg.max_filter_nodes = int_of(v),
        "lance_max_scan_rows" => cfg.lance_max_scan_rows = int_of(v),
        "store_timeout_s" => cfg.store_timeout_s = float_of(v),
        "lance_lock_wait_s" => cfg.lance_lock_wait_s = float_of(v),
        "lance_workers" => cfg.lance_workers = int_of(v),
        "max_query_chars" => cfg.max_query_chars = int_of(v),
        "max_jobs_kept" => cfg.max_jobs_kept = int_of(v),
        "embed_batch" => cfg.embed_batch = int_of(v),
        "default_rate_per_min" => cfg.default_rate_per_min = int_of(v),
        "default_daily_queries" => cfg.default_daily_queries = int_of(v),
        "default_daily_writes" => cfg.default_daily_writes = int_of(v),
        "sse_max_seconds" => cfg.sse_max_seconds = int_of(v),
        "sse_max_streams" => cfg.sse_max_streams = int_of(v),
        _ => {}
    }
}

/// pydantic's `input_value` repr: longer than 50 characters is cut to 25 + "..." + 24.
fn input_repr(v: &PyValue) -> String {
    let r: Vec<char> = v.repr().chars().collect();
    if r.len() > 50 {
        format!(
            "{}...{}",
            r[..25].iter().collect::<String>(),
            r[r.len() - 24..].iter().collect::<String>()
        )
    } else {
        r.into_iter().collect()
    }
}

/// `str(ValidationError)`. Like `hub_core::pyd`'s, except that `path_type` (a `PydanticCustomError` raised by
/// pydantic's Python-level path validator) carries no "For further information" URL line.
pub fn validation_text(e: &ValidationError) -> String {
    let n = e.errors.len();
    let mut out = format!("{n} validation error{} for {}", if n == 1 { "" } else { "s" }, e.title);
    for it in &e.errors {
        if !it.loc.is_empty() {
            out.push_str(&format!("\n{}", pyd::loc_str(&it.loc)));
        }
        out.push_str(&format!(
            "\n  {} [type={}, input_value={}, input_type={}]",
            it.msg,
            it.kind,
            input_repr(&it.input),
            it.input.type_name()
        ));
        if it.kind != "path_type" {
            out.push_str(&format!(
                "\n    For further information visit https://errors.pydantic.dev/2.13/v/{}",
                it.kind
            ));
        }
    }
    out
}

/// `load_config(path)`: `Err` is the text Python's traceback ends with (`str(exc)` of the ValidationError, the YAML
/// error, the OSError, or `config must be a mapping`).
pub fn load_config(path: Option<&str>) -> Result<Config, String> {
    load_config_with(path, &env::process())
}

/// [`load_config`] with an explicit environment.
pub fn load_config_with(path: Option<&str>, env: &Env) -> Result<Config, String> {
    let path = path
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .or_else(|| env("AGENT_KNOWLEDGE_CONFIG").filter(|p| !p.is_empty()));
    let mut raw: Vec<(PyValue, PyValue)> = Vec::new();
    if let Some(p) = path {
        let text = std::fs::read(&p).map_err(|e| hub_core::py::os::error_str(&e, Some(&p)))?;
        let text = String::from_utf8(text).map_err(|_| format!("'utf-8' codec can't decode the config file {p}"))?;
        let doc = hub_core::yaml::load(&text, hub_core::yaml::Loader::Safe).map_err(|e| e.full)?;
        match doc {
            PyValue::None => {}
            PyValue::Dict(d) => raw = d,
            _ => return Err("config must be a mapping".into()),
        }
    }
    let env_url = env::get(env, "KNOWLEDGE_ROUTER_URL");
    if !env_url.is_empty() && !raw.iter().any(|(k, _)| k.as_str() == Some("router_url")) {
        raw.push((PyValue::str("router_url"), PyValue::Str(env_url)));
    }
    let home = env::get(env, "HOME");
    let mut cfg = validate(&PyValue::Dict(raw), &home).map_err(|e| validation_text(&e))?;
    cfg.data_dir = py_path(&hub_core::py::os::expanduser(&cfg.data_dir.to_string_lossy(), &home));
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pathlib_normalisation() {
        assert_eq!(py_path("/a//b/./c/"), PathBuf::from("/a/b/c"));
        assert_eq!(py_path("//a"), PathBuf::from("//a"));
        assert_eq!(py_path("///a"), PathBuf::from("/a"));
        assert_eq!(py_path("a/../b"), PathBuf::from("a/../b"));
        assert_eq!(py_path(""), PathBuf::from("."));
    }

    #[test]
    fn errors_in_field_order_and_extras_last() {
        let raw = hub_core::yaml::load(
            "zzz: 1\nport: x\nrouter_api_key_env: sk-live-123\nhost: 5\n",
            hub_core::yaml::Loader::Safe,
        )
        .unwrap_or_default();
        let Err(e) = validate(&raw, "/home/u") else {
            panic!("expected errors")
        };
        let got: Vec<String> = e
            .errors
            .iter()
            .map(|x| format!("{}: {}", pyd::loc_str(&x.loc), x.msg))
            .collect();
        assert_eq!(
            got,
            vec![
                "host: Input should be a valid string",
                "port: Input should be a valid integer, unable to parse string as an integer",
                "router_api_key_env: Value error, must be an environment variable NAME, not a value",
                "zzz: Extra inputs are not permitted",
            ]
        );
    }
}
