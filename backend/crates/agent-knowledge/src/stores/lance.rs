//! Port of stores/lance.py: the embedded LanceDB store, one table `ns_<namespace>` per namespace under `lance_dir`.
//!
//! Engine: the `lance` crate =12.0.0, the exact Lance version inside Python lancedb 0.39.0 (PORTING.md §15), driven
//! the way lancedb 0.39 drives it, so the two implementations read and write each other's tables (tests/lance_compat.rs):
//! schema `id, text, meta_json, doc_id: utf8; vector: fixed_size_list<item: float32>[dim]` created with the default
//! `WriteParams` (v2 manifest paths), merge-insert on `id` (update all / insert all / keep), vector search =
//! `nearest(vector, q, k)` + `minimum_nprobes(20)` + `limit(k)` + prefilter + cosine, delete by SQL predicate.
//! Metadata is stored as a JSON string. Filters are NOT compiled into SQL: the validated AST is evaluated over
//! `(id, meta_json)` to resolve matching ids, and only those ids (which match a strict regex and therefore cannot carry
//! SQL syntax) are used in `id IN (...)`. A plain `doc_id` equality uses `doc_id = '<id>'` with the id re-validated.
//!
//! Blocking discipline (Python: a bounded thread pool + per-namespace locks): every operation runs in its own task that
//! first takes one of `workers` permits, then the namespace lock with a bounded wait (`store_busy` 429); the caller
//! waits at most `timeout_s` (`store_timeout` 429) while a running operation finishes in the background, holding the
//! lock, and a queued one that was given up on never starts.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::TryStreamExt;
use futures::future::BoxFuture;
use hub_core::PyValue;
use lance::Dataset;
use lance::dataset::{MergeInsertBuilder, WhenMatched, WhenNotMatched, WhenNotMatchedBySource, WriteMode, WriteParams};
use lance::deps::arrow_array::cast::AsArray;
use lance::deps::arrow_array::types::Float32Type;
use lance::deps::arrow_array::{
    Array, ArrayRef, FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator, RecordBatchReader, StringArray,
};
use lance::deps::arrow_schema::{DataType, Field, Schema};
use lance_linalg::distance::DistanceType;

use super::{Hit, Record, StoreError, StoreResult, StoreStats, VectorStore, check_ids};
use crate::filters::{Node, Op, evaluate};
use crate::pyjson::{self, Style, obj};

pub const MAX_FILTER_IDS: usize = 50_000;

/// A test seam: awaited inside the worker, under the namespace lock, before the operation (Python's tests
/// monkeypatch `_query` with a slow one).
pub type BeforeOp = Arc<dyn Fn(&str, &str) -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Clone)]
pub struct LanceOptions {
    pub max_scan_rows: i64,
    pub timeout_s: f64,
    pub lock_wait_s: f64,
    pub workers: usize,
    pub before_op: Option<BeforeOp>,
}

impl Default for LanceOptions {
    fn default() -> Self {
        Self {
            max_scan_rows: 200_000,
            timeout_s: 20.0,
            lock_wait_s: 5.0,
            workers: 4,
            before_op: None,
        }
    }
}

struct Inner {
    path: PathBuf,
    max_scan: i64,
    before_op: Option<BeforeOp>,
}

pub struct LanceStore {
    inner: Arc<Inner>,
    timeout: Duration,
    lock_wait: Duration,
    pool: Arc<tokio::sync::Semaphore>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

fn lance_err(_e: lance::Error) -> StoreError {
    // lancedb raises its own exceptions here, which the service does not catch (a 500 / the job's internal_error)
    StoreError::internal()
}

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(if s.is_finite() && s > 0.0 { s } else { 0.0 })
}

/// `LanceDBConnection.list_tables()` for a local directory: the `<name>.lance` dataset directories, sorted.
pub fn table_names(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().to_string();
        if e.file_type()?.is_dir()
            && let Some(t) = name.strip_suffix(".lance")
        {
            out.push(t.to_string());
        }
    }
    out.sort();
    Ok(out)
}

/// Rows of one table (`tbl.count_rows()`).
pub async fn count_rows(dir: &Path, table: &str) -> lance::Result<usize> {
    let uri = dir.join(format!("{table}.lance"));
    let ds = Dataset::open(&uri.to_string_lossy()).await?;
    ds.count_rows(None).await
}

/// `LanceStore.table_name(ns)`.
pub fn table_name(ns: &str) -> String {
    format!("ns_{ns}")
}

