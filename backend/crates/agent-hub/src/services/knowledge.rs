//! Port of services/knowledge.py: same-origin proxy to the knowledge service (HUB_KNOWLEDGE_URL): GET/POST/DELETE,
//! allowlisted first segment (health backends namespaces jobs indexes tokens), segment regex, <=6 segments, query <=1024,
//! body/response caps, 503 `knowledge_unavailable` when down (never a hang), an upstream 401/403 becomes 502
//! `knowledge_upstream_unauthorized`. The admin key (HUB_KNOWLEDGE_ADMIN_KEY / legacy HUB_KNOWLEDGE_TOKEN, else the 0600
//! KNOWLEDGE_ADMIN_KEY_FILE read at call time) is never logged, returned or audited. reqwest with no proxy (trust_env
//! False), no redirects, connect/read timeouts = HUB_KNOWLEDGE_TIMEOUT_S (httpx's per-operation timeout). Security
//! control S-KPROXY. Owner: WS-C.
//!
//! The SSE passthrough checks the path, the key, the connection and the upstream status BEFORE the stream starts, so
//! those failures are problem responses (the CORE route's contract); Python starts a 200 stream and then aborts it.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use hub_core::{HubResult, Problem};
use regex::Regex;
use serde_json::{Value, json};

use crate::config::Settings;

pub const UNREACHABLE: &str = "the knowledge service is not reachable; start it with `cd hub/knowledge && uv run agent-knowledge` (it listens on the URL in HUB_KNOWLEDGE_URL)";
pub const DEFAULT_KEY_FILE: &str = "~/.local/share/agent-knowledge/admin.key";
pub const ALLOWED_PREFIXES: [&str; 6] = ["health", "backends", "namespaces", "jobs", "indexes", "tokens"];
pub const MAX_QUERY: usize = 1024;

static SEGMENT: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"\A[A-Za-z0-9._-]{1,128}\z"));

/// `check_path(path)`: 404 `knowledge_path_denied` (not proxied) / 400 (invalid segment).
pub fn check_path(path: &str) -> HubResult<Vec<String>> {
    let parts: Vec<String> = path.split('/').map(str::to_string).collect();
    if !ALLOWED_PREFIXES.contains(&parts[0].as_str()) || parts.len() > 6 {
        return Err(Problem::bad_request("path is not proxied")
            .code("knowledge_path_denied")
            .status(404));
    }
    for p in &parts {
        if p == "." || p == ".." || !SEGMENT.is_match(p) {
            return Err(Problem::bad_request("invalid path segment")
                .code("knowledge_path_denied")
                .status(400));
        }
    }
    Ok(parts)
}

fn unavailable() -> Problem {
    Problem::unavailable(UNREACHABLE).code("knowledge_unavailable")
}

/// The KNOWLEDGE service refused our key. That is a hub configuration problem (502), never the browser's 401/403.
fn unauthorized() -> Problem {
    Problem::new(
        502,
        "knowledge_upstream_unauthorized",
        format!(
            "the knowledge service rejected the hub's admin key: the hub is not configured with a valid knowledge admin key. Set HUB_KNOWLEDGE_ADMIN_KEY (or point KNOWLEDGE_ADMIN_KEY_FILE at the key file); by default the hub reads {DEFAULT_KEY_FILE}, which the knowledge service writes on its first start."
        ),
    )
}

fn key_unusable(detail: String) -> Problem {
    Problem::new(502, "knowledge_key_unusable", detail)
}

pub struct KnowledgeProxy {
    pub cfg: Settings,
    client: OnceLock<Option<reqwest::Client>>,
}

impl KnowledgeProxy {
    pub fn new(cfg: &Settings) -> Self {
        Self {
            cfg: cfg.clone(),
            client: OnceLock::new(),
        }
    }

