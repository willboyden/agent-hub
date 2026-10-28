//! Port of service.py: namespaces, ingest jobs, query, delete. Route handlers do authn/authz; this layer enforces the
//! data-shape rules (dims, caps, metadata validation) so they hold no matter who calls it.
//!
//! Concurrency (Python: one asyncio loop): at most 2 jobs run at once, write jobs and deletes on one namespace are
//! serialised by a per-namespace lock (FIFO), the chunk-cap check + reservation is atomic under one lock, and a job's
//! visible state changes (state + error + finished) happen together.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use futures::FutureExt;
use hub_core::PyValue;
use hub_core::common::now;
use regex::Regex;

use crate::chunking::chunk_text;
use crate::config::Config;
use crate::db::{Db, NsRow};
use crate::embed::Embedder;
use crate::errors::KnowledgeError;
use crate::filters::{FIELD, Node, parse_filter};
use crate::paths::{check_glob, iter_files, resolve_base};
use crate::pyjson::{obj, opt_f, opt_s};
use crate::stores::{Hit, NS_RE, Record, StoreError, VectorStore};

static DOC_ID_CHARS: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^[A-Za-z0-9._:@/-]{1,150}$"));
const RESERVED: [&str; 3] = ["doc_id", "chunk", "source"];
const TERMINAL: [&str; 2] = ["done", "failed"];

/// `DOC_ID.fullmatch(s)`: `(?!.*\.\.)(?!.*//)[A-Za-z0-9._:@/-]{1,150}` (no ".." anywhere: ids may look like paths).
pub fn doc_id_ok(s: &str) -> bool {
    DOC_ID_CHARS.is_match(s) && !s.contains("..") && !s.contains("//")
}

/// `KnowledgeError(e.status, e.code, str(e))` for a StoreError (an internal one stays the 500).
pub fn ke(e: StoreError) -> KnowledgeError {
    if e.internal {
        KnowledgeError::internal()
    } else {
        KnowledgeError::new(e.status, &e.code, e.message)
    }
}

/// A job's mutable fields.
#[derive(Debug, Clone, PartialEq)]
pub struct JobState {
    pub state: String,
    pub created: f64,
    pub started: Option<f64>,
    pub finished: Option<f64>,
    pub docs_total: i64,
    pub docs_done: i64,
    pub chunks: i64,
    pub skipped: i64,
    pub error: Option<String>,
    pub error_code: Option<String>,
    pub reserved: i64,
}

pub struct Job {
    pub id: String,
    pub ns: String,
    st: Mutex<JobState>,
}

impl Job {
    fn new(id: String, ns: String) -> Self {
        Self {
            id,
            ns,
            st: Mutex::new(JobState {
                state: "queued".into(),
                created: now(),
                started: None,
                finished: None,
                docs_total: 0,
                docs_done: 0,
                chunks: 0,
                skipped: 0,
                error: None,
                error_code: None,
                reserved: 0,
            }),
        }
    }

    pub fn snapshot(&self) -> JobState {
        self.st
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }

    fn update<T>(&self, f: impl FnOnce(&mut JobState) -> T) -> T {
        match self.st.lock() {
            Ok(mut g) => f(&mut g),
            Err(p) => f(&mut p.into_inner()),
        }
    }

    pub fn state(&self) -> String {
        self.snapshot().state
    }

    pub fn is_terminal(&self) -> bool {
        TERMINAL.contains(&self.state().as_str())
    }

    /// `job.view()`: `asdict(job)` without `reserved`.
    pub fn view(&self) -> PyValue {
        self.view_of(self.snapshot())
    }

    /// The view and the state from ONE snapshot (the SSE loop must not report a state its last event did not show).
    pub fn view_and_state(&self) -> (PyValue, String) {
        let s = self.snapshot();
        let state = s.state.clone();
        (self.view_of(s), state)
    }

