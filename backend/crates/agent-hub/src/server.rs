//! main.py: `create_app` and the `agent-hub` entry point (127.0.0.1:8792 by default; `agent-hub open|stop` for the
//! desktop launcher). Lifespan: the first start writes the bootstrap admin key (0600, path logged, never the key);
//! shutdown kills a running SkillEvaluator process group. Owner: CORE (LOCKED).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use http::Request;
use tokio::net::TcpListener;

use crate::api::{AppState, deps::SseLimiter, routes, security};
use crate::config::{Settings, app_root};
use crate::services::hub::Hub;

/// `default_frontend_dir()`.
pub fn default_frontend_dir() -> std::path::PathBuf {
    app_root().join("frontend")
}

/// `create_app(cfg, hub)`: the route table (without /metrics when disabled, without the SPA route when there is no
/// frontend directory) and the shared state.
pub fn create_app(hub: Arc<Hub>) -> Arc<AppState> {
    let cfg = hub.cfg();
    let root = cfg.frontend_dir.clone().unwrap_or_else(default_frontend_dir);
    let frontend = if root.is_dir() {
        Some(hub_core::py::os::realpath(&root))
    } else {
        None
    };
    let routes = routes::table()
        .into_iter()
        .filter(|r| (cfg.enable_metrics || r.path != "/metrics") && (frontend.is_some() || r.path != "/{path:path}"))
        .collect();
    Arc::new(AppState {
        hub,
        routes,
        frontend,
        knowledge_sse: SseLimiter::new(cfg.max_sse_connections),
        body_deadline: crate::api::BODY_DEADLINE,
    })
}

/// Connection limits of the served hub (audit F8; uvicorn bounded idle keep-alive connections by default, hyper is
/// configured explicitly here).
#[derive(Clone, Copy, Debug)]
pub struct ServeLimits {
    /// time a client has to send a request's headers; hyper starts this clock whenever it waits for a request head,
    /// so it also closes a keep-alive connection that stays idle this long
    pub header_read_timeout: Duration,
    /// open connections at most; more wait in the kernel's listen backlog until one closes
    pub max_connections: usize,
    /// how long a shutdown waits for open connections (in-flight requests) before closing them
    pub shutdown_grace: Duration,
}

impl Default for ServeLimits {
    fn default() -> Self {
        Self {
            header_read_timeout: Duration::from_secs(30),
            max_connections: 256,
            shutdown_grace: Duration::from_secs(10),
        }
    }
}

/// SIGINT or SIGTERM.
async fn shutdown_signal() {
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        () = term => {},
    }
}

/// The lifespan's startup part.
pub fn on_startup(hub: &Hub) {
    let data = hub.cfg().data_dir;
    match hub.auth.ensure_bootstrap_key(&data) {
        Ok(Some(path)) => tracing::info!("first start: admin API key written to {} (0600)", path.display()),
        Ok(None) => {
            if data.join(crate::services::auth::BOOTSTRAP_KEY_FILE).exists() {
                tracing::warn!(
                    "bootstrap admin key file exists: copy the key somewhere safe and DELETE the file after first use"
                );
            }
        }
        Err(_) => tracing::error!("could not write the bootstrap admin key"),
    }
}

/// Serve until SIGINT/SIGTERM on an already-bound listener.
pub async fn serve(app: Arc<AppState>, listener: TcpListener) -> std::io::Result<()> {
    serve_with(app, listener, ServeLimits::default(), shutdown_signal()).await
}

/// Serve with explicit limits until `shutdown` completes: HTTP/1.1 (keep-alive) on each accepted connection, every
/// request through [`security::handle`]. On shutdown no new connection is accepted, open ones finish their current
/// request (up to `shutdown_grace`) and the hub is closed.
pub async fn serve_with(
    app: Arc<AppState>,
    listener: TcpListener,
    limits: ServeLimits,
    shutdown: impl std::future::Future<Output = ()>,
) -> std::io::Result<()> {
    let port = listener.local_addr()?.port();
    on_startup(&app.hub);
    let hub = app.hub.clone();
    let permits = Arc::new(tokio::sync::Semaphore::new(limits.max_connections.max(1)));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut conns = tokio::task::JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        let permit = tokio::select! {
            () = &mut shutdown => break,
            p = permits.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => break,
            },
        };
        let accepted = tokio::select! {
            () = &mut shutdown => break,
            a = listener.accept() => a,
        };
        let Ok((stream, peer)) = accepted else {
            // EMFILE and friends: back off instead of spinning
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let (app, mut stop) = (app.clone(), stop_rx.clone());
        let header_timeout = limits.header_read_timeout;
        conns.spawn(async move {
            let _permit = permit;
            let svc = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                let app = app.clone();
                async move {
                    Ok::<_, std::convert::Infallible>(security::handle(app, peer, port, req.map(Body::new)).await)
                }
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(header_timeout)
                .keep_alive(true)
                .serve_connection(hyper_util::rt::TokioIo::new(stream), svc);
            tokio::pin!(conn);
            tokio::select! {
                _ = conn.as_mut() => {},
                () = async { let _ = stop.wait_for(|s| *s).await; } => {
                    conn.as_mut().graceful_shutdown();
                    let _ = conn.await;
                }
            }
        });
        while conns.try_join_next().is_some() {} // reap finished connections
    }
    let _ = stop_tx.send(true);
    let drain = async { while conns.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(limits.shutdown_grace, drain).await;
    conns.abort_all();
    hub.close();
    Ok(())
}

/// Bind `host:port` like uvicorn.
pub async fn bind(cfg: &Settings) -> std::io::Result<TcpListener> {
    let port = u16::try_from(cfg.port).map_err(|_| std::io::Error::other("port out of range"))?;
    TcpListener::bind((cfg.host.as_str(), port)).await
}

/// `agent-hub [open|stop]` (main.py `run`). Returns the process exit status.
#[allow(clippy::print_stderr)]
pub fn run_main(args: &[String]) -> i32 {
    if let Some(cmd) = args.first().filter(|c| *c == "open" || *c == "stop") {
        return if cmd == "open" {
            crate::desktop::open_hub(&args[1..])
        } else {
            crate::desktop::stop_hub(&args[1..])
        };
    }
    let cfg = match crate::config::load_settings(&Vec::new()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    if let Err(e) = crate::config::check_bind(&cfg.host) {
        eprintln!("error: {e}");
        return 1;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .try_init();
    let hub = match Hub::new(cfg.clone(), crate::services::hub::HubOptions::default()) {
        Ok(h) => h,
        Err(p) => {
            eprintln!("error: {}: {}", p.code, p.detail);
            return 1;
        }
    };
    let Ok(rt) = tokio::runtime::Builder::new_multi_thread().enable_all().build() else {
        return 1;
    };
    rt.block_on(async move {
        let listener = match bind(&cfg).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("error: cannot bind {}:{}: {e}", cfg.host, cfg.port);
                return 1;
            }
        };
        tracing::info!(
            "Uvicorn-compatible server listening on http://{}:{}",
            cfg.host,
            cfg.port
        );
        match serve(create_app(hub), listener).await {
            Ok(()) => 0,
            Err(_) => 1,
        }
    })
}
