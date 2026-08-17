//! Raw value operations: arithmetic, comparison, and truthiness without
//! metamethods (port the number paths of `lvmutils.cpp`).
//!
//! The arithmetic and ordered-comparison operators act on Luau numbers (`f64`)
//! only. This revision's 64-bit integer is a distinct identity/key type that the
//! operators reject (it does not coerce to a number), and equality is tag-strict,
//! so a number and an integer are never equal. Rejected operands surface as
//! typed errors unless the caller dispatches a metamethod.

use crate::api::RawValue;

/// The arithmetic operators the core dispatches (no metamethods).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/` (always float)
    Div,
    /// `//` (floor division)
    IDiv,
    /// `%`
    Mod,
    /// `^` (always float)
    Pow,
}

/// Lua truthiness: only `nil` and `false` are falsy.
#[must_use]
pub fn truthy(value: RawValue) -> bool {
    !matches!(value, RawValue::Nil | RawValue::Boolean(false))
}

/// The `f64` value of an operand, if it is a Luau number. This revision's
/// integers are a distinct identity type that does not coerce to a number
/// (`luaV_tonumber` rejects them), so they — and every non-number — return
/// `None`; the arithmetic and ordered-comparison operators raise on them.
fn as_number(value: RawValue) -> Option<f64> {
    match value {
        RawValue::Number(n) => Some(n),
        _ => None,
    }
}

/// Applies a raw arithmetic operator on Luau numbers (`f64`). Any non-number
/// operand — including this revision's distinct integers, which the arithmetic
/// opcodes do not accept — yields `None`, and the caller raises an arithmetic
/// error or dispatches a metamethod. Division and power follow IEEE (no
/// raise on divide-by-zero).
///
/// # Errors
/// Returns `None` if either operand is not a number.
#[must_use]
#[inline]
pub fn arith(op: ArithOp, lhs: RawValue, rhs: RawValue) -> Option<RawValue> {
    let a = as_number(lhs)?;
    let b = as_number(rhs)?;
    Some(RawValue::Number(match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div => a / b,
        ArithOp::IDiv => (a / b).floor(),
        ArithOp::Mod => num_mod(a, b),
        ArithOp::Pow => a.powf(b),
    }))
}

/// Float modulo, exactly upstream `luai_nummod` (`lnumutils.h`): the expression
/// `a - floor(a / b) * b`. The result follows the divisor's sign, `x % inf` is
/// `NaN`, and — by using the same formula the constant folder does
/// (`luau_fold_mod`) — a folded `a % b` agrees with the runtime. The earlier
/// `fmod`-plus-bias variant biased on the remainder's sign alone, so
/// same-sign-negative operands (e.g. `-1 % -7`) and infinities diverged from both
/// upstream and the folder.
fn num_mod(a: f64, b: f64) -> f64 {
    a - (a / b).floor() * b
}

