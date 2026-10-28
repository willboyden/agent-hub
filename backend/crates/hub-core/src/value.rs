//! `PyValue`: what `yaml.safe_load` returns, with Python semantics (ordered dicts whose keys may be any hashable
//! scalar, ints vs floats vs bools kept apart, `date`/`datetime` from YAML timestamps, `bytes` from `!!binary`, `set`
//! from `!!set`). Content frontmatter keeps unknown keys verbatim, so the hub must carry these values through.
//! Owner: CORE. Conversions to JSON follow the two Python paths the hub uses (see [`PyValue::to_json`]).

use serde_json::{Map, Number, Value};

use crate::py::{float, text};

/// A naive or offset-aware `datetime.datetime`.
#[derive(Debug, Clone, PartialEq)]
pub struct DateTime {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub microsecond: u32,
    /// UTC offset in seconds (`None` = naive).
    pub offset_s: Option<i32>,
}

#[derive(Debug, Clone, Default)]
pub enum PyValue {
    #[default]
    None,
    Bool(bool),
    Int(i64),
    /// An int outside i64 (Python ints are unbounded), kept as its decimal text.
    BigInt(String),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Date {
        year: i32,
        month: u8,
        day: u8,
    },
    DateTime(DateTime),
    List(Vec<PyValue>),
    /// Insertion-ordered; keys are unique under Python equality (`1 == 1.0 == True`).
    Dict(Vec<(PyValue, PyValue)>),
    Set(Vec<PyValue>),
}

impl PartialEq for PyValue {
    /// Python `==`: numbers compare across bool/int/float; containers element-wise; dicts order-insensitive.
    fn eq(&self, other: &Self) -> bool {
        use PyValue as P;
        if let (Some(a), Some(b)) = (self.as_number(), other.as_number()) {
            return a == b;
        }
        match (self, other) {
            (P::None, P::None) => true,
            (P::BigInt(a), P::BigInt(b)) | (P::Str(a), P::Str(b)) => a == b,
            (P::Bytes(a), P::Bytes(b)) => a == b,
            (
                P::Date {
                    year: a,
                    month: b,
                    day: c,
                },
                P::Date {
                    year: x,
                    month: y,
                    day: z,
                },
            ) => (a, b, c) == (x, y, z),
            (P::DateTime(a), P::DateTime(b)) => a == b,
            (P::List(a), P::List(b)) => a == b,
            (P::Dict(a), P::Dict(b)) => {
                a.len() == b.len() && a.iter().all(|(k, v)| b.iter().any(|(k2, v2)| k == k2 && v == v2))
            }
            (P::Set(a), P::Set(b)) => a.len() == b.len() && a.iter().all(|x| b.contains(x)),
            _ => false,
        }
    }
}

impl PyValue {
    fn as_number(&self) -> Option<f64> {
        match self {
            PyValue::Bool(b) => Some(f64::from(u8::from(*b))),
            #[allow(clippy::cast_precision_loss)]
            PyValue::Int(i) => Some(*i as f64),
            PyValue::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn str(s: impl Into<String>) -> Self {
        PyValue::Str(s.into())
    }

    pub fn as_str(&self) -> Option<&str> {
        if let PyValue::Str(s) = self { Some(s) } else { None }
    }

    pub fn as_dict(&self) -> Option<&[(PyValue, PyValue)]> {
        if let PyValue::Dict(d) = self { Some(d) } else { None }
    }

    pub fn as_list(&self) -> Option<&[PyValue]> {
        if let PyValue::List(l) = self { Some(l) } else { None }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, PyValue::None)
    }

    /// `d.get(key)` for a str key.
    pub fn get(&self, key: &str) -> Option<&PyValue> {
        self.as_dict()?
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v)
    }

