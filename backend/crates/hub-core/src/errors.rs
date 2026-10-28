//! `ProblemError` (domain/errors.py) and the RFC 7807 body (api/errors.py `problem()`). Every fallible hub operation
//! returns `HubResult<T>`; the API layer renders the error as `application/problem+json`:
//! `{"type": "urn:agent-hub:<code>", "title": <code with _ as spaces>, "status", "code", "detail", **extra}`.
//! Owner: CORE (LOCKED).

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub status: u16,
    pub code: String,
    pub detail: String,
    /// Extra members merged into the body after `detail` (Python `**exc.extra`), in insertion order. Boxed so a
    /// `HubResult` stays small.
    pub extra: Box<Map<String, Value>>,
    /// `title` override; `None` = `code.replace('_', ' ')` (what every Python path produces).
    pub title: Option<Box<str>>,
}

pub type HubResult<T> = Result<T, Problem>;

impl std::fmt::Display for Problem {
    /// `str(exc)` of a ProblemError is its detail.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for Problem {}

impl Problem {
    pub fn new(status: u16, code: &str, detail: impl Into<String>) -> Self {
        Self {
            status,
            code: code.to_string(),
            detail: detail.into(),
            extra: Box::default(),
            title: None,
        }
    }
    /// `ProblemError(detail)`: 400 bad_request.
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(400, "bad_request", detail)
    }
    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(401, "unauthorized", detail)
    }
    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(403, "forbidden", detail)
    }
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(404, "not_found", detail)
    }
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(409, "conflict", detail)
    }
    /// `Unprocessable`: 422 with code `validation_error` unless overridden.
    pub fn unprocessable(detail: impl Into<String>) -> Self {
        Self::new(422, "validation_error", detail)
    }
    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self::new(503, "unavailable", detail)
    }
    /// An unexpected failure: exactly what FastAPI's catch-all handler returns (the cause is logged, never sent).
    pub fn internal() -> Self {
        Self::new(500, "internal_error", "unexpected server error")
    }
    /// A route or service the Rust port does not implement yet (phase-1 stubs). Never produced by Python.
    pub fn not_ported(module: &str) -> Self {
        Self::new(501, "not_ported", format!("not ported: {module}"))
    }
    /// `code=` keyword of ProblemError.
    #[must_use]
    pub fn code(mut self, code: &str) -> Self {
        self.code = code.to_string();
        self
    }
    /// `status=` keyword of ProblemError.
    #[must_use]
    pub fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
    /// `**extra` keyword arguments (e.g. `errors=[...]`, `would_discard=[...]`, `fix="..."`).
    #[must_use]
    pub fn with(mut self, key: &str, value: Value) -> Self {
        self.extra.insert(key.to_string(), value);
        self
    }

    /// The problem+json body.
    pub fn body(&self) -> Value {
        let mut m = Map::new();
        m.insert("type".into(), Value::String(format!("urn:agent-hub:{}", self.code)));
        m.insert(
            "title".into(),
            Value::String(
                self.title
                    .as_deref()
                    .map_or_else(|| self.code.replace('_', " "), str::to_string),
            ),
        );
        m.insert("status".into(), Value::from(self.status));
        m.insert("code".into(), Value::String(self.code.clone()));
        m.insert("detail".into(), Value::String(self.detail.clone()));
        for (k, v) in self.extra.iter() {
            m.insert(k.clone(), v.clone());
        }
        Value::Object(m)
    }
}

impl From<std::io::Error> for Problem {
    /// An unhandled OSError in Python reaches the catch-all handler: 500 internal_error.
    fn from(_: std::io::Error) -> Self {
        Problem::internal()
    }
}
