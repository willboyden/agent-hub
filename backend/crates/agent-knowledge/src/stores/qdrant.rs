//! Port of stores/qdrant.py: Qdrant over plain REST (reqwest, rustls). One collection per namespace (`ak_<ns>`), HNSW
//! graph and (optionally) vectors on disk. Filters are translated from the validated AST into Qdrant's structured JSON
//! filter; no string building. The API key comes from the env var NAMED in the config; a Qdrant that answers without a
//! key is refused (re-probed at most every 30 s) unless KNOWLEDGE_ALLOW_UNAUTH_QDRANT=1. Error messages never relay
//! the upstream body. Request bodies are httpx's JSON (compact, UTF-8, `allow_nan=False`).

use std::sync::Mutex;
use std::time::Instant;

use futures::future::BoxFuture;
use hub_core::PyValue;

use super::{Hit, Record, StoreError, StoreResult, StoreStats, VectorStore, check_ids};
use crate::embed::{http_client, py_float_str, transport_error_name};
use crate::env::{self, Env};
use crate::filters::{Node, py_eq, to_qdrant};
use crate::pyjson::{self, Style, obj};

const UUID_NS: [u8; 16] = [
    0x6f, 0x1a, 0x4c, 0x1e, 0x3b, 0x52, 0x4d, 0x6a, 0x9f, 0x0e, 0x6b, 0x1b, 0x6a, 0x1f, 0x7a, 0x10,
];

pub const REFUSAL: &str = "qdrant answers without an API key, so any local process could bypass token scopes; enable \
QDRANT__SERVICE__API_KEY (or set KNOWLEDGE_ALLOW_UNAUTH_QDRANT=1 to accept the risk)";

/// SHA-1 (FIPS 180-4), only for `uuid.uuid5`.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

/// `point_id(rid)` = `str(uuid.uuid5(_UUID_NS, rid))`.
pub fn point_id(rid: &str) -> String {
    let mut data = UUID_NS.to_vec();
    data.extend_from_slice(rid.as_bytes());
    let d = sha1(&data);
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// `QdrantStore.collection(ns)`.
pub fn collection(ns: &str) -> String {
    format!("ak_{ns}")
}

pub struct QdrantStore {
    url: String,
    key_env: Option<String>,
    vec_disk: bool,
    pub allow_unauth: bool,
    probe: Mutex<(Option<Instant>, Option<bool>)>,
    client: reqwest::Client,
    env: Env,
}

/// Python truthiness of a JSON value.
fn truthy(v: &PyValue) -> bool {
    match v {
        PyValue::None => false,
        PyValue::Bool(b) => *b,
        PyValue::Int(i) => *i != 0,
        PyValue::Float(f) => *f != 0.0,
        PyValue::Str(s) => !s.is_empty(),
        PyValue::List(l) | PyValue::Set(l) => !l.is_empty(),
        PyValue::Dict(d) => !d.is_empty(),
        PyValue::BigInt(_) | PyValue::Bytes(_) | PyValue::Date { .. } | PyValue::DateTime(_) => true,
    }
}

/// `(x or {})` followed by `.get(...)`: a falsy value acts as `{}`, a truthy non-dict raises AttributeError.
fn or_dict(v: Option<&PyValue>) -> Result<Option<&PyValue>, StoreError> {
    match v {
        None => Ok(None),
        Some(x) if !truthy(x) => Ok(None),
        Some(x @ PyValue::Dict(_)) => Ok(Some(x)),
        Some(_) => Err(StoreError::internal()),
    }
}

fn dget<'a>(d: Option<&'a PyValue>, key: &str) -> Option<&'a PyValue> {
    d.and_then(|x| x.get(key))
}

/// Python `int(x)`; `Err` = an exception the caller does not catch.
fn py_int(v: &PyValue) -> Result<i64, StoreError> {
    match v {
        PyValue::Int(i) => Ok(*i),
        PyValue::Bool(b) => Ok(i64::from(*b)),
        #[allow(clippy::cast_possible_truncation)]
        PyValue::Float(f) if f.is_finite() && f.abs() < 9.2e18 => Ok(f.trunc() as i64),
        PyValue::Str(s) => hub_core::py::text::int_str(s)
            .and_then(|i| i64::try_from(i).ok())
            .ok_or_else(StoreError::internal),
        _ => Err(StoreError::internal()),
    }
}

/// Python `float(x)` inside a caught `(KeyError, TypeError, ValueError)`: `Ok(None)` = caught, `Err` = not caught.
fn py_float(v: &PyValue) -> Result<Option<f64>, StoreError> {
    match v {
        PyValue::Float(f) => Ok(Some(*f)),
        #[allow(clippy::cast_precision_loss)]
        PyValue::Int(i) => Ok(Some(*i as f64)),
        PyValue::Bool(b) => Ok(Some(f64::from(u8::from(*b)))),
        PyValue::BigInt(s) => match s.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Some(f)),
            _ => Err(StoreError::internal()),
        },
        PyValue::Str(s) => Ok(py_float_str(s).ok()),
        _ => Ok(None),
    }
}

