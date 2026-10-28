//! PyYAML 6.0.3 `scanner.py` (+ `tokens.py`), ported method by method: the same token queue, the same simple-key
//! bookkeeping (`possible_simple_keys` per flow level, the 1024-character / single-line limit, `required` keys),
//! the same lazy pull model (`check_token` / `peek_token` / `get_token` scan only as far as the parser asks), so the
//! same input fails with the same message at the same mark.

use indexmap::IndexMap;

use super::error::{Error, Mark, repr_char};
use super::reader::Reader;

/// Block scalar / flow scalar style (`token.style`).
pub(crate) type Style = Option<char>;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kind {
    StreamStart,
    StreamEnd,
    /// `DirectiveToken(name, value)`: `YAML` -> (major, minor) as digit strings; `TAG` -> (handle, prefix).
    Directive {
        name: String,
        value: DirectiveValue,
    },
    DocumentStart,
    DocumentEnd,
    BlockSequenceStart,
    BlockMappingStart,
    BlockEnd,
    FlowSequenceStart,
    FlowMappingStart,
    FlowSequenceEnd,
    FlowMappingEnd,
    Key,
    Value,
    BlockEntry,
    FlowEntry,
    Alias(String),
    Anchor(String),
    /// `(handle, suffix)`; handle None for `!<...>` and the bare `!`.
    Tag(Option<String>, String),
    Scalar {
        value: String,
        plain: bool,
        style: Style,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DirectiveValue {
    None,
    Yaml(String, String),
    Tag(String, String),
}

impl Kind {
    /// `Token.id`.
    pub(crate) fn id(&self) -> &'static str {
        match self {
            Kind::StreamStart => "<stream start>",
            Kind::StreamEnd => "<stream end>",
            Kind::Directive { .. } => "<directive>",
            Kind::DocumentStart => "<document start>",
            Kind::DocumentEnd => "<document end>",
            Kind::BlockSequenceStart => "<block sequence start>",
            Kind::BlockMappingStart => "<block mapping start>",
            Kind::BlockEnd => "<block end>",
            Kind::FlowSequenceStart => "[",
            Kind::FlowMappingStart => "{",
            Kind::FlowSequenceEnd => "]",
            Kind::FlowMappingEnd => "}",
            Kind::Key => "?",
            Kind::Value => ":",
            Kind::BlockEntry => "-",
            Kind::FlowEntry => ",",
            Kind::Alias(_) => "<alias>",
            Kind::Anchor(_) => "<anchor>",
            Kind::Tag(..) => "<tag>",
            Kind::Scalar { .. } => "<scalar>",
        }
    }
}

/// The discriminant only (for `check_token(*choices)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum T {
    StreamStart,
    StreamEnd,
    Directive,
    DocumentStart,
    DocumentEnd,
    BlockSequenceStart,
    BlockMappingStart,
    BlockEnd,
    FlowSequenceStart,
    FlowMappingStart,
    FlowSequenceEnd,
    FlowMappingEnd,
    Key,
    Value,
    BlockEntry,
    FlowEntry,
    Alias,
    Anchor,
    Tag,
    Scalar,
}