    /// `httpx.AsyncClient(base_url=..., timeout=..., follow_redirects=False, trust_env=False)`, built on first use.
    fn client(&self) -> Option<&reqwest::Client> {
        self.client
            .get_or_init(|| {
                // an infinite timeout (pydantic accepts "inf") means none, as in httpx; from_secs_f64 would panic on it
                let secs = self.cfg.knowledge_timeout_s;
                let t = Duration::from_secs_f64(if secs.is_finite() { secs.max(0.001) } else { 1.0e7 });
                reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(t)
                    .read_timeout(t)
                    .build()
                    .ok()
            })
            .as_ref()
    }

    /// `base_url + path (+ "?" + query)`: the base keeps its own path prefix, like httpx's relative-URL merge.
    fn url(&self, path: &str, query: &str) -> Option<reqwest::Url> {
        let base = format!("{}/", self.cfg.knowledge_url.trim_end_matches('/'));
        let mut s = format!("{base}{}", path.trim_start_matches('/'));
        if !query.is_empty() {
            s.push('?');
            s.push_str(query);
        }
        reqwest::Url::parse(&s).ok()
    }

    /// `_key()`: (admin key, source). Setting/env wins; else the knowledge service's own 0600 key file, read at call
    /// time. The key is never logged, returned, or audited.
    fn key(&self) -> HubResult<(Option<String>, &'static str)> {
        let direct = self
            .cfg
            .knowledge_admin_key
            .clone()
            .filter(|k| !k.is_empty())
            .or_else(|| self.cfg.knowledge_token.clone());
        if let Some(d) = direct {
            let d = hub_core::py::text::strip(&d);
            if !d.is_empty() {
                return Ok((Some(d.to_string()), "setting"));
            }
        }
        let home = crate::config::home();
        let f = hub_core::py::os::expanduser(
            &self.cfg.knowledge_admin_key_file.to_string_lossy(),
            &home.to_string_lossy(),
        );
        let Ok(st) = std::fs::symlink_metadata(&f) else {
            return Ok((None, "none"));
        };
        if !st.file_type().is_file() {
            return Err(key_unusable(format!("{f} must be a regular file (not a symlink)")));
        }
        if st.permissions().mode() & 0o077 != 0 {
            return Err(key_unusable(format!(
                "{f} must be mode 0600 or stricter; refusing to use it (chmod 600 it)"
            )));
        }
        if st.size() > 4096 {
            return Err(key_unusable(format!("{f} is too large to be a key file")));
        }
        match std::fs::read_to_string(&f) {
            Ok(text) => {
                let k = hub_core::py::text::strip(&text);
                Ok((if k.is_empty() { None } else { Some(k.to_string()) }, "file"))
            }
            Err(_) => Ok((None, "none")),
        }
    }

