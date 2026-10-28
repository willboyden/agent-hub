//! Agent Knowledge (Rust port): scoped vector stores (LanceDB embedded / Qdrant REST) behind a hardened localhost API.
//! Contract: backend/PORTING.md (WS-D owns every file of this crate).

pub mod api;
pub mod app;
pub mod chunking;
pub mod config;
pub mod db;
pub mod embed;
pub mod env;
pub mod errors;
pub mod filters;
pub mod paths;
/// The CPython-exact JSON decoder/encoder (lives in hub-core since A-11; re-exported for the existing paths).
pub use hub_core::py::pyjson;
pub mod ratelimit;
pub mod security;
pub mod service;
pub mod stores;
pub mod telemetry;

/// main.py `run`: `agent-knowledge [--config FILE] [--allow-bind-any]`; refuses a non-loopback host without the flag.
/// Exit codes: 2 usage error, 1 startup failure (the message Python's traceback ends with), 0 after a clean shutdown.
#[allow(clippy::print_stderr)]
pub fn main_entry(args: &[String]) -> i32 {
    use clap::Parser;
    #[derive(Parser)]
    #[command(name = "agent-knowledge", disable_version_flag = true)]
    struct Cli {
        /// YAML config (default: $AGENT_KNOWLEDGE_CONFIG or built-in defaults)
        #[arg(long)]
        config: Option<String>,
        /// bind the configured non-loopback host (ONLY inside a container on an internal network)
        #[arg(long = "allow-bind-any")]
        allow_bind_any: bool,
    }
    let cli = match Cli::try_parse_from(std::iter::once("agent-knowledge".to_string()).chain(args.iter().cloned())) {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() { 2 } else { 0 };
        }
    };
    let cfg = match config::load_config(cli.config.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    if !["127.0.0.1", "::1", "localhost"].contains(&cfg.host.as_str()) && !cli.allow_bind_any {
        eprintln!("refusing a non-loopback host without --allow-bind-any (container use only; front it with a broker)");
        return 1;
    }
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        // our own messages at INFO like logging.basicConfig(level=INFO); the Lance engine's per-write INFO events off
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn,agent_knowledge=info"))
        .try_init();
    let Ok(port) = u16::try_from(cfg.port) else {
        eprintln!("OverflowError: bind(): port must be 0-65535.");
        return 1;
    };
    // Generous stacks: request values can nest up to pyjson::MAX_DEPTH levels and are cloned/dropped recursively.
    let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(32 * 1024 * 1024)
        .build()
    else {
        return 1;
    };
    rt.block_on(async move {
        let host = cfg.host.clone();
        let app = match app::create_app(cfg) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        };
        let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("error: cannot bind {host}:{port}: {e}");
                return 1;
            }
        };
        match app::serve(app, listener).await {
            Ok(()) => 0,
            Err(_) => 1,
        }
    })
}
