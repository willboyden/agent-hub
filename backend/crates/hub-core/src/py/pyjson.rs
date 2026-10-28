//! CPython 3.13 `json` over [`PyValue`], the one CPython-exact JSON decoder of the port (moved from agent-knowledge,
//! PORTING.md addendum A-11; the hub's request bodies and the knowledge service both use it):
//!
//! * [`loads_bytes`] = `json.loads(bytes)` (Starlette's `request.json()`, httpx's `Response.json()`): encoding detection
//!   (`json.detect_encoding`: UTF-8 with or without BOM, UTF-16/32 LE/BE), decoding with `surrogatepass`, then the C
//!   scanner (`_json.c`): the `JSONDecodeError.pos` of every error (FastAPI reports it as `loc: ["body", pos]`),
//!   `NaN`/`Infinity`, unbounded ints (`BigInt` beyond i64; more than 4300 digits is a `ValueError`), duplicate keys
//!   (last value wins, first position kept) and the C recursion limit (nesting deeper than [`MAX_DEPTH`] is a
//!   `RecursionError`). Golden cases from the real `json.loads`: tests/golden/json_loads.json.
//! * [`loads_str`] = `json.loads(str)` (Lance `meta_json`): no encoding step, a leading U+FEFF is an error at 0.
//! * [`dumps`] in the three flavours the service writes: `json.dumps(x)` (audit log, `meta_json`, scopes, SSE data),
//!   Starlette `JSONResponse` (problem bodies, httpx request bodies: compact, `ensure_ascii=False`) and FastAPI's
//!   pydantic serialisation of `dict[str, Any]` return values (compact, pydantic float text, NaN/inf as `null`).
//!
//! The parser is iterative, so hostile nesting cannot exhaust a thread stack.
//!
//! Known difference (PORTING.md D-K-SURROGATE): a lone UTF-16 surrogate (`"\ud800"`, or one passed through by
//! `surrogatepass`) cannot live in a Rust `String`; it decodes to U+FFFD. Character positions are unaffected.

use std::collections::HashMap;

use crate::py::float;
use crate::value::PyValue;

/// Deepest `[`/`{` nesting the Python service decodes from a request body: CPython 3.13's C scanner stops at 9998
/// levels from a plain script, and the server's own call chain (uvicorn -> Starlette -> FastAPI) uses 7 more of the
/// C recursion budget. Measured 2026-10-28 against `agent_knowledge.main` over HTTP: 9991 parses, 9992 is a
/// RecursionError (a 400). The same limit applies to httpx `Response.json()` and `meta_json`.
pub const MAX_DEPTH: usize = 9991;
/// `sys.get_int_max_str_digits()` default.
const INT_MAX_STR_DIGITS: usize = 4300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// `json.JSONDecodeError` (a `ValueError`) at this character index.
    Json { pos: usize, msg: &'static str },
    /// `UnicodeDecodeError` (a `ValueError`): the bytes do not decode in the detected encoding.
    Unicode,
    /// `ValueError` from `int()`: more than 4300 digits.
    IntDigits,
    /// `RecursionError` (NOT a `ValueError`): nested deeper than [`MAX_DEPTH`].
    Recursion,
}

impl DecodeError {
    /// Python catches it as `ValueError` (everything but the recursion error).
    pub fn is_value_error(&self) -> bool {
        !matches!(self, DecodeError::Recursion)
    }
}

// ------------------------------------------------------------------------------------------------ bytes -> chars

