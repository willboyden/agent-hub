//! Port of stores/memory.py: the in-memory store, the reference implementation of query/filter semantics (used by the
//! offline tests). Collections keep dict insertion order; an upsert of an existing id keeps its position.

use std::collections::HashMap;
use std::sync::Mutex;

use futures::future::BoxFuture;
use hub_core::PyValue;

use super::{Hit, Record, StoreError, StoreResult, StoreStats, VectorStore, check_ids, py_sum};
use crate::filters::{Node, evaluate};
use crate::pyjson::obj;

#[derive(Default)]
struct Inner {
    data: HashMap<String, Vec<Record>>,
    dims: HashMap<String, i64>,
}

#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

/// `_cos(a, b)`.
pub fn cos(a: &[f64], b: &[f64]) -> f64 {
    let na = py_sum(a.iter().map(|x| x * x)).sqrt();
    let nb = py_sum(b.iter().map(|x| x * x)).sqrt();
    if na != 0.0 && nb != 0.0 {
        py_sum(a.iter().zip(b).map(|(x, y)| x * y)) / (na * nb)
    } else {
        0.0
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, StoreError> {
        self.inner.lock().map_err(|_| StoreError::internal())
    }
}

fn dim_ok(len: usize, dim: i64) -> bool {
    i64::try_from(len).is_ok_and(|l| l == dim)
}

impl VectorStore for MemoryStore {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn ensure<'a>(&'a self, ns: &'a str, dim: i64) -> StoreResult<'a, ()> {
        Box::pin(async move {
            let mut g = self.lock()?;
            if *g.dims.entry(ns.to_string()).or_insert(dim) != dim {
                return Err(StoreError::dim_mismatch(
                    "existing collection has a different dimension",
                ));
            }
            g.data.entry(ns.to_string()).or_default();
            Ok(())
        })
    }

    fn upsert<'a>(&'a self, ns: &'a str, records: Vec<Record>) -> StoreResult<'a, usize> {
        Box::pin(async move {
            check_ids(records.iter().map(|r| r.id.as_str()))?;
            let n = records.len();
            let mut g = self.lock()?;
            for r in records {
                let Some(&dim) = g.dims.get(ns) else {
                    return Err(StoreError::internal());
                };
                if !dim_ok(r.vector.len(), dim) {
                    return Err(StoreError::dim_mismatch(
                        "vector dimension does not match the collection",
                    ));
                }
                let Some(col) = g.data.get_mut(ns) else {
                    return Err(StoreError::internal());
                };
                match col.iter_mut().find(|x| x.id == r.id) {
                    Some(slot) => *slot = r,
                    None => col.push(r),
                }
            }
            Ok(n)
        })
    }

    fn query<'a>(
        &'a self,
        ns: &'a str,
        vector: &'a [f64],
        k: i64,
        flt: Option<&'a Node>,
        min_score: Option<f64>,
    ) -> StoreResult<'a, Vec<Hit>> {
        Box::pin(async move {
            let g = self.lock()?;
            let Some(&dim) = g.dims.get(ns) else {
                return Err(StoreError::internal());
            };
            if !dim_ok(vector.len(), dim) {
                return Err(StoreError::dim_mismatch(
                    "vector dimension does not match the collection",
                ));
            }
            let Some(col) = g.data.get(ns) else {
                return Err(StoreError::internal());
            };
            let mut hits = Vec::new();
            for r in col {
                if flt.is_some_and(|f| !evaluate(f, &r.meta)) {
                    continue;
                }
                let s = cos(vector, &r.vector);
                if min_score.is_none_or(|m| s >= m) {
                    hits.push(Hit {
                        id: PyValue::str(r.id.as_str()),
                        score: s,
                        text: PyValue::str(r.text.as_str()),
                        meta: r.meta.clone(),
                    });
                }
            }
            hits.sort_by(|a, b| {
                (-a.score)
                    .partial_cmp(&-b.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.id.as_str().cmp(&b.id.as_str()))
            });
            hits.truncate(usize::try_from(k.max(0)).unwrap_or(0));
            Ok(hits)
        })
    }

    fn delete<'a>(&'a self, ns: &'a str, flt: &'a Node) -> StoreResult<'a, i64> {
        Box::pin(async move {
            let mut g = self.lock()?;
            let Some(col) = g.data.get_mut(ns) else {
                return Err(StoreError::internal());
            };
            let before = col.len();
            col.retain(|r| !evaluate(flt, &r.meta));
            Ok(i64::try_from(before - col.len()).unwrap_or(0))
        })
    }

    fn stats<'a>(&'a self, ns: &'a str) -> StoreResult<'a, StoreStats> {
        Box::pin(async move {
            let g = self.lock()?;
            Ok(StoreStats {
                count: i64::try_from(g.data.get(ns).map_or(0, Vec::len)).unwrap_or(0),
                disk_bytes: Some(0),
                disk_bytes_estimated: false,
            })
        })
    }

    fn drop_ns<'a>(&'a self, ns: &'a str) -> StoreResult<'a, ()> {
        Box::pin(async move {
            let mut g = self.lock()?;
            g.data.remove(ns);
            g.dims.remove(ns);
            Ok(())
        })
    }

    fn health(&self) -> BoxFuture<'_, PyValue> {
        Box::pin(async { obj([("ok", PyValue::Bool(true)), ("kind", PyValue::str("memory"))]) })
    }
}