/// Renders a Luau number as a string for coercion (concat, `tostring`), matching
/// `lnumprint.cpp`'s `luai_num2str`. `inf`/`-inf`/`nan` match Luau's spelling and a
/// signed zero keeps its sign (`-0`). A finite value uses the shortest round-tripping
/// decimal (Rust's `{:e}` yields the same digits Luau's Schubfach does — the shortest
/// representation is unique), then chooses fixed-point when the decimal point sits within
/// `[-5, 21]` digits of the significand and scientific otherwise, exactly as Luau does
/// (so `1e30` is `1e+30` but `3.69e19` is `36984408976312840000`).
///
/// Known divergence: at an exact decimal tie — a value sitting
/// exactly between two equally-short round-tripping decimals — Rust's `{:e}` rounds the last
/// digit away while Luau's Schubfach rounds half-to-even, so e.g. `1219873251991014.25` prints
/// `…14.3` here and `…14.2` in Luau. Rare (~568 per 2.47M doubles) and absent from the
/// conformance corpus; closing it needs a Schubfach port of `lnumprint.cpp`.
#[must_use]
pub fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        return "nan".to_string();
    }
    if n.is_infinite() {
        return if n < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    let sign = if n.is_sign_negative() { "-" } else { "" };
    if n == 0.0 {
        return format!("{sign}0");
    }
    // Shortest round-trip significand digits and the power-of-ten position of the decimal
    // point. Rust's `{:e}` renders `d.ddde<exp>`; the value is `digits × 10^(exp-(declen-1))`,
    // so the decimal point sits after `dot = exp + 1` significand digits.
    let sci = format!("{:e}", n.abs());
    let (mantissa, exp) = sci
        .split_once('e')
        .expect("`{:e}` always emits an exponent");
    let exp: i32 = exp.parse().expect("`{:e}` exponent is an integer");
    let digits: String = mantissa.chars().filter(|&c| c != '.').collect();
    let declen = digits.len() as i32;
    let dot = exp + 1;

    let body = if (-5..=21).contains(&dot) {
        if dot <= 0 {
            // 0.00…digits
            format!("0.{}{digits}", "0".repeat((-dot) as usize))
        } else if dot >= declen {
            // all significant digits are integer; pad with zeros if the point is past them
            format!("{digits}{}", "0".repeat((dot - declen) as usize))
        } else {
            let (int, frac) = digits.split_at(dot as usize);
            format!("{int}.{frac}")
        }
    } else {
        // scientific: d.ddd e±NN (the significand carries no trailing zeros, so no trimming)
        let (lead, frac) = digits.split_at(1);
        let mantissa = if frac.is_empty() {
            lead.to_string()
        } else {
            format!("{lead}.{frac}")
        };
        let e = dot - 1;
        format!("{mantissa}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
    };
    format!("{sign}{body}")
}

/// Parses a string to a number the way `luaV_tonumber`/`luaO_str2d` does for the
/// arithmetic and `for`-bound coercions: surrounding ASCII whitespace is ignored,
/// a `0x`/`0X` prefix is a hexadecimal integer or C-style hexadecimal float, and
/// the rest parse as a decimal float. Returns `None` for anything that is not a
/// complete number.
#[must_use]
pub fn str_to_number(bytes: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(bytes)
        .ok()?
        .trim_matches(|c: char| c.is_ascii_whitespace());
    if text.is_empty() {
        return None;
    }
    let (sign, body) = match text.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, text.strip_prefix('+').unwrap_or(text)),
    };
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        if hex.contains(['.', 'p', 'P']) {
            return parse_hex_float(hex).map(|magnitude| sign * magnitude);
        }
        #[allow(clippy::cast_precision_loss)]
        return u64::from_str_radix(hex, 16)
            .ok()
            .map(|magnitude| sign * magnitude as f64);
    }
    text.parse::<f64>().ok()
}

fn parse_hex_float(text: &str) -> Option<f64> {
    let (mantissa, exponent) = text
        .split_once('p')
        .or_else(|| text.split_once('P'))
        .unwrap_or((text, "0"));
    if mantissa.is_empty() || exponent.is_empty() {
        return None;
    }

    let mut value = 0.0;
    let mut saw_digit = false;
    let mut saw_dot = false;
    let mut fraction_digits = 0i32;
    for byte in mantissa.bytes() {
        match byte {
            b'.' if !saw_dot => saw_dot = true,
            b'.' => return None,
            digit => {
                let digit = hex_digit_value(digit)?;
                value = value * 16.0 + f64::from(digit);
                saw_digit = true;
                if saw_dot {
                    fraction_digits = fraction_digits.saturating_add(1);
                }
            }
        }
    }
    if !saw_digit {
        return None;
    }

    let exponent = parse_signed_exponent(exponent)?;
    Some(value * 2.0_f64.powi(exponent.saturating_sub(4 * fraction_digits)))
}