    fn headers(&self, has_body: bool, accept: &str) -> HubResult<reqwest::header::HeaderMap> {
        let mut h = reqwest::header::HeaderMap::new();
        if let Ok(v) = reqwest::header::HeaderValue::from_str(accept) {
            h.insert(reqwest::header::ACCEPT, v);
        }
        let (key, _) = self.key()?;
        if let Some(k) = key
            && let Ok(mut v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {k}"))
        {
            v.set_sensitive(true);
            h.insert(reqwest::header::AUTHORIZATION, v);
        }
        if has_body {
            h.insert(
                reqwest::header::CONTENT_TYPE,
                reqwest::header::HeaderValue::from_static("application/json"),
            );
        }
        Ok(h)
    }

    /// Small health view for the UI: `{reachable, authenticated, backends, key_source, url, detail?}`; never key
    /// material.
    /// One whole call (connect, request, the complete response) may take at most 4 x `HUB_KNOWLEDGE_TIMEOUT_S` (at most
    /// 10 minutes), on top of httpx-style per-operation timeouts: an upstream that keeps trickling bytes can no longer
    /// hold a hub request open forever (audit F8). A stream's body is bounded by its idle timeout and byte cap instead.
    pub fn total_deadline(&self) -> Duration {
        let t = self.cfg.knowledge_timeout_s;
        let t = if t.is_finite() { t.max(0.001) } else { 30.0 };
        Duration::from_secs_f64((t * 4.0).min(600.0))
    }

    /// GET /api/v1/knowledge/health: reachable / authenticated / backends, never the key.
    pub async fn summary(&self) -> Value {
        match tokio::time::timeout(self.total_deadline(), self.summary_inner()).await {
            Ok(v) => v,
            Err(_) => {
                let source = self.key().map_or("unusable", |(_, s)| s);
                json!({"reachable": false, "authenticated": false, "backends": null, "key_source": source,
                       "url": self.cfg.knowledge_url, "detail": UNREACHABLE})
            }
        }
    }

    async fn summary_inner(&self) -> Value {
        let (key, source, note) = match self.key() {
            Ok((k, s)) => (k, s, String::new()),
            Err(p) => (None, "unusable", p.detail),
        };
        let mut out = json!({"reachable": false, "authenticated": false, "backends": null, "key_source": source,
                             "url": self.cfg.knowledge_url});
        let set_detail = |out: &mut Value, d: String| {
            if let Some(m) = out.as_object_mut() {
                m.insert("detail".into(), Value::String(d));
            }
        };
        let (Some(client), Some(health), Some(backends)) =
            (self.client(), self.url("health", ""), self.url("backends", ""))
        else {
            set_detail(&mut out, UNREACHABLE.into());
            return out;
        };
        let plain = {
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(
                reqwest::header::ACCEPT,
                reqwest::header::HeaderValue::from_static("application/json"),
            );
            h
        };
        let ok = match client.get(health).headers(plain.clone()).send().await {
            Ok(r) => r.bytes().await.is_ok(),
            Err(_) => false,
        };
        if !ok {
            set_detail(&mut out, UNREACHABLE.into());
            return out;
        }
        out["reachable"] = Value::Bool(true);
        let headers = if key.is_some() {
            match self.headers(false, "application/json") {
                Ok(h) => h,
                Err(p) => {
                    set_detail(&mut out, p.detail);
                    return out;
                }
            }
        } else {
            plain
        };
        let Ok(r) = client.get(backends).headers(headers).send().await else {
            set_detail(&mut out, UNREACHABLE.into());
            return out;
        };
        let status = r.status().as_u16();
        let body = read_capped(r, usize::try_from(self.cfg.knowledge_response_cap_bytes).unwrap_or(0)).await;
        let Ok(body) = body else {
            set_detail(&mut out, UNREACHABLE.into());
            return out;
        };
        if status == 200 {
            out["authenticated"] = Value::Bool(true);
            out["backends"] = body
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .unwrap_or(Value::Null);
        } else if status == 401 || status == 403 {
            set_detail(&mut out, unauthorized().detail);
        } else {
            set_detail(&mut out, format!("knowledge service answered {status}"));
        }
        if !note.is_empty() {
            set_detail(&mut out, note);
        }
        out
    }

    /// GET/POST/DELETE passthrough: (status, body, content type).
    pub async fn forward(&self, method: &str, path: &str, query: &str, body: Bytes) -> HubResult<(u16, Bytes, String)> {
        if !matches!(method, "GET" | "POST" | "DELETE") {
            return Err(Problem::new(
                405,
                "method_not_allowed",
                "method not allowed on the knowledge proxy",
            ));
        }
        check_path(path)?;
        if query.chars().count() > MAX_QUERY {
            return Err(Problem::bad_request("query string too long"));
        }
        if i64::try_from(body.len()).unwrap_or(i64::MAX) > self.cfg.knowledge_body_cap_bytes {
            // DELETE bodies are dropped below
            return Err(Problem::new(413, "payload_too_large", "request body too large"));
        }
        let headers = self.headers(!body.is_empty(), "application/json")?;
        let (Some(client), Some(url)) = (self.client(), self.url(path, query)) else {
            return Err(unavailable());
        };
        let m = match method {
            "POST" => reqwest::Method::POST,
            "DELETE" => reqwest::Method::DELETE,
            _ => reqwest::Method::GET,
        };
        let mut req = client.request(m, url).headers(headers);
        if method == "POST" && !body.is_empty() {
            req = req.body(body);
        }
        let cap = usize::try_from(self.cfg.knowledge_response_cap_bytes).unwrap_or(0);
        tokio::time::timeout(self.total_deadline(), Self::exchange(req, cap))
            .await
            .unwrap_or_else(|_| Err(unavailable()))
    }

    /// Send a forwarded request and read its capped response: (status, body, content type).
    async fn exchange(req: reqwest::RequestBuilder, cap: usize) -> HubResult<(u16, Bytes, String)> {
        let resp = req.send().await.map_err(|_| unavailable())?;
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return Err(unauthorized());
        }
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let ctype = if ["application/json", "application/problem+json", "text/plain"].contains(&ctype.as_str()) {
            ctype
        } else {
            "application/octet-stream".to_string()
        };
        match read_capped(resp, cap).await {
            Err(()) => Err(unavailable()),
            Ok(None) => Err(Problem::new(502, "upstream_too_large", "knowledge response too large")),
            Ok(Some(data)) => Ok((status, data, ctype)),
        }
    }