    fn view_of(&self, s: JobState) -> PyValue {
        obj([
            ("id", PyValue::str(self.id.as_str())),
            ("ns", PyValue::str(self.ns.as_str())),
            ("state", PyValue::Str(s.state)),
            ("created", PyValue::Float(s.created)),
            ("started", opt_f(s.started)),
            ("finished", opt_f(s.finished)),
            ("docs_total", PyValue::Int(s.docs_total)),
            ("docs_done", PyValue::Int(s.docs_done)),
            ("chunks", PyValue::Int(s.chunks)),
            ("skipped", PyValue::Int(s.skipped)),
            ("error", opt_s(s.error.as_deref())),
            ("error_code", opt_s(s.error_code.as_deref())),
        ])
    }
}

/// `PreparedDoc(doc_id, chunks, meta)`.
#[derive(Debug, Clone)]
pub struct PreparedDoc {
    pub doc_id: String,
    pub chunks: Vec<String>,
    pub meta: Vec<(String, PyValue)>,
}

/// `clean_meta(meta)`.
pub fn clean_meta(meta: &PyValue) -> Result<Vec<(String, PyValue)>, KnowledgeError> {
    let empty = Vec::new();
    let items = meta.as_dict().unwrap_or(&empty);
    if items.len() > 32 {
        return Err(KnowledgeError::new(422, "invalid_metadata", "at most 32 metadata keys"));
    }
    let mut out = Vec::new();
    for (k, v) in items {
        let Some(key) = k.as_str().filter(|k| FIELD.is_match(k) && !RESERVED.contains(k)) else {
            return Err(KnowledgeError::new(
                422,
                "invalid_metadata",
                "metadata keys must match [A-Za-z_][A-Za-z0-9_]* and not be reserved",
            ));
        };
        let ok = match v {
            PyValue::Bool(_) | PyValue::Int(_) | PyValue::BigInt(_) | PyValue::Float(_) => true,
            PyValue::Str(s) => s.chars().count() <= 1024,
            _ => false,
        };
        if !ok {
            return Err(KnowledgeError::new(
                422,
                "invalid_metadata",
                "metadata values must be scalars (strings <= 1024 chars)",
            ));
        }
        out.push((key.to_string(), v.clone()));
    }
    Ok(out)
}

pub type MetricsHook = Arc<dyn Fn(&Job) + Send + Sync>;

pub struct Service {
    pub cfg: Arc<Config>,
    pub db: Arc<Db>,
    pub stores: Vec<(String, Arc<dyn VectorStore>)>,
    pub embedder: Arc<dyn Embedder>,
    jobs: Mutex<Vec<Arc<Job>>>,
    sem: Arc<tokio::sync::Semaphore>,
    ns_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    cap_lock: tokio::sync::Mutex<()>,
    reserved: Mutex<HashMap<String, i64>>,
    pub metrics_hook: Option<MetricsHook>,
}

type JobResult = Result<(), KnowledgeError>;

impl Service {
    pub fn new(
        cfg: Arc<Config>,
        db: Arc<Db>,
        stores: Vec<(String, Arc<dyn VectorStore>)>,
        embedder: Arc<dyn Embedder>,
        metrics_hook: Option<MetricsHook>,
    ) -> Self {
        Self {
            cfg,
            db,
            stores,
            embedder,
            jobs: Mutex::new(Vec::new()),
            sem: Arc::new(tokio::sync::Semaphore::new(2)),
            ns_locks: Mutex::new(HashMap::new()),
            cap_lock: tokio::sync::Mutex::new(()),
            reserved: Mutex::new(HashMap::new()),
            metrics_hook,
        }
    }

    pub fn store(&self, name: &str) -> Option<Arc<dyn VectorStore>> {
        self.stores.iter().find(|(n, _)| n == name).map(|(_, s)| s.clone())
    }

    // --- namespaces ----------------------------------------------------------------------------------------------
    pub fn ns_row(&self, ns: &str) -> Result<NsRow, KnowledgeError> {
        match self.db.ns_row(ns) {
            Ok(Some(r)) => Ok(r),
            Ok(None) => Err(KnowledgeError::new(404, "namespace_not_found", "no such namespace")),
            Err(_) => Err(KnowledgeError::internal()),
        }
    }

