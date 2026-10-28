//! api/routers/knowledge.py: same-origin proxy to the knowledge service (GET/POST/DELETE; `tokens`, `indexes`, `backends`
//! are admin-only reads; `health` is a summary, not a passthrough; `.../stream` is SSE, connection-capped with an idle
//! timeout). Owner: CORE (LOCKED); the proxy client is services/knowledge.rs (WS-C).

use std::sync::Arc;

use bytes::Bytes;
use hub_core::Problem;

use super::{Resp, reply};
use crate::api::errors::{problem, raw};
use crate::api::{AppState, Req};

/// Releases the SSE slot when the response body is dropped (finished, idle-timed-out or client gone).
struct Slot(Arc<AppState>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.knowledge_sse.release();
    }
}

/// deps.py `sse_response`: `text/event-stream`, `X-Accel-Buffering: no`, a silent stream ends after the idle timeout.
fn sse_response(slot: Slot, mut stream: futures::stream::BoxStream<'static, hub_core::HubResult<Bytes>>) -> Resp {
    let idle = std::time::Duration::from_secs_f64(slot.0.hub.cfg().sse_idle_timeout_s.max(0.0));
    let body = async_stream::stream! {
        let _slot = slot;
        while let Ok(Some(Ok(chunk))) = tokio::time::timeout(idle, futures::StreamExt::next(&mut stream)).await {
            yield Ok::<Bytes, std::io::Error>(chunk);
        }
    };
    let mut resp = http::Response::new(axum::body::Body::from_stream(body));
    let h = resp.headers_mut();
    h.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    h.insert(http::header::CACHE_CONTROL, http::HeaderValue::from_static("no-cache"));
    h.insert("x-accel-buffering", http::HeaderValue::from_static("no"));
    resp
}

pub const ADMIN_ONLY_READS: [&str; 3] = ["tokens", "indexes", "backends"];

pub async fn proxy(s: Arc<AppState>, r: Req) -> Resp {
    let path = r.param("path");
    let first = path.split('/').next().unwrap_or("");
    if ADMIN_ONLY_READS.contains(&first) && r.principal.role != "admin" {
        return problem(&Problem::forbidden(
            "this knowledge path lists credentials or backend internals: admin only",
        ));
    }
    if path == "health" && r.method == "GET" {
        return reply(200, Ok(s.hub.knowledge.summary().await));
    }
    if r.method == "GET" && path.ends_with("/stream") {
        if !s.knowledge_sse.acquire() {
            return problem(&Problem::new(
                429,
                "too_many_streams",
                "too many open streams; close one and retry",
            ));
        }
        // the guard exists from the moment the slot is taken: if the client goes away while the upstream is still
        // being asked (this future is dropped), the slot is released too (audit F7)
        let slot = Slot(s.clone());
        return match s.hub.knowledge.stream(&path, &r.query).await {
            Ok(stream) => sse_response(slot, stream),
            Err(p) => problem(&p), // dropping `slot` releases it
        };
    }
    let body = if r.method == "POST" {
        r.use_body().clone()
    } else {
        Bytes::new()
    };
    match s.hub.knowledge.forward(&r.method, &path, &r.query, body).await {
        Ok((status, data, ctype)) => raw(status, &ctype, data.to_vec()),
        Err(p) => problem(&p),
    }
}