/// `_in_clause(ids)`: the regex has no quote/backslash/paren, so the literals cannot break out.
fn in_clause(ids: &[String]) -> Result<String, StoreError> {
    check_ids(ids.iter().map(String::as_str))?;
    let lits: Vec<String> = ids.iter().map(|i| format!("'{i}'")).collect();
    Ok(format!("id IN ({})", lits.join(", ")))
}

/// The table schema Python creates.
pub fn table_schema(dim: i32) -> Schema {
    Schema::new(vec![
        Field::new("id", DataType::Utf8, true),
        Field::new("text", DataType::Utf8, true),
        Field::new("meta_json", DataType::Utf8, true),
        Field::new("doc_id", DataType::Utf8, true),
        Field::new(
            "vector",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), dim),
            true,
        ),
    ])
}

fn vector_dim(schema: &Schema) -> Result<i64, StoreError> {
    match schema.field_with_name("vector").map(|f| f.data_type().clone()) {
        Ok(DataType::FixedSizeList(_, n)) => Ok(i64::from(n)),
        _ => Err(StoreError::internal()), // `.type.list_size` raises AttributeError
    }
}

fn dim_ok(len: usize, dim: i64) -> bool {
    i64::try_from(len).is_ok_and(|l| l == dim)
}

impl Inner {
    fn uri(&self, ns: &str) -> String {
        self.path
            .join(format!("{}.lance", table_name(ns)))
            .to_string_lossy()
            .into_owned()
    }

    fn tables(&self) -> Result<Vec<String>, StoreError> {
        table_names(&self.path).map_err(|_| StoreError::internal())
    }

    async fn open(&self, ns: &str) -> Result<Dataset, StoreError> {
        Dataset::open(&self.uri(ns)).await.map_err(lance_err)
    }

    async fn ensure(&self, ns: &str, dim: i64) -> Result<(), StoreError> {
        if self.tables()?.contains(&table_name(ns)) {
            let ds = self.open(ns).await?;
            let schema: Schema = ds.schema().into();
            if vector_dim(&schema)? != dim {
                return Err(StoreError::dim_mismatch(
                    "existing lance table has a different dimension",
                ));
            }
            return Ok(());
        }
        let d = i32::try_from(dim).map_err(|_| StoreError::internal())?;
        let schema = Arc::new(table_schema(d));
        let reader: Box<dyn RecordBatchReader + Send> =
            Box::new(RecordBatchIterator::new(Vec::<Result<RecordBatch, _>>::new(), schema));
        let params = WriteParams {
            mode: WriteMode::Create,
            ..WriteParams::default()
        };
        Dataset::write(reader, self.uri(ns).as_str(), Some(params))
            .await
            .map_err(lance_err)?;
        Ok(())
    }

    async fn upsert(&self, ns: &str, records: Vec<Record>) -> Result<usize, StoreError> {
        check_ids(records.iter().map(|r| r.id.as_str()))?;
        if records.is_empty() {
            return Ok(0);
        }
        let ds = self.open(ns).await?;
        let schema: Schema = ds.schema().into();
        let dim = vector_dim(&schema)?;
        if records.iter().any(|r| !dim_ok(r.vector.len(), dim)) {
            return Err(StoreError::dim_mismatch("vector dimension does not match the table"));
        }
        let batch = rows_batch(&schema, &records)?;
        let schema = batch.schema();
        let mut b = MergeInsertBuilder::try_new(Arc::new(ds), vec!["id".to_string()]).map_err(lance_err)?;
        b.when_matched(WhenMatched::UpdateAll)
            .when_not_matched(WhenNotMatched::InsertAll)
            .when_not_matched_by_source(WhenNotMatchedBySource::Keep)
            .use_index(true);
        let job = b.try_build().map_err(lance_err)?;
        let reader: Box<dyn RecordBatchReader + Send> = Box::new(RecordBatchIterator::new(vec![Ok(batch)], schema));
        job.execute_reader(reader).await.map_err(lance_err)?;
        Ok(records.len())
    }

    /// `_where(ns, flt)`: the cheap path for a plain doc_id equality.
    fn where_(flt: &Node) -> Result<Option<String>, StoreError> {
        if let Node::Cmp {
            field,
            op: Op::Eq,
            value: PyValue::Str(v),
        } = flt
            && field == "doc_id"
        {
            check_ids([v.as_str()])?;
            return Ok(Some(format!("doc_id = '{v}'")));
        }
        Ok(None)
    }

