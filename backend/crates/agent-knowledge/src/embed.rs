//! Port of embed.py. [`RouterEmbedder`] talks only to the configured router URL (`<base>/v1/embeddings`); the API key
//! comes from an env var NAME, is read at call time and never logged or included in errors; redirects are never
//! followed (a redirect must never carry the bearer key to another host). [`HashEmbedder`] is the deterministic
//! feature-hashing embedder used by tests.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use hub_core::PyValue;
use sha2::{Digest, Sha256};

use crate::env::{self, Env};
use crate::pyjson::{self, Style, obj};

/// `EmbedError(message, status=502)`: the API answers `status` with code `embedding_failed`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct EmbedError {
    pub message: String,
    pub status: u16,
    /// Python raised something that is not an EmbedError (the caller's 500 / the job's `internal_error`).
    pub internal: bool,
}

impl EmbedError {
    fn new(message: impl Into<String>, status: u16) -> Self {
        Self {
            message: message.into(),
            status,
            internal: false,
        }
    }

    fn internal() -> Self {
        Self {
            message: "internal error".into(),
            status: 500,
            internal: true,
        }
    }
}

pub type Vectors = Vec<Vec<f64>>;

/// The `Embedder` protocol.
pub trait Embedder: Send + Sync {
    fn embed<'a>(&'a self, texts: &'a [String], model: &'a str) -> BoxFuture<'a, Result<Vectors, EmbedError>>;
}

/// `HashEmbedder(dim)`: bag-of-words feature hashing into `dim` buckets, L2-normalised.
pub struct HashEmbedder {
    pub dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }

    /// `re.findall(r"\w+", text.lower())`: runs of Python word characters (`str.isalnum()` or `_`).
    fn tokens(text: &str) -> Vec<String> {
        let lower = text.to_lowercase();
        let mut out = Vec::new();
        let mut cur = String::new();
        for c in lower.chars() {
            if c == '_' || hub_core::py::text::is_alnum(c.encode_utf8(&mut [0u8; 4])) {
                cur.push(c);
            } else if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    pub fn one(&self, text: &str) -> Vec<f64> {
        let mut v = vec![0.0f64; self.dim];
        if self.dim == 0 {
            return v;
        }
        for tok in Self::tokens(text) {
            let d = Sha256::digest(tok.as_bytes());
            let mut h8 = [0u8; 8];
            h8.copy_from_slice(&d[..8]);
            let h = u64::from_be_bytes(h8);
            let i = usize::try_from(h % self.dim as u64).unwrap_or(0);
            v[i] += if (h >> 63) & 1 == 1 { 1.0 } else { -1.0 };
        }
        // math.sqrt(sum(x * x for x in v)) or 1.0  (sum() is compensated in CPython >= 3.12)
        let s = crate::stores::py_sum(v.iter().map(|x| x * x)).sqrt();
        let norm = if s == 0.0 { 1.0 } else { s };
        v.iter().map(|x| x / norm).collect()
    }
}

impl Embedder for HashEmbedder {
    fn embed<'a>(&'a self, texts: &'a [String], _model: &'a str) -> BoxFuture<'a, Result<Vectors, EmbedError>> {
        Box::pin(async move { Ok(texts.iter().map(|t| self.one(t)).collect()) })
    }
}

/// The async sleep between retries (injectable for tests).
pub type Sleep = Arc<dyn Fn(f64) -> BoxFuture<'static, ()> + Send + Sync>;

pub fn tokio_sleep() -> Sleep {
    Arc::new(|s: f64| Box::pin(tokio::time::sleep(Duration::from_secs_f64(s.max(0.0)))))
}

/// The httpx exception class name for a transport failure (Python records `type(e).__name__` only: messages can embed
/// URLs).
pub fn transport_error_name(e: &reqwest::Error) -> &'static str {
    if e.is_connect() {
        if e.is_timeout() {
            "ConnectTimeout"
        } else {
            "ConnectError"
        }
    } else if e.is_timeout() {
        "ReadTimeout"
    } else if e.is_body() || e.is_decode() {
        "ReadError"
    } else {
        "RemoteProtocolError"
    }
}

/// An httpx-like client: no redirects, per-phase timeout, proxies from the environment like `trust_env`.
pub fn http_client(timeout_s: f64) -> reqwest::Client {
    let t = Duration::from_secs_f64(if timeout_s.is_finite() && timeout_s > 0.0 {
        timeout_s
    } else {
        60.0
    });
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(t)
        .read_timeout(t)
        .build()
        .unwrap_or_default()
}

pub struct RouterEmbedder {
    url: String,
    key_env: String,
    batch: i64,
    retries: u32,
    sleep: Sleep,
    env: Env,
    client: reqwest::Client,
}