fn has_non_finite(v: &PyValue) -> bool {
    match v {
        PyValue::Float(f) => !f.is_finite(),
        PyValue::List(l) | PyValue::Set(l) => l.iter().any(has_non_finite),
        PyValue::Dict(d) => d.iter().any(|(_, x)| has_non_finite(x)),
        _ => false,
    }
}

impl QdrantStore {
    /// `QdrantStore(url, api_key_env, vectors_on_disk, timeout=..., allow_unauth=...)`.
    pub fn new(
        url: &str,
        api_key_env: Option<&str>,
        vectors_on_disk: bool,
        timeout_s: f64,
        allow_unauth: bool,
        env: Env,
    ) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            key_env: api_key_env.map(str::to_string),
            vec_disk: vectors_on_disk,
            allow_unauth,
            probe: Mutex::new((None, None)),
            client: http_client(timeout_s),
            env,
        }
    }

    fn key(&self) -> String {
        self.key_env
            .as_deref()
            .map(|n| env::get(&self.env, n))
            .unwrap_or_default()
    }

    pub fn key_configured(&self) -> bool {
        !self.key().is_empty()
    }

    /// `probe_unauthenticated()`: does Qdrant answer a data-plane request WITHOUT a key? (`None` = unreachable)
    pub async fn probe_unauthenticated(&self) -> Option<bool> {
        match self.client.get(format!("{}/collections", self.url)).send().await {
            Ok(r) => Some(r.status().as_u16() == 200),
            Err(_) => None,
        }
    }

    /// `refusal()`: why this backend is refused, or None.
    pub async fn refusal_reason(&self) -> Option<String> {
        if self.allow_unauth {
            return None;
        }
        let stale = self
            .probe
            .lock()
            .map(|g| g.0.is_none_or(|t| t.elapsed().as_secs_f64() > 30.0))
            .unwrap_or(true);
        if stale {
            let r = self.probe_unauthenticated().await;
            if let Ok(mut g) = self.probe.lock() {
                *g = (Some(Instant::now()), r);
            }
        }
        let result = self.probe.lock().ok().and_then(|g| g.1);
        (result == Some(true)).then(|| REFUSAL.to_string())
    }

    /// `_req(method, path, ok_404=..., params=..., json=...)`.
    async fn req(
        &self,
        method: reqwest::Method,
        path: &str,
        ok_404: bool,
        wait: bool,
        body: Option<&PyValue>,
    ) -> Result<Option<PyValue>, StoreError> {
        if let Some(why) = self.refusal_reason().await {
            return Err(StoreError::new(why, "qdrant_unauthenticated", 503));
        }
        let mut url = format!("{}{path}", self.url);
        if wait {
            url.push_str("?wait=true");
        }
        let mut rb = self.client.request(method, url);
        let key = self.key();
        if !key.is_empty() {
            rb = rb.header("api-key", key);
        }
        if let Some(b) = body {
            if has_non_finite(b) {
                return Err(StoreError::internal()); // json.dumps(allow_nan=False) raises ValueError
            }
            rb = rb
                .header("content-type", "application/json")
                .body(pyjson::dumps(b, Style::Compact));
        }
        let r = match rb.send().await {
            Ok(r) => r,
            Err(e) if e.is_builder() => return Err(StoreError::internal()),
            Err(e) => {
                return Err(StoreError::backend(format!(
                    "qdrant unreachable ({})",
                    transport_error_name(&e)
                )));
            }
        };
        let code = r.status().as_u16();
        if code == 404 && ok_404 {
            return Ok(None);
        }
        if code >= 400 {
            return Err(StoreError::backend(format!("qdrant returned HTTP {code}")));
        }
        let bytes = r
            .bytes()
            .await
            .map_err(|e| StoreError::backend(format!("qdrant unreachable ({})", transport_error_name(&e))))?;
        match pyjson::loads_bytes(&bytes) {
            Ok(v @ PyValue::Dict(_)) => Ok(Some(v)),
            Ok(_) => Ok(Some(PyValue::Dict(Vec::new()))),
            Err(e) if e.is_value_error() => Err(StoreError::backend("qdrant returned non-JSON")),
            Err(_) => Err(StoreError::internal()),
        }
    }

    async fn count(&self, ns: &str, flt: Option<&Node>) -> Result<i64, StoreError> {
        let mut body = vec![(PyValue::str("exact"), PyValue::Bool(true))];
        if let Some(f) = flt {
            body.push((
                PyValue::str("filter"),
                to_qdrant(f).map_err(|e| StoreError::new(e.0, "invalid_filter", 422))?,
            ));
        }
        let res = self
            .req(
                reqwest::Method::POST,
                &format!("/collections/{}/points/count", collection(ns)),
                false,
                false,
                Some(&PyValue::Dict(body)),
            )
            .await?;
        let result = or_dict(dget(or_dict(res.as_ref())?, "result"))?;
        match dget(result, "count") {
            None => Ok(0),
            Some(v) => py_int(v),
        }
    }
}