    async fn matching_ids(&self, ds: &Dataset, flt: &Node) -> Result<Vec<String>, StoreError> {
        let count = i64::try_from(ds.count_rows(None).await.map_err(lance_err)?).unwrap_or(i64::MAX);
        if count > self.max_scan {
            return Err(StoreError::new(
                "namespace too large for this filter on the embedded backend; use a doc_id filter or Qdrant",
                "scan_cap",
                422,
            ));
        }
        let mut scan = ds.scan();
        scan.project(&["id", "meta_json"]).map_err(lance_err)?;
        scan.limit(Some(count.max(1)), None).map_err(lance_err)?;
        let batches: Vec<RecordBatch> = scan
            .try_into_stream()
            .await
            .map_err(lance_err)?
            .try_collect()
            .await
            .map_err(lance_err)?;
        let mut ids = Vec::new();
        for b in &batches {
            let id = str_col(b, "id")?;
            let meta = str_col(b, "meta_json")?;
            for i in 0..b.num_rows() {
                let m = meta_at(meta, i)?;
                if evaluate(flt, &m) {
                    ids.push(id.value(i).to_string());
                }
            }
        }
        if ids.len() > MAX_FILTER_IDS {
            return Err(StoreError::new(
                "filter matches too many rows for the embedded backend; narrow it",
                "filter_too_broad",
                422,
            ));
        }
        Ok(ids)
    }

    async fn query(
        &self,
        ns: &str,
        vector: Vec<f64>,
        k: i64,
        flt: Option<Node>,
        min_score: Option<f64>,
    ) -> Result<Vec<Hit>, StoreError> {
        let ds = self.open(ns).await?;
        let schema: Schema = ds.schema().into();
        let dim = vector_dim(&schema)?;
        if !dim_ok(vector.len(), dim) {
            return Err(StoreError::dim_mismatch("vector dimension does not match the table"));
        }
        #[allow(clippy::cast_possible_truncation)]
        let q = Float32Array::from(vector.iter().map(|x| *x as f32).collect::<Vec<f32>>());
        let kk = usize::try_from(k).map_err(|_| StoreError::internal())?;
        let mut where_clause = None;
        if let Some(f) = &flt {
            where_clause = match Self::where_(f)? {
                Some(w) => Some(w),
                None => {
                    let ids = self.matching_ids(&ds, f).await?;
                    if ids.is_empty() {
                        return Ok(Vec::new());
                    }
                    Some(in_clause(&ids)?)
                }
            };
        }
        let mut scan = ds.scan();
        scan.nearest("vector", &q, kk).map_err(lance_err)?;
        scan.minimum_nprobes(20);
        scan.limit(Some(k), None).map_err(lance_err)?;
        scan.use_index(true);
        scan.prefilter(true);
        if let Some(w) = &where_clause {
            scan.filter(w).map_err(lance_err)?;
        }
        scan.distance_metric(DistanceType::Cosine);
        let batches: Vec<RecordBatch> = scan
            .try_into_stream()
            .await
            .map_err(lance_err)?
            .try_collect()
            .await
            .map_err(lance_err)?;
        let mut hits = Vec::new();
        for b in &batches {
            let id = str_col(b, "id")?;
            let text = str_col(b, "text")?;
            let meta = str_col(b, "meta_json")?;
            let dist = b
                .column_by_name("_distance")
                .and_then(|c| c.as_primitive_opt::<Float32Type>())
                .ok_or_else(StoreError::internal)?;
            for i in 0..b.num_rows() {
                let score = 1.0 - f64::from(dist.value(i));
                if min_score.is_none_or(|m| score >= m) {
                    hits.push(Hit {
                        id: PyValue::str(id.value(i)),
                        score,
                        text: PyValue::str(text.value(i)),
                        meta: meta_at(meta, i)?,
                    });
                }
            }
        }
        Ok(hits)
    }

    async fn delete(&self, ns: &str, flt: Node) -> Result<i64, StoreError> {
        if let Some(w) = Self::where_(&flt)? {
            let n = self
                .open(ns)
                .await?
                .count_rows(Some(w.clone()))
                .await
                .map_err(lance_err)?;
            let mut ds = self.open(ns).await?;
            ds.delete(&w).await.map_err(lance_err)?;
            return Ok(i64::try_from(n).unwrap_or(i64::MAX));
        }
        let ds = self.open(ns).await?;
        let ids = self.matching_ids(&ds, &flt).await?;
        for chunk in ids.chunks(1000) {
            let mut ds = self.open(ns).await?;
            ds.delete(&in_clause(chunk)?).await.map_err(lance_err)?;
        }
        Ok(i64::try_from(ids.len()).unwrap_or(i64::MAX))
    }

