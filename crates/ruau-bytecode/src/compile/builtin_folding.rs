use super::analysis::ConstantValue;
use crate::opcodes::BuiltinFunction;

pub(super) fn fold_builtin_constant(
    function_id: u8,
    args: &[Option<ConstantValue>],
) -> Option<ConstantValue> {
    match function_id {
        BuiltinFunction::MATH_ABS => fold_math_unary(args, f64::abs),
        BuiltinFunction::MATH_ACOS => fold_math_unary(args, f64::acos),
        BuiltinFunction::MATH_ASIN => fold_math_unary(args, f64::asin),
        BuiltinFunction::MATH_ATAN2 => fold_math_binary(args, f64::atan2),
        BuiltinFunction::MATH_ATAN => fold_math_unary(args, f64::atan),
        BuiltinFunction::MATH_CEIL => fold_math_unary(args, f64::ceil),
        BuiltinFunction::MATH_COSH => fold_math_unary(args, f64::cosh),
        BuiltinFunction::MATH_COS => fold_math_unary(args, f64::cos),
        BuiltinFunction::MATH_DEG => fold_math_unary(args, |value| value / RAD_DEG),
        BuiltinFunction::MATH_EXP => fold_math_unary(args, f64::exp),
        BuiltinFunction::MATH_FLOOR => fold_math_unary(args, f64::floor),
        BuiltinFunction::MATH_FMOD => fold_math_binary(args, |left, right| left % right),
        // frexp and modf return multiple values upstream, so they are not folded.
        BuiltinFunction::MATH_LDEXP => {
            fold_math_binary(args, |left, right| left * 2f64.powi(right as i32))
        }
        BuiltinFunction::MATH_LOG10 => fold_math_unary(args, f64::log10),
        BuiltinFunction::MATH_LOG => fold_math_log(args),
        BuiltinFunction::MATH_MAX => {
            fold_math_variadic(args, |left, right| if right > left { right } else { left })
        }
        BuiltinFunction::MATH_MIN => {
            fold_math_variadic(args, |left, right| if right < left { right } else { left })
        }
        BuiltinFunction::MATH_POW => fold_math_binary(args, f64::powf),
        BuiltinFunction::MATH_RAD => fold_math_unary(args, |value| value * RAD_DEG),
        BuiltinFunction::MATH_SINH => fold_math_unary(args, f64::sinh),
        BuiltinFunction::MATH_SIN => fold_math_unary(args, f64::sin),
        BuiltinFunction::MATH_SQRT => fold_math_unary(args, f64::sqrt),
        BuiltinFunction::MATH_TANH => fold_math_unary(args, f64::tanh),
        BuiltinFunction::MATH_TAN => fold_math_unary(args, f64::tan),
        BuiltinFunction::BIT32_ARSHIFT => fold_bit32_arshift(args),
        BuiltinFunction::BIT32_BAND => {
            fold_bit32_variadic(args, |left, right| left & right).map(number_from_u32)
        }
        BuiltinFunction::BIT32_BNOT => fold_bit32_unary(args, |value| !value).map(number_from_u32),
        BuiltinFunction::BIT32_BOR => {
            fold_bit32_variadic(args, |left, right| left | right).map(number_from_u32)
        }
        BuiltinFunction::BIT32_BXOR => {
            fold_bit32_variadic(args, |left, right| left ^ right).map(number_from_u32)
        }
        BuiltinFunction::BIT32_BTEST => fold_bit32_variadic(args, |left, right| left & right)
            .map(|value| ConstantValue::Bool(value != 0)),
        BuiltinFunction::BIT32_EXTRACT => fold_bit32_extract(args),
        BuiltinFunction::BIT32_LROTATE => fold_bit32_rotate(args, ShiftDirection::Left),
        BuiltinFunction::BIT32_LSHIFT => fold_bit32_shift(args, ShiftDirection::Left),
        BuiltinFunction::BIT32_REPLACE => fold_bit32_replace(args),
        BuiltinFunction::BIT32_RROTATE => fold_bit32_rotate(args, ShiftDirection::Right),
        BuiltinFunction::BIT32_RSHIFT => fold_bit32_shift(args, ShiftDirection::Right),
        BuiltinFunction::TYPE => fold_typeof(args),
        BuiltinFunction::STRING_BYTE => fold_string_byte(args),
        BuiltinFunction::STRING_CHAR => fold_string_char(args),
        BuiltinFunction::STRING_LEN => fold_string_len(args),
        BuiltinFunction::TYPEOF => fold_type(args),
        BuiltinFunction::STRING_SUB => fold_string_sub(args),
        BuiltinFunction::MATH_CLAMP => fold_math_clamp(args),
        BuiltinFunction::MATH_SIGN => fold_math_unary(args, |value| {
            if value > 0.0 {
                1.0
            } else if value < 0.0 {
                -1.0
            } else {
                0.0
            }
        }),
        BuiltinFunction::MATH_ROUND => fold_math_unary(args, f64::round),
        BuiltinFunction::VECTOR => fold_vector(args),
        BuiltinFunction::MATH_LERP => fold_math_lerp(args),
        BuiltinFunction::MATH_ISNAN => fold_math_unary_bool(args, f64::is_nan),
        BuiltinFunction::MATH_ISINF => fold_math_unary_bool(args, f64::is_infinite),
        BuiltinFunction::MATH_ISFINITE => fold_math_unary_bool(args, f64::is_finite),
        _ => None,
    }
}

