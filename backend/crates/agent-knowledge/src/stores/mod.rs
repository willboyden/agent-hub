//! Port of stores/base.py: the VectorStore port (ensure, upsert, query, delete, stats, drop, health), `Record`, `Hit`,
//! `StoreStats`, `StoreError` (code, status) and the id/namespace patterns.

#[cfg(feature = "lance")]
pub mod lance;
pub mod memory;
pub mod qdrant;

use std::sync::LazyLock;

use futures::future::BoxFuture;
use hub_core::PyValue;
use regex::Regex;

use crate::filters::Node;

pub const ID_RE_TEXT: &str = r"[A-Za-z0-9._:#@/-]{1,200}";
pub const NS_RE_TEXT: &str = r"[a-z0-9][a-z0-9_-]{0,47}";
pub static ID_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(&format!("^{ID_RE_TEXT}$")));
pub static NS_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(&format!("^{NS_RE_TEXT}$")));

/// `StoreError(message, code="backend_error", status=502)`; `StoreDimMismatch` is code `dim_mismatch`, 409.
/// `internal` marks failures that Python raises as some other exception (a 500 / the job's `internal_error`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct StoreError {
    pub message: String,
    pub code: String,
    pub status: u16,
    pub internal: bool,
}

impl StoreError {
    pub fn new(message: impl Into<String>, code: &str, status: u16) -> Self {
        Self {
            message: message.into(),
            code: code.into(),
            status,
            internal: false,
        }
    }

    pub fn backend(message: impl Into<String>) -> Self {
        Self::new(message, "backend_error", 502)
    }

    pub fn dim_mismatch(message: impl Into<String>) -> Self {
        Self::new(message, "dim_mismatch", 409)
    }

    pub fn internal() -> Self {
        Self {
            message: "internal error".into(),
            code: "internal_error".into(),
            status: 500,
            internal: true,
        }
    }
}

/// `Record(id, text, vector, meta)`; `meta` is a dict of scalars (`PyValue::Dict`).
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub id: String,
    pub text: String,
    pub vector: Vec<f64>,
    pub meta: PyValue,
}

/// `Hit(id, score, text, meta)`. Qdrant payloads are untrusted JSON, so id/text/meta keep whatever type came back.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: PyValue,
    pub score: f64,
    pub text: PyValue,
    pub meta: PyValue,
}

/// `StoreStats(count, disk_bytes, disk_bytes_estimated=False)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreStats {
    pub count: i64,
    pub disk_bytes: Option<i64>,
    pub disk_bytes_estimated: bool,
}

pub type StoreResult<'a, T> = BoxFuture<'a, Result<T, StoreError>>;

/// The configured stores by name, in `default_stores` order (`lancedb`, then `qdrant`).
pub type Stores = Vec<(String, std::sync::Arc<dyn VectorStore>)>;

/// The `VectorStore` protocol.
pub trait VectorStore: Send + Sync {
    fn name(&self) -> &'static str;
    fn ensure<'a>(&'a self, ns: &'a str, dim: i64) -> StoreResult<'a, ()>;
    fn upsert<'a>(&'a self, ns: &'a str, records: Vec<Record>) -> StoreResult<'a, usize>;
    fn query<'a>(
        &'a self,
        ns: &'a str,
        vector: &'a [f64],
        k: i64,
        flt: Option<&'a Node>,
        min_score: Option<f64>,
    ) -> StoreResult<'a, Vec<Hit>>;
    fn delete<'a>(&'a self, ns: &'a str, flt: &'a Node) -> StoreResult<'a, i64>;
    fn stats<'a>(&'a self, ns: &'a str) -> StoreResult<'a, StoreStats>;
    fn drop_ns<'a>(&'a self, ns: &'a str) -> StoreResult<'a, ()>;
    fn health(&self) -> BoxFuture<'_, PyValue>;
    /// Qdrant's `refusal()` (None elsewhere: `getattr(q, "refusal", None)`).
    fn refusal(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async { None })
    }
}

/// `check_ids(ids)`.
pub fn check_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> Result<(), StoreError> {
    for i in ids {
        if !ID_RE.is_match(i) {
            return Err(StoreError::new("invalid id", "invalid_id", 422));
        }
    }
    Ok(())
}

/// CPython 3.12+ `sum()` over floats (start 0): the first item is added exactly, the rest with Neumaier's
/// compensated summation, the compensation applied at the end when finite.
pub fn py_sum(xs: impl IntoIterator<Item = f64>) -> f64 {
    let mut it = xs.into_iter();
    let Some(first) = it.next() else { return 0.0 };
    let mut f = 0.0 + first;
    let mut c = 0.0f64;
    for x in it {
        let t = f + x;
        if f.abs() >= x.abs() {
            c += (f - t) + x;
        } else {
            c += (x - t) + f;
        }
        f = t;
    }
    if c != 0.0 && c.is_finite() {
        f += c;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::py_sum;

    #[test]
    fn neumaier_like_cpython() {
        // sum([0.1] * 10) == 1.0 in Python >= 3.12 (it was 0.9999999999999999 before)
        assert_eq!(py_sum(vec![0.1; 10]), 1.0);
        assert_eq!(py_sum(vec![1e100, 1.0, -1e100]), 1.0);
        assert_eq!(py_sum(Vec::new()), 0.0);
    }
}