/// `json.detect_encoding`.
fn detect_encoding(b: &[u8]) -> &'static str {
    if b.starts_with(&[0, 0, 0xFE, 0xFF]) || b.starts_with(&[0xFF, 0xFE, 0, 0]) {
        return "utf-32";
    }
    if b.starts_with(&[0xFE, 0xFF]) || b.starts_with(&[0xFF, 0xFE]) {
        return "utf-16";
    }
    if b.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return "utf-8-sig";
    }
    if b.len() >= 4 {
        if b[0] == 0 {
            return if b[1] != 0 { "utf-16-be" } else { "utf-32-be" };
        }
        if b[1] == 0 {
            return if b[2] != 0 || b[3] != 0 {
                "utf-16-le"
            } else {
                "utf-32-le"
            };
        }
    } else if b.len() == 2 {
        if b[0] == 0 {
            return "utf-16-be";
        }
        if b[1] == 0 {
            return "utf-16-le";
        }
    }
    "utf-8"
}

/// A code point decoded with `surrogatepass`: surrogates become U+FFFD (D-K-SURROGATE).
fn cp(c: u32) -> Option<char> {
    if (0xD800..=0xDFFF).contains(&c) {
        Some('\u{FFFD}')
    } else {
        char::from_u32(c)
    }
}

/// UTF-8 with `surrogatepass` (encoded surrogates `ED A0..BF xx` are accepted).
fn decode_utf8(b: &[u8]) -> Option<Vec<char>> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c0 = b[i];
        let (n, min, init) = match c0 {
            0x00..=0x7F => {
                out.push(char::from(c0));
                i += 1;
                continue;
            }
            0xC2..=0xDF => (2, 0x80, u32::from(c0 & 0x1F)),
            0xE0..=0xEF => (3, 0x800, u32::from(c0 & 0x0F)),
            0xF0..=0xF4 => (4, 0x1_0000, u32::from(c0 & 0x07)),
            _ => return None,
        };
        if i + n > b.len() {
            return None;
        }
        let mut c = init;
        for &x in &b[i + 1..i + n] {
            if x & 0xC0 != 0x80 {
                return None;
            }
            c = (c << 6) | u32::from(x & 0x3F);
        }
        if c < min || c > 0x10_FFFF {
            return None;
        }
        out.push(cp(c)?);
        i += n;
    }
    Some(out)
}

fn decode_utf16(b: &[u8], big: bool) -> Option<Vec<char>> {
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u32> = b
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| {
            if big {
                u32::from(u16::from_be_bytes([p[0], p[1]]))
            } else {
                u32::from(u16::from_le_bytes([p[0], p[1]]))
            }
        })
        .collect();
    let mut out = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if (0xD800..0xDC00).contains(&u) && i + 1 < units.len() && (0xDC00..0xE000).contains(&units[i + 1]) {
            out.push(char::from_u32(
                0x1_0000 + ((u - 0xD800) << 10) + (units[i + 1] - 0xDC00),
            )?);
            i += 2;
        } else {
            out.push(cp(u)?);
            i += 1;
        }
    }
    Some(out)
}

fn decode_utf32(b: &[u8], big: bool) -> Option<Vec<char>> {
    if !b.len().is_multiple_of(4) {
        return None;
    }
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            let u = if big {
                u32::from_be_bytes([p[0], p[1], p[2], p[3]])
            } else {
                u32::from_le_bytes([p[0], p[1], p[2], p[3]])
            };
            cp(u)
        })
        .collect()
}

/// `bytes.decode(detect_encoding(b), "surrogatepass")`.
pub fn decode_text(b: &[u8]) -> Option<Vec<char>> {
    match detect_encoding(b) {
        "utf-32" => {
            let big = b.starts_with(&[0, 0, 0xFE, 0xFF]);
            decode_utf32(&b[4..], big)
        }
        "utf-16" => {
            let big = b.starts_with(&[0xFE, 0xFF]);
            decode_utf16(&b[2..], big)
        }
        "utf-8-sig" => decode_utf8(&b[3..]),
        "utf-16-be" => decode_utf16(b, true),
        "utf-16-le" => decode_utf16(b, false),
        "utf-32-be" => decode_utf32(b, true),
        "utf-32-le" => decode_utf32(b, false),
        _ => decode_utf8(b),
    }
}