impl RouterEmbedder {
    /// `RouterEmbedder(base_url, api_key_env, batch=64, retries=4, timeout=60.0)`.
    pub fn new(base_url: &str, api_key_env: &str, batch: i64) -> Self {
        Self::with(base_url, api_key_env, batch, 4, 60.0, tokio_sleep(), env::process())
    }

    pub fn with(
        base_url: &str,
        api_key_env: &str,
        batch: i64,
        retries: u32,
        timeout_s: f64,
        sleep: Sleep,
        env: Env,
    ) -> Self {
        Self {
            url: format!("{}/v1/embeddings", base_url.trim_end_matches('/')),
            key_env: api_key_env.to_string(),
            batch,
            retries,
            sleep,
            env,
            client: http_client(timeout_s),
        }
    }

    async fn batch_call(&self, batch: &[String], model: &str) -> Result<Vectors, EmbedError> {
        let body = pyjson::dumps(
            &obj([
                ("model", PyValue::str(model)),
                (
                    "input",
                    PyValue::List(batch.iter().map(|t| PyValue::str(t.as_str())).collect()),
                ),
            ]),
            Style::Compact,
        );
        let mut last = "unknown".to_string();
        for attempt in 0..=self.retries {
            let key = env::get(&self.env, &self.key_env);
            let mut req = self
                .client
                .post(&self.url)
                .header("content-type", "application/json")
                .body(body.clone());
            if !key.is_empty() {
                req = req.header("authorization", format!("Bearer {key}"));
            }
            match req.send().await {
                Err(e) if e.is_builder() => return Err(EmbedError::internal()),
                Err(e) => last = transport_error_name(&e).to_string(),
                Ok(r) => {
                    let code = r.status().as_u16();
                    if code == 200 {
                        let bytes = match r.bytes().await {
                            Ok(b) => b,
                            Err(e) => {
                                last = transport_error_name(&e).to_string();
                                if attempt < self.retries {
                                    (self.sleep)(backoff(attempt)).await;
                                }
                                continue;
                            }
                        };
                        return parse(&bytes, batch.len());
                    }
                    last = format!("HTTP {code}");
                    if ![408, 429, 500, 502, 503, 504].contains(&code) {
                        return Err(EmbedError::new(
                            format!("router rejected the embeddings request ({last})"),
                            if [400, 404, 422].contains(&code) { 400 } else { 502 },
                        ));
                    }
                }
            }
            if attempt < self.retries {
                (self.sleep)(backoff(attempt)).await;
            }
        }
        Err(EmbedError::new(
            format!("router embeddings failed after {} attempts ({last})", self.retries + 1),
            502,
        ))
    }
}

/// `min(0.5 * 2**attempt, 8.0)`.
fn backoff(attempt: u32) -> f64 {
    (0.5 * 2f64.powi(i32::try_from(attempt).unwrap_or(30))).min(8.0)
}

/// Python `float(x)` for a JSON value (`Err` = TypeError/ValueError; `Ok(None)` = OverflowError, not caught).
fn py_float(v: &PyValue) -> Result<Option<f64>, NotAFloat> {
    match v {
        PyValue::Float(f) => Ok(Some(*f)),
        #[allow(clippy::cast_precision_loss)]
        PyValue::Int(i) => Ok(Some(*i as f64)),
        PyValue::Bool(b) => Ok(Some(f64::from(u8::from(*b)))),
        PyValue::BigInt(s) => match s.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Some(f)),
            _ => Ok(None),
        },
        PyValue::Str(s) => py_float_str(s).map(Some),
        _ => Err(NotAFloat),
    }
}

/// Python's `ValueError: could not convert string to float`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotAFloat;

/// `float(str)`: whitespace stripped, `inf`/`nan`/`infinity` any case with a sign, `_` between digits.
pub fn py_float_str(s: &str) -> Result<f64, NotAFloat> {
    let t = hub_core::py::text::strip(s);
    let lower = t.to_ascii_lowercase();
    let (neg, body) = match lower.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, lower.strip_prefix('+').unwrap_or(&lower)),
    };
    let sign = if neg { -1.0 } else { 1.0 };
    if body == "inf" || body == "infinity" {
        return Ok(sign * f64::INFINITY);
    }
    if body == "nan" {
        return Ok(f64::NAN);
    }
    if body.is_empty()
        || body.starts_with('_')
        || body.ends_with('_')
        || body.contains("__")
        || !body
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | '+' | '-' | '_'))
    {
        return Err(NotAFloat);
    }
    let cleaned: String = body.chars().filter(|&c| c != '_').collect();
    cleaned.parse::<f64>().map(|f| sign * f).map_err(|_| NotAFloat)
}

