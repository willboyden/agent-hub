//! `agent-knowledge` (main.py `run`).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(agent_knowledge::main_entry(&args));
}