const PI: f64 = std::f64::consts::PI;
const RAD_DEG: f64 = PI / 180.0;
const E: f64 = std::f64::consts::E;
const PHI: f64 = 1.618_033_988_749_895;
const SQRT2: f64 = std::f64::consts::SQRT_2;
const TAU: f64 = std::f64::consts::TAU;
const STRING_CHAR_FOLD_LIMIT: usize = 128;

pub(super) fn math_member_constant(member: &str) -> Option<ConstantValue> {
    Some(ConstantValue::Number(match member {
        "pi" => PI,
        "huge" => f64::INFINITY,
        "nan" => f64::NAN,
        "e" => E,
        "phi" => PHI,
        "sqrt2" => SQRT2,
        "tau" => TAU,
        _ => return None,
    }))
}

fn number_arg(arg: Option<&ConstantValue>) -> Option<f64> {
    match arg? {
        ConstantValue::Number(value) => Some(*value),
        ConstantValue::Integer(value) => Some(*value as f64),
        _ => None,
    }
}

fn string_arg(arg: Option<&ConstantValue>) -> Option<&str> {
    match arg? {
        // A string containing invalid UTF-8 bytes carries the `U+FFFF` byte-preservation marker,
        // whose char/byte form differs from the decoded Luau bytes; declining to fold defers the
        // operation to a runtime call over the correctly decoded constant.
        ConstantValue::String(value) if !value.contains('\u{ffff}') => Some(value),
        _ => None,
    }
}

fn fold_math_unary(
    args: &[Option<ConstantValue>],
    op: impl FnOnce(f64) -> f64,
) -> Option<ConstantValue> {
    let [arg] = args else {
        return None;
    };
    Some(ConstantValue::Number(op(number_arg(arg.as_ref())?)))
}

fn fold_math_unary_bool(
    args: &[Option<ConstantValue>],
    op: impl FnOnce(f64) -> bool,
) -> Option<ConstantValue> {
    let [arg] = args else {
        return None;
    };
    Some(ConstantValue::Bool(op(number_arg(arg.as_ref())?)))
}

fn fold_math_binary(
    args: &[Option<ConstantValue>],
    op: impl FnOnce(f64, f64) -> f64,
) -> Option<ConstantValue> {
    let [left, right] = args else {
        return None;
    };
    Some(ConstantValue::Number(op(
        number_arg(left.as_ref())?,
        number_arg(right.as_ref())?,
    )))
}

fn fold_math_variadic(
    args: &[Option<ConstantValue>],
    op: impl Fn(f64, f64) -> f64,
) -> Option<ConstantValue> {
    let (first, rest) = args.split_first()?;
    let mut result = number_arg(first.as_ref())?;
    for arg in rest {
        result = op(result, number_arg(arg.as_ref())?);
    }
    Some(ConstantValue::Number(result))
}

fn fold_math_log(args: &[Option<ConstantValue>]) -> Option<ConstantValue> {
    match args {
        [arg] => Some(ConstantValue::Number(number_arg(arg.as_ref())?.ln())),
        [arg, base] => {
            let arg = number_arg(arg.as_ref())?;
            let base = number_arg(base.as_ref())?;
            Some(ConstantValue::Number(if base == 2.0 {
                arg.log2()
            } else if base == 10.0 {
                arg.log10()
            } else {
                arg.ln() / base.ln()
            }))
        }
        _ => None,
    }
}