    /// SSE passthrough with a byte cap; the caller wraps it in the connection-capped, idle-timed response.
    pub async fn stream(&self, path: &str, query: &str) -> HubResult<BoxStream<'static, HubResult<Bytes>>> {
        check_path(path)?;
        let headers = self.headers(false, "text/event-stream")?;
        let (Some(client), Some(url)) = (self.client(), self.url(path, query)) else {
            return Err(unavailable());
        };
        let resp = tokio::time::timeout(self.total_deadline(), client.get(url).headers(headers).send())
            .await
            .map_err(|_| unavailable())?
            .map_err(|_| unavailable())?;
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return Err(unauthorized());
        }
        if status != 200 {
            return Err(Problem::unavailable("knowledge stream unavailable").code("knowledge_unavailable"));
        }
        let cap = usize::try_from(self.cfg.knowledge_response_cap_bytes).unwrap_or(0);
        let mut body = resp.bytes_stream();
        let s = async_stream::stream! {
            let mut sent = 0usize;
            while let Some(chunk) = body.next().await {
                match chunk {
                    Ok(c) => {
                        sent += c.len();
                        if sent > cap {
                            break;
                        }
                        yield Ok(c);
                    }
                    Err(_) => {
                        yield Err(unavailable());
                        break;
                    }
                }
            }
        };
        Ok(s.boxed())
    }

    /// For GET /settings `info.knowledge`: `{"url", "token": "[set, N chars]" | "[unset]", "key_file"}`.
    pub fn status(&self) -> Value {
        let t = self
            .cfg
            .knowledge_admin_key
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| self.cfg.knowledge_token.clone())
            .filter(|t| !t.is_empty());
        json!({
            "url": self.cfg.knowledge_url,
            "token": t.map_or_else(|| "[unset]".to_string(), |t| format!("[set, {} chars]", t.chars().count())),
            "key_file": self.cfg.knowledge_admin_key_file.to_string_lossy(),
        })
    }
}

/// The whole body, `Ok(None)` past `cap` bytes, `Err` on a transport error.
async fn read_capped(resp: reqwest::Response, cap: usize) -> Result<Option<Bytes>, ()> {
    let mut out = Vec::new();
    let mut s = resp.bytes_stream();
    while let Some(chunk) = s.next().await {
        let c = chunk.map_err(|_| ())?;
        if out.len() + c.len() > cap {
            return Ok(None);
        }
        out.extend_from_slice(&c);
    }
    Ok(Some(Bytes::from(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_rules() {
        assert!(check_path("namespaces/docs/query").is_ok());
        for (p, st) in [
            ("admin/secrets", 404),
            ("shutdown", 404),
            ("../etc", 404),
            ("namespaces/../../x", 400),
            ("namespaces/a b", 400),
            ("namespaces/", 400),
            ("a/b/c/d/e/f/g", 404),
            ("namespaces/1/2/3/4/5/6", 404),
        ] {
            let e = check_path(p).err();
            assert_eq!(e.as_ref().map(|e| e.status), Some(st), "{p}");
            assert_eq!(e.map(|e| e.code), Some("knowledge_path_denied".into()));
        }
    }
}
