//! Python `int(s, base)` / `float(s)` as SafeConstructor calls them, with CPython 3.13's messages
//! (`invalid literal for int() with base 10: '...'` with the repr cut at 200 characters, the 4300-digit limit for
//! decimal strings, `could not convert string to float: '...'`), and the small unsigned big-integer arithmetic the
//! constructor needs (sexagesimal `1:30:00`, `0x`/`0o`/`0b` literals beyond i64). Results that do not fit an i64 are
//! returned as decimal text ([`PyValue::BigInt`]).

use super::error::Error;
use crate::py::text;
use crate::value::PyValue;

/// CPython's default `sys.get_int_max_str_digits()`.
pub(crate) const MAX_STR_DIGITS: usize = 4300;

/// Magnitude in base 10^9 limbs, least significant first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BigUint(Vec<u32>);

const LIMB: u64 = 1_000_000_000;
/// More limbs than this means more than 4300 decimal digits (`str()` of the int would raise in Python).
const MAX_LIMBS: usize = MAX_STR_DIGITS / 9 + 2;

impl BigUint {
    pub(crate) fn zero() -> Self {
        BigUint(Vec::new())
    }

    /// `self = self * m + a`.
    pub(crate) fn mul_add(&mut self, m: u32, a: u32) {
        let mut carry = u64::from(a);
        for limb in &mut self.0 {
            let v = u64::from(*limb) * u64::from(m) + carry;
            *limb = u32::try_from(v % LIMB).unwrap_or(0);
            carry = v / LIMB;
        }
        while carry > 0 {
            self.0.push(u32::try_from(carry % LIMB).unwrap_or(0));
            carry /= LIMB;
        }
    }

    pub(crate) fn add(&mut self, other: &BigUint) {
        let mut carry = 0u64;
        let n = self.0.len().max(other.0.len());
        self.0.resize(n, 0);
        for i in 0..n {
            let v = u64::from(self.0[i]) + u64::from(other.0.get(i).copied().unwrap_or(0)) + carry;
            self.0[i] = u32::try_from(v % LIMB).unwrap_or(0);
            carry = v / LIMB;
        }
        if carry > 0 {
            self.0.push(u32::try_from(carry).unwrap_or(0));
        }
    }

    pub(crate) fn mul_small(&self, m: u32) -> BigUint {
        let mut r = self.clone();
        r.mul_add(m, 0);
        r
    }

    fn trim(&mut self) {
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
    }

    pub(crate) fn too_long(&self) -> bool {
        self.0.len() > MAX_LIMBS
    }

    pub(crate) fn to_decimal(&self) -> String {
        let mut v = self.clone();
        v.trim();
        let Some((last, rest)) = v.0.split_last() else {
            return "0".into();
        };
        let mut s = last.to_string();
        for limb in rest.iter().rev() {
            s.push_str(&format!("{limb:09}"));
        }
        s
    }

    /// `float(int)` (correctly rounded, like CPython's int -> float conversion).
    pub(crate) fn to_f64(&self) -> f64 {
        self.to_decimal().parse::<f64>().unwrap_or(f64::INFINITY)
    }
}

/// An int value (sign + magnitude) as the PyValue the hub uses.
pub(crate) fn int_value(negative: bool, mag: &BigUint) -> Result<PyValue, Error> {
    let digits = mag.to_decimal();
    if digits.len() > MAX_STR_DIGITS {
        // The int itself is fine in Python, but PyValue::BigInt is decimal text, and Python's str()/json of it raises
        // this exact ValueError (DEVIATION D-YAML-HUGEINT: raised at load time instead of at first use).
        return Err(Error::python(
            "ValueError",
            "Exceeds the limit (4300 digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit",
        ));
    }
    let signed = if negative && digits != "0" {
        format!("-{digits}")
    } else {
        digits
    };
    Ok(match signed.parse::<i64>() {
        Ok(i) => PyValue::Int(i),
        Err(_) => PyValue::BigInt(signed),
    })
}

/// The 4300-digit check `int()` applies to a decimal string (and the YAML directive's `int(self.prefix(n))`).
pub(crate) fn check_str_digits(digits: &str) -> Result<(), Error> {
    let n = digits.chars().count();
    if n > MAX_STR_DIGITS {
        return Err(Error::python(
            "ValueError",
            format!(
                "Exceeds the limit ({MAX_STR_DIGITS} digits) for integer string conversion: value has {n} digits; use sys.set_int_max_str_digits() to increase the limit"
            ),
        ));
    }
    Ok(())
}

/// `%.200R`: the repr, cut at 200 characters.
fn repr200(s: &str) -> String {
    let r = super::pyunicode::repr(s);
    text::take_chars(&r, 200).to_string()
}

fn invalid_int(s: &str, base: u32) -> Error {
    Error::python(
        "ValueError",
        format!("invalid literal for int() with base {base}: {}", repr200(s)),
    )
}

/// CPython `_PyUnicode_TransformDecimalAndSpaceToASCII`, which `int()` and `float()` apply first: ASCII stays, other
/// whitespace becomes ' ', any Unicode decimal digit becomes its ASCII digit, anything else ends the text with '?'.
fn to_ascii_number(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if (c as u32) < 127 {
            out.push(c);
        } else if text::is_space(c) {
            out.push(' ');
        } else if let Some(d) = super::pyunicode::decimal(c).and_then(|d| char::from_digit(d, 10)) {
            out.push(d);
        } else {
            out.push('?');
            break;
        }
    }
    out
}

