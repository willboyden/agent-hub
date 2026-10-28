//! Python object semantics the YAML layer depends on:
//! * dict-key identity: `{1: a, 1.0: b, true: c}` is ONE key (`1 == 1.0 == True`), aware datetimes are equal when they
//!   name the same instant, `date != datetime`, naive != aware ([`HashKey`]);
//! * ordering for `safe_dump(sort_keys=True)`: `sorted(dict.items())`, which falls back to the insertion order when
//!   two keys cannot be compared (`TypeError` is swallowed by the representer) ([`sort_items`]).

use std::cmp::Ordering;

use crate::value::{DateTime, PyValue};

/// A value usable as a hash-map key with Python's `==` for hashable YAML values (numbers compare exactly across
/// bool/int/float; NaN keys are all the same object in PyYAML's SafeConstructor, `nan_value`, so they collide).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum HashKey {
    None,
    /// An integral number as exact decimal text ("-0" never appears).
    Int(String),
    /// A finite, non-integral float (bit pattern).
    Frac(u64),
    Inf(bool),
    NaN,
    Str(String),
    Bytes(Vec<u8>),
    Date(i32, u8, u8),
    Naive(i32, u8, u8, u8, u8, u8, u32),
    /// UTC instant in microseconds.
    Aware(i128),
}

/// Exact decimal text of an integral, finite float.
fn float_int_text(f: f64) -> String {
    let s = format!("{f:.0}");
    if s == "-0" { "0".into() } else { s }
}

fn epoch_micros(d: &DateTime) -> i128 {
    let days = days_from_civil(d.year, d.month, d.day);
    let secs = i128::from(days) * 86_400 + i128::from(d.hour) * 3600 + i128::from(d.minute) * 60 + i128::from(d.second)
        - i128::from(d.offset_s.unwrap_or(0));
    secs * 1_000_000 + i128::from(d.microsecond)
}

/// Days since 1970-01-01 (proleptic Gregorian).
fn days_from_civil(y: i32, m: u8, d: u8) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl HashKey {
    /// None for unhashable values (list, dict, set).
    pub(crate) fn of(v: &PyValue) -> Option<HashKey> {
        Some(match v {
            PyValue::None => HashKey::None,
            PyValue::Bool(b) => HashKey::Int(if *b { "1" } else { "0" }.into()),
            PyValue::Int(i) => HashKey::Int(i.to_string()),
            PyValue::BigInt(s) => HashKey::Int(s.clone()),
            PyValue::Float(f) => {
                if f.is_nan() {
                    HashKey::NaN
                } else if f.is_infinite() {
                    HashKey::Inf(*f > 0.0)
                } else if f.fract() == 0.0 {
                    HashKey::Int(float_int_text(*f))
                } else {
                    HashKey::Frac(f.to_bits())
                }
            }
            PyValue::Str(s) => HashKey::Str(s.clone()),
            PyValue::Bytes(b) => HashKey::Bytes(b.clone()),
            PyValue::Date { year, month, day } => HashKey::Date(*year, *month, *day),
            PyValue::DateTime(d) => match d.offset_s {
                None => HashKey::Naive(d.year, d.month, d.day, d.hour, d.minute, d.second, d.microsecond),
                Some(_) => HashKey::Aware(epoch_micros(d)),
            },
            PyValue::List(_) | PyValue::Dict(_) | PyValue::Set(_) => return None,
        })
    }
}

/// Python's type name for `unhashable type: '<name>'`.
pub(crate) fn unhashable_name(v: &PyValue) -> &'static str {
    v.type_name()
}

// ---- ordering -----------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Num,
    Str,
    Bytes,
    Date,
    Naive,
    Aware,
    /// None (and containers): never orderable.
    Other,
}

fn class(v: &PyValue) -> Class {
    match v {
        PyValue::Bool(_) | PyValue::Int(_) | PyValue::BigInt(_) | PyValue::Float(_) => Class::Num,
        PyValue::Str(_) => Class::Str,
        PyValue::Bytes(_) => Class::Bytes,
        PyValue::Date { .. } => Class::Date,
        PyValue::DateTime(d) if d.offset_s.is_none() => Class::Naive,
        PyValue::DateTime(_) => Class::Aware,
        _ => Class::Other,
    }
}

/// An exact integer: sign and decimal magnitude (no leading zeros).
fn int_parts(v: &PyValue) -> Option<(bool, String)> {
    let s = match v {
        PyValue::Bool(b) => u8::from(*b).to_string(),
        PyValue::Int(i) => i.to_string(),
        PyValue::BigInt(s) => s.clone(),
        _ => return None,
    };
    Some(match s.strip_prefix('-') {
        Some(m) => (m != "0", m.to_string()),
        None => (false, s),
    })
}