fn fold_math_clamp(args: &[Option<ConstantValue>]) -> Option<ConstantValue> {
    let [value, min, max] = args else {
        return None;
    };
    let value = number_arg(value.as_ref())?;
    let min = number_arg(min.as_ref())?;
    let max = number_arg(max.as_ref())?;
    (min <= max).then(|| ConstantValue::Number(value.clamp(min, max)))
}

fn fold_math_lerp(args: &[Option<ConstantValue>]) -> Option<ConstantValue> {
    let [a, b, t] = args else {
        return None;
    };
    let a = number_arg(a.as_ref())?;
    let b = number_arg(b.as_ref())?;
    let t = number_arg(t.as_ref())?;
    Some(ConstantValue::Number(if t == 1.0 {
        b
    } else {
        a + (b - a) * t
    }))
}

fn bit32_arg(arg: Option<&ConstantValue>) -> Option<u32> {
    Some(number_arg(arg)? as i64 as u32)
}

fn number_from_u32(value: u32) -> ConstantValue {
    ConstantValue::Number(f64::from(value))
}

fn fold_bit32_unary(args: &[Option<ConstantValue>], op: impl FnOnce(u32) -> u32) -> Option<u32> {
    let [arg] = args else {
        return None;
    };
    Some(op(bit32_arg(arg.as_ref())?))
}

fn fold_bit32_variadic(
    args: &[Option<ConstantValue>],
    op: impl Fn(u32, u32) -> u32,
) -> Option<u32> {
    let (first, rest) = args.split_first()?;
    let mut result = bit32_arg(first.as_ref())?;
    for arg in rest {
        result = op(result, bit32_arg(arg.as_ref())?);
    }
    Some(result)
}

fn shift_arg(arg: Option<&ConstantValue>) -> Option<i32> {
    Some(number_arg(arg)? as i32)
}

fn fold_bit32_arshift(args: &[Option<ConstantValue>]) -> Option<ConstantValue> {
    let [value, shift] = args else {
        return None;
    };
    let value = bit32_arg(value.as_ref())?;
    let shift = shift_arg(shift.as_ref())?;
    (0..32)
        .contains(&shift)
        .then(|| number_from_u32(((value as i32) >> shift) as u32))
}

/// Which way a bit32 shift or rotate moves.
#[derive(Clone, Copy, Eq, PartialEq)]
enum ShiftDirection {
    Left,
    Right,
}

fn fold_bit32_shift(
    args: &[Option<ConstantValue>],
    direction: ShiftDirection,
) -> Option<ConstantValue> {
    let [value, shift] = args else {
        return None;
    };
    let value = bit32_arg(value.as_ref())?;
    let shift = shift_arg(shift.as_ref())?;
    if !(0..32).contains(&shift) {
        return None;
    }
    Some(number_from_u32(match direction {
        ShiftDirection::Right => value >> shift,
        ShiftDirection::Left => value << shift,
    }))
}

fn fold_bit32_rotate(
    args: &[Option<ConstantValue>],
    direction: ShiftDirection,
) -> Option<ConstantValue> {
    let [value, shift] = args else {
        return None;
    };
    let value = bit32_arg(value.as_ref())?;
    let shift = shift_arg(shift.as_ref())?;
    Some(number_from_u32(match direction {
        ShiftDirection::Right => value.rotate_right((shift & 31) as u32),
        ShiftDirection::Left => value.rotate_left((shift & 31) as u32),
    }))
}

fn bit_mask(width: i32) -> u32 {
    !(0xffff_fffeu32 << (width - 1))
}

fn fold_bit32_extract(args: &[Option<ConstantValue>]) -> Option<ConstantValue> {
    let [value, field, rest @ ..] = args else {
        return None;
    };
    let value = bit32_arg(value.as_ref())?;
    let field = shift_arg(field.as_ref())?;
    let width = match rest {
        [] => 1,
        [width] => shift_arg(width.as_ref())?,
        _ => return None,
    };
    (field >= 0 && width > 0 && field + width <= 32)
        .then(|| number_from_u32((value >> field) & bit_mask(width)))
}

fn fold_bit32_replace(args: &[Option<ConstantValue>]) -> Option<ConstantValue> {
    let [number, replacement, field, rest @ ..] = args else {
        return None;
    };
    let number = bit32_arg(number.as_ref())?;
    let replacement = bit32_arg(replacement.as_ref())?;
    let field = shift_arg(field.as_ref())?;
    let width = match rest {
        [] => 1,
        [width] => shift_arg(width.as_ref())?,
        _ => return None,
    };
    if field < 0 || width <= 0 || field + width > 32 {
        return None;
    }
    let mask = bit_mask(width);
    Some(number_from_u32(
        (number & !(mask << field)) | ((replacement & mask) << field),
    ))
}

