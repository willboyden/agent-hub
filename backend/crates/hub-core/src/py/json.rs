//! Byte-exact `json.dumps` (CPython 3.13) and pydantic `model_dump_json` over `serde_json::Value`.
//!
//! The workspace enables serde_json's `preserve_order`, so an object keeps insertion order like a Python dict; pass
//! `sort_keys` where Python does. Every file the hub writes as JSON and every JSON digest goes through here:
//!
//! | Python call site                                                  | Rust                                  |
//! |-------------------------------------------------------------------|---------------------------------------|
//! | `json.dumps(x)` (audit params, settings values, scopes)           | `dumps(x, &Opts::default())`          |
//! | `json.dumps(doc, indent=2, sort_keys=True, ensure_ascii=False)+"\n"` (adapters `dump_json`) | `Opts::adapter()` |
//! | `json.dumps(obj, sort_keys=True, separators=(",", ":"), default=str)` (merge `_canon`, digests) | `Opts::canon()` |
//! | `json.dumps(rec, indent=1)` (apply logs)                          | `Opts { indent: Some(1), ..}`         |
//! | `model.model_dump_json()` / `(indent=2)` (plans, locks)           | `pydantic(x, None)` / `pydantic(x, Some(2))` |

use serde_json::Value;

use super::float;

/// `json.dumps` keyword arguments that the hub uses.
#[derive(Debug, Clone)]
pub struct Opts {
    pub indent: Option<usize>,
    pub sort_keys: bool,
    pub ensure_ascii: bool,
    /// `separators=(item, key)`; `None` = Python's default (`", "`/`": "`, or `","`/`": "` when `indent` is set).
    pub separators: Option<(&'static str, &'static str)>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            indent: None,
            sort_keys: false,
            ensure_ascii: true,
            separators: None,
        }
    }
}

impl Opts {
    /// adapters/_util.py `dump_json`: `indent=2, sort_keys=True, ensure_ascii=False` (the caller appends "\n").
    pub fn adapter() -> Self {
        Self {
            indent: Some(2),
            sort_keys: true,
            ensure_ascii: false,
            separators: None,
        }
    }
    /// `sort_keys=True, separators=(",", ":")` (merge `_canon`, `attest.mcp_blob`, verify digests).
    pub fn canon() -> Self {
        Self {
            indent: None,
            sort_keys: true,
            ensure_ascii: true,
            separators: Some((",", ":")),
        }
    }
}

/// `json.dumps(value, **opts)`.
pub fn dumps(value: &Value, opts: &Opts) -> String {
    let (item_sep, key_sep) = opts.separators.unwrap_or(if opts.indent.is_some() {
        (",", ": ")
    } else {
        (", ", ": ")
    });
    let mut out = String::new();
    let ctx = Ctx {
        indent: opts.indent,
        sort_keys: opts.sort_keys,
        ensure_ascii: opts.ensure_ascii,
        item_sep,
        key_sep,
        pydantic: false,
    };
    write_value(&mut out, value, &ctx, 0);
    out
}

/// pydantic `model_dump_json(indent=...)` of an already-dumped model: compact separators (`,` `:`), non-ASCII kept,
/// pydantic float formatting, non-finite floats as `null`. With `indent`, the same layout as `json.dumps(indent=N)`.
pub fn pydantic(value: &Value, indent: Option<usize>) -> String {
    let (item_sep, key_sep) = if indent.is_some() { (",", ": ") } else { (",", ":") };
    let mut out = String::new();
    let ctx = Ctx {
        indent,
        sort_keys: false,
        ensure_ascii: false,
        item_sep,
        key_sep,
        pydantic: true,
    };
    write_value(&mut out, value, &ctx, 0);
    out
}

struct Ctx<'a> {
    indent: Option<usize>,
    sort_keys: bool,
    ensure_ascii: bool,
    item_sep: &'a str,
    key_sep: &'a str,
    pydantic: bool,
}