    /// Python `type(x).__name__` (pydantic's `input_type`, and messages).
    pub fn type_name(&self) -> &'static str {
        match self {
            PyValue::None => "NoneType",
            PyValue::Bool(_) => "bool",
            PyValue::Int(_) | PyValue::BigInt(_) => "int",
            PyValue::Float(_) => "float",
            PyValue::Str(_) => "str",
            PyValue::Bytes(_) => "bytes",
            PyValue::Date { .. } => "date",
            PyValue::DateTime(_) => "datetime",
            PyValue::List(_) => "list",
            PyValue::Dict(_) => "dict",
            PyValue::Set(_) => "set",
        }
    }

    /// `repr(x)`.
    pub fn repr(&self) -> String {
        match self {
            PyValue::None => "None".into(),
            PyValue::Bool(b) => if *b { "True" } else { "False" }.into(),
            PyValue::Int(i) => i.to_string(),
            PyValue::BigInt(s) => s.clone(),
            PyValue::Float(f) => float::repr(*f),
            PyValue::Str(s) => text::repr(s),
            PyValue::Bytes(b) => bytes_repr(b),
            PyValue::Date { year, month, day } => format!("datetime.date({year}, {month}, {day})"),
            PyValue::DateTime(d) => datetime_repr(d),
            PyValue::List(l) => format!("[{}]", l.iter().map(PyValue::repr).collect::<Vec<_>>().join(", ")),
            PyValue::Dict(d) => format!(
                "{{{}}}",
                d.iter()
                    .map(|(k, v)| format!("{}: {}", k.repr(), v.repr()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            PyValue::Set(s) if s.is_empty() => "set()".into(),
            PyValue::Set(s) => format!("{{{}}}", s.iter().map(PyValue::repr).collect::<Vec<_>>().join(", ")),
        }
    }

    /// `str(x)` (a str is itself; everything else its repr, except date/datetime which use isoformat with a space).
    pub fn py_str(&self) -> String {
        match self {
            PyValue::Str(s) => s.clone(),
            PyValue::Date { .. } => self.isoformat().unwrap_or_default(),
            PyValue::DateTime(_) => self.isoformat().map(|s| s.replacen('T', " ", 1)).unwrap_or_default(),
            other => other.repr(),
        }
    }

    /// `date.isoformat()` / `datetime.isoformat()`.
    pub fn isoformat(&self) -> Option<String> {
        match self {
            PyValue::Date { year, month, day } => Some(format!("{year:04}-{month:02}-{day:02}")),
            PyValue::DateTime(d) => {
                let mut s = format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                    d.year, d.month, d.day, d.hour, d.minute, d.second
                );
                if d.microsecond != 0 {
                    s.push_str(&format!(".{:06}", d.microsecond));
                }
                if let Some(off) = d.offset_s {
                    let sign = if off < 0 { '-' } else { '+' };
                    let a = off.unsigned_abs();
                    s.push_str(&format!("{sign}{:02}:{:02}", a / 3600, (a % 3600) / 60));
                    if a % 60 != 0 {
                        s.push_str(&format!(":{:02}", a % 60));
                    }
                }
                Some(s)
            }
            _ => None,
        }
    }

    /// JSON for the API (FastAPI `jsonable_encoder` + `json.dumps`) and for pydantic `mode="json"` dumps of `Any`
    /// fields: date/datetime -> ISO text, bytes -> UTF-8 text (lossy), set -> list, dict keys -> their JSON-key text
    /// (`True` -> "true", `None` -> "null", `1.5` -> "1.5", strings as is). Non-finite floats stay floats (the API
    /// encoder writes `Infinity`/`NaN` like `json.dumps`).
    pub fn to_json(&self) -> Value {
        match self {
            PyValue::None => Value::Null,
            PyValue::Bool(b) => Value::Bool(*b),
            PyValue::Int(i) => Value::Number(Number::from(*i)),
            PyValue::BigInt(s) => s
                .parse::<f64>()
                .ok()
                .and_then(Number::from_f64)
                .map_or(Value::Null, Value::Number),
            PyValue::Float(f) => Number::from_f64(*f).map_or(Value::Null, Value::Number),
            PyValue::Str(s) => Value::String(s.clone()),
            PyValue::Bytes(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
            PyValue::Date { .. } | PyValue::DateTime(_) => Value::String(self.isoformat().unwrap_or_default()),
            PyValue::List(l) | PyValue::Set(l) => Value::Array(l.iter().map(PyValue::to_json).collect()),
            PyValue::Dict(d) => {
                let mut m = Map::new();
                for (k, v) in d {
                    m.insert(k.json_key(), v.to_json());
                }
                Value::Object(m)
            }
        }
    }

    /// The text `json.dumps` uses for a non-str dict key.
    pub fn json_key(&self) -> String {
        match self {
            PyValue::Str(s) => s.clone(),
            PyValue::None => "null".into(),
            PyValue::Bool(b) => if *b { "true" } else { "false" }.into(),
            PyValue::Float(f) => float::json(*f),
            other => other.py_str(),
        }
    }

    /// From JSON (a request body): numbers become Int when integral in JSON text, else Float.
    pub fn from_json(v: &Value) -> Self {
        match v {
            Value::Null => PyValue::None,
            Value::Bool(b) => PyValue::Bool(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    PyValue::Int(i)
                } else if let Some(u) = n.as_u64() {
                    PyValue::BigInt(u.to_string())
                } else {
                    PyValue::Float(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            Value::String(s) => PyValue::Str(s.clone()),
            Value::Array(a) => PyValue::List(a.iter().map(PyValue::from_json).collect()),
            Value::Object(m) => PyValue::Dict(
                m.iter()
                    .map(|(k, v)| (PyValue::Str(k.clone()), PyValue::from_json(v)))
                    .collect(),
            ),
        }
    }
}

fn bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') {
        '"'
    } else {
        '\''
    };
    let mut out = format!("b{quote}");
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            c if char::from(c) == quote => {
                out.push('\\');
                out.push(char::from(c));
            }
            0x20..=0x7e => out.push(char::from(c)),
            c => out.push_str(&format!("\\x{c:02x}")),
        }
    }
    out.push(quote);
    out
}

