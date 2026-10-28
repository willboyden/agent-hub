//! Agent Hub core library: Python-compatible primitives and the domain layer shared by the hub server, `hubctl` and
//! the knowledge service. Module ownership is in backend/PORTING.md (one owner per file).

pub mod common;
pub mod dotenv;
pub mod errors;
pub mod ids;
pub mod models;
pub mod pathsafe;
pub mod py;
pub mod pyd;
pub mod value;
pub mod yaml;
pub mod yamlsafe;

pub use errors::{HubResult, Problem};
pub use value::PyValue;

/// Compile a regex from a constant pattern. Every caller passes a literal covered by unit tests, so a failure is a
/// programming error caught by `cargo test`; it falls back to a never-matching regex instead of panicking at runtime.
pub fn re(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern).unwrap_or_else(|_| never())
}

fn never() -> regex::Regex {
    // `[^\s\S]` matches nothing and always compiles; the second fallback is unreachable.
    regex::Regex::new(r"[^\s\S]").unwrap_or_else(|_| regex::Regex::new("").unwrap_or_else(|_| never()))
}
