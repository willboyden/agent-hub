//! Python `str` semantics that differ from Rust's: `isspace`, `split()`, `strip()`, `splitlines()`, `isprintable` and
//! `repr()` of a string. Messages the API returns embed `repr()` (`{name!r}`), so it must be exact.

/// `str.isspace()` for one character (bidi WS/B/S or category Zs), CPython 3.13.
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | '\u{1c}'..='\u{1f}' | ' ' | '\u{85}' | '\u{a0}' | '\u{1680}'
            | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// `s.split()` (no separator): runs of whitespace separate, leading/trailing whitespace ignored.
pub fn split_ws(s: &str) -> Vec<&str> {
    s.split(is_space).filter(|p| !p.is_empty()).collect()
}

/// `s.strip()` with the default whitespace set.
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `s.lstrip()`.
pub fn lstrip(s: &str) -> &str {
    s.trim_start_matches(is_space)
}

/// `s.rstrip()`.
pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

/// `" ".join(text.split())`.
pub fn collapse_ws(s: &str) -> String {
    split_ws(s).join(" ")
}

fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

/// `s.splitlines(keepends)`: splits on every Python line boundary (`\r\n` counts as one).
pub fn splitlines(s: &str, keepends: bool) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if !is_line_break(c) {
            continue;
        }
        let mut end = i + c.len_utf8();
        if c == '\r'
            && let Some(&(j, '\n')) = it.peek()
        {
            end = j + 1;
            it.next();
        }
        out.push(if keepends { &s[start..end] } else { &s[start..i] });
        start = end;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// `str.isprintable()` for one character, exact for the backend's CPython 3.13 (Unicode 15.1 tables in
/// [`super::pyunicode`]; this closes PORTING.md D-REPR).
pub fn is_printable(c: char) -> bool {
    super::pyunicode::is_printable(c)
}

/// `repr(s)` of a Python `str`.
pub fn repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if is_printable(c) => out.push(c),
            c => {
                let cp = c as u32;
                if cp < 0x100 {
                    out.push_str(&format!("\\x{cp:02x}"));
                } else if cp < 0x10000 {
                    out.push_str(&format!("\\u{cp:04x}"));
                } else {
                    out.push_str(&format!("\\U{cp:08x}"));
                }
            }
        }
    }
    out.push(quote);
    out
}

/// `s[:n]` counted in code points (Python slices count characters, not bytes).
pub fn take_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// `len(s)` in code points.
pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// `str.isalnum()` on a whole string (non-empty, every char alphanumeric).
pub fn is_alnum(s: &str) -> bool {
    !s.is_empty() && s.chars().all(char::is_alphanumeric)
}

/// `int(s)` for a str (whitespace stripped, optional sign, `_` only between digits); None where Python raises.
pub fn parse_int(s: &str) -> Option<i64> {
    let t = strip(s);
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if t.is_empty()
        || t.starts_with('_')
        || t.ends_with('_')
        || t.contains("__")
        || !t.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    let digits: String = t.chars().filter(|c| *c != '_').collect();
    let v = digits.parse::<i64>().ok()?;
    Some(if neg { -v } else { v })
}

/// `unicodedata.decimal(c)`: the value of a decimal digit of any script, None for anything else.
pub fn decimal(c: char) -> Option<u32> {
    super::pyunicode::decimal(c)
}

/// `int(s)` for a str with Python's full rules: surrounding whitespace stripped, an optional sign, decimal digits of
/// any script, `_` only between digits. Python ints are unbounded; this saturates at the i128 range, which keeps every
/// comparison the callers make (sizes, offsets) the same. None where Python raises ValueError.
pub fn int_str(s: &str) -> Option<i128> {
    let t = strip(s);
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if t.is_empty() || t.starts_with('_') || t.ends_with('_') || t.contains("__") {
        return None;
    }
    let mut v: i128 = 0;
    for c in t.chars() {
        if c == '_' {
            continue;
        }
        let d = i128::from(decimal(c)?);
        v = v.saturating_mul(10).saturating_add(d);
    }
    Some(if neg { -v } else { v })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_str_follows_python_int() {
        assert_eq!(decimal('7'), Some(7));
        assert_eq!(decimal('\u{0661}'), Some(1));
        assert_eq!(decimal('\u{1fbf9}'), Some(9));
        assert_eq!(decimal('\u{00b2}'), None); // superscript two: isdigit() but not a decimal
        assert_eq!(decimal('a'), None);
        assert_eq!(int_str(" +1_0 "), Some(10));
        assert_eq!(int_str("-3"), Some(-3));
        assert_eq!(int_str("\u{0661}\u{0662}"), Some(12));
        for bad in ["", "_1", "1_", "1__0", "0x1", "--3", "1 2", "+", "\u{00b2}"] {
            assert_eq!(int_str(bad), None, "{bad:?}");
        }
        assert_eq!(int_str("99999999999999999999999999999999999999999999"), Some(i128::MAX));
    }

    #[test]
    fn unassigned_code_points_are_not_printable() {
        // was D-REPR: Unicode 15.1 leaves U+0378 unassigned, so CPython escapes it
        assert!(!is_printable('\u{378}'));
        assert_eq!(repr("\u{378}"), "'\\u0378'");
        assert_eq!(repr("a\u{e01ef}"), "'a\u{e01ef}'"); // a variation selector (Mn) is printable
    }

    #[test]
    fn repr_quotes_and_escapes() {
        assert_eq!(repr("abc"), "'abc'");
        assert_eq!(repr("it's"), "\"it's\"");
        assert_eq!(repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(repr("t\tn\nr\r\\"), "'t\\tn\\nr\\r\\\\'");
        assert_eq!(repr("\u{0}\u{7f}é\u{a0}\u{2028}😀"), "'\\x00\\x7fé\\xa0\\u2028😀'");
    }

    #[test]
    fn splitlines_like_python() {
        assert_eq!(splitlines("a\r\nb\rc\nd", false), vec!["a", "b", "c", "d"]);
        assert_eq!(splitlines("a\nb\n", true), vec!["a\n", "b\n"]);
        assert_eq!(splitlines("a\u{2028}b\u{0c}", false), vec!["a", "b"]);
        assert!(splitlines("", false).is_empty());
    }

    #[test]
    fn whitespace_set() {
        assert_eq!(split_ws(" a\u{1f}b\u{a0}c  "), vec!["a", "b", "c"]);
        assert_eq!(strip("\u{85} x \u{3000}"), "x");
        assert_eq!(take_chars("héllo", 2), "hé");
    }
}
