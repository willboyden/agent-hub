//! Port of python-dotenv 1.2.3 `dotenv_values(path)` (parser.py + variables.py + main.resolve_variables, interpolate=True,
//! override=True). Both `~/.config/agent-hub/env` readers use it: pydantic-settings for the `HUB_*` settings, and
//! `hubctl` for `HUB_API_KEY`. Values are never logged. Owner: CORE.

use std::collections::HashMap;

use regex::Regex;

/// Python `\s` (str.isspace) as a character class body.
const WS: &str =
    r"\t\n\x0B\x0C\r\x1C-\x1F \x{85}\x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}";
/// `[^\S\r\n]`: whitespace except CR/LF.
const HWS: &str = r"\t\x0B\x0C\x1C-\x1F \x{85}\x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}";

struct Res {
    multiline_ws: Regex,
    whitespace: Regex,
    export: Regex,
    single_quoted_key: Regex,
    unquoted_key: Regex,
    equal_sign: Regex,
    single_quoted_value: Regex,
    double_quoted_value: Regex,
    unquoted_value: Regex,
    comment: Regex,
    end_of_line: Regex,
    rest_of_line: Regex,
    ws_hash: Regex,
    posix_var: Regex,
}

fn res() -> Option<Res> {
    let r = |p: String| Regex::new(&p).ok();
    Some(Res {
        multiline_ws: r(format!("^[{WS}]*"))?,
        whitespace: r(format!("^[{HWS}]*"))?,
        export: r(format!("^(?:export[{HWS}]+)?"))?,
        single_quoted_key: r("^'([^']+)'".into())?,
        unquoted_key: r(format!("^([^=#{WS}]+)"))?,
        equal_sign: r(format!("^(=[{HWS}]*)"))?,
        single_quoted_value: r(r"^(?s)'((?:\\.|[^'\\])*)'".into())?,
        double_quoted_value: r(r#"^(?s)"((?:\\.|[^"\\])*)""#.into())?,
        unquoted_value: r(r"^([^\r\n]*)".into())?,
        comment: r(format!("^(?:[{HWS}]*#[^\\r\\n]*)?"))?,
        end_of_line: r(format!("^[{HWS}]*(?:\\r\\n|\\n|\\r|\\z)"))?,
        rest_of_line: r(r"^[^\r\n]*(?:\r|\n|\r\n)?".into())?,
        ws_hash: r(format!("[{WS}]+#.*"))?,
        posix_var: r(r"\$\{(?P<name>[^}:]*)(?::-(?P<default>[^}]*))?\}".into())?,
    })
}

struct Reader<'a> {
    s: &'a str,
    pos: usize,
}

impl Reader<'_> {
    fn has_next(&self) -> bool {
        self.pos < self.s.len()
    }
    fn peek(&self) -> Option<char> {
        self.s[self.pos..].chars().next()
    }
    /// Match `re` at the current position; advance; return the capture groups (None = pattern not found).
    fn read(&mut self, re: &Regex) -> Option<Vec<String>> {
        let caps = re.captures(&self.s[self.pos..])?;
        let whole = caps.get(0)?;
        let groups = (1..caps.len())
            .map(|i| caps.get(i).map(|m| m.as_str().to_string()).unwrap_or_default())
            .collect();
        self.pos += whole.end();
        Some(groups)
    }
}

fn decode_escapes(value: &str, double: bool) -> String {
    let mut out = String::with_capacity(value.len());
    let mut it = value.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\'
            && let Some(&n) = it.peek()
        {
            let rep = match (n, double) {
                ('\\', _) => Some('\\'),
                ('\'', _) => Some('\''),
                ('"', true) => Some('"'),
                ('a', true) => Some('\u{07}'),
                ('b', true) => Some('\u{08}'),
                ('f', true) => Some('\u{0c}'),
                ('n', true) => Some('\n'),
                ('r', true) => Some('\r'),
                ('t', true) => Some('\t'),
                ('v', true) => Some('\u{0b}'),
                _ => None,
            };
            if let Some(r) = rep {
                out.push(r);
                it.next();
                continue;
            }
        }
        out.push(c);
    }
    out
}

fn parse_value(rd: &mut Reader<'_>, re: &Res) -> Option<String> {
    match rd.peek() {
        Some('\'') => rd.read(&re.single_quoted_value).map(|g| decode_escapes(&g[0], false)),
        Some('"') => rd.read(&re.double_quoted_value).map(|g| decode_escapes(&g[0], true)),
        None | Some('\n' | '\r') => Some(String::new()),
        Some(_) => {
            let g = rd.read(&re.unquoted_value)?;
            let cut = re.ws_hash.replace(&g[0], "");
            Some(crate::py::text::rstrip(&cut).to_string())
        }
    }
}

