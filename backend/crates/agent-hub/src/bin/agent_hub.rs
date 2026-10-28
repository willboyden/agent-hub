//! `agent-hub` (main.py `run`): serve the API + UI, or `agent-hub open|stop` (desktop launcher).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(agent_hub::server::run_main(&args));
}