impl Kind {
    pub(crate) fn t(&self) -> T {
        match self {
            Kind::StreamStart => T::StreamStart,
            Kind::StreamEnd => T::StreamEnd,
            Kind::Directive { .. } => T::Directive,
            Kind::DocumentStart => T::DocumentStart,
            Kind::DocumentEnd => T::DocumentEnd,
            Kind::BlockSequenceStart => T::BlockSequenceStart,
            Kind::BlockMappingStart => T::BlockMappingStart,
            Kind::BlockEnd => T::BlockEnd,
            Kind::FlowSequenceStart => T::FlowSequenceStart,
            Kind::FlowMappingStart => T::FlowMappingStart,
            Kind::FlowSequenceEnd => T::FlowSequenceEnd,
            Kind::FlowMappingEnd => T::FlowMappingEnd,
            Kind::Key => T::Key,
            Kind::Value => T::Value,
            Kind::BlockEntry => T::BlockEntry,
            Kind::FlowEntry => T::FlowEntry,
            Kind::Alias(_) => T::Alias,
            Kind::Anchor(_) => T::Anchor,
            Kind::Tag(..) => T::Tag,
            Kind::Scalar { .. } => T::Scalar,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub(crate) kind: Kind,
    pub(crate) start: Mark,
    pub(crate) end: Mark,
}

#[derive(Debug, Clone, Copy)]
struct SimpleKey {
    token_number: usize,
    required: bool,
    index: usize,
    line: usize,
    column: usize,
    mark: Mark,
}

/// `'\0 \t\r\n\x85  '`
fn z_ws(c: char) -> bool {
    matches!(c, '\0' | ' ' | '\t' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// `'\0 \r\n\x85  '` (no tab)
fn z_sp(c: char) -> bool {
    matches!(c, '\0' | ' ' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// `'\0\r\n\x85  '`
fn z_br(c: char) -> bool {
    matches!(c, '\0' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// `'\r\n\x85  '`
fn br(c: char) -> bool {
    matches!(c, '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

fn alnum_dash(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

fn icol(c: usize) -> isize {
    isize::try_from(c).unwrap_or(isize::MAX)
}

pub(crate) struct Scanner {
    pub(crate) r: Reader,
    done: bool,
    flow_level: usize,
    tokens: Vec<Token>,
    tokens_taken: usize,
    indent: isize,
    indents: Vec<isize>,
    allow_simple_key: bool,
    possible_simple_keys: IndexMap<usize, SimpleKey>,
}

type R<T> = Result<T, Error>;

impl Scanner {
    pub(crate) fn new(r: Reader) -> Self {
        let mut s = Self {
            r,
            done: false,
            flow_level: 0,
            tokens: Vec::new(),
            tokens_taken: 0,
            indent: -1,
            indents: Vec::new(),
            allow_simple_key: true,
            possible_simple_keys: IndexMap::new(),
        };
        // fetch_stream_start
        let mark = s.r.get_mark();
        s.tokens.push(Token {
            kind: Kind::StreamStart,
            start: mark,
            end: mark,
        });
        s
    }

    fn mark(&self) -> Mark {
        self.r.get_mark()
    }

    fn peek(&self, i: usize) -> char {
        self.r.peek(i)
    }

    fn forward(&mut self, n: usize) {
        self.r.forward(n);
    }

    fn column(&self) -> usize {
        self.r.column
    }

    // ---- public ----------------------------------------------------------------------------------------------

    pub(crate) fn check_token(&mut self, choices: &[T]) -> R<bool> {
        self.fill()?;
        Ok(match self.tokens.first() {
            Some(t) => choices.is_empty() || choices.contains(&t.kind.t()),
            None => false,
        })
    }

    pub(crate) fn peek_token(&mut self) -> R<Option<&Token>> {
        self.fill()?;
        Ok(self.tokens.first())
    }

    pub(crate) fn get_token(&mut self) -> R<Option<Token>> {
        self.fill()?;
        if self.tokens.is_empty() {
            return Ok(None);
        }
        self.tokens_taken += 1;
        Ok(Some(self.tokens.remove(0)))
    }

    fn fill(&mut self) -> R<()> {
        while self.need_more_tokens()? {
            self.fetch_more_tokens()?;
        }
        Ok(())
    }

    // ---- private ---------------------------------------------------------------------------------------------

    fn need_more_tokens(&mut self) -> R<bool> {
        if self.done {
            return Ok(false);
        }
        if self.tokens.is_empty() {
            return Ok(true);
        }
        self.stale_possible_simple_keys()?;
        Ok(self.next_possible_simple_key() == Some(self.tokens_taken))
    }

    fn fetch_more_tokens(&mut self) -> R<()> {
        self.scan_to_next_token();
        self.stale_possible_simple_keys()?;
        self.unwind_indent(icol(self.column()));
        let ch = self.peek(0);
        match ch {
            '\0' => return self.fetch_stream_end(),
            '%' if self.check_directive() => return self.fetch_directive(),
            '-' if self.check_document_start() => return self.fetch_document_indicator(Kind::DocumentStart),
            '.' if self.check_document_end() => return self.fetch_document_indicator(Kind::DocumentEnd),
            '[' => return self.fetch_flow_collection_start(Kind::FlowSequenceStart),
            '{' => return self.fetch_flow_collection_start(Kind::FlowMappingStart),
            ']' => return self.fetch_flow_collection_end(Kind::FlowSequenceEnd),
            '}' => return self.fetch_flow_collection_end(Kind::FlowMappingEnd),
            ',' => return self.fetch_flow_entry(),
            _ => {}
        }
        if ch == '-' && self.check_block_entry() {
            return self.fetch_block_entry();
        }
        if ch == '?' && self.check_key() {
            return self.fetch_key();
        }
        if ch == ':' && self.check_value() {
            return self.fetch_value();
        }
        match ch {
            '*' => return self.fetch_anchor_or_alias(true),
            '&' => return self.fetch_anchor_or_alias(false),
            '!' => return self.fetch_tag(),
            '|' if self.flow_level == 0 => return self.fetch_block_scalar('|'),
            '>' if self.flow_level == 0 => return self.fetch_block_scalar('>'),
            '\'' => return self.fetch_flow_scalar('\''),
            '"' => return self.fetch_flow_scalar('"'),
            _ => {}
        }
        if self.check_plain() {
            return self.fetch_plain();
        }
        Err(Error::marked(
            Some("while scanning for the next token"),
            None,
            format!("found character {} that cannot start any token", repr_char(ch)),
            Some(self.mark()),
        ))
    }

    // Simple keys treatment.

    fn next_possible_simple_key(&self) -> Option<usize> {
        self.possible_simple_keys.values().map(|k| k.token_number).min()
    }

    fn stale_possible_simple_keys(&mut self) -> R<()> {
        let levels: Vec<usize> = self.possible_simple_keys.keys().copied().collect();
        for level in levels {
            let Some(key) = self.possible_simple_keys.get(&level).copied() else {
                continue;
            };
            if key.line != self.r.line || self.r.pointer - key.index > 1024 {
                if key.required {
                    return Err(Error::marked(
                        Some("while scanning a simple key"),
                        Some(key.mark),
                        "could not find expected ':'",
                        Some(self.mark()),
                    ));
                }
                self.possible_simple_keys.shift_remove(&level);
            }
        }
        Ok(())
    }

    fn save_possible_simple_key(&mut self) -> R<()> {
        let required = self.flow_level == 0 && self.indent == icol(self.column());
        if self.allow_simple_key {
            self.remove_possible_simple_key()?;
            let token_number = self.tokens_taken + self.tokens.len();
            let key = SimpleKey {
                token_number,
                required,
                index: self.r.pointer,
                line: self.r.line,
                column: self.column(),
                mark: self.mark(),
            };
            self.possible_simple_keys.insert(self.flow_level, key);
        }
        Ok(())
    }

    fn remove_possible_simple_key(&mut self) -> R<()> {
        if let Some(key) = self.possible_simple_keys.get(&self.flow_level).copied() {
            if key.required {
                return Err(Error::marked(
                    Some("while scanning a simple key"),
                    Some(key.mark),
                    "could not find expected ':'",
                    Some(self.mark()),
                ));
            }
            self.possible_simple_keys.shift_remove(&self.flow_level);
        }
        Ok(())
    }

    // Indentation functions.

    fn unwind_indent(&mut self, column: isize) {
        if self.flow_level > 0 {
            return;
        }
        while self.indent > column {
            let mark = self.mark();
            self.indent = self.indents.pop().unwrap_or(-1);
            self.tokens.push(Token {
                kind: Kind::BlockEnd,
                start: mark,
                end: mark,
            });
        }
    }

    fn add_indent(&mut self, column: isize) -> bool {
        if self.indent < column {
            self.indents.push(self.indent);
            self.indent = column;
            return true;
        }
        false
    }

    // Fetchers.

    fn simple(&mut self, kind: Kind, n: usize) {
        let start = self.mark();
        self.forward(n);
        let end = self.mark();
        self.tokens.push(Token { kind, start, end });
    }

    fn fetch_stream_end(&mut self) -> R<()> {
        self.unwind_indent(-1);
        self.remove_possible_simple_key()?;
        self.allow_simple_key = false;
        self.possible_simple_keys.clear();
        let mark = self.mark();
        self.tokens.push(Token {
            kind: Kind::StreamEnd,
            start: mark,
            end: mark,
        });
        self.done = true;
        Ok(())
    }

    fn fetch_directive(&mut self) -> R<()> {
        self.unwind_indent(-1);
        self.remove_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_directive()?;
        self.tokens.push(t);
        Ok(())
    }

    fn fetch_document_indicator(&mut self, kind: Kind) -> R<()> {
        self.unwind_indent(-1);
        self.remove_possible_simple_key()?;
        self.allow_simple_key = false;
        self.simple(kind, 3);
        Ok(())
    }

    fn fetch_flow_collection_start(&mut self, kind: Kind) -> R<()> {
        self.save_possible_simple_key()?;
        self.flow_level = self.flow_level.wrapping_add(1);
        self.allow_simple_key = true;
        self.simple(kind, 1);
        Ok(())
    }

    fn fetch_flow_collection_end(&mut self, kind: Kind) -> R<()> {
        self.remove_possible_simple_key()?;
        // Python's flow_level may go negative on a stray ']' / '}'; it is only ever compared with 0 (truthiness) and
        // used as a dict key. Keep it signed in spirit: a stray closer at level 0 makes the level -1 (truthy).
        self.flow_level = self.flow_level.wrapping_sub(1);
        self.allow_simple_key = false;
        self.simple(kind, 1);
        Ok(())
    }

    fn fetch_flow_entry(&mut self) -> R<()> {
        self.allow_simple_key = true;
        self.remove_possible_simple_key()?;
        self.simple(Kind::FlowEntry, 1);
        Ok(())
    }

    fn fetch_block_entry(&mut self) -> R<()> {
        if self.flow_level == 0 {
            if !self.allow_simple_key {
                return Err(Error::marked(
                    None,
                    None,
                    "sequence entries are not allowed here",
                    Some(self.mark()),
                ));
            }
            if self.add_indent(icol(self.column())) {
                let mark = self.mark();
                self.tokens.push(Token {
                    kind: Kind::BlockSequenceStart,
                    start: mark,
                    end: mark,
                });
            }
        }
        self.allow_simple_key = true;
        self.remove_possible_simple_key()?;
        self.simple(Kind::BlockEntry, 1);
        Ok(())
    }

    fn fetch_key(&mut self) -> R<()> {
        if self.flow_level == 0 {
            if !self.allow_simple_key {
                return Err(Error::marked(
                    None,
                    None,
                    "mapping keys are not allowed here",
                    Some(self.mark()),
                ));
            }
            if self.add_indent(icol(self.column())) {
                let mark = self.mark();
                self.tokens.push(Token {
                    kind: Kind::BlockMappingStart,
                    start: mark,
                    end: mark,
                });
            }
        }
        self.allow_simple_key = self.flow_level == 0;
        self.remove_possible_simple_key()?;
        self.simple(Kind::Key, 1);
        Ok(())
    }

    fn fetch_value(&mut self) -> R<()> {
        if let Some(key) = self.possible_simple_keys.get(&self.flow_level).copied() {
            self.possible_simple_keys.shift_remove(&self.flow_level);
            let at = key
                .token_number
                .saturating_sub(self.tokens_taken)
                .min(self.tokens.len());
            self.tokens.insert(
                at,
                Token {
                    kind: Kind::Key,
                    start: key.mark,
                    end: key.mark,
                },
            );
            if self.flow_level == 0 && self.add_indent(icol(key.column)) {
                self.tokens.insert(
                    at,
                    Token {
                        kind: Kind::BlockMappingStart,
                        start: key.mark,
                        end: key.mark,
                    },
                );
            }
            self.allow_simple_key = false;
        } else {
            if self.flow_level == 0 {
                if !self.allow_simple_key {
                    return Err(Error::marked(
                        None,
                        None,
                        "mapping values are not allowed here",
                        Some(self.mark()),
                    ));
                }
                if self.add_indent(icol(self.column())) {
                    let mark = self.mark();
                    self.tokens.push(Token {
                        kind: Kind::BlockMappingStart,
                        start: mark,
                        end: mark,
                    });
                }
            }
            self.allow_simple_key = self.flow_level == 0;
            self.remove_possible_simple_key()?;
        }
        self.simple(Kind::Value, 1);
        Ok(())
    }

    fn fetch_anchor_or_alias(&mut self, alias: bool) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_anchor(alias)?;
        self.tokens.push(t);
        Ok(())
    }

    fn fetch_tag(&mut self) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_tag()?;
        self.tokens.push(t);
        Ok(())
    }

    fn fetch_block_scalar(&mut self, style: char) -> R<()> {
        self.allow_simple_key = true;
        self.remove_possible_simple_key()?;
        let t = self.scan_block_scalar(style)?;
        self.tokens.push(t);
        Ok(())
    }

    fn fetch_flow_scalar(&mut self, style: char) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_flow_scalar(style)?;
        self.tokens.push(t);
        Ok(())
    }

    fn fetch_plain(&mut self) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_plain();
        self.tokens.push(t);
        Ok(())
    }

    // Checkers.

    fn check_directive(&self) -> bool {
        self.column() == 0
    }

    fn check_document_start(&self) -> bool {
        self.column() == 0 && self.r.prefix(3) == "---" && z_ws(self.peek(3))
    }

    fn check_document_end(&self) -> bool {
        self.column() == 0 && self.r.prefix(3) == "..." && z_ws(self.peek(3))
    }

    fn check_block_entry(&self) -> bool {
        z_ws(self.peek(1))
    }

    fn check_key(&self) -> bool {
        self.flow_level != 0 || z_ws(self.peek(1))
    }

    fn check_value(&self) -> bool {
        self.flow_level != 0 || z_ws(self.peek(1))
    }

    fn check_plain(&self) -> bool {
        let ch = self.peek(0);
        let indicator = z_ws(ch) || "-?:,[]{}#&*!|>'\"%@`".contains(ch);
        !indicator || (!z_ws(self.peek(1)) && (ch == '-' || (self.flow_level == 0 && (ch == '?' || ch == ':'))))
    }

    // Scanners.

    fn scan_to_next_token(&mut self) {
        if self.r.pointer == 0 && self.peek(0) == '\u{feff}' {
            self.forward(1);
        }
        loop {
            while self.peek(0) == ' ' {
                self.forward(1);
            }
            if self.peek(0) == '#' {
                while !z_br(self.peek(0)) {
                    self.forward(1);
                }
            }
            if self.scan_line_break().is_empty() {
                break;
            }
            if self.flow_level == 0 {
                self.allow_simple_key = true;
            }
        }
    }

    fn scan_directive(&mut self) -> R<Token> {
        let start_mark = self.mark();
        self.forward(1);
        let name = self.scan_directive_name(start_mark)?;
        let value;
        let end_mark;
        if name == "YAML" {
            value = self.scan_yaml_directive_value(start_mark)?;
            end_mark = self.mark();
        } else if name == "TAG" {
            value = self.scan_tag_directive_value(start_mark)?;
            end_mark = self.mark();
        } else {
            value = DirectiveValue::None;
            end_mark = self.mark();
            while !z_br(self.peek(0)) {
                self.forward(1);
            }
        }
        self.scan_directive_ignored_line(start_mark)?;
        Ok(Token {
            kind: Kind::Directive { name, value },
            start: start_mark,
            end: end_mark,
        })
    }

    fn directive_err(&self, start_mark: Mark, problem: String) -> Error {
        Error::marked(
            Some("while scanning a directive"),
            Some(start_mark),
            problem,
            Some(self.mark()),
        )
    }

    fn scan_directive_name(&mut self, start_mark: Mark) -> R<String> {
        let mut length = 0;
        let mut ch = self.peek(length);
        while alnum_dash(ch) {
            length += 1;
            ch = self.peek(length);
        }
        if length == 0 {
            return Err(self.directive_err(
                start_mark,
                format!("expected alphabetic or numeric character, but found {}", repr_char(ch)),
            ));
        }
        let value = self.r.prefix(length);
        self.forward(length);
        let ch = self.peek(0);
        if !z_sp(ch) {
            return Err(self.directive_err(
                start_mark,
                format!("expected alphabetic or numeric character, but found {}", repr_char(ch)),
            ));
        }
        Ok(value)
    }

    fn scan_yaml_directive_value(&mut self, start_mark: Mark) -> R<DirectiveValue> {
        while self.peek(0) == ' ' {
            self.forward(1);
        }
        let major = self.scan_yaml_directive_number(start_mark)?;
        if self.peek(0) != '.' {
            return Err(self.directive_err(
                start_mark,
                format!("expected a digit or '.', but found {}", repr_char(self.peek(0))),
            ));
        }
        self.forward(1);
        let minor = self.scan_yaml_directive_number(start_mark)?;
        if !z_sp(self.peek(0)) {
            return Err(self.directive_err(
                start_mark,
                format!("expected a digit or ' ', but found {}", repr_char(self.peek(0))),
            ));
        }
        Ok(DirectiveValue::Yaml(major, minor))
    }

    fn scan_yaml_directive_number(&mut self, start_mark: Mark) -> R<String> {
        let ch = self.peek(0);
        if !ch.is_ascii_digit() {
            return Err(self.directive_err(start_mark, format!("expected a digit, but found {}", repr_char(ch))));
        }
        let mut length = 0;
        while self.peek(length).is_ascii_digit() {
            length += 1;
        }
        let digits = self.r.prefix(length);
        // `int(self.prefix(length))`: CPython 3.13 refuses decimal strings over 4300 digits.
        super::pyint::check_str_digits(&digits)?;
        self.forward(length);
        Ok(digits)
    }

    fn scan_tag_directive_value(&mut self, start_mark: Mark) -> R<DirectiveValue> {
        while self.peek(0) == ' ' {
            self.forward(1);
        }
        let handle = self.scan_tag_handle("directive", start_mark)?;
        let ch = self.peek(0);
        if ch != ' ' {
            return Err(self.directive_err(start_mark, format!("expected ' ', but found {}", repr_char(ch))));
        }
        while self.peek(0) == ' ' {
            self.forward(1);
        }
        let prefix = self.scan_tag_uri("directive", start_mark)?;
        let ch = self.peek(0);
        if !z_sp(ch) {
            return Err(self.directive_err(start_mark, format!("expected ' ', but found {}", repr_char(ch))));
        }
        Ok(DirectiveValue::Tag(handle, prefix))
    }

    fn scan_directive_ignored_line(&mut self, start_mark: Mark) -> R<()> {
        while self.peek(0) == ' ' {
            self.forward(1);
        }
        if self.peek(0) == '#' {
            while !z_br(self.peek(0)) {
                self.forward(1);
            }
        }
        let ch = self.peek(0);
        if !z_br(ch) {
            return Err(self.directive_err(
                start_mark,
                format!("expected a comment or a line break, but found {}", repr_char(ch)),
            ));
        }
        self.scan_line_break();
        Ok(())
    }

    fn scan_anchor(&mut self, alias: bool) -> R<Token> {
        let start_mark = self.mark();
        let name = if alias { "alias" } else { "anchor" };
        self.forward(1);
        let mut length = 0;
        let mut ch = self.peek(length);
        while alnum_dash(ch) {
            length += 1;
            ch = self.peek(length);
        }
        let err = |me: &Self, ch: char| {
            Error::marked(
                Some(&format!("while scanning an {name}")),
                Some(start_mark),
                format!("expected alphabetic or numeric character, but found {}", repr_char(ch)),
                Some(me.mark()),
            )
        };
        if length == 0 {
            return Err(err(self, ch));
        }
        let value = self.r.prefix(length);
        self.forward(length);
        let ch = self.peek(0);
        if !(z_ws(ch) || "?:,]}%@`".contains(ch)) {
            return Err(err(self, ch));
        }
        let end_mark = self.mark();
        Ok(Token {
            kind: if alias { Kind::Alias(value) } else { Kind::Anchor(value) },
            start: start_mark,
            end: end_mark,
        })
    }

    fn scan_tag(&mut self) -> R<Token> {
        let start_mark = self.mark();
        let mut ch = self.peek(1);
        let handle;
        let suffix;
        if ch == '<' {
            handle = None;
            self.forward(2);
            suffix = self.scan_tag_uri("tag", start_mark)?;
            if self.peek(0) != '>' {
                return Err(Error::marked(
                    Some("while parsing a tag"),
                    Some(start_mark),
                    format!("expected '>', but found {}", repr_char(self.peek(0))),
                    Some(self.mark()),
                ));
            }
            self.forward(1);
        } else if z_ws(ch) {
            handle = None;
            suffix = "!".to_string();
            self.forward(1);
        } else {
            let mut length = 1;
            let mut use_handle = false;
            while !z_sp(ch) {
                if ch == '!' {
                    use_handle = true;
                    break;
                }
                length += 1;
                ch = self.peek(length);
            }
            if use_handle {
                handle = Some(self.scan_tag_handle("tag", start_mark)?);
            } else {
                handle = Some("!".to_string());
                self.forward(1);
            }
            suffix = self.scan_tag_uri("tag", start_mark)?;
        }
        let ch = self.peek(0);
        if !z_sp(ch) {
            return Err(Error::marked(
                Some("while scanning a tag"),
                Some(start_mark),
                format!("expected ' ', but found {}", repr_char(ch)),
                Some(self.mark()),
            ));
        }
        let end_mark = self.mark();
        Ok(Token {
            kind: Kind::Tag(handle, suffix),
            start: start_mark,
            end: end_mark,
        })
    }

    fn scan_block_scalar(&mut self, style: char) -> R<Token> {
        let folded = style == '>';
        let mut chunks = String::new();
        let start_mark = self.mark();
        self.forward(1);
        let (chomping, increment) = self.scan_block_scalar_indicators(start_mark)?;
        self.scan_block_scalar_ignored_line(start_mark)?;

        let min_indent = (self.indent + 1).max(1);
        let indent;
        let mut breaks;
        let mut end_mark;
        match increment {
            None => {
                let (b, max_indent, e) = self.scan_block_scalar_indentation();
                breaks = b;
                end_mark = e;
                indent = min_indent.max(icol(max_indent));
            }
            Some(inc) => {
                indent = min_indent + inc - 1;
                let (b, e) = self.scan_block_scalar_breaks(indent);
                breaks = b;
                end_mark = e;
            }
        }
        let mut line_break = String::new();

        while icol(self.column()) == indent && self.peek(0) != '\0' {
            for b in &breaks {
                chunks.push_str(b);
            }
            let leading_non_space = !matches!(self.peek(0), ' ' | '\t');
            let mut length = 0;
            while !z_br(self.peek(length)) {
                length += 1;
            }
            chunks.push_str(&self.r.prefix(length));
            self.forward(length);
            line_break = self.scan_line_break();
            let (b, e) = self.scan_block_scalar_breaks(indent);
            breaks = b;
            end_mark = e;
            if icol(self.column()) == indent && self.peek(0) != '\0' {
                if folded && line_break == "\n" && leading_non_space && !matches!(self.peek(0), ' ' | '\t') {
                    if breaks.is_empty() {
                        chunks.push(' ');
                    }
                } else {
                    chunks.push_str(&line_break);
                }
            } else {
                break;
            }
        }

        if chomping != Some(false) {
            chunks.push_str(&line_break);
        }
        if chomping == Some(true) {
            for b in &breaks {
                chunks.push_str(b);
            }
        }
        Ok(Token {
            kind: Kind::Scalar {
                value: chunks,
                plain: false,
                style: Some(style),
            },
            start: start_mark,
            end: end_mark,
        })
    }

    fn scan_block_scalar_indicators(&mut self, start_mark: Mark) -> R<(Option<bool>, Option<isize>)> {
        let mut chomping = None;
        let mut increment = None;
        let bad0 = |me: &Self| {
            Error::marked(
                Some("while scanning a block scalar"),
                Some(start_mark),
                "expected indentation indicator in the range 1-9, but found 0",
                Some(me.mark()),
            )
        };
        let mut ch = self.peek(0);
        if ch == '+' || ch == '-' {
            chomping = Some(ch == '+');
            self.forward(1);
            ch = self.peek(0);
            if let Some(d) = ch.to_digit(10) {
                if d == 0 {
                    return Err(bad0(self));
                }
                increment = Some(isize::try_from(d).unwrap_or(1));
                self.forward(1);
            }
        } else if let Some(d) = ch.to_digit(10) {
            if d == 0 {
                return Err(bad0(self));
            }
            increment = Some(isize::try_from(d).unwrap_or(1));
            self.forward(1);
            ch = self.peek(0);
            if ch == '+' || ch == '-' {
                chomping = Some(ch == '+');
                self.forward(1);
            }
        }
        let ch = self.peek(0);
        if !z_sp(ch) {
            return Err(Error::marked(
                Some("while scanning a block scalar"),
                Some(start_mark),
                format!(
                    "expected chomping or indentation indicators, but found {}",
                    repr_char(ch)
                ),
                Some(self.mark()),
            ));
        }
        Ok((chomping, increment))
    }

    fn scan_block_scalar_ignored_line(&mut self, start_mark: Mark) -> R<()> {
        while self.peek(0) == ' ' {
            self.forward(1);
        }
        if self.peek(0) == '#' {
            while !z_br(self.peek(0)) {
                self.forward(1);
            }
        }
        let ch = self.peek(0);
        if !z_br(ch) {
            return Err(Error::marked(
                Some("while scanning a block scalar"),
                Some(start_mark),
                format!("expected a comment or a line break, but found {}", repr_char(ch)),
                Some(self.mark()),
            ));
        }
        self.scan_line_break();
        Ok(())
    }

    fn scan_block_scalar_indentation(&mut self) -> (Vec<String>, usize, Mark) {
        let mut chunks = Vec::new();
        let mut max_indent = 0;
        let mut end_mark = self.mark();
        while matches!(self.peek(0), ' ' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            if self.peek(0) == ' ' {
                self.forward(1);
                if self.column() > max_indent {
                    max_indent = self.column();
                }
            } else {
                chunks.push(self.scan_line_break());
                end_mark = self.mark();
            }
        }
        (chunks, max_indent, end_mark)
    }

    fn scan_block_scalar_breaks(&mut self, indent: isize) -> (Vec<String>, Mark) {
        let mut chunks = Vec::new();
        let mut end_mark = self.mark();
        while icol(self.column()) < indent && self.peek(0) == ' ' {
            self.forward(1);
        }
        while br(self.peek(0)) {
            chunks.push(self.scan_line_break());
            end_mark = self.mark();
            while icol(self.column()) < indent && self.peek(0) == ' ' {
                self.forward(1);
            }
        }
        (chunks, end_mark)
    }

    fn scan_flow_scalar(&mut self, style: char) -> R<Token> {
        let double = style == '"';
        let mut chunks = String::new();
        let start_mark = self.mark();
        let quote = self.peek(0);
        self.forward(1);
        self.scan_flow_scalar_non_spaces(double, start_mark, &mut chunks)?;
        while self.peek(0) != quote {
            self.scan_flow_scalar_spaces(double, start_mark, &mut chunks)?;
            self.scan_flow_scalar_non_spaces(double, start_mark, &mut chunks)?;
        }
        self.forward(1);
        let end_mark = self.mark();
        Ok(Token {
            kind: Kind::Scalar {
                value: chunks,
                plain: false,
                style: Some(style),
            },
            start: start_mark,
            end: end_mark,
        })
    }

    fn scan_flow_scalar_non_spaces(&mut self, double: bool, start_mark: Mark, chunks: &mut String) -> R<()> {
        loop {
            let mut length = 0;
            while !matches!(
                self.peek(length),
                '\'' | '"' | '\\' | '\0' | ' ' | '\t' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}'
            ) {
                length += 1;
            }
            if length > 0 {
                chunks.push_str(&self.r.prefix(length));
                self.forward(length);
            }
            let ch = self.peek(0);
            if !double && ch == '\'' && self.peek(1) == '\'' {
                chunks.push('\'');
                self.forward(2);
            } else if (double && ch == '\'') || (!double && (ch == '"' || ch == '\\')) {
                chunks.push(ch);
                self.forward(1);
            } else if double && ch == '\\' {
                self.forward(1);
                let ch = self.peek(0);
                if let Some(rep) = escape_replacement(ch) {
                    chunks.push(rep);
                    self.forward(1);
                } else if let Some(len) = escape_code_len(ch) {
                    self.forward(1);
                    for k in 0..len {
                        if !self.peek(k).is_ascii_hexdigit() {
                            return Err(Error::marked(
                                Some("while scanning a double-quoted scalar"),
                                Some(start_mark),
                                format!(
                                    "expected escape sequence of {len} hexadecimal numbers, but found {}",
                                    repr_char(self.peek(k))
                                ),
                                Some(self.mark()),
                            ));
                        }
                    }
                    let code = u32::from_str_radix(&self.r.prefix(len), 16).unwrap_or(0);
                    match char::from_u32(code) {
                        Some(c) => chunks.push(c),
                        // chr() of a surrogate is a lone surrogate in Python; a Rust String cannot hold one, so
                        // keep the code point as U+FFFD (DEVIATION D-YAML-SURROGATE).
                        None if code <= 0x10ffff => chunks.push('\u{fffd}'),
                        None => {
                            return Err(Error::python("ValueError", "chr() arg not in range(0x110000)"));
                        }
                    }
                    self.forward(len);
                } else if br(ch) {
                    self.scan_line_break();
                    self.scan_flow_scalar_breaks(start_mark, chunks)?;
                } else {
                    return Err(Error::marked(
                        Some("while scanning a double-quoted scalar"),
                        Some(start_mark),
                        format!("found unknown escape character {}", repr_char(ch)),
                        Some(self.mark()),
                    ));
                }
            } else {
                return Ok(());
            }
        }
    }

    fn scan_flow_scalar_spaces(&mut self, _double: bool, start_mark: Mark, chunks: &mut String) -> R<()> {
        let mut length = 0;
        while matches!(self.peek(length), ' ' | '\t') {
            length += 1;
        }
        let whitespaces = self.r.prefix(length);
        self.forward(length);
        let ch = self.peek(0);
        if ch == '\0' {
            return Err(Error::marked(
                Some("while scanning a quoted scalar"),
                Some(start_mark),
                "found unexpected end of stream",
                Some(self.mark()),
            ));
        } else if br(ch) {
            let line_break = self.scan_line_break();
            let mut breaks = String::new();
            let n = self.scan_flow_scalar_breaks(start_mark, &mut breaks)?;
            if line_break != "\n" {
                chunks.push_str(&line_break);
            } else if n == 0 {
                chunks.push(' ');
            }
            chunks.push_str(&breaks);
        } else {
            chunks.push_str(&whitespaces);
        }
        Ok(())
    }

    /// Returns the number of line breaks appended.
    fn scan_flow_scalar_breaks(&mut self, start_mark: Mark, chunks: &mut String) -> R<usize> {
        let mut n = 0;
        loop {
            let prefix = self.r.prefix(3);
            if (prefix == "---" || prefix == "...") && z_ws(self.peek(3)) {
                return Err(Error::marked(
                    Some("while scanning a quoted scalar"),
                    Some(start_mark),
                    "found unexpected document separator",
                    Some(self.mark()),
                ));
            }
            while matches!(self.peek(0), ' ' | '\t') {
                self.forward(1);
            }
            if br(self.peek(0)) {
                chunks.push_str(&self.scan_line_break());
                n += 1;
            } else {
                return Ok(n);
            }
        }
    }

    fn scan_plain(&mut self) -> Token {
        let mut chunks = String::new();
        let start_mark = self.mark();
        let mut end_mark = start_mark;
        let indent = self.indent + 1;
        let mut spaces: Vec<String> = Vec::new();
        loop {
            let mut length = 0;
            if self.peek(0) == '#' {
                break;
            }
            loop {
                let ch = self.peek(length);
                if z_ws(ch)
                    || (ch == ':' && {
                        let n = self.peek(length + 1);
                        z_ws(n) || (self.flow_level != 0 && ",[]{}".contains(n))
                    })
                    || (self.flow_level != 0 && ",?[]{}".contains(ch))
                {
                    break;
                }
                length += 1;
            }
            if length == 0 {
                break;
            }
            self.allow_simple_key = false;
            for s in &spaces {
                chunks.push_str(s);
            }
            chunks.push_str(&self.r.prefix(length));
            self.forward(length);
            end_mark = self.mark();
            match self.scan_plain_spaces() {
                Some(s) if !s.is_empty() => spaces = s,
                _ => break,
            }
            if self.peek(0) == '#' || (self.flow_level == 0 && icol(self.column()) < indent) {
                break;
            }
        }
        Token {
            kind: Kind::Scalar {
                value: chunks,
                plain: true,
                style: None,
            },
            start: start_mark,
            end: end_mark,
        }
    }

    /// None where Python returns None (a document separator follows the line break).
    fn scan_plain_spaces(&mut self) -> Option<Vec<String>> {
        let mut chunks = Vec::new();
        let mut length = 0;
        while self.peek(length) == ' ' {
            length += 1;
        }
        let whitespaces = self.r.prefix(length);
        self.forward(length);
        let ch = self.peek(0);
        if br(ch) {
            let line_break = self.scan_line_break();
            self.allow_simple_key = true;
            let sep = |me: &Self| {
                let p = me.r.prefix(3);
                (p == "---" || p == "...") && z_ws(me.peek(3))
            };
            if sep(self) {
                return None;
            }
            let mut breaks = Vec::new();
            while matches!(self.peek(0), ' ' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}') {
                if self.peek(0) == ' ' {
                    self.forward(1);
                } else {
                    breaks.push(self.scan_line_break());
                    if sep(self) {
                        return None;
                    }
                }
            }
            if line_break != "\n" {
                chunks.push(line_break);
            } else if breaks.is_empty() {
                chunks.push(" ".to_string());
            }
            chunks.extend(breaks);
        } else if !whitespaces.is_empty() {
            chunks.push(whitespaces);
        }
        Some(chunks)
    }

    fn scan_tag_handle(&mut self, name: &str, start_mark: Mark) -> R<String> {
        let err = |me: &Self, ch: char| {
            Error::marked(
                Some(&format!("while scanning a {name}")),
                Some(start_mark),
                format!("expected '!', but found {}", repr_char(ch)),
                Some(me.mark()),
            )
        };
        let ch = self.peek(0);
        if ch != '!' {
            return Err(err(self, ch));
        }
        let mut length = 1;
        let mut ch = self.peek(length);
        if ch != ' ' {
            while alnum_dash(ch) {
                length += 1;
                ch = self.peek(length);
            }
            if ch != '!' {
                self.forward(length);
                return Err(err(self, ch));
            }
            length += 1;
        }
        let value = self.r.prefix(length);
        self.forward(length);
        Ok(value)
    }

    fn scan_tag_uri(&mut self, name: &str, start_mark: Mark) -> R<String> {
        let mut chunks = String::new();
        let mut any = false;
        let mut length = 0;
        let mut ch = self.peek(length);
        while ch.is_ascii_alphanumeric() || "-;/?:@&=+$,_.!~*'()[]%".contains(ch) {
            if ch == '%' {
                chunks.push_str(&self.r.prefix(length));
                self.forward(length);
                length = 0;
                chunks.push_str(&self.scan_uri_escapes(name, start_mark)?);
                any = true;
            } else {
                length += 1;
            }
            ch = self.peek(length);
        }
        if length > 0 {
            chunks.push_str(&self.r.prefix(length));
            self.forward(length);
            any = true;
        }
        if !any {
            return Err(Error::marked(
                Some(&format!("while parsing a {name}")),
                Some(start_mark),
                format!("expected URI, but found {}", repr_char(ch)),
                Some(self.mark()),
            ));
        }
        Ok(chunks)
    }

    fn scan_uri_escapes(&mut self, name: &str, start_mark: Mark) -> R<String> {
        let mut codes = Vec::new();
        let mark = self.mark();
        while self.peek(0) == '%' {
            self.forward(1);
            for k in 0..2 {
                if !self.peek(k).is_ascii_hexdigit() {
                    return Err(Error::marked(
                        Some(&format!("while scanning a {name}")),
                        Some(start_mark),
                        format!(
                            "expected URI escape sequence of 2 hexadecimal numbers, but found {}",
                            repr_char(self.peek(k))
                        ),
                        Some(self.mark()),
                    ));
                }
            }
            codes.push(u8::from_str_radix(&self.r.prefix(2), 16).unwrap_or(0));
            self.forward(2);
        }
        match std::str::from_utf8(&codes) {
            Ok(s) => Ok(s.to_string()),
            Err(e) => Err(Error::marked(
                Some(&format!("while scanning a {name}")),
                Some(start_mark),
                utf8_decode_error(&codes, &e),
                Some(mark),
            )),
        }
    }

    /// `scan_line_break`: `\r\n`, `\r`, `\n`, `\x85` -> `\n`; ` `/` ` kept; anything else -> "".
    fn scan_line_break(&mut self) -> String {
        let ch = self.peek(0);
        if matches!(ch, '\r' | '\n' | '\u{85}') {
            if self.r.prefix(2) == "\r\n" {
                self.forward(2);
            } else {
                self.forward(1);
            }
            return "\n".into();
        } else if matches!(ch, '\u{2028}' | '\u{2029}') {
            self.forward(1);
            return ch.to_string();
        }
        String::new()
    }
}

/// `Scanner.ESCAPE_REPLACEMENTS`.
fn escape_replacement(c: char) -> Option<char> {
    Some(match c {
        '0' => '\0',
        'a' => '\x07',
        'b' => '\x08',
        't' | '\t' => '\t',
        'n' => '\n',
        'v' => '\x0b',
        'f' => '\x0c',
        'r' => '\r',
        'e' => '\x1b',
        ' ' => ' ',
        '"' => '"',
        '\\' => '\\',
        '/' => '/',
        'N' => '\u{85}',
        '_' => '\u{a0}',
        'L' => '\u{2028}',
        'P' => '\u{2029}',
        _ => return None,
    })
}

/// `Scanner.ESCAPE_CODES`.
fn escape_code_len(c: char) -> Option<usize> {
    match c {
        'x' => Some(2),
        'u' => Some(4),
        'U' => Some(8),
        _ => None,
    }
}

/// `str(UnicodeDecodeError)` from `bytes(codes).decode('utf-8')`.
pub(crate) fn utf8_decode_error(data: &[u8], e: &std::str::Utf8Error) -> String {
    let start = e.valid_up_to();
    let (end, reason) = match e.error_len() {
        None => (data.len(), "unexpected end of data"),
        Some(n) => {
            let b = data[start];
            let invalid_start = n == 1 && (b >= 0xf5 || (0x80..=0xc1).contains(&b));
            (
                start + n,
                if invalid_start {
                    "invalid start byte"
                } else {
                    "invalid continuation byte"
                },
            )
        }
    };
    if end - start == 1 {
        format!(
            "'utf-8' codec can't decode byte 0x{:02x} in position {start}: {reason}",
            data[start]
        )
    } else {
        format!(
            "'utf-8' codec can't decode bytes in position {start}-{}: {reason}",
            end - 1
        )
    }
}