/// `json.loads(b)` for bytes.
pub fn loads_bytes(b: &[u8]) -> Result<PyValue, DecodeError> {
    let chars = decode_text(b).ok_or(DecodeError::Unicode)?;
    Decoder { s: &chars }.decode()
}

/// `json.loads(s)` for a str.
pub fn loads_str(s: &str) -> Result<PyValue, DecodeError> {
    let chars: Vec<char> = s.chars().collect();
    if chars.first() == Some(&'\u{FEFF}') {
        return Err(DecodeError::Json {
            pos: 0,
            msg: "Unexpected UTF-8 BOM (decode using utf-8-sig)",
        });
    }
    Decoder { s: &chars }.decode()
}

// ------------------------------------------------------------------------------------------------ the scanner

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

enum Frame {
    Arr(Vec<PyValue>),
    /// pairs so far, their key index, the key whose value is being scanned
    Obj(Vec<(PyValue, PyValue)>, HashMap<String, usize>, String),
}

struct Decoder<'a> {
    s: &'a [char],
}

fn err(pos: usize, msg: &'static str) -> DecodeError {
    DecodeError::Json { pos, msg }
}

impl Decoder<'_> {
    fn at(&self, i: usize) -> Option<char> {
        self.s.get(i).copied()
    }

    fn skip_ws(&self, mut i: usize) -> usize {
        while i < self.s.len() && is_ws(self.s[i]) {
            i += 1;
        }
        i
    }

    fn starts(&self, i: usize, lit: &str) -> bool {
        let n = lit.chars().count();
        i + n <= self.s.len() && self.s[i..i + n].iter().copied().eq(lit.chars())
    }

    /// `JSONDecoder.decode`.
    fn decode(&self) -> Result<PyValue, DecodeError> {
        let idx = self.skip_ws(0);
        let (v, end) = self.scan(idx)?;
        let end = self.skip_ws(end);
        if end != self.s.len() {
            return Err(err(end, "Extra data"));
        }
        Ok(v)
    }

    /// One JSON term at `idx` (`scan_once`), iteratively. `Err(Json{msg: "Expecting value"})` stands for the
    /// StopIteration CPython turns into that message.
    fn scan(&self, idx: usize) -> Result<(PyValue, usize), DecodeError> {
        let mut stack: Vec<Frame> = Vec::new();
        let mut idx = idx;
        loop {
            // ---- scan a term starting at idx, or open a container
            let mut value: Option<(PyValue, usize)> = None;
            let Some(c) = self.at(idx) else {
                return Err(err(idx, "Expecting value"));
            };
            match c {
                '"' => value = Some(self.scanstring(idx + 1)?),
                '{' | '[' => {
                    if stack.len() + 1 > MAX_DEPTH {
                        return Err(DecodeError::Recursion);
                    }
                    let i = self.skip_ws(idx + 1);
                    if c == '[' {
                        if self.at(i) == Some(']') {
                            value = Some((PyValue::List(Vec::new()), i + 1));
                        } else {
                            stack.push(Frame::Arr(Vec::new()));
                            idx = i;
                            continue;
                        }
                    } else if self.at(i) == Some('}') {
                        value = Some((PyValue::Dict(Vec::new()), i + 1));
                    } else {
                        if self.at(i) != Some('"') {
                            return Err(err(i, "Expecting property name enclosed in double quotes"));
                        }
                        let (key, next) = self.key_and_colon(i)?;
                        stack.push(Frame::Obj(Vec::new(), HashMap::new(), key));
                        idx = next;
                        continue;
                    }
                }
                'n' if self.starts(idx, "null") => value = Some((PyValue::None, idx + 4)),
                't' if self.starts(idx, "true") => value = Some((PyValue::Bool(true), idx + 4)),
                'f' if self.starts(idx, "false") => value = Some((PyValue::Bool(false), idx + 5)),
                'N' if self.starts(idx, "NaN") => value = Some((PyValue::Float(f64::NAN), idx + 3)),
                'I' if self.starts(idx, "Infinity") => value = Some((PyValue::Float(f64::INFINITY), idx + 8)),
                '-' if self.starts(idx, "-Infinity") => {
                    value = Some((PyValue::Float(f64::NEG_INFINITY), idx + 9));
                }
                _ => {}
            }
            let (mut v, mut end) = match value {
                Some(x) => x,
                None => self.number(idx)?,
            };
            // ---- hand the finished term to the enclosing containers
            loop {
                let Some(top) = stack.last_mut() else {
                    return Ok((v, end));
                };
                let i = self.skip_ws(end);
                match top {
                    Frame::Arr(items) => {
                        items.push(v);
                        match self.at(i) {
                            Some(']') => {
                                let Some(Frame::Arr(items)) = stack.pop() else {
                                    unreachable_frame()?
                                };
                                v = PyValue::List(items);
                                end = i + 1;
                                continue;
                            }
                            Some(',') => {
                                let j = self.skip_ws(i + 1);
                                if self.at(j) == Some(']') {
                                    return Err(err(i, "Illegal trailing comma before end of array"));
                                }
                                idx = j;
                                break;
                            }
                            _ => return Err(err(i, "Expecting ',' delimiter")),
                        }
                    }
                    Frame::Obj(pairs, index, key) => {
                        let k = std::mem::take(key);
                        // dict(pairs): a repeated key keeps its first position and takes the last value
                        if let Some(&slot) = index.get(&k) {
                            pairs[slot].1 = v;
                        } else {
                            index.insert(k.clone(), pairs.len());
                            pairs.push((PyValue::Str(k), v));
                        }
                        match self.at(i) {
                            Some('}') => {
                                let Some(Frame::Obj(pairs, _, _)) = stack.pop() else {
                                    unreachable_frame()?
                                };
                                v = PyValue::Dict(pairs);
                                end = i + 1;
                                continue;
                            }
                            Some(',') => {
                                let j = self.skip_ws(i + 1);
                                if self.at(j) == Some('}') {
                                    return Err(err(i, "Illegal trailing comma before end of object"));
                                }
                                if self.at(j) != Some('"') {
                                    return Err(err(j, "Expecting property name enclosed in double quotes"));
                                }
                                let (nk, next) = self.key_and_colon(j)?;
                                *key = nk;
                                idx = next;
                                break;
                            }
                            _ => return Err(err(i, "Expecting ',' delimiter")),
                        }
                    }
                }
            }
        }
    }

    /// A key at `i` (a `"`), the `:` and the whitespace after it; returns (key, index of the value).
    fn key_and_colon(&self, i: usize) -> Result<(String, usize), DecodeError> {
        let (k, next) = self.scanstring(i + 1)?;
        let PyValue::Str(k) = k else {
            return Err(err(i, "Expecting value"));
        };
        let j = self.skip_ws(next);
        if self.at(j) != Some(':') {
            return Err(err(j, "Expecting ':' delimiter"));
        }
        Ok((k, self.skip_ws(j + 1)))
    }

    /// `scanstring_unicode` (strict): `end` is the index after the opening quote.
    fn scanstring(&self, end: usize) -> Result<(PyValue, usize), DecodeError> {
        let s = self.s;
        let len = s.len();
        let begin = end - 1;
        let mut out = String::new();
        let mut end = end;
        loop {
            let mut next = end;
            let mut c = '\0';
            while next < len {
                let d = s[next];
                if d == '"' || d == '\\' {
                    c = d;
                    break;
                }
                if (d as u32) <= 0x1f {
                    return Err(err(next, "Invalid control character at"));
                }
                next += 1;
            }
            if next >= len {
                return Err(err(begin, "Unterminated string starting at"));
            }
            out.extend(&s[end..next]);
            next += 1;
            if c == '"' {
                return Ok((PyValue::Str(out), next));
            }
            if next == len {
                return Err(err(begin, "Unterminated string starting at"));
            }
            let e = s[next];
            if e != 'u' {
                end = next + 1;
                let ch = match e {
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    _ => return Err(err(end - 2, "Invalid \\escape")),
                };
                out.push(ch);
                continue;
            }
            next += 1;
            end = next + 4;
            if end >= len {
                return Err(err(next - 1, "Invalid \\uXXXX escape"));
            }
            let mut u = hex4(&s[next..end]).ok_or_else(|| err(end - 5, "Invalid \\uXXXX escape"))?;
            if (0xD800..0xDC00).contains(&u) && end + 6 < len && s[end] == '\\' && s[end + 1] == 'u' {
                let u2 = hex4(&s[end + 2..end + 6]).ok_or_else(|| err(end + 1, "Invalid \\uXXXX escape"))?;
                if (0xDC00..0xE000).contains(&u2) {
                    u = 0x1_0000 + (((u - 0xD800) << 10) | (u2 - 0xDC00));
                    end += 6;
                }
            }
            out.push(cp(u).unwrap_or('\u{FFFD}'));
        }
    }

    /// `_match_number_unicode`.
    fn number(&self, start: usize) -> Result<(PyValue, usize), DecodeError> {
        let s = self.s;
        let len = s.len();
        let digit = |i: usize| i < len && s[i].is_ascii_digit();
        let mut idx = start;
        if s.get(idx) == Some(&'-') {
            idx += 1;
            if idx >= len {
                return Err(err(start, "Expecting value"));
            }
        }
        match s.get(idx) {
            Some('1'..='9') => {
                idx += 1;
                while digit(idx) {
                    idx += 1;
                }
            }
            Some('0') => idx += 1,
            _ => return Err(err(start, "Expecting value")),
        }
        let mut is_float = false;
        if idx + 1 < len && s[idx] == '.' && s[idx + 1].is_ascii_digit() {
            is_float = true;
            idx += 2;
            while digit(idx) {
                idx += 1;
            }
        }
        if idx + 1 < len && (s[idx] == 'e' || s[idx] == 'E') {
            let e_start = idx;
            idx += 1;
            if idx + 1 < len && (s[idx] == '-' || s[idx] == '+') {
                idx += 1;
            }
            while digit(idx) {
                idx += 1;
            }
            if s[idx - 1].is_ascii_digit() {
                is_float = true;
            } else {
                idx = e_start;
            }
        }
        let text: String = s[start..idx].iter().collect();
        if is_float {
            // float(): correctly rounded; overflow gives inf like CPython's PyFloat_FromString
            let f = text.parse::<f64>().unwrap_or(f64::NAN);
            return Ok((PyValue::Float(f), idx));
        }
        Ok((int_value(&text)?, idx))
    }
}

