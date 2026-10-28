//! Port of errors.py: RFC 7807 problem+json errors (`urn:agent-knowledge:<code>`). Details never echo request bodies.

use hub_core::PyValue;

use crate::pyjson::{self, Style, obj};

/// `KnowledgeError(status, code, detail)`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
pub struct KnowledgeError {
    pub status: u16,
    pub code: String,
    pub detail: String,
}

impl KnowledgeError {
    pub fn new(status: u16, code: &str, detail: impl Into<String>) -> Self {
        Self {
            status,
            code: code.to_string(),
            detail: detail.into(),
        }
    }

    /// Python's unhandled-exception path (`_500`): `problem(500, "internal_error", "internal error")`.
    pub fn internal() -> Self {
        Self::new(500, "internal_error", "internal error")
    }
}

/// `problem_body(status, code, detail)`.
pub fn problem_body(status: u16, code: &str, detail: &str) -> PyValue {
    obj([
        ("type", PyValue::Str(format!("urn:agent-knowledge:{code}"))),
        ("title", PyValue::Str(code.replace('_', " "))),
        ("status", PyValue::Int(i64::from(status))),
        ("code", PyValue::str(code)),
        ("detail", PyValue::str(detail)),
    ])
}

/// The bytes of `problem(...)` (Starlette `JSONResponse`: compact, `ensure_ascii=False`).
pub fn problem_text(status: u16, code: &str, detail: &str) -> String {
    pyjson::dumps(&problem_body(status, code, detail), Style::Compact)
}
