//! YAML with PyYAML 6.0.3 semantics: `yaml.safe_load` (YAML 1.1 implicit types: `yes`/`on` booleans, `0`-prefixed
//! octal, sexagesimal `1:30`, timestamps -> date/datetime, `!!binary`, `!!set`, merge keys) and `yaml.safe_dump`
//! byte-for-byte (block style, PyYAML's quoting decisions, line folding at `width`, indentless sequences in mappings).
//!
//! Owner: WS-Y. LOCKED API (this file's public items). The engine is a port of PyYAML's pure-Python implementation
//! (the reference; libyaml is not used by `safe_load`/`safe_dump`): reader, scanner, parser, composer, resolver and
//! SafeConstructor for loading; SafeRepresenter, serializer and emitter for dumping. Proven by the golden corpus
//! generated with PyYAML itself (backend/parity/gen/gen_yaml_golden.py -> hub-core/tests/golden/yaml_*.json).

mod composer;
mod constructor;
mod dumper;
mod emitter;
mod error;
mod parser;
mod pyint;
mod pyobj;
use crate::py::pyunicode;
mod reader;
mod resolver;
mod scanner;

use crate::value::PyValue;

pub use error::Mark;

/// A YAML error. `problem` is PyYAML's `str(exc).splitlines()[0]` (yamlsafe surfaces only that line); `full` is the
/// whole PyYAML message (context + marks).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{full}")]
pub struct YamlError {
    /// The FIRST LINE of `str(exc)`, which is what Agent Hub's `yamlsafe` reports (`str(exc).splitlines()[0]`); for a
    /// marked error that is often PyYAML's `context` ("while parsing a block mapping"), not its `exc.problem`
    /// attribute (they coincide in 99 of the 468 errors of the load golden corpus). Nothing in Agent Hub reads
    /// PyYAML's `exc.problem` (PORTING.md A-12).
    pub problem: String,
    pub full: String,
    /// True for the strict loader's own refusals (anchors, aliases, duplicate keys): in Python those are yamlsafe's
    /// `YamlError` raised from inside the loader, NOT a `yaml.YAMLError`, so yamlsafe does not prefix "invalid YAML: ".
    pub custom: bool,
    /// PyYAML's `exc.problem_mark` for a marked error (scanner, parser, composer, constructor); None for the reader's
    /// "unacceptable character" error (PyYAML gives it a `position` instead), the strict loader's own refusals and
    /// the Python exceptions that escape the loader.
    pub problem_mark: Option<Mark>,
}

impl YamlError {
    pub fn new(problem: impl Into<String>) -> Self {
        let p = problem.into();
        Self {
            full: p.clone(),
            problem: p,
            custom: false,
            problem_mark: None,
        }
    }
    pub fn custom(problem: impl Into<String>) -> Self {
        Self {
            custom: true,
            ..Self::new(problem)
        }
    }
}

/// Which loader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loader {
    /// `yaml.SafeLoader` (`yaml.safe_load`): anchors/aliases allowed, a duplicate key silently keeps the last value.
    Safe,
    /// domain/yamlsafe.py `_StrictLoader`: an alias -> "YAML aliases are not allowed", an anchor -> "YAML anchors are not
    /// allowed", a duplicate key -> "duplicate key: <repr>". (The size/depth/node caps are yamlsafe.rs's job.)
    Strict,
}

/// `yaml.load(text, Loader=...)` of a single document (an empty stream is `None`).
pub fn load(text: &str, loader: Loader) -> Result<PyValue, YamlError> {
    let strict = loader == Loader::Strict;
    let reader = reader::Reader::new(text).map_err(|e| e.render(&[]))?;
    let mut c = composer::Composer::new(parser::Parser::new(scanner::Scanner::new(reader)), strict);
    let result = match c.get_single_node() {
        Ok(None) => Ok(PyValue::None),
        Ok(Some(root)) => constructor::Constructor::new(&mut c.nodes, strict).construct_document(root),
        Err(e) => Err(e),
    };
    result.map_err(|e| e.render(&c.p.s.r.buffer))
}

/// The Python exceptions that `yaml.load` lets escape unwrapped (they are not `yaml.YAMLError`s, so yamlsafe does not
/// turn them into its own error either). See [`YamlError::python_exception`].
const PYTHON_EXCEPTIONS: [&str; 10] = [
    "ValueError",
    "KeyError",
    "IndexError",
    "AttributeError",
    "TypeError",
    "OverflowError",
    "RecursionError",
    "MemoryError",
    "UnboundLocalError",
    "UnicodeDecodeError",
];

