//! domain/yamlsafe.py: the hardened YAML entry points every content read goes through. Security control S-YAML:
//! no Python tags (safe loader only), no anchors/aliases (billion laughs), no duplicate keys, size/depth/node caps.
//! Owner: WS-Y (with the engine in yaml/). Messages are part of the API (content issues).
//!
//! Python lets a few non-YAML exceptions escape `yamlsafe.load` unchanged (a `ValueError` for `date: 2024-02-30`, a
//! `KeyError` for `!!bool maybe`, a `RecursionError` for ~330 nested collections, ...). [`load`] and
//! [`split_frontmatter`] return those as `YamlError(str(exc))` (what callers that catch `ValueError` see);
//! [`load_exact`] / [`split_frontmatter_exact`] keep them apart for callers that must answer like Python
//! (DEVIATION D-YAML-PYEXC in PORTING.md).

use crate::value::PyValue;
use crate::yaml::{self, DumpOpts, Loader};

pub const MAX_DEPTH: usize = 12;
pub const MAX_NODES: usize = 5000;
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct YamlError(pub String);

/// What `yamlsafe.load` raises: its own `YamlError`, or a Python exception that escapes `yaml.load` unwrapped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    #[error("{0}")]
    Yaml(YamlError),
    /// `type(exc).__name__` and `str(exc)`.
    #[error("{message}")]
    Python { exc_type: &'static str, message: String },
}

impl From<LoadError> for YamlError {
    fn from(e: LoadError) -> Self {
        match e {
            LoadError::Yaml(y) => y,
            LoadError::Python { message, .. } => YamlError(message),
        }
    }
}

fn walk(obj: &PyValue, depth: usize, counter: &mut usize) -> Result<(), YamlError> {
    *counter += 1;
    if depth > MAX_DEPTH {
        return Err(YamlError("document nested too deeply".into()));
    }
    if *counter > MAX_NODES {
        return Err(YamlError("document has too many nodes".into()));
    }
    match obj {
        PyValue::Dict(d) => {
            for (k, v) in d {
                walk(k, depth + 1, counter)?;
                walk(v, depth + 1, counter)?;
            }
        }
        PyValue::List(l) => {
            for v in l {
                walk(v, depth + 1, counter)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `load(text, max_bytes=...)`, keeping Python's escaping exceptions apart.
pub fn load_exact(text: &str, max_bytes: usize) -> Result<PyValue, LoadError> {
    if text.len() > max_bytes {
        return Err(LoadError::Yaml(YamlError("document too large".into())));
    }
    let data = yaml::load(text, Loader::Strict).map_err(|e| {
        if e.custom {
            LoadError::Yaml(YamlError(e.problem))
        } else if let Some(exc_type) = e.python_exception() {
            LoadError::Python {
                exc_type,
                message: e.problem,
            }
        } else {
            LoadError::Yaml(YamlError(format!(
                "invalid YAML: {}",
                if e.full.is_empty() {
                    "parse error"
                } else {
                    e.problem.as_str()
                }
            )))
        }
    })?;
    walk(&data, 0, &mut 0).map_err(LoadError::Yaml)?;
    Ok(data)
}

/// `load(text, max_bytes=...)`.
pub fn load(text: &str, max_bytes: usize) -> Result<PyValue, YamlError> {
    load_exact(text, max_bytes).map_err(YamlError::from)
}

/// `dump(data)`: `safe_dump(sort_keys=False, allow_unicode=True, default_flow_style=False, width=100)`.
pub fn dump(data: &PyValue) -> String {
    yaml::dump(data, &DumpOpts::CONTENT)
}

/// `split_frontmatter(text)` keeping Python's escaping exceptions apart.
pub fn split_frontmatter_exact(text: &str) -> Result<(Vec<(PyValue, PyValue)>, String), LoadError> {
    let refuse = |m: &str| LoadError::Yaml(YamlError(m.into()));
    if !text.starts_with("---\n") && !text.starts_with("---\r\n") {
        return Err(refuse("missing YAML frontmatter"));
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let end = (1..lines.len()).find(|&i| lines[i].trim_end_matches('\r') == "---");
    let Some(end) = end else {
        return Err(refuse("unterminated YAML frontmatter"));
    };
    let meta = load_exact(&lines[1..end].join("\n"), DEFAULT_MAX_BYTES)?;
    let meta = match meta {
        PyValue::None => Vec::new(),
        PyValue::Dict(d) => d,
        _ => return Err(refuse("frontmatter must be a mapping")),
    };
    Ok((meta, lines[end + 1..].join("\n")))
}

/// `split_frontmatter(text)`: `---\n<yaml>\n---\n<body>` -> (meta, body). The body is returned untouched.
pub fn split_frontmatter(text: &str) -> Result<(Vec<(PyValue, PyValue)>, String), YamlError> {
    split_frontmatter_exact(text).map_err(YamlError::from)
}

/// `join_frontmatter(meta, body)`.
pub fn join_frontmatter(meta: &[(PyValue, PyValue)], body: &str) -> String {
    format!("---\n{}---\n{body}", dump(&PyValue::Dict(meta.to_vec())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_and_frontmatter() {
        assert_eq!(
            load("a: &x 1\nb: *x\n", DEFAULT_MAX_BYTES).err().map(|e| e.0),
            Some("YAML anchors are not allowed".into())
        );
        assert_eq!(load("x", 0).err().map(|e| e.0), Some("document too large".into()));
        let deep = (0..14).fold(String::from("1"), |acc, _| format!("[{acc}]"));
        assert_eq!(
            load(&deep, DEFAULT_MAX_BYTES).err().map(|e| e.0),
            Some("document nested too deeply".into())
        );
        let (meta, body) = split_frontmatter("---\nname: a\n---\nbody\n").unwrap_or_default();
        assert_eq!(meta.len(), 1);
        assert_eq!(body, "body\n");
        assert!(split_frontmatter("name: a").is_err());
    }

    #[test]
    fn python_exceptions_stay_apart() {
        assert_eq!(
            load_exact("d: 2024-02-30\n", DEFAULT_MAX_BYTES),
            Err(LoadError::Python {
                exc_type: "ValueError",
                message: "day is out of range for month".into()
            })
        );
        assert_eq!(
            load("d: 2024-02-30\n", DEFAULT_MAX_BYTES),
            Err(YamlError("day is out of range for month".into()))
        );
        assert_eq!(
            load("a: [\n", DEFAULT_MAX_BYTES).map_err(|e| e.0),
            Err("invalid YAML: while parsing a flow node".into())
        );
    }
}