fn cmp_mag(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn cmp_int(a: &(bool, String), b: &(bool, String)) -> Ordering {
    match (a.0, b.0) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => cmp_mag(&a.1, &b.1),
        (true, true) => cmp_mag(&b.1, &a.1),
    }
}

/// Exact int-vs-float comparison (None when the float is NaN).
fn cmp_int_float(i: &(bool, String), f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f.is_infinite() {
        return Some(if f > 0.0 { Ordering::Less } else { Ordering::Greater });
    }
    let fl = f.floor();
    let fi = float_int_text(fl);
    let fparts = match fi.strip_prefix('-') {
        Some(m) => (true, m.to_string()),
        None => (false, fi),
    };
    match cmp_int(i, &fparts) {
        Ordering::Equal if f != fl => Some(Ordering::Less),
        o => Some(o),
    }
}

/// Python `a < b` for two orderable values of the same [`Class`]; None = False both ways (NaN).
fn py_cmp(a: &PyValue, b: &PyValue) -> Option<Ordering> {
    match (a, b) {
        (PyValue::Float(x), PyValue::Float(y)) => x.partial_cmp(y),
        (PyValue::Float(x), _) => int_parts(b).and_then(|i| cmp_int_float(&i, *x)).map(Ordering::reverse),
        (_, PyValue::Float(y)) => int_parts(a).and_then(|i| cmp_int_float(&i, *y)),
        (PyValue::Str(x), PyValue::Str(y)) => Some(x.cmp(y)),
        (PyValue::Bytes(x), PyValue::Bytes(y)) => Some(x.cmp(y)),
        (
            PyValue::Date {
                year: a1,
                month: a2,
                day: a3,
            },
            PyValue::Date {
                year: b1,
                month: b2,
                day: b3,
            },
        ) => Some((a1, a2, a3).cmp(&(b1, b2, b3))),
        (PyValue::DateTime(x), PyValue::DateTime(y)) => {
            if x.offset_s.is_some() {
                Some(epoch_micros(x).cmp(&epoch_micros(y)))
            } else {
                Some(
                    (x.year, x.month, x.day, x.hour, x.minute, x.second, x.microsecond).cmp(&(
                        y.year,
                        y.month,
                        y.day,
                        y.hour,
                        y.minute,
                        y.second,
                        y.microsecond,
                    )),
                )
            }
        }
        _ => match (int_parts(a), int_parts(b)) {
            (Some(x), Some(y)) => Some(cmp_int(&x, &y)),
            _ => None,
        },
    }
}

/// `sorted(mapping.items())` with the representer's `except TypeError: pass`. Keys of a dict are distinct, so only
/// the keys are compared. Any two keys of incomparable types make every comparison sort raise (some pair across the
/// type boundary must be compared), so the original order is kept. NaN keys are ordered by a stable sort treating
/// "neither is smaller" as equal (CPython's timsort may order them differently: scoped out).
pub(crate) fn sort_items<T>(items: &mut [T], key: impl Fn(&T) -> &PyValue) {
    let Some(first) = items.first() else { return };
    let c = class(key(first));
    if items.len() < 2 {
        return;
    }
    if c == Class::Other || items.iter().any(|i| class(key(i)) != c) {
        return;
    }
    items.sort_by(|a, b| py_cmp(key(a), key(b)).unwrap_or(Ordering::Equal));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_key_equality_and_order() {
        assert_eq!(HashKey::of(&PyValue::Int(1)), HashKey::of(&PyValue::Float(1.0)));
        assert_eq!(HashKey::of(&PyValue::Bool(true)), HashKey::of(&PyValue::Int(1)));
        assert_ne!(HashKey::of(&PyValue::Str("1".into())), HashKey::of(&PyValue::Int(1)));
        assert_eq!(HashKey::of(&PyValue::Float(-0.0)), HashKey::of(&PyValue::Int(0)));
        assert!(HashKey::of(&PyValue::List(vec![])).is_none());
        let mut items = vec![
            (PyValue::Int(3), ()),
            (PyValue::Float(1.5), ()),
            (PyValue::Bool(true), ()),
        ];
        sort_items(&mut items, |i| &i.0);
        assert_eq!(items[0].0, PyValue::Bool(true));
        assert_eq!(items[1].0, PyValue::Float(1.5));
        let mut mixed = vec![(PyValue::str("b"), ()), (PyValue::Int(1), ()), (PyValue::str("a"), ())];
        sort_items(&mut mixed, |i| &i.0);
        assert_eq!(mixed[0].0, PyValue::str("b"));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }
}