fn hex4(cs: &[char]) -> Option<u32> {
    let mut u = 0u32;
    for c in cs {
        u = (u << 4) | c.to_digit(16)?;
    }
    Some(u)
}

/// `int(text)` for a JSON integer literal (`-?(0|[1-9][0-9]*)`).
fn int_value(text: &str) -> Result<PyValue, DecodeError> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.len() > INT_MAX_STR_DIGITS {
        return Err(DecodeError::IntDigits);
    }
    Ok(match text.parse::<i64>() {
        Ok(i) => PyValue::Int(i),
        // -0 is int 0; anything outside i64 keeps its canonical decimal text
        Err(_) => PyValue::BigInt(text.to_string()),
    })
}

#[allow(clippy::unnecessary_wraps)]
fn unreachable_frame<T>() -> Result<T, DecodeError> {
    Err(DecodeError::Recursion)
}

// ------------------------------------------------------------------------------------------------ encoding

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// `json.dumps(x)`: `", "` / `": "`, `ensure_ascii=True`, `NaN`/`Infinity`.
    Std,
    /// Starlette `JSONResponse` and httpx request bodies: compact, `ensure_ascii=False`.
    Compact,
    /// FastAPI's pydantic `dump_json` of a `dict[str, Any]` result: compact, raw UTF-8, pydantic floats (`null` for
    /// NaN/inf).
    Pydantic,
}