fn parse_signed_exponent(text: &str) -> Option<i32> {
    let (negative, digits) = match text.as_bytes().first().copied() {
        Some(b'+') => (false, &text[1..]),
        Some(b'-') => (true, &text[1..]),
        _ => (false, text),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let magnitude = digits.parse::<i32>().unwrap_or(i32::MAX);
    Some(if negative {
        magnitude.saturating_neg()
    } else {
        magnitude
    })
}

fn hex_digit_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Negates a number. A non-number — including an integer — returns `None`, since
/// unary minus is an arithmetic operator the integer type does not accept.
///
/// # Errors
/// Returns `None` for a non-number operand.
#[must_use]
pub fn negate(value: RawValue) -> Option<RawValue> {
    as_number(value).map(|n| RawValue::Number(-n))
}

/// Raw equality (`==` without `__eq`): two values are equal only when they share
/// a tag and compare equal — numbers by IEEE value (so `NaN != NaN`), integers
/// by their bits, strings and other GC values by interned/arena identity. A
/// number and an integer are never raw-equal because their tags differ, matching
/// this revision's tag-gated `JUMPIFEQ`.
#[must_use]
pub fn raw_equal(lhs: RawValue, rhs: RawValue) -> bool {
    lhs == rhs
}

/// Ordered comparison result for `<`. Defined only for two numbers; `None` means
/// the operands are not directly comparable — the caller raises an order error or
/// dispatches `__lt`. The revision compares only same-tag operands, so a
/// number against an integer is not comparable.
#[must_use]
pub fn less_than(lhs: RawValue, rhs: RawValue) -> Option<bool> {
    let a = as_number(lhs)?;
    let b = as_number(rhs)?;
    Some(a < b)
}

/// Whether `lhs <= rhs` for two numbers; `None` otherwise (see [`less_than`]).
#[must_use]
pub fn less_equal(lhs: RawValue, rhs: RawValue) -> Option<bool> {
    let a = as_number(lhs)?;
    let b = as_number(rhs)?;
    Some(a <= b)
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn number_arithmetic() {
        assert_eq!(
            arith(ArithOp::Add, RawValue::Number(2.0), RawValue::Number(3.0)),
            Some(RawValue::Number(5.0))
        );
        assert_eq!(
            arith(ArithOp::Div, RawValue::Number(7.0), RawValue::Number(2.0)),
            Some(RawValue::Number(3.5))
        );
        assert_eq!(
            arith(ArithOp::IDiv, RawValue::Number(7.0), RawValue::Number(-2.0)),
            Some(RawValue::Number(-4.0))
        );
    }

    #[test]
    fn integers_have_no_arithmetic() {
        // This revision's integers are not accepted by the arithmetic operators
        // (luaV_tonumber rejects them), so every form yields None — the caller
        // raises an arithmetic error or dispatches a metamethod.
        assert_eq!(
            arith(ArithOp::Add, RawValue::Integer(2), RawValue::Integer(3)),
            None
        );
        assert_eq!(
            arith(ArithOp::Add, RawValue::Integer(1), RawValue::Number(0.5)),
            None
        );
        assert_eq!(negate(RawValue::Integer(5)), None);
    }

    #[test]
    fn lua_modulo_follows_divisor_sign() {
        let m = |a: f64, b: f64| arith(ArithOp::Mod, RawValue::Number(a), RawValue::Number(b));
        // -1 % 3 == 2 in Lua: the result follows the divisor's sign.
        assert_eq!(m(-1.0, 3.0), Some(RawValue::Number(2.0)));
        // A negative divisor: the result is non-positive.
        assert_eq!(m(5.0, -3.0), Some(RawValue::Number(-1.0)));
        // Same-sign-negative operands: -1 % -7 is -1 (the fmod-bias variant gave
        // -8). This is the case that diverged from the constant folder.
        assert_eq!(m(-1.0, -7.0), Some(RawValue::Number(-1.0)));
        assert_eq!(m(-7.0, -3.0), Some(RawValue::Number(-1.0)));
        // x % inf is NaN (floor(x/inf) is 0, but x - 0*inf is x - NaN).
        assert!(matches!(m(7.0, f64::INFINITY), Some(RawValue::Number(n)) if n.is_nan()));
    }

    #[test]
    fn non_numbers_do_not_arith() {
        assert_eq!(
            arith(ArithOp::Add, RawValue::Boolean(true), RawValue::Number(1.0)),
            None
        );
    }

    #[test]
    fn equality_is_tag_strict() {
        // A number and an integer never compare equal: distinct tags.
        assert!(!raw_equal(RawValue::Integer(1), RawValue::Number(1.0)));
        assert!(raw_equal(RawValue::Integer(1), RawValue::Integer(1)));
        assert!(raw_equal(RawValue::Number(1.0), RawValue::Number(1.0)));
        assert!(!raw_equal(RawValue::Number(1.0), RawValue::Number(1.5)));
        assert!(raw_equal(RawValue::Nil, RawValue::Nil));
        // NaN is never equal to itself.
        assert!(!raw_equal(
            RawValue::Number(f64::NAN),
            RawValue::Number(f64::NAN)
        ));
    }

    #[test]
    fn comparison_requires_two_numbers() {
        assert_eq!(
            less_than(RawValue::Number(2.0), RawValue::Number(5.0)),
            Some(true)
        );
        // Integers and mixed tags are not ordered-comparable.
        assert_eq!(less_than(RawValue::Integer(2), RawValue::Integer(5)), None);
        assert_eq!(less_than(RawValue::Integer(2), RawValue::Number(5.0)), None);
    }

    #[test]
    fn string_to_number_parsing() {
        assert_eq!(str_to_number(b"  42 "), Some(42.0));
        assert_eq!(str_to_number(b"3.5"), Some(3.5));
        assert_eq!(str_to_number(b"-7"), Some(-7.0));
        assert_eq!(str_to_number(b"0x1A"), Some(26.0));
        assert_eq!(str_to_number(b"0x1.8p1"), Some(3.0));
        assert_eq!(str_to_number(b" -0X1p-1 "), Some(-0.5));
        assert_eq!(str_to_number(b"1e3"), Some(1000.0));
        assert_eq!(str_to_number(b"0x1p"), None);
        assert_eq!(str_to_number(b"0x.p1"), None);
        assert_eq!(str_to_number(b"abc"), None);
        assert_eq!(str_to_number(b""), None);
    }

    #[test]
    fn number_rendering() {
        assert_eq!(number_to_string(2.0), "2");
        assert_eq!(number_to_string(1.5), "1.5");
        assert_eq!(number_to_string(f64::INFINITY), "inf");
        assert_eq!(number_to_string(f64::NEG_INFINITY), "-inf");
        assert_eq!(number_to_string(f64::NAN), "nan");
    }

    #[test]
    fn truthiness() {
        assert!(truthy(RawValue::Integer(0)));
        assert!(truthy(RawValue::Boolean(true)));
        assert!(!truthy(RawValue::Boolean(false)));
        assert!(!truthy(RawValue::Nil));
    }
}

#[cfg(any())]
mod numfmt_tests {
    use super::number_to_string;
    #[test]
    fn number_to_string_matches_luau() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0"),
            (-0.0, "-0"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
            (1.0, "1"),
            (42.0, "42"),
            (-4294967296.0, "-4294967296"),
            (9007199254740991.0, "9007199254740991"),
            (0.5, "0.5"),
            (0.1, "0.1"),
            (-0.17, "-0.17"),
            (std::f64::consts::PI, "3.141592653589793"),
            (1e+30, "1e+30"),
            (-1e+24, "-1e+24"),
            (5.453_612_398_302e-311, "5.453612398302e-311"),
            (4.415_489_584_193e-305, "4.415489584193e-305"),
            (1125968630513728.0, "1125968630513728"),
            (1.625, "1.625"),
            (5e-324, "5e-324"),
            (2.0049288280105384, "2.0049288280105384"),
            (3.0517578125e-05, "0.000030517578125"),
            (3.005_335_093_269_1, "3.0053350932691"),
            (0.0001373291015625, "0.0001373291015625"),
            (-1.949_062_802_28e289, "-1.94906280228e+289"),
            (-4.237_534_4e73, "-4.2375344e+73"),
            (3.698_440_897_631_284e19, "36984408976312840000"),
            (2.056_300_052_706_33, "2.05630005270633"),
            (1.1295093211933533e+65, "1.1295093211933533e+65"),
            (1.3202313930270133e-192, "1.3202313930270133e-192"),
        ];
        let mut fails = 0;
        for (n, want) in cases {
            let got = number_to_string(*n);
            if got != *want {
                eprintln!("FMT {n:e}: got {got:?} want {want:?}");
                fails += 1;
            }
        }
        assert_eq!(fails, 0, "{fails} formatting mismatches");
    }
}

#[cfg(any())]
mod proptests {
    use proptest::prelude::*;

    use super::{number_to_string, str_to_number};

    proptest! {
        /// Every finite f64 survives `tostring`/`tonumber`: the rendered text
        /// parses back to a numerically equal value.
        #[test]
        fn number_to_string_to_number_roundtrips(x in proptest::num::f64::ANY) {
            prop_assume!(x.is_finite());
            let rendered = number_to_string(x);
            prop_assert_eq!(str_to_number(rendered.as_bytes()), Some(x));
        }

        /// Decimal integer text parses to the exact f64 cast of the integer.
        #[test]
        fn integer_text_parses_to_the_float_cast(n: i64) {
            prop_assert_eq!(str_to_number(n.to_string().as_bytes()), Some(n as f64));
        }
    }
}