fn newline(out: &mut String, ctx: &Ctx<'_>, level: usize) {
    if let Some(n) = ctx.indent {
        out.push('\n');
        out.push_str(&" ".repeat(n * level));
    }
}

fn write_value(out: &mut String, v: &Value, ctx: &Ctx<'_>, level: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                out.push_str(&i.to_string());
            } else if let Some(u) = n.as_u64() {
                out.push_str(&u.to_string());
            } else {
                let f = n.as_f64().unwrap_or(f64::NAN);
                out.push_str(&if ctx.pydantic {
                    float::pydantic_json(f)
                } else {
                    float::json(f)
                });
            }
        }
        Value::String(s) => write_str(out, s, ctx.ensure_ascii),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(ctx.item_sep);
                }
                newline(out, ctx, level + 1);
                write_value(out, it, ctx, level + 1);
            }
            newline(out, ctx, level);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            if ctx.sort_keys {
                // Python sorts str keys by code point; Rust's str ordering is byte order of UTF-8, which is the same.
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            out.push('{');
            for (i, (k, val)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(ctx.item_sep);
                }
                newline(out, ctx, level + 1);
                write_str(out, k, ctx.ensure_ascii);
                out.push_str(ctx.key_sep);
                write_value(out, val, ctx, level + 1);
            }
            newline(out, ctx, level);
            out.push('}');
        }
    }
}

/// A JSON string literal exactly as CPython's encoder writes it.
pub fn write_str(out: &mut String, s: &str, ensure_ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ensure_ascii && (c as u32) > 0x7f => {
                let cp = c as u32;
                if cp > 0xffff {
                    let v = cp - 0x10000;
                    out.push_str(&format!("\\u{:04x}\\u{:04x}", 0xd800 + (v >> 10), 0xdc00 + (v & 0x3ff)));
                } else {
                    out.push_str(&format!("\\u{cp:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn default_separators_and_ascii() {
        let v = json!({"b": [1, 2.0, "é"], "a": {"x": null, "y": true}});
        assert_eq!(
            dumps(&v, &Opts::default()),
            r#"{"b": [1, 2.0, "\u00e9"], "a": {"x": null, "y": true}}"#
        );
        assert_eq!(
            dumps(&v, &Opts::canon()),
            r#"{"a":{"x":null,"y":true},"b":[1,2.0,"\u00e9"]}"#
        );
    }

    #[test]
    fn indent_layout() {
        let v = json!({"z": 1, "a": [1, {}], "e": []});
        assert_eq!(
            dumps(&v, &Opts::adapter()),
            "{\n  \"a\": [\n    1,\n    {}\n  ],\n  \"e\": [],\n  \"z\": 1\n}"
        );
        let one = Opts {
            indent: Some(1),
            ..Opts::default()
        };
        assert_eq!(dumps(&json!({"k": [1]}), &one), "{\n \"k\": [\n  1\n ]\n}");
    }

    #[test]
    fn escapes_like_cpython() {
        let v = json!("a\"\\\n\u{1f}\u{7f}é\u{2028}😀");
        let want = format!("{}{}{}", r#""a\"\\\n\u001f"#, '\u{7f}', r#"\u00e9\u2028\ud83d\ude00""#);
        assert_eq!(dumps(&v, &Opts::default()), want);
        assert_eq!(dumps(&v, &Opts::adapter()), "\"a\\\"\\\\\\n\\u001f\u{7f}é\u{2028}😀\"");
    }

    #[test]
    fn pydantic_compact_and_indent() {
        let v = json!({"a": 1e-7, "s": "é", "l": [1]});
        assert_eq!(pydantic(&v, None), "{\"a\":1e-7,\"s\":\"é\",\"l\":[1]}");
        assert_eq!(pydantic(&json!({"l": [1]}), Some(2)), "{\n  \"l\": [\n    1\n  ]\n}");
    }
}
