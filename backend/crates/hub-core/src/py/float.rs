//! Python float formatting.
//!
//! * [`repr`]: `repr(float)` / `str(float)` / `json.dumps(float)` (CPython `float_repr_style = 'short'`): the shortest
//!   round-trip digits, fixed notation when the decimal point position `decpt` is in `-3..=16`, else `1e+16` style with
//!   a signed, at-least-two-digit exponent; integral values keep `.0`.
//! * [`pydantic_json`]: pydantic-core 2.x `model_dump_json` (fixed for exponents -5..=15, else `1.5e-7` / `1e+16`).
//!   Verified against pydantic 2.13.5 on 16 probes (see tests); WS-B2 extends the golden when it ports locks/plans.

/// Digits and decimal point position of a scientific-notation string (`"1.5e-7"` -> `("15", -6)`).
fn sci_digits(s: &str) -> (String, i32) {
    let (mant, exp) = s.split_once('e').unwrap_or((s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() {
        "0".to_string()
    } else {
        digits.to_string()
    };
    (digits, exp + 1)
}

/// Shortest round-trip digits (no leading/trailing zeros, at least one digit) and the decimal point position
/// (`value = 0.DIGITS * 10^decpt`), as CPython (`_Py_dg_dtoa` mode 0) and pydantic-core choose them: among the shortest
/// strings that round-trip, the one nearest the exact value, a tie going to the EVEN digit. Rust's shortest `{:e}`
/// breaks such ties upwards (`-1133092563735992.25` -> `...992.3`, Python `...992.2`), so the correctly rounded string
/// of the same length (`{:.Ne}`, exact half-to-even) wins whenever it also round-trips. Only for finite, non-zero
/// magnitudes. Golden-tested against Python: tests/float_golden.rs.
fn digits_and_decpt(x: f64) -> (String, i32) {
    let a = x.abs();
    let (digits, decpt) = sci_digits(&format!("{a:e}"));
    let alt = format!("{a:.prec$e}", prec = digits.len().saturating_sub(1));
    if alt.parse::<f64>().ok() == Some(a) {
        return sci_digits(&alt);
    }
    (digits, decpt)
}

fn fixed(digits: &str, decpt: i32) -> String {
    let n = i32::try_from(digits.len()).unwrap_or(i32::MAX);
    if decpt <= 0 {
        format!("0.{}{}", "0".repeat(usize::try_from(-decpt).unwrap_or(0)), digits)
    } else if decpt >= n {
        format!("{}{}.0", digits, "0".repeat(usize::try_from(decpt - n).unwrap_or(0)))
    } else {
        let k = usize::try_from(decpt).unwrap_or(0);
        format!("{}.{}", &digits[..k], &digits[k..])
    }
}

fn mantissa(digits: &str) -> String {
    if digits.len() == 1 {
        digits.to_string()
    } else {
        format!("{}.{}", &digits[..1], &digits[1..])
    }
}

/// `repr(x)` for a Python float (`inf`, `-inf`, `nan` for the non-finite values).
pub fn repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    if x == 0.0 {
        return format!("{sign}0.0");
    }
    let (digits, decpt) = digits_and_decpt(x);
    if !(-3..=16).contains(&decpt) {
        let e = decpt - 1;
        let esign = if e < 0 { '-' } else { '+' };
        format!("{sign}{}e{esign}{:02}", mantissa(&digits), e.abs())
    } else {
        format!("{sign}{}", fixed(&digits, decpt))
    }
}

/// `json.dumps(x)` for a float (allow_nan=True): `Infinity`, `-Infinity`, `NaN`, else [`repr`].
pub fn json(x: f64) -> String {
    if x.is_nan() {
        "NaN".into()
    } else if x.is_infinite() {
        if x > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        repr(x)
    }
}

/// pydantic-core `model_dump_json` float (non-finite values serialise as `null`, pydantic's default `ser_json_inf_nan`).
pub fn pydantic_json(x: f64) -> String {
    if !x.is_finite() {
        return "null".into();
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    if x == 0.0 {
        return format!("{sign}0.0");
    }
    let (digits, decpt) = digits_and_decpt(x);
    let e = decpt - 1;
    if (-5..=15).contains(&e) {
        format!("{sign}{}", fixed(&digits, decpt))
    } else {
        let esign = if e < 0 { "-" } else { "+" };
        format!("{sign}{}e{esign}{}", mantissa(&digits), e.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_matches_cpython() {
        let cases = [
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (1.5e-7, "1.5e-07"),
            (1e-5, "1e-05"),
            (1e-4, "0.0001"),
            (0.0001234, "0.0001234"),
            (1.5e300, "1.5e+300"),
            (123_456_789.125, "123456789.125"),
            (-0.0, "-0.0"),
            (100.0, "100.0"),
            (0.1, "0.1"),
            (1.0, "1.0"),
            (9_999_999_999_999_998.0, "9999999999999998.0"),
            (1_761_600_000.123_456, "1761600000.123456"),
            (5e-324, "5e-324"),
            (1.797_693_134_862_315_7e308, "1.7976931348623157e+308"),
        ];
        for (x, want) in cases {
            assert_eq!(repr(x), want, "{x:e}");
        }
        assert_eq!(json(f64::INFINITY), "Infinity");
        assert_eq!(json(f64::NAN), "NaN");
    }

    #[test]
    fn pydantic_matches_probe() {
        // pydantic 2.13.5 model_dump_json, probed 2026-10-27
        let cases = [
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (1.5e-7, "1.5e-7"),
            (1e-5, "0.00001"),
            (1e-4, "0.0001"),
            (0.0001234, "0.0001234"),
            (1.5e300, "1.5e+300"),
            (2.5e-300, "2.5e-300"),
            (123_456_789.125, "123456789.125"),
            (-0.0, "-0.0"),
            (1e22, "1e+22"),
            (1.797_693_134_862_315_7e308, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
            (100.0, "100.0"),
            (1e17, "1e+17"),
            (12_345_678_901_234_567_890.0, "1.2345678901234567e+19"),
        ];
        for (x, want) in cases {
            assert_eq!(pydantic_json(x), want, "{x:e}");
        }
        assert_eq!(pydantic_json(f64::INFINITY), "null");
    }
}