    pub fn store_for(&self, row: &NsRow) -> Result<Arc<dyn VectorStore>, KnowledgeError> {
        self.store(&row.backend).ok_or_else(|| {
            KnowledgeError::new(
                503,
                "backend_unavailable",
                format!("backend {} is not configured", row.backend),
            )
        })
    }

    pub async fn create_namespace(
        &self,
        name: &str,
        backend: &str,
        model: Option<&str>,
        dim: i64,
        description: &str,
    ) -> Result<PyValue, KnowledgeError> {
        if !NS_RE.is_match(name) {
            return Err(KnowledgeError::new(
                422,
                "invalid_namespace",
                "namespace must match [a-z0-9][a-z0-9_-]{0,47}",
            ));
        }
        if !(1..=8192).contains(&dim) {
            return Err(KnowledgeError::new(422, "invalid_dim", "dim must be 1..8192"));
        }
        let Some(store) = self.store(backend) else {
            return Err(KnowledgeError::new(
                422,
                "backend_unavailable",
                format!("backend {backend} is not configured"),
            ));
        };
        if self.db.ns_row(name).map_err(|_| KnowledgeError::internal())?.is_some() {
            return Err(KnowledgeError::new(409, "namespace_exists", "namespace already exists"));
        }
        store.ensure(name, dim).await.map_err(ke)?;
        self.db
            .ns_insert(&NsRow {
                name: name.into(),
                backend: backend.into(),
                embedding_model: model.map_or_else(|| self.cfg.default_embedding_model.clone(), str::to_string),
                dim,
                description: description.into(),
                created: now(),
            })
            .map_err(|_| KnowledgeError::internal())?;
        self.ns_view(name).await
    }

    /// `ns_view(name)`: a backend that is down still lists the namespace (count/disk None).
    pub async fn ns_view(&self, name: &str) -> Result<PyValue, KnowledgeError> {
        let row = self.ns_row(name)?;
        let (count, disk, est) = match self.store_for(&row) {
            Ok(store) => match store.stats(name).await {
                Ok(st) => (
                    PyValue::Int(st.count),
                    st.disk_bytes.map_or(PyValue::None, PyValue::Int),
                    st.disk_bytes_estimated,
                ),
                Err(e) if e.internal => return Err(KnowledgeError::internal()),
                Err(_) => (PyValue::None, PyValue::None, false),
            },
            Err(_) => (PyValue::None, PyValue::None, false),
        };
        Ok(obj([
            ("name", PyValue::Str(row.name)),
            ("backend", PyValue::Str(row.backend)),
            ("embedding_model", PyValue::Str(row.embedding_model)),
            ("dim", PyValue::Int(row.dim)),
            ("description", PyValue::Str(row.description)),
            ("created", PyValue::Float(row.created)),
            ("count", count),
            ("disk_bytes", disk),
            ("disk_bytes_estimated", PyValue::Bool(est)),
        ]))
    }

    pub async fn drop_namespace(&self, name: &str) -> Result<(), KnowledgeError> {
        let row = self.ns_row(name)?;
        self.store_for(&row)?.drop_ns(name).await.map_err(ke)?;
        self.db.ns_delete(name).map_err(|_| KnowledgeError::internal())?;
        self.db.drop_ns_scopes(name).map_err(|_| KnowledgeError::internal())?;
        Ok(())
    }

    // --- embedding helpers ---------------------------------------------------------------------------------------
    async fn embed(&self, row: &NsRow, texts: &[String]) -> Result<Vec<Vec<f64>>, KnowledgeError> {
        let vecs = self.embedder.embed(texts, &row.embedding_model).await.map_err(|e| {
            if e.internal {
                KnowledgeError::internal()
            } else {
                KnowledgeError::new(e.status, "embedding_failed", e.message)
            }
        })?;
        if vecs.iter().any(|v| i64::try_from(v.len()).ok() != Some(row.dim)) {
            return Err(KnowledgeError::new(
                409,
                "dim_mismatch",
                format!(
                    "embedding model {} returned a different dimension than the namespace",
                    row.embedding_model
                ),
            ));
        }
        Ok(vecs)
    }