/// Folds `type(constant)`: vectors fold only under `typeof`, which names them.
fn fold_type(args: &[Option<ConstantValue>]) -> Option<ConstantValue> {
    fold_type_name(args, None)
}

/// Folds `typeof(constant)`, which additionally names vectors.
fn fold_typeof(args: &[Option<ConstantValue>]) -> Option<ConstantValue> {
    fold_type_name(args, Some("vector"))
}

fn fold_type_name(
    args: &[Option<ConstantValue>],
    vector_name: Option<&str>,
) -> Option<ConstantValue> {
    let [arg] = args else {
        return None;
    };
    Some(ConstantValue::String(
        match arg.as_ref()? {
            ConstantValue::Nil => "nil",
            ConstantValue::Bool(_) => "boolean",
            ConstantValue::Number(_) => "number",
            ConstantValue::Integer(_) => "integer",
            ConstantValue::String(_) => "string",
            ConstantValue::Vector { .. } => vector_name?,
        }
        .to_owned(),
    ))
}

fn fold_string_byte(args: &[Option<ConstantValue>]) -> Option<ConstantValue> {
    match args {
        [value] => {
            let value = string_arg(value.as_ref())?.as_bytes();
            value
                .first()
                .map(|byte| ConstantValue::Number(f64::from(*byte)))
        }
        [value, index] => {
            let value = string_arg(value.as_ref())?.as_bytes();
            let index = number_arg(index.as_ref())? as i32;
            if index > 0 && (index as usize) <= value.len() {
                Some(ConstantValue::Number(f64::from(value[index as usize - 1])))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn fold_string_char(args: &[Option<ConstantValue>]) -> Option<ConstantValue> {
    if args.len() >= STRING_CHAR_FOLD_LIMIT {
        return None;
    }
    let mut bytes = Vec::with_capacity(args.len());
    for arg in args {
        let byte = number_arg(arg.as_ref())? as i32;
        if !(0..=u8::MAX as i32).contains(&byte) {
            return None;
        }
        if byte > i32::from(b'\x7f') {
            return None;
        }
        bytes.push(byte as u8);
    }
    String::from_utf8(bytes).ok().map(ConstantValue::String)
}

fn fold_string_len(args: &[Option<ConstantValue>]) -> Option<ConstantValue> {
    let [value] = args else {
        return None;
    };
    Some(ConstantValue::Number(
        string_arg(value.as_ref())?.len() as f64
    ))
}

fn fold_string_sub(args: &[Option<ConstantValue>]) -> Option<ConstantValue> {
    let [value, start, rest @ ..] = args else {
        return None;
    };
    let value = string_arg(value.as_ref())?.as_bytes();
    let len = value.len() as i32;
    let mut start = number_arg(start.as_ref())? as i32;
    let mut end = match rest {
        [] => len,
        [end] => number_arg(end.as_ref())? as i32,
        _ => return None,
    };

    if start < 0 {
        start += len + 1;
    }
    if end < 0 {
        end += len + 1;
    }
    if end < 1 {
        return Some(ConstantValue::String(String::new()));
    }

    start = start.max(1);
    end = end.min(len);

    if start <= end {
        String::from_utf8(value[start as usize - 1..end as usize].to_vec())
            .ok()
            .map(ConstantValue::String)
    } else {
        Some(ConstantValue::String(String::new()))
    }
}

fn fold_vector(args: &[Option<ConstantValue>]) -> Option<ConstantValue> {
    if !(2..=4).contains(&args.len()) {
        return None;
    }
    let mut values = [0.0f32; 4];
    for (index, arg) in args.iter().enumerate() {
        values[index] = number_arg(arg.as_ref())? as f32;
    }
    Some(ConstantValue::Vector {
        bits: values.map(f32::to_bits),
    })
}

#[cfg(test)]
mod tests {
    use super::fold_string_char;
    use crate::compile::ConstantValue;

    #[test]
    fn string_char_fold_defers_non_ascii_bytes_to_runtime() {
        let ascii = [
            Some(ConstantValue::Number(65.0)),
            Some(ConstantValue::Number(66.0)),
        ];
        assert_eq!(
            fold_string_char(&ascii),
            Some(ConstantValue::String("AB".to_owned()))
        );

        let utf8 = [
            Some(ConstantValue::Number(195.0)),
            Some(ConstantValue::Number(169.0)),
        ];
        assert_eq!(fold_string_char(&utf8), None);
    }
}
