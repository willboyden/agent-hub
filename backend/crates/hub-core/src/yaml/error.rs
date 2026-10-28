//! PyYAML 6.0.3 `error.py` (`Mark`, `MarkedYAMLError.__str__`) and `reader.py` `ReaderError`, plus the Python
//! exceptions that escape `yaml.load` unwrapped (a `ValueError` from `int()`/`datetime()`, a `KeyError` from the bool
//! table, ...). The input is always a Python `str`, so the stream name is `<unicode string>` and every mark carries a
//! snippet of the buffer (`text + '\0'`; `pointer == index`).

use super::YamlError;
use crate::py::text;

/// `Mark.name` for a `str` stream.
pub(crate) const NAME: &str = "<unicode string>";

/// PyYAML's `Mark` without its name and buffer: 0-based `line` and `column`, and `index` = the position in the input
/// `str` counted in code points (Python string indices). A BOM does not advance the column, `\r\n` is one line break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mark {
    pub index: usize,
    pub line: usize,
    pub column: usize,
}

/// `MarkedYAMLError(context, context_mark, problem, problem_mark, note)`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Marked {
    pub(crate) context: Option<String>,
    pub(crate) context_mark: Option<Mark>,
    pub(crate) problem: Option<String>,
    pub(crate) problem_mark: Option<Mark>,
}

/// Everything `yaml.load` can raise.
#[derive(Debug, Clone)]
pub(crate) enum Error {
    /// ScannerError / ParserError / ComposerError / ConstructorError.
    Marked(Box<Marked>),
    /// `ReaderError` for a `str` stream: a character outside the printable set.
    Reader { position: usize, character: u32 },
    /// domain/yamlsafe.py's own `YamlError`, raised from inside the strict loader (anchors, aliases, duplicate keys).
    Strict(String),
    /// A Python exception that is not a `yaml.YAMLError` (it escapes `yaml.load` and `yamlsafe.load` unchanged).
    Python { exc: &'static str, msg: String },
}

impl Error {
    pub(crate) fn marked(
        context: Option<&str>,
        context_mark: Option<Mark>,
        problem: impl Into<String>,
        problem_mark: Option<Mark>,
    ) -> Self {
        Error::Marked(Box::new(Marked {
            context: context.map(str::to_string),
            context_mark,
            problem: Some(problem.into()),
            problem_mark,
        }))
    }

    pub(crate) fn python(exc: &'static str, msg: impl Into<String>) -> Self {
        Error::Python { exc, msg: msg.into() }
    }

    /// `str(exc)` (with the loader's buffer for the snippets) turned into the API's [`YamlError`].
    pub(crate) fn render(&self, buffer: &[char]) -> YamlError {
        match self {
            Error::Marked(m) => {
                let full = marked_str(m, buffer);
                YamlError {
                    problem: first_line(&full),
                    full,
                    custom: false,
                    problem_mark: m.problem_mark,
                }
            }
            Error::Reader { position, character } => {
                let full = format!(
                    "unacceptable character #x{character:04x}: special characters are not allowed\n  in \"{NAME}\", position {position}"
                );
                YamlError {
                    problem: first_line(&full),
                    full,
                    custom: false,
                    problem_mark: None,
                }
            }
            Error::Strict(msg) => YamlError::custom(msg.clone()),
            Error::Python { exc, msg } => YamlError {
                problem: msg.clone(),
                full: format!("{exc}: {msg}"),
                custom: false,
                problem_mark: None,
            },
        }
    }
}

/// `str(exc).splitlines()[0]` (an empty message stays empty; yamlsafe then says "parse error").
fn first_line(s: &str) -> String {
    text::splitlines(s, false)
        .first()
        .map(|l| (*l).to_string())
        .unwrap_or_default()
}

fn is_break_or_nul(c: char) -> bool {
    matches!(c, '\0' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// `Mark.get_snippet(indent=4, max_length=75)`.
fn snippet(mark: &Mark, buffer: &[char]) -> String {
    const INDENT: usize = 4;
    // `pointer - start > max_length/2 - 1` with max_length = 75 is `> 36.5`, i.e. `> 36` in integers.
    const HALF: usize = 36;
    let pointer = mark.index.min(buffer.len());
    let mut head = "";
    let mut start = pointer;
    while start > 0 && !is_break_or_nul(buffer[start - 1]) {
        start -= 1;
        if pointer - start > HALF {
            head = " ... ";
            start += 5;
            break;
        }
    }
    let mut tail = "";
    let mut end = pointer;
    while end < buffer.len() && !is_break_or_nul(buffer[end]) {
        end += 1;
        if end - pointer > HALF {
            tail = " ... ";
            end -= 5;
            break;
        }
    }
    let body: String = buffer[start..end].iter().collect();
    format!(
        "{}{head}{body}{tail}\n{}^",
        " ".repeat(INDENT),
        " ".repeat(INDENT + pointer - start + head.len())
    )
}

/// `str(Mark)`.
pub(crate) fn mark_str(mark: &Mark, buffer: &[char]) -> String {
    format!(
        "  in \"{NAME}\", line {}, column {}:\n{}",
        mark.line + 1,
        mark.column + 1,
        snippet(mark, buffer)
    )
}

/// `MarkedYAMLError.__str__`.
fn marked_str(m: &Marked, buffer: &[char]) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(c) = &m.context {
        lines.push(c.clone());
    }
    if let Some(cm) = &m.context_mark {
        let differs = match (&m.problem, &m.problem_mark) {
            (Some(_), Some(pm)) => cm.line != pm.line || cm.column != pm.column,
            _ => true,
        };
        if differs {
            lines.push(mark_str(cm, buffer));
        }
    }
    if let Some(p) = &m.problem {
        lines.push(p.clone());
    }
    if let Some(pm) = &m.problem_mark {
        lines.push(mark_str(pm, buffer));
    }
    lines.join("\n")
}

/// `repr(ch)` of a one-character `str` (the `%r` in scanner messages).
pub(crate) fn repr_char(c: char) -> String {
    super::pyunicode::repr(c.encode_utf8(&mut [0u8; 4]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_truncates_like_pyyaml() {
        let buf: Vec<char> = format!("{}\0", "x".repeat(100)).chars().collect();
        let m = Mark {
            index: 50,
            line: 0,
            column: 50,
        };
        let s = mark_str(&m, &buf);
        assert_eq!(
            s,
            format!(
                "  in \"<unicode string>\", line 1, column 51:\n     ... {} ... \n{}^",
                "x".repeat(64),
                " ".repeat(41)
            )
        );
    }
}