    // --- query ---------------------------------------------------------------------------------------------------
    #[allow(clippy::too_many_arguments)]
    pub async fn query(
        &self,
        ns: &str,
        text: Option<&str>,
        vector: Option<Vec<f64>>,
        k: i64,
        flt_raw: Option<&PyValue>,
        min_score: Option<f64>,
        model: Option<&str>,
    ) -> Result<Vec<Hit>, KnowledgeError> {
        let row = self.ns_row(ns)?;
        if text.is_none() == vector.is_none() {
            return Err(KnowledgeError::new(
                422,
                "invalid_query",
                "send exactly one of text or vector",
            ));
        }
        if let Some(m) = model
            && m != row.embedding_model
        {
            return Err(KnowledgeError::new(
                409,
                "model_mismatch",
                format!("namespace is embedded with {}", row.embedding_model),
            ));
        }
        if !(1..=self.cfg.max_k).contains(&k) {
            return Err(KnowledgeError::new(
                422,
                "invalid_k",
                format!("k must be 1..{}", self.cfg.max_k),
            ));
        }
        let flt = match flt_raw {
            Some(raw) => Some(
                parse_filter(raw, usize::try_from(self.cfg.max_filter_nodes.max(0)).unwrap_or(0))
                    .map_err(|e| KnowledgeError::new(422, "invalid_filter", e.0))?,
            ),
            None => None,
        };
        let vector = match text {
            Some(t) => {
                if i64::try_from(t.chars().count()).unwrap_or(i64::MAX) > self.cfg.max_query_chars {
                    return Err(KnowledgeError::new(413, "query_too_long", "query text too long"));
                }
                let mut v = self.embed(&row, &[t.to_string()]).await?;
                if v.is_empty() {
                    return Err(KnowledgeError::internal()); // `[0]` of an empty list
                }
                v.swap_remove(0)
            }
            None => vector.unwrap_or_default(),
        };
        if i64::try_from(vector.len()).ok() != Some(row.dim) {
            return Err(KnowledgeError::new(
                409,
                "dim_mismatch",
                format!("vector has {} dims; namespace expects {}", vector.len(), row.dim),
            ));
        }
        self.store_for(&row)?
            .query(ns, &vector, k, flt.as_ref(), min_score)
            .await
            .map_err(ke)
    }

    // --- delete --------------------------------------------------------------------------------------------------
    pub async fn delete_document(&self, ns: &str, doc_id: &str) -> Result<i64, KnowledgeError> {
        let row = self.ns_row(ns)?;
        if !doc_id_ok(doc_id) {
            return Err(KnowledgeError::new(422, "invalid_id", "invalid document id"));
        }
        let lock = self.ns_lock(ns);
        let _g = lock.lock().await;
        self.store_for(&row)?
            .delete(ns, &Node::eq_str("doc_id", doc_id))
            .await
            .map_err(ke)
    }

    // --- ingest --------------------------------------------------------------------------------------------------
    /// `prepare_inline(docs, size, overlap)` over the validated `DocIn` dicts.
    pub fn prepare_inline(
        &self,
        docs: &[PyValue],
        size: i64,
        overlap: i64,
    ) -> Result<Vec<PreparedDoc>, KnowledgeError> {
        let max_docs = self.cfg.max_docs_per_request;
        if docs.is_empty() || i64::try_from(docs.len()).unwrap_or(i64::MAX) > max_docs {
            return Err(KnowledgeError::new(
                422,
                "invalid_documents",
                format!("send 1..{max_docs} documents"),
            ));
        }
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for d in docs {
            let text = d.get("text").and_then(PyValue::as_str).unwrap_or_default();
            let max_chars = self.cfg.max_doc_chars;
            if hub_core::py::text::strip(text).is_empty()
                || i64::try_from(text.chars().count()).unwrap_or(i64::MAX) > max_chars
            {
                return Err(KnowledgeError::new(
                    422,
                    "invalid_documents",
                    format!("document text must be 1..{max_chars} chars"),
                ));
            }
            let did = match d.get("id").and_then(PyValue::as_str) {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => hub_core::common::sha256(text.as_bytes())[..16].to_string(),
            };
            if !doc_id_ok(&did) || seen.contains(&did) {
                return Err(KnowledgeError::new(
                    422,
                    "invalid_id",
                    "document ids must match [A-Za-z0-9._:@/-]{1,150} and be unique",
                ));
            }
            seen.insert(did.clone());
            let meta = clean_meta(d.get("metadata").unwrap_or(&PyValue::None))?;
            out.push(PreparedDoc {
                doc_id: did,
                chunks: chunk(text, size, overlap)?,
                meta,
            });
        }
        Ok(out)
    }

