//! The Settings page's config editor (Rust-only routes, PORTING.md A-18): draft, validate, export. Admin only. The GETs
//! read a shipped template or the policy floor (fixed paths, 64 KiB cap); the POST runs the real loader on a draft
//! and returns diagnostics. Nothing here writes a file, changes a permission or runs a command. The draft body is
//! audited as metadata only (security.rs CONTENT_PATHS), because a draft env file can hold the knowledge admin key.
//! Every POST also passes the pipeline's Host / Origin / X-Agent-Hub / body-cap checks; at most
//! [`MAX_CONCURRENT_CHECKS`] checks run at once, more answer 429.

use std::sync::{Arc, LazyLock};

use hub_core::Problem;
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::{Resp, reply, run};
use crate::api::deps::{Errors, body_dict, require_admin};
use crate::api::errors::{problem, validation};
use crate::api::{AppState, Req};
use crate::services::config_files;

/// Checks in flight at once; one more is refused with 429 instead of queueing behind them.
pub const MAX_CONCURRENT_CHECKS: usize = 4;
/// Shared so a permit can move into the blocking task: it is released when the check ends, not when the request
/// future is dropped (a client that disconnects must not free a slot while its check still runs).
static CHECKS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_CHECKS)));

/// A slot for one check, or None when [`MAX_CONCURRENT_CHECKS`] are running. The caller moves it into the blocking
/// task that runs the check.
fn try_permit() -> Option<OwnedSemaphorePermit> {
    Arc::clone(&CHECKS).try_acquire_owned().ok()
}

macro_rules! admin {
    ($r:expr) => {
        if let Err(p) = require_admin(&$r.principal) {
            return problem(&p);
        }
    };
}

/// GET /api/v1/config-files.
pub async fn list(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    reply(
        200,
        run(&s, |h| {
            let cfg = h.cfg();
            Ok(config_files::catalog(
                &cfg.content_dir,
                &cfg.policy_file,
                &crate::config::env_file(),
            ))
        })
        .await,
    )
}

/// GET /api/v1/config-files/{file}/templates/{name}.
pub async fn template(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let (file, name) = (r.param("file"), r.param("name"));
    reply(
        200,
        run(&s, move |_| {
            config_files::template_text(&crate::config::app_root(), &file, &name)
        })
        .await,
    )
}

/// GET /api/v1/config-files/{file}/current (the policy floor only).
pub async fn current(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let file = r.param("file");
    reply(
        200,
        run(&s, move |h| config_files::current_text(&file, &h.cfg().policy_file)).await,
    )
}

/// POST /api/v1/config-files/{file}/validate  body `{"text": "..."}`.
pub async fn validate(s: Arc<AppState>, r: Req) -> Resp {
    admin!(r);
    let mut e = Errors::default();
    let body = body_dict(&r, &mut e);
    let Some(b) = body.filter(|_| e.is_empty()) else {
        return validation(e.into_value());
    };
    let Some(text) = b.get("text").and_then(Value::as_str).map(str::to_string) else {
        return problem(&Problem::unprocessable("`text` (a string) is required").code("text_required"));
    };
    let Some(permit) = try_permit() else {
        return problem(&Problem::new(
            429,
            "rate_limited",
            "too many config checks at once; try again in a moment",
        ));
    };
    let file = r.param("file");
    reply(
        200,
        run(&s, move |h| {
            let _permit = permit;
            if file == "client-spec" {
                h.check_client_file(&text)
            } else {
                config_files::check(&file, &text)
            }
        })
        .await,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request whose future is dropped (the client went away) keeps its slot until the check itself ends.
    #[tokio::test]
    async fn a_dropped_request_keeps_its_slot_until_the_check_ends() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let held: Vec<OwnedSemaphorePermit> = (1..MAX_CONCURRENT_CHECKS).filter_map(|_| try_permit()).collect();
        assert_eq!(held.len(), MAX_CONCURRENT_CHECKS - 1);
        let request = tokio::spawn(async move {
            let Some(permit) = try_permit() else { return };
            let _ = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let _ = rx.recv();
            })
            .await;
        });
        while CHECKS.available_permits() > 0 {
            tokio::task::yield_now().await;
        }
        request.abort();
        let _ = request.await;
        assert!(
            try_permit().is_none(),
            "the aborted request's check still holds its slot"
        );
        let _ = tx.send(());
        let mut freed = None;
        for _ in 0..1000 {
            freed = try_permit();
            if freed.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(freed.is_some(), "the slot comes back when the check ends");
        drop(held);
    }
}