/// `RouterEmbedder._parse(r, n)`.
fn parse(bytes: &[u8], n: usize) -> Result<Vectors, EmbedError> {
    let malformed = || EmbedError::new("router returned a malformed embeddings response", 502);
    let doc = match pyjson::loads_bytes(bytes) {
        Ok(d) => d,
        Err(e) if e.is_value_error() => return Err(malformed()),
        Err(_) => return Err(EmbedError::internal()),
    };
    let Some(data) = doc.get("data") else {
        return Err(malformed());
    };
    let Some(rows) = data.as_list() else {
        return Err(malformed());
    };
    // sorted(data, key=lambda d: d["index"]): every row is a dict with mutually comparable indexes
    let mut keyed: Vec<(&PyValue, &PyValue)> = Vec::with_capacity(rows.len());
    for d in rows {
        let Some(idx) = d.get("index") else {
            return Err(malformed());
        };
        keyed.push((idx, d));
    }
    let numeric = keyed.iter().all(|(k, _)| {
        matches!(
            k,
            PyValue::Int(_) | PyValue::BigInt(_) | PyValue::Float(_) | PyValue::Bool(_)
        )
    });
    let strings = keyed.iter().all(|(k, _)| matches!(k, PyValue::Str(_)));
    if keyed.len() > 1 && !numeric && !strings {
        return Err(malformed());
    }
    let key_f = |v: &PyValue| -> f64 {
        match py_float(v) {
            Ok(Some(f)) => f,
            _ => 0.0,
        }
    };
    if strings {
        keyed.sort_by(|a, b| a.0.as_str().cmp(&b.0.as_str()));
    } else {
        keyed.sort_by(|a, b| key_f(a.0).partial_cmp(&key_f(b.0)).unwrap_or(std::cmp::Ordering::Equal));
    }
    let mut vecs = Vec::with_capacity(keyed.len());
    for (_, d) in keyed {
        let Some(emb) = d.get("embedding") else {
            return Err(malformed());
        };
        // `for x in d["embedding"]`: a list, a str (its characters) or a dict (its keys) all iterate
        let owned: Vec<PyValue>;
        let items: &[PyValue] = match emb {
            PyValue::List(l) => l,
            PyValue::Str(s) => {
                owned = s.chars().map(|c| PyValue::Str(c.to_string())).collect();
                &owned
            }
            PyValue::Dict(d) => {
                owned = d.iter().map(|(k, _)| k.clone()).collect();
                &owned
            }
            _ => return Err(malformed()),
        };
        let mut v = Vec::with_capacity(items.len());
        for x in items {
            match py_float(x) {
                Ok(Some(f)) => v.push(f),
                Ok(None) => return Err(EmbedError::internal()),
                Err(NotAFloat) => return Err(malformed()),
            }
        }
        vecs.push(v);
    }
    let lens: std::collections::BTreeSet<usize> = vecs.iter().map(Vec::len).collect();
    if vecs.len() != n || lens.len() > 1 {
        return Err(EmbedError::new("router returned the wrong number of embeddings", 502));
    }
    Ok(vecs)
}

impl Embedder for RouterEmbedder {
    fn embed<'a>(&'a self, texts: &'a [String], model: &'a str) -> BoxFuture<'a, Result<Vectors, EmbedError>> {
        Box::pin(async move {
            // range(0, len(texts), batch): a zero step is a ValueError, a negative one yields nothing
            let batch = match usize::try_from(self.batch) {
                Ok(0) => return Err(EmbedError::internal()),
                Ok(b) => b,
                Err(_) => return Ok(Vec::new()),
            };
            let mut out = Vec::new();
            for chunk in texts.chunks(batch) {
                out.extend(self.batch_call(chunk, model).await?);
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_embedder_tokens_and_norm() {
        assert_eq!(
            HashEmbedder::tokens("Hello, wörld_x 42!"),
            vec!["hello", "wörld_x", "42"]
        );
        let v = HashEmbedder::new(16).one("hello world");
        let n: f64 = v.iter().map(|x| x * x).sum();
        assert!((n - 1.0).abs() < 1e-12);
        assert_eq!(HashEmbedder::new(4).one("   "), vec![0.0; 4]);
    }

    #[test]
    fn float_of_strings() {
        assert_eq!(py_float_str(" 1_000.5 "), Ok(1000.5));
        assert_eq!(py_float_str("-Infinity"), Ok(f64::NEG_INFINITY));
        assert!(py_float_str("1__0").is_err());
        assert!(py_float_str("abc").is_err());
    }
}