    pub fn ns_lock(&self, ns: &str) -> Arc<tokio::sync::Mutex<()>> {
        match self.ns_locks.lock() {
            Ok(mut m) => m.entry(ns.to_string()).or_default().clone(),
            Err(p) => p.into_inner().entry(ns.to_string()).or_default().clone(),
        }
    }

    /// `reserve(ns, new_chunks)`: atomically check the namespace chunk cap and reserve room for a write.
    pub async fn reserve(&self, ns: &str, new_chunks: i64) -> Result<(), KnowledgeError> {
        let row = self.ns_row(ns)?;
        let _g = self.cap_lock.lock().await;
        let have = self.store_for(&row)?.stats(ns).await.map_err(ke)?.count;
        let pending = self.reserved_for(ns);
        if have + pending + new_chunks > self.cfg.max_chunks_per_namespace {
            return Err(KnowledgeError::new(
                409,
                "namespace_full",
                "namespace chunk limit reached",
            ));
        }
        if let Ok(mut r) = self.reserved.lock() {
            *r.entry(ns.to_string()).or_insert(0) += new_chunks;
        }
        Ok(())
    }

    pub fn reserved_for(&self, ns: &str) -> i64 {
        self.reserved.lock().map_or(0, |r| r.get(ns).copied().unwrap_or(0))
    }

    /// All outstanding reservations (tests: `svc._reserved == {}`).
    pub fn reservations(&self) -> HashMap<String, i64> {
        self.reserved.lock().map(|r| r.clone()).unwrap_or_default()
    }

    pub fn release(&self, ns: &str, n: i64) {
        if let Ok(mut r) = self.reserved.lock() {
            let left = r.get(ns).copied().unwrap_or(0) - n;
            if left > 0 {
                r.insert(ns.to_string(), left);
            } else {
                r.remove(ns);
            }
        }
    }

    pub fn jobs(&self) -> Vec<Arc<Job>> {
        self.jobs.lock().map(|j| j.clone()).unwrap_or_default()
    }

    pub fn job(&self, id: &str) -> Option<Arc<Job>> {
        self.jobs().into_iter().find(|j| j.id == id)
    }

    /// `start_job(...)`; returns the job and its view taken before the task can run (Python returns `job.view()`
    /// before the event loop schedules the task).
    #[allow(clippy::too_many_arguments)]
    pub fn start_job(
        self: &Arc<Self>,
        ns: &str,
        docs: Option<Vec<PreparedDoc>>,
        path: Option<String>,
        glob: &str,
        size: i64,
        overlap: i64,
        reserved: i64,
    ) -> Result<(Arc<Job>, PyValue), KnowledgeError> {
        let job = {
            let Ok(mut jobs) = self.jobs.lock() else {
                return Err(KnowledgeError::internal());
            };
            if jobs.iter().filter(|j| !j.is_terminal()).count() >= 16 {
                drop(jobs);
                self.release(ns, reserved);
                return Err(KnowledgeError::new(429, "too_many_jobs", "too many active ingest jobs"));
            }
            let job = Arc::new(Job::new(hub_core::common::token_hex(6), ns.to_string()));
            job.update(|s| s.reserved = reserved);
            jobs.push(job.clone());
            let keep = usize::try_from(self.cfg.max_jobs_kept.max(0)).unwrap_or(0);
            while jobs.len() > keep {
                let Some(i) = jobs.iter().position(|j| j.is_terminal()) else {
                    break;
                };
                jobs.remove(i);
            }
            job
        };
        let view = job.view();
        let svc = self.clone();
        let j = job.clone();
        let glob = glob.to_string();
        tokio::spawn(async move { svc.run_job(j, docs, path, glob, size, overlap).await });
        Ok((job, view))
    }