    async fn stats(&self, ns: &str) -> Result<StoreStats, StoreError> {
        let ds = self.open(ns).await?;
        let count = i64::try_from(ds.count_rows(None).await.map_err(lance_err)?).unwrap_or(i64::MAX);
        let d = self.path.join(format!("{}.lance", table_name(ns)));
        let size = if d.exists() { tree_size(&d) } else { 0 };
        Ok(StoreStats {
            count,
            disk_bytes: Some(i64::try_from(size).unwrap_or(i64::MAX)),
            disk_bytes_estimated: false,
        })
    }

    async fn drop_ns(&self, ns: &str) -> Result<(), StoreError> {
        if self.tables()?.contains(&table_name(ns)) {
            std::fs::remove_dir_all(self.path.join(format!("{}.lance", table_name(ns))))
                .map_err(|_| StoreError::internal())?;
        }
        Ok(())
    }
}

/// `sum(f.stat().st_size for f in d.rglob("*") if f.is_file())` (symlinks followed for the size and the file test).
fn tree_size(d: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![d.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            if is_dir {
                stack.push(path);
            } else if let Ok(m) = std::fs::metadata(&path)
                && m.is_file()
            {
                total += m.len();
            }
        }
    }
    total
}

fn str_col<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a StringArray, StoreError> {
    b.column_by_name(name)
        .and_then(|c| c.as_string_opt::<i32>())
        .ok_or_else(StoreError::internal)
}

/// `json.loads(row["meta_json"])` (a failure is an uncaught exception in Python).
fn meta_at(col: &StringArray, i: usize) -> Result<PyValue, StoreError> {
    if col.is_null(i) {
        return Err(StoreError::internal());
    }
    pyjson::loads_str(col.value(i)).map_err(|_| StoreError::internal())
}

/// The rows Python hands to `merge_insert`: `{"id", "text", "meta_json": json.dumps(meta), "doc_id":
/// str(meta.get("doc_id", "")), "vector"}`, in the table's schema.
fn rows_batch(schema: &Schema, records: &[Record]) -> Result<RecordBatch, StoreError> {
    let mut cols: Vec<ArrayRef> = Vec::new();
    for f in schema.fields() {
        let col: ArrayRef = match f.name().as_str() {
            "id" => Arc::new(StringArray::from_iter_values(records.iter().map(|r| r.id.clone()))),
            "text" => Arc::new(StringArray::from_iter_values(records.iter().map(|r| r.text.clone()))),
            "meta_json" => Arc::new(StringArray::from_iter_values(
                records.iter().map(|r| pyjson::dumps(&r.meta, Style::Std)),
            )),
            "doc_id" => Arc::new(StringArray::from_iter_values(
                records
                    .iter()
                    .map(|r| r.meta.get("doc_id").map_or_else(String::new, PyValue::py_str)),
            )),
            "vector" => {
                let DataType::FixedSizeList(item, n) = f.data_type() else {
                    return Err(StoreError::internal());
                };
                #[allow(clippy::cast_possible_truncation)]
                let values: Vec<f32> = records
                    .iter()
                    .flat_map(|r| r.vector.iter().map(|x| *x as f32))
                    .collect();
                Arc::new(
                    FixedSizeListArray::try_new(item.clone(), *n, Arc::new(Float32Array::from(values)), None)
                        .map_err(|_| StoreError::internal())?,
                )
            }
            _ => return Err(StoreError::internal()),
        };
        cols.push(col);
    }
    RecordBatch::try_new(Arc::new(schema.clone()), cols).map_err(|_| StoreError::internal())
}