fn datetime_repr(d: &DateTime) -> String {
    let mut parts = vec![
        d.year.to_string(),
        d.month.to_string(),
        d.day.to_string(),
        d.hour.to_string(),
        d.minute.to_string(),
    ];
    if d.second != 0 || d.microsecond != 0 {
        parts.push(d.second.to_string());
    }
    if d.microsecond != 0 {
        parts.push(d.microsecond.to_string());
    }
    let tz = match d.offset_s {
        None => String::new(),
        Some(0) => ", tzinfo=datetime.timezone.utc".into(),
        Some(off) => format!(
            ", tzinfo=datetime.timezone(datetime.timedelta({}))",
            timedelta_args(off)
        ),
    };
    format!("datetime.datetime({}{tz})", parts.join(", "))
}

fn timedelta_args(off: i32) -> String {
    // repr(timedelta): days normalised so seconds is non-negative
    let days = off.div_euclid(86_400);
    let secs = off.rem_euclid(86_400);
    match (days, secs) {
        (0, s) => format!("seconds={s}"),
        (d, 0) => format!("days={d}"),
        (d, s) => format!("days={d}, seconds={s}"),
    }
}

impl serde::Serialize for PyValue {
    /// As [`PyValue::to_json`] (what FastAPI's encoder / pydantic `mode="json"` produce for an `Any` value).
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for PyValue {
    /// From JSON (lock files, plans and request bodies are JSON).
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        Ok(PyValue::from_json(&v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_equality_and_repr() {
        assert_eq!(PyValue::Int(1), PyValue::Bool(true));
        assert_eq!(PyValue::Float(1.0), PyValue::Int(1));
        assert_ne!(PyValue::Str("1".into()), PyValue::Int(1));
        let d = PyValue::Dict(vec![(
            PyValue::str("a"),
            PyValue::List(vec![PyValue::None, PyValue::Float(0.5)]),
        )]);
        assert_eq!(d.repr(), "{'a': [None, 0.5]}");
        assert_eq!(
            PyValue::Date {
                year: 2024,
                month: 1,
                day: 2
            }
            .to_json(),
            Value::String("2024-01-02".into())
        );
        assert_eq!(PyValue::Bytes(b"a'\x00".to_vec()).repr(), "b\"a'\\x00\"");
    }
}