    async fn run_job(
        self: Arc<Self>,
        job: Arc<Job>,
        docs: Option<Vec<PreparedDoc>>,
        path: Option<String>,
        glob: String,
        size: i64,
        overlap: i64,
    ) {
        let Ok(_permit) = self.sem.clone().acquire_owned().await else {
            return;
        };
        let lock = self.ns_lock(&job.ns);
        let _g = lock.lock().await; // one write job at a time per namespace: no interleaved replace
        job.update(|s| {
            s.state = "running".into();
            s.started = Some(now());
        });
        let body = std::panic::AssertUnwindSafe(self.job_body(&job, docs, path, &glob, size, overlap))
            .catch_unwind()
            .await;
        let outcome: JobResult = body.unwrap_or_else(|_| Err(KnowledgeError::internal()));
        let reserved = job.snapshot().reserved;
        self.release(&job.ns, reserved);
        job.update(|s| {
            match &outcome {
                Ok(()) => s.state = "done".into(),
                Err(e) => {
                    s.state = "failed".into();
                    s.error = Some(e.detail.clone());
                    s.error_code = Some(e.code.clone());
                }
            }
            s.finished = Some(now());
        });
        if let Some(hook) = &self.metrics_hook {
            hook(&job);
        }
    }

    async fn job_body(
        &self,
        job: &Job,
        docs: Option<Vec<PreparedDoc>>,
        path: Option<String>,
        glob: &str,
        size: i64,
        overlap: i64,
    ) -> JobResult {
        let row = self.ns_row(&job.ns)?;
        let store = self.store_for(&row)?;
        match docs {
            Some(docs) => {
                job.update(|s| s.docs_total = i64::try_from(docs.len()).unwrap_or(0));
                for d in docs {
                    self.ingest_doc(job, &row, &store, d, false).await?;
                }
            }
            None => {
                let path = path.unwrap_or_default();
                self.ingest_path(job, &row, &store, &path, glob, size, overlap).await?;
            }
        }
        Ok(())
    }

    async fn ingest_doc(
        &self,
        job: &Job,
        row: &NsRow,
        store: &Arc<dyn VectorStore>,
        d: PreparedDoc,
        reserve: bool,
    ) -> JobResult {
        if !d.chunks.is_empty() {
            let n = i64::try_from(d.chunks.len()).unwrap_or(0);
            if reserve {
                // path jobs only learn their size file by file; inline jobs reserved up front
                self.reserve(&job.ns, n).await?;
                job.update(|s| s.reserved += n);
            }
            let r = self.write_doc(job, row, store, &d).await;
            // committed (or failed): the store's own count is now the truth, so drop this doc's reservation
            self.release(&job.ns, n);
            job.update(|s| s.reserved = (s.reserved - n).max(0));
            r?;
        }
        job.update(|s| s.docs_done += 1);
        Ok(())
    }

    async fn write_doc(&self, job: &Job, row: &NsRow, store: &Arc<dyn VectorStore>, d: &PreparedDoc) -> JobResult {
        let vecs = self.embed(row, &d.chunks).await?;
        // replace semantics: drop the document's previous chunks first, so a shrinking doc leaves no stale ones
        store
            .delete(&job.ns, &Node::eq_str("doc_id", &d.doc_id))
            .await
            .map_err(ke)?;
        if vecs.len() != d.chunks.len() {
            return Err(KnowledgeError::internal()); // zip(..., strict=True)
        }
        let recs: Vec<Record> = d
            .chunks
            .iter()
            .zip(vecs)
            .enumerate()
            .map(|(i, (t, v))| {
                let mut meta: Vec<(PyValue, PyValue)> = d
                    .meta
                    .iter()
                    .map(|(k, x)| (PyValue::str(k.as_str()), x.clone()))
                    .collect();
                meta.push((PyValue::str("doc_id"), PyValue::str(d.doc_id.as_str())));
                meta.push((PyValue::str("chunk"), PyValue::Int(i64::try_from(i).unwrap_or(0))));
                Record {
                    id: format!("{}#{i}", d.doc_id),
                    text: t.clone(),
                    vector: v,
                    meta: PyValue::Dict(meta),
                }
            })
            .collect();
        let n = i64::try_from(recs.len()).unwrap_or(0);
        store.upsert(&job.ns, recs).await.map_err(ke)?;
        job.update(|s| s.chunks += n);
        Ok(())
    }

