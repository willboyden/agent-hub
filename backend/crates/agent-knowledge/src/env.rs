//! The environment as the service reads it (`os.environ.get`). Everything that reads an env var takes an [`Env`], so
//! tests pass a map instead of mutating the process environment (edition 2024 makes `set_var` unsafe, and this
//! workspace forbids unsafe code).

use std::collections::HashMap;
use std::sync::Arc;

pub type Env = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The real process environment (values decoded lossily, like Python's surrogateescape would show them).
pub fn process() -> Env {
    Arc::new(|name: &str| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned()))
}

/// A fixed map (tests).
pub fn from_pairs(pairs: &[(&str, &str)]) -> Env {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    Arc::new(move |name: &str| m.get(name).cloned())
}

/// `os.environ.get(name, "")`.
pub fn get(env: &Env, name: &str) -> String {
    env(name).unwrap_or_default()
}