pub fn dumps(v: &PyValue, style: Style) -> String {
    let mut out = String::new();
    write(&mut out, v, style);
    out
}

fn write_key(out: &mut String, k: &PyValue, style: Style) {
    let text = match k {
        PyValue::Str(s) => s.clone(),
        PyValue::Float(f) if style != Style::Pydantic => float::json(*f),
        other => other.json_key(),
    };
    crate::py::json::write_str(out, &text, style == Style::Std);
}

fn write(out: &mut String, v: &PyValue, style: Style) {
    let (item, kv) = if style == Style::Std { (", ", ": ") } else { (",", ":") };
    match v {
        PyValue::None => out.push_str("null"),
        PyValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        PyValue::Int(i) => out.push_str(&i.to_string()),
        PyValue::BigInt(s) => out.push_str(s),
        PyValue::Float(f) => out.push_str(&match style {
            Style::Pydantic => float::pydantic_json(*f),
            _ => float::json(*f),
        }),
        PyValue::Str(s) => crate::py::json::write_str(out, s, style == Style::Std),
        PyValue::List(items) | PyValue::Set(items) => {
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(item);
                }
                write(out, x, style);
            }
            out.push(']');
        }
        PyValue::Dict(pairs) => {
            out.push('{');
            for (i, (k, x)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push_str(item);
                }
                write_key(out, k, style);
                out.push_str(kv);
                write(out, x, style);
            }
            out.push('}');
        }
        other => crate::py::json::write_str(out, &other.py_str(), style == Style::Std),
    }
}