/// One binding: Ok(Some((key, value))) | Ok(None) (blank/comment) | Err(()) (a malformed line, skipped).
fn parse_binding(rd: &mut Reader<'_>, re: &Res) -> Result<Option<(String, Option<String>)>, ()> {
    rd.read(&re.multiline_ws).ok_or(())?;
    if !rd.has_next() {
        return Ok(None);
    }
    rd.read(&re.export).ok_or(())?;
    let key = match rd.peek() {
        Some('#') => None,
        Some('\'') => Some(rd.read(&re.single_quoted_key).ok_or(())?.remove(0)),
        _ => Some(rd.read(&re.unquoted_key).ok_or(())?.remove(0)),
    };
    rd.read(&re.whitespace).ok_or(())?;
    let value = if rd.peek() == Some('=') {
        rd.read(&re.equal_sign).ok_or(())?;
        Some(parse_value(rd, re).ok_or(())?)
    } else {
        None
    };
    rd.read(&re.comment).ok_or(())?;
    rd.read(&re.end_of_line).ok_or(())?;
    Ok(key.map(|k| (k, value)))
}

/// Raw `(key, value)` pairs in file order (`value` None for a bare `KEY` line).
pub fn parse(text: &str) -> Vec<(String, Option<String>)> {
    let Some(re) = res() else { return Vec::new() };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rd = Reader { s: text, pos: 0 };
    let mut out = Vec::new();
    while rd.has_next() {
        let start = rd.pos;
        match parse_binding(&mut rd, &re) {
            Ok(Some(kv)) => out.push(kv),
            Ok(None) => {}
            Err(()) => {
                rd.pos = start;
                rd.read(&re.multiline_ws);
                if rd.read(&re.rest_of_line).is_none() || rd.pos == start {
                    // a binding that consumed nothing: step one char so the loop always ends
                    if let Some(c) = rd.peek() {
                        rd.pos += c.len_utf8();
                    }
                }
            }
        }
        if rd.pos == start
            && let Some(c) = rd.peek()
        {
            rd.pos += c.len_utf8();
        }
    }
    out
}

/// `dotenv_values(path)` semantics on already-read text: `${NAME}` / `${NAME:-default}` interpolation against the
/// process environment overlaid with the values defined so far (override=True). Later duplicates win.
pub fn values(text: &str, environ: &HashMap<String, String>) -> Vec<(String, Option<String>)> {
    let Some(re) = res() else { return Vec::new() };
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for (k, v) in parse(text) {
        let resolved = v.map(|v| {
            let mut s = String::new();
            let mut cursor = 0;
            for caps in re.posix_var.captures_iter(&v) {
                let Some(m) = caps.get(0) else { continue };
                s.push_str(&v[cursor..m.start()]);
                let name = caps.name("name").map_or("", |x| x.as_str());
                let default = caps.name("default").map(|x| x.as_str());
                let from_file = out.iter().rev().find(|(n, _)| n == name).map(|(_, val)| val.clone());
                let val = match from_file {
                    Some(Some(val)) => val,
                    Some(None) => String::new(),
                    None => environ
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| default.unwrap_or("").to_string()),
                };
                s.push_str(&val);
                cursor = m.end();
            }
            s.push_str(&v[cursor..]);
            s
        });
        if let Some(slot) = out.iter_mut().find(|(n, _)| *n == k) {
            slot.1 = resolved;
        } else {
            out.push((k, resolved));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_python_dotenv() {
        let env = HashMap::from([("HOME".to_string(), "/h".to_string())]);
        let text = "# c\nexport A=1\nB = ' two '\nC=\"x\\ny\" # note\nD=plain # comment\nE\nF=${HOME}/x\nG=${NOPE:-dflt}\n'H'=v\nbad line =\n";
        let got = values(text, &env);
        let get = |k: &str| got.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("A"), Some(Some("1".into())));
        assert_eq!(get("B"), Some(Some(" two ".into())));
        assert_eq!(get("C"), Some(Some("x\ny".into())));
        assert_eq!(get("D"), Some(Some("plain".into())));
        assert_eq!(get("E"), Some(None));
        assert_eq!(get("F"), Some(Some("/h/x".into())));
        assert_eq!(get("G"), Some(Some("dflt".into())));
        assert_eq!(get("H"), Some(Some("v".into())));
    }
}
