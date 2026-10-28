//! Python-compatible primitives. The API returns, and the hub writes, text that Python produced (`repr()` in messages,
//! `json.dumps` layouts, float formatting, `fnmatch` decisions, `str.splitlines`), so these are ported exactly rather
//! than approximated with the nearest Rust idiom. Owner: CORE (PORTING.md module map).

pub mod float;
pub mod fnmatch;
pub mod json;
pub mod os;
pub mod pyjson;
pub mod pyunicode;
pub mod starlette;
pub mod text;

pub use fnmatch::fnmatchcase;
pub use text::repr;