/// Builds a `PyValue::Dict` from `(key, value)` pairs (insertion order).
pub fn obj<I, K>(pairs: I) -> PyValue
where
    I: IntoIterator<Item = (K, PyValue)>,
    K: Into<String>,
{
    PyValue::Dict(pairs.into_iter().map(|(k, v)| (PyValue::Str(k.into()), v)).collect())
}

/// `PyValue` for an optional float.
pub fn opt_f(v: Option<f64>) -> PyValue {
    v.map_or(PyValue::None, PyValue::Float)
}

/// `PyValue` for an optional string.
pub fn opt_s(v: Option<&str>) -> PyValue {
    v.map_or(PyValue::None, PyValue::str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(s: &str) -> Option<usize> {
        match loads_bytes(s.as_bytes()) {
            Err(DecodeError::Json { pos, .. }) => Some(pos),
            _ => None,
        }
    }

    #[test]
    fn basic_values_and_errors() {
        assert_eq!(
            loads_str(r#"{"a": [1, 2.5, "x", null, true]}"#).map(|v| v.repr()).ok(),
            Some("{'a': [1, 2.5, 'x', None, True]}".to_string())
        );
        assert_eq!(pos(""), Some(0));
        assert_eq!(pos("{"), Some(1));
        assert_eq!(pos("[1,]"), Some(2));
        assert_eq!(pos("{\"a\" 1}"), Some(5));
        assert_eq!(pos("[1] x"), Some(4));
        assert_eq!(
            loads_str("12345678901234567890").map(|v| v.repr()).ok(),
            Some("12345678901234567890".into())
        );
        assert!(matches!(loads_str("-0"), Ok(PyValue::Int(0))));
    }

    #[test]
    fn dumps_flavours() {
        let v = obj([
            ("a", PyValue::Float(1e-7)),
            ("é", PyValue::List(vec![PyValue::Int(1), PyValue::None])),
        ]);
        assert_eq!(dumps(&v, Style::Std), r#"{"a": 1e-07, "\u00e9": [1, null]}"#);
        assert_eq!(dumps(&v, Style::Compact), r#"{"a":1e-07,"é":[1,null]}"#);
        assert_eq!(dumps(&v, Style::Pydantic), r#"{"a":1e-7,"é":[1,null]}"#);
    }
}