/// `Py_ISSPACE` stripping (ASCII ' \t\n\v\f\r' only, after the transform).
fn strip_ascii_ws(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'))
}

/// `int(s, base)` for base 2, 8, 10 or 16. Returns (negative, magnitude).
pub(crate) fn py_int(s: &str, base: u32) -> Result<(bool, BigUint), Error> {
    let ascii = to_ascii_number(s);
    let t = strip_ascii_ws(&ascii);
    let (neg, body) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let mut body = body;
    let prefix = match base {
        16 => Some(['x', 'X']),
        8 => Some(['o', 'O']),
        2 => Some(['b', 'B']),
        _ => None,
    };
    let mut had_prefix = false;
    if let Some(p) = prefix
        && body.len() >= 2
        && body.starts_with('0')
        && body[1..].starts_with(p)
    {
        body = &body[2..];
        had_prefix = true;
    }
    // underscores: single, between digits (or right after a base prefix)
    let chars: Vec<char> = body.chars().collect();
    if base == 10 {
        let run = chars
            .iter()
            .take_while(|c| c.is_ascii_digit() || **c == '_')
            .filter(|c| **c != '_')
            .count();
        check_str_digits(&"0".repeat(run))?;
    }
    if chars.is_empty() {
        return Err(invalid_int(s, base));
    }
    let mut mag = BigUint::zero();
    let mut prev_us = !had_prefix; // a leading '_' is only valid right after the prefix
    let mut any = false;
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' {
            if prev_us || i + 1 == chars.len() {
                return Err(invalid_int(s, base));
            }
            prev_us = true;
            continue;
        }
        prev_us = false;
        let Some(d) = c.to_digit(base) else {
            return Err(invalid_int(s, base));
        };
        mag.mul_add(base, d);
        any = true;
        if mag.too_long() && base != 10 {
            return int_value(neg, &mag).map(|_| (neg, mag));
        }
    }
    if !any {
        return Err(invalid_int(s, base));
    }
    Ok((neg, mag))
}

/// `float(s)`: Python whitespace stripped, then Rust's correctly rounded parser (the same grammar for the decimal
/// and inf/nan spellings Python accepts; underscores were removed by the caller).
pub(crate) fn py_float(s: &str) -> Result<f64, Error> {
    let ascii = to_ascii_number(s);
    let t = strip_ascii_ws(&ascii);
    let err = || {
        Error::python(
            "ValueError",
            format!("could not convert string to float: {}", super::pyunicode::repr(s)),
        )
    };
    if !valid_python_float(t) {
        return Err(err());
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let lower = body.to_ascii_lowercase();
    let mag = match lower.as_str() {
        "inf" | "infinity" => f64::INFINITY,
        "nan" => f64::NAN,
        _ => lower.parse::<f64>().map_err(|_| err())?,
    };
    Ok(if neg { -mag } else { mag })
}

/// Python's float grammar: `[sign] (digits [. [digits]] | . digits) [e [sign] digits] | [sign] (inf|infinity|nan)`.
fn valid_python_float(t: &str) -> bool {
    let b = t.strip_prefix(['+', '-']).unwrap_or(t);
    let lower = b.to_ascii_lowercase();
    if matches!(lower.as_str(), "inf" | "infinity" | "nan") {
        return true;
    }
    let (mant, exp) = match lower.find('e') {
        Some(i) => (&lower[..i], Some(&lower[i + 1..])),
        None => (lower.as_str(), None),
    };
    let (ip, fp) = match mant.find('.') {
        Some(i) => (&mant[..i], Some(&mant[i + 1..])),
        None => (mant, None),
    };
    let digits = |x: &str| x.chars().all(|c| c.is_ascii_digit());
    if !digits(ip) || !fp.is_none_or(digits) {
        return false;
    }
    if ip.is_empty() && fp.is_none_or(str::is_empty) {
        return false;
    }
    match exp {
        None => true,
        Some(e) => {
            let e = e.strip_prefix(['+', '-']).unwrap_or(e);
            !e.is_empty() && digits(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_int_and_float_rules() {
        let v = |s: &str, b| py_int(s, b).ok().map(|(n, m)| (n, m.to_decimal()));
        assert_eq!(v(" 12 ", 10), Some((false, "12".into())));
        assert_eq!(v("0x1F", 16), Some((false, "31".into())));
        assert_eq!(v("-17", 8), Some((true, "15".into())));
        assert_eq!(v("", 16), None);
        assert_eq!(v("08", 8), None);
        assert_eq!(
            v("18446744073709551616", 10),
            Some((false, "18446744073709551616".into()))
        );
        assert!(py_float("1.5e+3").is_ok());
        assert!(py_float(" .5 ").is_ok());
        assert!(py_float("1.").is_ok());
        assert!(py_float("infinity").is_ok());
        assert!(py_float("x").is_err());
        assert!(py_float("1e").is_err());
        assert!(py_float(".").is_err());
        assert_eq!(v("\u{663}\u{662}", 10), Some((false, "32".into())));
        assert_eq!(v("\u{3000}12\u{85}", 10), Some((false, "12".into())));
        assert_eq!(v("\x1c12", 10), None);
        assert_eq!(v("\u{661}\u{662}", 16), Some((false, "18".into())));
        assert_eq!(py_float("\u{661}.\u{665}").ok(), Some(1.5));
        let e = py_int(&"x".repeat(300), 10).err().map(|e| e.render(&[]).problem);
        assert_eq!(e.map(|s| s.chars().count()), Some(240));
    }
}