impl LanceStore {
    /// `LanceStore(path, max_scan_rows=, timeout_s=, lock_wait_s=, workers=)`: creates the directory (0700 leaf).
    pub fn new(path: &Path, opts: LanceOptions) -> std::io::Result<Self> {
        crate::db::mkdir_parents(path, 0o700)?;
        Ok(Self {
            inner: Arc::new(Inner {
                path: path.to_path_buf(),
                max_scan: opts.max_scan_rows,
                before_op: opts.before_op,
            }),
            timeout: secs(opts.timeout_s),
            lock_wait: secs(opts.lock_wait_s),
            pool: Arc::new(tokio::sync::Semaphore::new(opts.workers.max(1))),
            locks: Mutex::new(HashMap::new()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    fn ns_lock(&self, ns: &str) -> Arc<tokio::sync::Mutex<()>> {
        match self.locks.lock() {
            Ok(mut m) => m.entry(ns.to_string()).or_default().clone(),
            Err(_) => Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// `_run(ns, fn, *a)`.
    async fn run<T, F, Fut>(&self, ns: &str, op: &'static str, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Inner>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, StoreError>> + Send + 'static,
    {
        let lock = self.ns_lock(ns);
        let pool = self.pool.clone();
        let inner = self.inner.clone();
        let lock_wait = self.lock_wait;
        let abandoned = Arc::new(AtomicBool::new(false));
        let gave_up = abandoned.clone();
        let ns_owned = ns.to_string();
        let task = tokio::spawn(async move {
            let Ok(_permit) = pool.acquire_owned().await else {
                return Err(StoreError::internal());
            };
            if gave_up.load(Ordering::SeqCst) {
                return Err(StoreError::internal()); // a cancelled pool future never runs
            }
            let Ok(_guard) = tokio::time::timeout(lock_wait, lock.lock_owned()).await else {
                return Err(StoreError::new("namespace is busy; retry shortly", "store_busy", 429));
            };
            if let Some(hook) = &inner.before_op {
                hook(&ns_owned, op).await;
            }
            f(inner).await
        });
        match tokio::time::timeout(self.timeout, task).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(StoreError::internal()),
            Err(_) => {
                // The worker cannot be killed, but the caller is released and the namespace lock bounds pile-ups.
                abandoned.store(true, Ordering::SeqCst);
                Err(StoreError::new("store operation timed out", "store_timeout", 429))
            }
        }
    }
}

impl VectorStore for LanceStore {
    fn name(&self) -> &'static str {
        "lancedb"
    }

    fn ensure<'a>(&'a self, ns: &'a str, dim: i64) -> StoreResult<'a, ()> {
        let n = ns.to_string();
        Box::pin(self.run(ns, "ensure", move |i| async move { i.ensure(&n, dim).await }))
    }

    fn upsert<'a>(&'a self, ns: &'a str, records: Vec<Record>) -> StoreResult<'a, usize> {
        let n = ns.to_string();
        Box::pin(self.run(ns, "upsert", move |i| async move { i.upsert(&n, records).await }))
    }

    fn query<'a>(
        &'a self,
        ns: &'a str,
        vector: &'a [f64],
        k: i64,
        flt: Option<&'a Node>,
        min_score: Option<f64>,
    ) -> StoreResult<'a, Vec<Hit>> {
        let n = ns.to_string();
        let v = vector.to_vec();
        let f = flt.cloned();
        Box::pin(self.run(
            ns,
            "query",
            move |i| async move { i.query(&n, v, k, f, min_score).await },
        ))
    }

    fn delete<'a>(&'a self, ns: &'a str, flt: &'a Node) -> StoreResult<'a, i64> {
        let n = ns.to_string();
        let f = flt.clone();
        Box::pin(self.run(ns, "delete", move |i| async move { i.delete(&n, f).await }))
    }

    fn stats<'a>(&'a self, ns: &'a str) -> StoreResult<'a, StoreStats> {
        let n = ns.to_string();
        Box::pin(self.run(ns, "stats", move |i| async move { i.stats(&n).await }))
    }

    fn drop_ns<'a>(&'a self, ns: &'a str) -> StoreResult<'a, ()> {
        let n = ns.to_string();
        Box::pin(self.run(ns, "drop", move |i| async move { i.drop_ns(&n).await }))
    }

    fn health(&self) -> BoxFuture<'_, PyValue> {
        Box::pin(async move {
            let path = PyValue::Str(self.inner.path.to_string_lossy().into_owned());
            match table_names(&self.inner.path) {
                Ok(t) => obj([
                    ("ok", PyValue::Bool(true)),
                    ("path", path),
                    ("tables", PyValue::Int(i64::try_from(t.len()).unwrap_or(0))),
                ]),
                Err(e) => obj([
                    ("ok", PyValue::Bool(false)),
                    ("path", path),
                    ("error", PyValue::str(hub_core::py::os::error_name(&e))),
                ]),
            }
        })
    }
}