    /// `plan_path(path, glob)` -> (base, root, glob).
    pub fn plan_path(&self, path: &str, glob: &str) -> Result<(PathBuf, PathBuf, String), KnowledgeError> {
        let (base, root) = resolve_base(path, &self.cfg.ingest_roots)?;
        Ok((base, root, check_glob(glob)?))
    }

    #[allow(clippy::too_many_arguments)]
    async fn ingest_path(
        &self,
        job: &Job,
        row: &NsRow,
        store: &Arc<dyn VectorStore>,
        path: &str,
        glob: &str,
        size: i64,
        overlap: i64,
    ) -> JobResult {
        // re-resolve at run time: the tree may have changed
        let (base, root, glob) = self.plan_path(path, glob)?;
        let max_files = self.cfg.max_files_per_job;
        let root2 = root.clone();
        let files = tokio::task::spawn_blocking(move || iter_files(&base, &root2, &glob, max_files))
            .await
            .map_err(|_| KnowledgeError::internal())??;
        job.update(|s| s.docs_total = i64::try_from(files.len()).unwrap_or(0));
        let mut total_bytes: i64 = 0;
        for f in files {
            let f2 = f.clone();
            let Ok(Ok(raw)) = tokio::task::spawn_blocking(move || std::fs::read(&f2)).await else {
                job.update(|s| s.skipped += 1);
                continue;
            };
            total_bytes += i64::try_from(raw.len()).unwrap_or(i64::MAX);
            if total_bytes > self.cfg.max_job_bytes {
                return Err(KnowledgeError::new(413, "job_too_large", "ingest byte cap reached"));
            }
            if raw[..raw.len().min(4096)].contains(&0) {
                job.update(|s| s.skipped += 1);
                continue;
            }
            let rel = rel_str(&f, &root);
            let did = if doc_id_ok(&rel) {
                rel.clone()
            } else {
                format!("h_{}", &hub_core::common::sha256(rel.as_bytes())[..16])
            };
            let decoded = String::from_utf8_lossy(&raw);
            let max_chars = usize::try_from(self.cfg.max_doc_chars.max(0)).unwrap_or(0);
            let text: String = decoded.chars().take(max_chars).collect();
            let chunks = chunk(&text, size, overlap)?;
            let source: String = rel.chars().take(1024).collect();
            let d = PreparedDoc {
                doc_id: did,
                chunks,
                meta: vec![("source".into(), PyValue::Str(source))],
            };
            self.ingest_doc(job, row, store, d, true).await?;
        }
        Ok(())
    }
}

fn rel_str(f: &Path, root: &Path) -> String {
    f.strip_prefix(root).unwrap_or(f).to_string_lossy().into_owned()
}

/// `_chunk(text, size, overlap)`.
fn chunk(text: &str, size: i64, overlap: i64) -> Result<Vec<String>, KnowledgeError> {
    let s = usize::try_from(size).unwrap_or(0);
    let o = usize::try_from(overlap).unwrap_or(usize::MAX);
    chunk_text(text, s, o).map_err(|m| KnowledgeError::new(422, "invalid_chunking", m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_id_rules() {
        assert!(doc_id_ok("a/b:c@d.e-f_1"));
        assert!(!doc_id_ok("a//b"));
        assert!(!doc_id_ok("../x"));
        assert!(!doc_id_ok("bad id!"));
        assert!(!doc_id_ok(""));
        assert!(!doc_id_ok(&"x".repeat(151)));
    }
}