impl YamlError {
    /// `Some(<exception class>)` when Python raises a plain exception instead of a `yaml.YAMLError` for this input
    /// (e.g. `ValueError` for `date: 2024-02-30`, `KeyError` for `!!bool maybe`): `problem` is then `str(exc)` and
    /// `full` is `"<class>: <str(exc)>"`. A marked YAML error always has mark lines in `full`, so this is unambiguous.
    pub fn python_exception(&self) -> Option<&'static str> {
        if self.custom {
            return None;
        }
        PYTHON_EXCEPTIONS.into_iter().find(|name| {
            self.full.len() == name.len() + 2 + self.problem.len()
                && self.full.starts_with(name)
                && self.full[name.len()..].starts_with(": ")
                && self.full.ends_with(self.problem.as_str())
        })
    }
}

/// `yaml.safe_dump` keyword arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DumpOpts {
    pub sort_keys: bool,
    pub allow_unicode: bool,
    /// The hub always passes `default_flow_style=False` (block style; empty containers still print as `[]`/`{}`).
    pub default_flow_style: bool,
    pub width: usize,
}

impl DumpOpts {
    /// domain/yamlsafe.py `dump`: `sort_keys=False, allow_unicode=True, default_flow_style=False, width=100`.
    pub const CONTENT: DumpOpts = DumpOpts {
        sort_keys: false,
        allow_unicode: true,
        default_flow_style: false,
        width: 100,
    };
    /// adapters/_util.py `render_frontmatter` and merge.py `_dump_doc`: as CONTENT with `width=100000`.
    pub const FRONTMATTER: DumpOpts = DumpOpts {
        sort_keys: false,
        allow_unicode: true,
        default_flow_style: false,
        width: 100_000,
    };
    /// adapters/_util.py `dump_yaml`: `sort_keys=True`, `width=100000`.
    pub const ADAPTER: DumpOpts = DumpOpts {
        sort_keys: true,
        allow_unicode: true,
        default_flow_style: false,
        width: 100_000,
    };
}

/// `yaml.safe_dump(value, **opts)`, byte for byte (`default_flow_style: true` is PyYAML's `True`: flow style for every
/// collection). Not reproducible from a PyValue tree, by construction: PyYAML's `&id001`/`*id001` anchors for one Python
/// list/dict/date object reached twice (a PyValue has no identity), the hash order of a Python `set` (the PyValue
/// order is used), and `sort_keys` among NaN keys (a stable sort; CPython's timsort may differ).
pub fn dump(value: &PyValue, opts: &DumpOpts) -> String {
    let events = dumper::events(value, opts);
    emitter::Emitter::new(&events, opts.width, opts.allow_unicode).run()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deep documents load (or fail like Python) inside a 2 MiB thread, tokio's default worker/blocking stack.
    #[test]
    fn deep_documents_fit_a_small_stack() {
        let run = || {
            let flow = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
            let maps = |n: usize| format!("{}1{}", "{a: ".repeat(n), "}".repeat(n));
            let block = |n: usize| {
                let mut s: String = (0..n - 1).map(|i| format!("{}- \n", "  ".repeat(i))).collect();
                s.push_str(&format!("{}- 1\n", "  ".repeat(n - 1)));
                s
            };
            for doc in [flow(480), maps(480), block(480)] {
                assert!(load(&doc, Loader::Safe).is_ok());
                let deep = flow(5000);
                assert_eq!(
                    load(&deep, Loader::Safe).err().and_then(|e| e.python_exception()),
                    Some("RecursionError")
                );
                assert_eq!(
                    load(&doc, Loader::Strict).err().and_then(|e| e.python_exception()),
                    Some("RecursionError")
                );
            }
            assert!(load(&flow(320), Loader::Strict).is_ok());
            // a deeply nested mapping KEY is built "deep" by the strict loader
            let key = format!("? {}\n: v\n", flow(320));
            assert_eq!(
                load(&key, Loader::Strict).err().map(|e| e.full),
                Some("TypeError: unhashable type: 'list'".into())
            );
            let value = dump(&load(&flow(480), Loader::Safe).unwrap_or_default(), &DumpOpts::CONTENT);
            assert!(value.starts_with("- - - "));
        };
        let handle = std::thread::Builder::new().stack_size(2 * 1024 * 1024).spawn(run);
        assert!(handle.is_ok_and(|h| h.join().is_ok()));
    }
}
