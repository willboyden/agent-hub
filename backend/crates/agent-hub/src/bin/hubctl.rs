//! `hubctl` (cli.py `main`).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(agent_hub::cli::run(&args));
}