impl VectorStore for QdrantStore {
    fn name(&self) -> &'static str {
        "qdrant"
    }

    fn ensure<'a>(&'a self, ns: &'a str, dim: i64) -> StoreResult<'a, ()> {
        Box::pin(async move {
            let c = collection(ns);
            let info = self
                .req(reqwest::Method::GET, &format!("/collections/{c}"), true, false, None)
                .await?;
            if let Some(info) = info {
                let result = or_dict(info.get("result"))?;
                let config = or_dict(dget(result, "config"))?;
                let params = or_dict(dget(config, "params"))?;
                let size = match dget(params, "vectors") {
                    None => None,
                    Some(v @ PyValue::Dict(_)) => v.get("size"),
                    Some(_) => return Err(StoreError::internal()),
                };
                let same = size.is_some_and(|s| py_eq(s, &PyValue::Int(dim)));
                if !same {
                    return Err(StoreError::dim_mismatch(
                        "existing qdrant collection has a different dimension",
                    ));
                }
                return Ok(());
            }
            let body = obj([
                (
                    "vectors",
                    obj([
                        ("size", PyValue::Int(dim)),
                        ("distance", PyValue::str("Cosine")),
                        ("on_disk", PyValue::Bool(self.vec_disk)),
                    ]),
                ),
                ("hnsw_config", obj([("on_disk", PyValue::Bool(true))])),
                ("on_disk_payload", PyValue::Bool(true)),
            ]);
            self.req(
                reqwest::Method::PUT,
                &format!("/collections/{c}"),
                false,
                false,
                Some(&body),
            )
            .await?;
            // index the fields we always filter on
            let idx = obj([
                ("field_name", PyValue::str("meta.doc_id")),
                ("field_schema", PyValue::str("keyword")),
            ]);
            self.req(
                reqwest::Method::PUT,
                &format!("/collections/{c}/index"),
                false,
                false,
                Some(&idx),
            )
            .await?;
            Ok(())
        })
    }

    fn upsert<'a>(&'a self, ns: &'a str, records: Vec<Record>) -> StoreResult<'a, usize> {
        Box::pin(async move {
            check_ids(records.iter().map(|r| r.id.as_str()))?;
            let c = collection(ns);
            for chunk in records.chunks(128) {
                let pts: Vec<PyValue> = chunk
                    .iter()
                    .map(|r| {
                        obj([
                            ("id", PyValue::Str(point_id(&r.id))),
                            (
                                "vector",
                                PyValue::List(r.vector.iter().map(|x| PyValue::Float(*x)).collect()),
                            ),
                            (
                                "payload",
                                obj([
                                    ("_id", PyValue::str(r.id.as_str())),
                                    ("text", PyValue::str(r.text.as_str())),
                                    ("meta", r.meta.clone()),
                                ]),
                            ),
                        ])
                    })
                    .collect();
                let body = obj([("points", PyValue::List(pts))]);
                self.req(
                    reqwest::Method::PUT,
                    &format!("/collections/{c}/points"),
                    false,
                    true,
                    Some(&body),
                )
                .await?;
            }
            Ok(records.len())
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
            let mut body = vec![
                (
                    PyValue::str("query"),
                    PyValue::List(vector.iter().map(|x| PyValue::Float(*x)).collect()),
                ),
                (PyValue::str("limit"), PyValue::Int(k)),
                (PyValue::str("with_payload"), PyValue::Bool(true)),
            ];
            if let Some(f) = flt {
                body.push((
                    PyValue::str("filter"),
                    to_qdrant(f).map_err(|e| StoreError::new(e.0, "invalid_filter", 422))?,
                ));
            }
            if let Some(m) = min_score {
                body.push((PyValue::str("score_threshold"), PyValue::Float(m)));
            }
            let res = self
                .req(
                    reqwest::Method::POST,
                    &format!("/collections/{}/points/query", collection(ns)),
                    false,
                    false,
                    Some(&PyValue::Dict(body)),
                )
                .await?;
            let unexpected = || StoreError::backend("qdrant returned an unexpected query response");
            let result = or_dict(dget(or_dict(res.as_ref())?, "result"))?;
            let pts = match dget(result, "points") {
                Some(p) if truthy(p) => p,
                _ => return Ok(Vec::new()),
            };
            let Some(list) = pts.as_list() else {
                return Err(unexpected());
            };
            let mut hits = Vec::with_capacity(list.len());
            for p in list {
                let Some(payload @ PyValue::Dict(_)) = p.get("payload") else {
                    return Err(unexpected());
                };
                let Some(id) = payload.get("_id") else {
                    return Err(unexpected());
                };
                let Some(score) = p.get("score") else {
                    return Err(unexpected());
                };
                let Some(score) = py_float(score)? else {
                    return Err(unexpected());
                };
                hits.push(Hit {
                    id: id.clone(),
                    score,
                    text: payload.get("text").cloned().unwrap_or_else(|| PyValue::str("")),
                    meta: payload.get("meta").cloned().unwrap_or(PyValue::Dict(Vec::new())),
                });
            }
            Ok(hits)
        })
    }

    fn delete<'a>(&'a self, ns: &'a str, flt: &'a Node) -> StoreResult<'a, i64> {
        Box::pin(async move {
            let n = self.count(ns, Some(flt)).await?;
            let body = obj([(
                "filter",
                to_qdrant(flt).map_err(|e| StoreError::new(e.0, "invalid_filter", 422))?,
            )]);
            self.req(
                reqwest::Method::POST,
                &format!("/collections/{}/points/delete", collection(ns)),
                false,
                true,
                Some(&body),
            )
            .await?;
            Ok(n)
        })
    }

    fn stats<'a>(&'a self, ns: &'a str) -> StoreResult<'a, StoreStats> {
        Box::pin(async move {
            let info = self
                .req(
                    reqwest::Method::GET,
                    &format!("/collections/{}", collection(ns)),
                    false,
                    false,
                    None,
                )
                .await?;
            let res = or_dict(dget(or_dict(info.as_ref())?, "result"))?;
            let n = match dget(res, "points_count") {
                Some(v) if truthy(v) => py_int(v)?,
                _ => 0,
            };
            let vectors = or_dict(dget(or_dict(dget(or_dict(dget(res, "config"))?, "params"))?, "vectors"))?;
            let dim = match dget(vectors, "size") {
                Some(v) if truthy(v) => py_int(v)?,
                _ => 0,
            };
            // Qdrant's REST API exposes no on-disk size; this is vectors + a rough graph/payload overhead.
            Ok(StoreStats {
                count: n,
                disk_bytes: Some(n.saturating_mul(dim.saturating_mul(4).saturating_add(1024))),
                disk_bytes_estimated: true,
            })
        })
    }

    fn drop_ns<'a>(&'a self, ns: &'a str) -> StoreResult<'a, ()> {
        Box::pin(async move {
            self.req(
                reqwest::Method::DELETE,
                &format!("/collections/{}", collection(ns)),
                true,
                false,
                None,
            )
            .await?;
            Ok(())
        })
    }

    fn health(&self) -> BoxFuture<'_, PyValue> {
        Box::pin(async move {
            let r = match self.client.get(format!("{}/readyz", self.url)).send().await {
                Ok(r) => r,
                Err(e) => {
                    return obj([
                        ("ok", PyValue::Bool(false)),
                        ("url", PyValue::str(self.url.as_str())),
                        ("error", PyValue::str(transport_error_name(&e))),
                    ]);
                }
            };
            let status = r.status().as_u16();
            let why = self.refusal_reason().await;
            let unauth = self.probe_unauthenticated().await;
            obj([
                ("ok", PyValue::Bool(status == 200 && why.is_none())),
                ("url", PyValue::str(self.url.as_str())),
                ("status", PyValue::Int(i64::from(status))),
                ("refused", PyValue::Bool(why.is_some())),
                ("reason", why.map_or(PyValue::None, PyValue::Str)),
                (
                    "api_key",
                    PyValue::str(if self.key_configured() { "[set]" } else { "[unset]" }),
                ),
                ("unauthenticated_access", unauth.map_or(PyValue::None, PyValue::Bool)),
            ])
        })
    }

    fn refusal(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(self.refusal_reason())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_and_uuid5_match_python() {
        assert_eq!(hex::encode(sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hex::encode(sha1(&[b'a'; 1000])),
            "291e9a6c66994949b57ba5e650361e98fc36b1ba"
        );
        // generated with agent_knowledge.stores.qdrant.point_id
        assert_eq!(point_id("d#0"), "1b908dc0-b851-5746-b30a-e783cbc202d5");
        assert_eq!(point_id("a#0"), "031a9e56-95a5-55e3-8647-a26e875b91d8");
        assert_eq!(point_id("x"), "e16ab7dc-6bf9-5e01-ae37-b2757545b90c");
        assert_eq!(point_id("é#3"), "6ca592ed-0750-5565-8ce6-136b76e82d05");
        assert_eq!(point_id(""), "b0faf025-f35c-584f-acf5-49b6337ffa4c");
    }
}
