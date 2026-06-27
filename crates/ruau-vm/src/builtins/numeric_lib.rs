use super::*;

pub(super) fn dispatch(
    builtin: Builtin,
    heap: &mut Heap,
    args: &[RawValue],
) -> Exec<Vec<RawValue>> {
    match builtin {
        Builtin::MathFloor => math_unary(args, "floor", f64::floor),
        Builtin::MathCeil => math_unary(args, "ceil", f64::ceil),
        Builtin::MathAbs => math_unary(args, "abs", f64::abs),
        Builtin::MathSqrt => math_unary(args, "sqrt", f64::sqrt),
        Builtin::MathMax => math_reduce(args, "max", fold_max),
        Builtin::MathMin => math_reduce(args, "min", fold_min),
        Builtin::MathExp => math_unary(args, "exp", f64::exp),
        Builtin::MathLog => math_log(args),
        Builtin::MathLog10 => math_unary(args, "log10", f64::log10),
        Builtin::MathPow => math_binary(args, f64::powf),
        Builtin::MathFmod => math_binary(args, |x, y| x % y),
        Builtin::MathModf => math_modf(args),
        Builtin::MathFrexp => math_frexp(args),
        Builtin::MathLdexp => math_binary(args, |s, e| s * e.trunc().exp2()),
        Builtin::MathSin => math_unary(args, "sin", f64::sin),
        Builtin::MathCos => math_unary(args, "cos", f64::cos),
        Builtin::MathTan => math_unary(args, "tan", f64::tan),
        Builtin::MathAsin => math_unary(args, "asin", f64::asin),
        Builtin::MathAcos => math_unary(args, "acos", f64::acos),
        Builtin::MathAtan => math_unary(args, "atan", f64::atan),
        Builtin::MathAtan2 => math_binary(args, f64::atan2),
        Builtin::MathSinh => math_unary(args, "sinh", f64::sinh),
        Builtin::MathCosh => math_unary(args, "cosh", f64::cosh),
        Builtin::MathTanh => math_unary(args, "tanh", f64::tanh),
        Builtin::MathRad => math_unary(args, "rad", f64::to_radians),
        Builtin::MathDeg => math_unary(args, "deg", f64::to_degrees),
        Builtin::MathSign => math_unary(args, "sign", math_sign),
        Builtin::MathRound => math_unary(args, "round", f64::round),
        Builtin::MathClamp => math_clamp(args),
        Builtin::MathLerp => math_lerp(args),
        Builtin::MathMap => math_map(args),
        Builtin::MathIsNan => math_predicate(args, f64::is_nan),
        Builtin::MathIsInf => math_predicate(args, f64::is_infinite),
        Builtin::MathIsFinite => math_predicate(args, f64::is_finite),
        Builtin::MathRandom => math_random(heap, args),
        Builtin::MathRandomseed => math_randomseed(heap, args),
        Builtin::MathNoise => math_noise(args),
        Builtin::IntegerCreate => integer_create(args),
        Builtin::IntegerFromString => integer_fromstring(heap, args),
        Builtin::IntegerNeg => integer_unary(args, i64::wrapping_neg),
        Builtin::IntegerAdd => integer_binary(args, i64::wrapping_add),
        Builtin::IntegerSub => integer_binary(args, i64::wrapping_sub),
        Builtin::IntegerMul => integer_binary(args, i64::wrapping_mul),
        Builtin::IntegerDiv => integer_div(args),
        Builtin::IntegerIDiv => integer_idiv(args),
        Builtin::IntegerUDiv => integer_udiv(args),
        Builtin::IntegerURem => integer_urem(args),
        Builtin::IntegerMod => integer_mod(args),
        Builtin::IntegerRem => integer_rem(args),
        Builtin::IntegerMin => integer_extreme(args, |left, right| left < right),
        Builtin::IntegerMax => integer_extreme(args, |left, right| left > right),
        Builtin::IntegerClamp => integer_clamp(args),
        Builtin::IntegerBand => integer_reduce(args, -1, |left, right| left & right),
        Builtin::IntegerBor => integer_reduce(args, 0, |left, right| left | right),
        Builtin::IntegerBnot => integer_unary(args, |value| !value),
        Builtin::IntegerBxor => integer_reduce(args, 0, |left, right| left ^ right),
        Builtin::IntegerBtest => integer_btest(args),
        Builtin::IntegerLt => integer_compare(args, |left, right| left < right),
        Builtin::IntegerLe => integer_compare(args, |left, right| left <= right),
        Builtin::IntegerUlt => integer_unsigned_compare(args, |left, right| left < right),
        Builtin::IntegerUle => integer_unsigned_compare(args, |left, right| left <= right),
        Builtin::IntegerGt => integer_compare(args, |left, right| left > right),
        Builtin::IntegerGe => integer_compare(args, |left, right| left >= right),
        Builtin::IntegerUgt => integer_unsigned_compare(args, |left, right| left > right),
        Builtin::IntegerUge => integer_unsigned_compare(args, |left, right| left >= right),
        Builtin::IntegerLshift => integer_lshift(args),
        Builtin::IntegerRshift => integer_rshift(args),
        Builtin::IntegerArshift => integer_arshift(args),
        Builtin::IntegerLrotate => integer_lrotate(args),
        Builtin::IntegerRrotate => integer_rrotate(args),
        Builtin::IntegerExtract => integer_extract(args),
        Builtin::IntegerReplace => integer_replace(args),
        Builtin::IntegerCountrz => integer_count_zeros(args, u64::trailing_zeros),
        Builtin::IntegerCountlz => integer_count_zeros(args, u64::leading_zeros),
        Builtin::IntegerBswap => integer_unary(args, |value| (value as u64).swap_bytes() as i64),
        _ => unreachable!("non-numeric builtin dispatched to numeric_lib"),
    }
}

/// A number argument for the math library after the dispatcher has already
/// coerced numeric strings; an integer coerces to a number.
fn math_arg(args: &[RawValue], index: usize) -> Exec<f64> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::Number(n) => Ok(n),
        RawValue::Integer(i) => Ok(i as f64),
        _ => Err(err("bad argument to a math function (number expected)")),
    }
}

fn math_arg_named(args: &[RawValue], index: usize, name: &str) -> Exec<f64> {
    match args.get(index).copied() {
        Some(RawValue::Number(n)) => Ok(n),
        Some(RawValue::Integer(i)) => Ok(i as f64),
        Some(_) => Err(err(format!("invalid argument #{} to '{name}'", index + 1))),
        None => Err(err(format!("missing argument #{} to '{name}'", index + 1))),
    }
}

/// A one-argument math function (`floor`/`ceil`/`abs`/`sqrt`).
fn math_unary(args: &[RawValue], name: &str, op: fn(f64) -> f64) -> Exec<Vec<RawValue>> {
    Ok(vec![RawValue::Number(op(math_arg_named(args, 0, name)?))])
}

/// A variadic reducer (`max`/`min`) over at least one number argument.
/// `math.min`'s fold: keep the accumulator unless the next value is strictly
/// smaller (upstream `if (d < dmin) dmin = d`). NaN comparisons are false, so a
/// NaN in the accumulator (e.g. the first argument) propagates while a later NaN
/// is ignored — `min(nan, 2)` is NaN but `min(1, nan)` is 1, unlike `f64::min`
/// which discards NaN.
fn fold_min(acc: f64, value: f64) -> f64 {
    if value < acc { value } else { acc }
}

/// `math.max`'s fold: the mirror of [`fold_min`] (`if (d > dmax) dmax = d`).
fn fold_max(acc: f64, value: f64) -> f64 {
    if value > acc { value } else { acc }
}

fn math_reduce(args: &[RawValue], name: &str, op: fn(f64, f64) -> f64) -> Exec<Vec<RawValue>> {
    if args.is_empty() {
        return Err(err(format!(
            "bad argument #1 to 'math.{name}' (number expected, got no value)"
        )));
    }
    let mut acc = math_arg(args, 0)?;
    for index in 1..args.len() {
        acc = op(acc, math_arg(args, index)?);
    }
    Ok(vec![RawValue::Number(acc)])
}

/// A two-argument math function (`pow`/`fmod`/`atan2`/`ldexp`).
fn math_binary(args: &[RawValue], op: fn(f64, f64) -> f64) -> Exec<Vec<RawValue>> {
    Ok(vec![RawValue::Number(op(
        math_arg(args, 0)?,
        math_arg(args, 1)?,
    ))])
}

/// A one-argument predicate (`isnan`/`isinf`/`isfinite`).
fn math_predicate(args: &[RawValue], op: fn(f64) -> bool) -> Exec<Vec<RawValue>> {
    Ok(vec![RawValue::Boolean(op(math_arg(args, 0)?))])
}

/// `math.sign`: 1 for positive, -1 for negative, 0 for zero and NaN.
fn math_sign(n: f64) -> f64 {
    if n > 0.0 {
        1.0
    } else if n < 0.0 {
        -1.0
    } else {
        0.0
    }
}

/// `math.log(n, base?)`: natural log, or `log_base(n)` when a base is given. A
/// missing or `nil` base is the natural log; base 2 and 10 use the exact `log2`/
/// `log10` paths, like upstream.
fn math_log(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let n = math_arg(args, 0)?;
    let result = match args.get(1).copied() {
        None | Some(RawValue::Nil) => n.ln(),
        Some(_) => {
            let base = math_arg(args, 1)?;
            if base == 2.0 {
                n.log2()
            } else if base == 10.0 {
                n.log10()
            } else {
                n.ln() / base.ln()
            }
        }
    };
    Ok(vec![RawValue::Number(result)])
}

/// `math.modf(n)`: the integral and fractional parts (the fraction of a
/// non-finite number is zero, matching C `modf`).
fn math_modf(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let n = math_arg(args, 0)?;
    let (integral, fractional) = if n.is_nan() {
        (n, n)
    } else if n.is_infinite() {
        // C `modf`: the integral part is the infinity, the fraction a signed zero.
        (n, 0.0_f64.copysign(n))
    } else {
        (n.trunc(), n - n.trunc())
    };
    Ok(vec![
        RawValue::Number(integral),
        RawValue::Number(fractional),
    ])
}

/// `math.frexp(n)`: the mantissa `m` (`0.5 <= |m| < 1`) and exponent `e` with
/// `n == m * 2^e`; `(n, 0)` for zero and non-finite values.
fn math_frexp(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let n = math_arg(args, 0)?;
    let (mantissa, exponent) = frexp(n);
    Ok(vec![RawValue::Number(mantissa), RawValue::Number(exponent)])
}

fn frexp(n: f64) -> (f64, f64) {
    if n == 0.0 || !n.is_finite() {
        return (n, 0.0);
    }
    let bits = n.to_bits();
    let raw_exp = ((bits >> 52) & 0x7ff) as i64;
    if raw_exp == 0 {
        // Subnormal: scale into the normal range, then correct the exponent.
        let (m, e) = frexp(n * 2f64.powi(64));
        return (m, e - 64.0);
    }
    let exponent = raw_exp - 1022;
    // Force the stored exponent to 1022 so the mantissa lands in `[0.5, 1)`.
    let mantissa_bits = (bits & !(0x7ffu64 << 52)) | (1022u64 << 52);
    (f64::from_bits(mantissa_bits), exponent as f64)
}

/// `math.clamp(n, min, max)`: errors if `min > max`, like upstream.
fn math_clamp(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let n = math_arg(args, 0)?;
    let min = math_arg(args, 1)?;
    let max = math_arg(args, 2)?;
    // A NaN bound (incomparable) or `min > max` errors, like upstream.
    if min > max || min.is_nan() || max.is_nan() {
        return Err(err(
            "invalid argument #3 to 'math.clamp' (max must be greater than or equal to min)",
        ));
    }
    Ok(vec![RawValue::Number(n.max(min).min(max))])
}

/// `math.lerp(a, b, t)`: `a + (b - a) * t`, with `t == 1` returning `b` exactly.
fn math_lerp(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let a = math_arg(args, 0)?;
    let b = math_arg(args, 1)?;
    let t = math_arg(args, 2)?;
    let result = if t == 1.0 { b } else { a + (b - a) * t };
    Ok(vec![RawValue::Number(result)])
}

/// `math.map(x, inmin, inmax, outmin, outmax)`: re-ranges `x` linearly.
fn math_map(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let x = math_arg(args, 0)?;
    let in_min = math_arg(args, 1)?;
    let in_max = math_arg(args, 2)?;
    let out_min = math_arg(args, 3)?;
    let out_max = math_arg(args, 4)?;
    let result = out_min + (x - in_min) * (out_max - out_min) / (in_max - in_min);
    Ok(vec![RawValue::Number(result)])
}

/// An integer argument to `math.random`, truncated to a C `int` like upstream's
/// `luaL_checkinteger` (a float truncates toward zero, then wraps to 32 bits).
fn random_int_arg(args: &[RawValue], index: usize) -> Exec<i32> {
    arg_int(args, index)
        .map(|i| i as i32)
        .ok_or_else(|| err("bad argument to 'math.random' (number expected)"))
}

/// `math.random()` / `math.random(m)` / `math.random(m, n)`: a PCG32 draw,
/// mirroring upstream `math_random`. With no argument it returns a double in
/// `[0, 1)`; with one, an integer in `[1, m]`; with two, in `[m, n]`. Integer
/// results are `Number`s, like the rest of the stdlib's index returns.
fn math_random(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    match args.len() {
        0 => {
            let rl = u64::from(heap.next_random_u32());
            let rh = u64::from(heap.next_random_u32());
            // ldexp(double(rl | rh << 32), -64): a uniform double in [0, 1).
            let rd = ((rl | (rh << 32)) as f64) * 2.0_f64.powi(-64);
            Ok(vec![RawValue::Number(rd)])
        }
        1 => {
            let u = random_int_arg(args, 0)?;
            if u < 1 {
                return Err(err("bad argument #1 to 'math.random' (interval is empty)"));
            }
            let x = u64::from(u as u32).wrapping_mul(u64::from(heap.next_random_u32()));
            let r = 1 + (x >> 32);
            Ok(vec![RawValue::Number(r as f64)])
        }
        2 => {
            let l = random_int_arg(args, 0)?;
            let u = random_int_arg(args, 1)?;
            if l > u {
                return Err(err("bad argument #2 to 'math.random' (interval is empty)"));
            }
            // The span is computed in 32-bit wrapping arithmetic, like upstream;
            // a full-range span (`l = i32::MIN, u = i32::MAX`) is rejected because
            // `ul + 1` would overflow the multiply.
            let ul = (u as u32).wrapping_sub(l as u32);
            if ul == u32::MAX {
                return Err(err(
                    "bad argument #2 to 'math.random' (interval is too large)",
                ));
            }
            let x = u64::from(ul + 1).wrapping_mul(u64::from(heap.next_random_u32()));
            let r = l.wrapping_add((x >> 32) as i32);
            Ok(vec![RawValue::Number(f64::from(r))])
        }
        // Upstream `math_random` raises on any other arity (`lmathlib.cpp`).
        _ => Err(err("wrong number of arguments")),
    }
}

/// `math.randomseed(seed)`: reseeds the VM's `math.random` stream. The seed is
/// truncated to a C `int` then widened (sign-extended) to the PCG32 seed, like
/// upstream `math_randomseed`.
fn math_randomseed(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let seed = arg_int(args, 0)
        .ok_or_else(|| err("bad argument #1 to 'math.randomseed' (number expected)"))?;
    heap.seed_random((seed as i32) as u64);
    Ok(Vec::new())
}

/// `math.noise(x, y?, z?)`: 3-D Perlin gradient noise in roughly `[-1, 1]`,
/// computed in `f32` like upstream `perlin`. A missing or `nil` `y`/`z` defaults
/// to zero; a present non-number errors.
fn math_noise(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let x = math_arg(args, 0)?;
    let y = noise_coord(args, 1)?;
    let z = noise_coord(args, 2)?;
    let r = perlin(x as f32, y as f32, z as f32);
    Ok(vec![RawValue::Number(f64::from(r))])
}

/// An optional `math.noise` coordinate: absent or `nil` is zero, a number is
/// itself, anything else errors (upstream `luaL_argexpected`).
fn noise_coord(args: &[RawValue], index: usize) -> Exec<f64> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::Nil => Ok(0.0),
        RawValue::Number(n) => Ok(n),
        RawValue::Integer(i) => Ok(i as f64),
        _ => Err(err("bad argument to 'math.noise' (number expected)")),
    }
}

fn integer_arg(args: &[RawValue], index: usize) -> Exec<i64> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::Integer(value) => Ok(value),
        _ => Err(err(
            "bad argument to an integer function (integer expected)",
        )),
    }
}

fn integer_result(value: i64) -> Vec<RawValue> {
    vec![RawValue::Integer(value)]
}

fn integer_create(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    match args.first().copied().unwrap_or(RawValue::Nil) {
        RawValue::Integer(value) => Ok(integer_result(value)),
        RawValue::Number(value)
            if value.is_finite()
                && value.fract() == 0.0
                && value >= i64::MIN as f64
                && value < 9_223_372_036_854_775_808.0 =>
        {
            Ok(integer_result(value as i64))
        }
        RawValue::Number(_) => Ok(vec![RawValue::Nil]),
        _ => Err(err("bad argument #1 to 'integer.create' (number expected)")),
    }
}

fn integer_fromstring(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let bytes = match args.first().copied().unwrap_or(RawValue::Nil) {
        RawValue::String(handle) => heap.string(handle).map_or(&[][..], |string| string.bytes()),
        _ => {
            return Err(err(
                "bad argument #1 to 'integer.fromstring' (string expected)",
            ));
        }
    };
    let base = match args.get(1).copied() {
        None | Some(RawValue::Nil) => 10,
        Some(RawValue::Integer(value)) => value,
        Some(RawValue::Number(value)) => value as i64,
        Some(_) => {
            return Err(err(
                "bad argument #2 to 'integer.fromstring' (number expected)",
            ));
        }
    };
    if !(2..=36).contains(&base) {
        return Err(err(
            "bad argument #2 to 'integer.fromstring' (base out of range)",
        ));
    }
    Ok(vec![
        parse_integer_bytes(bytes, base as u32).map_or(RawValue::Nil, RawValue::Integer),
    ])
}

fn parse_integer_bytes(bytes: &[u8], base: u32) -> Option<i64> {
    let is_space = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r');
    let mut index = 0;
    while index < bytes.len() && is_space(bytes[index]) {
        index += 1;
    }
    let mut negate = false;
    if index < bytes.len() && matches!(bytes[index], b'+' | b'-') {
        negate = bytes[index] == b'-';
        index += 1;
    }
    let mut radix = base;
    if index + 1 < bytes.len() && bytes[index] == b'0' && matches!(bytes[index + 1], b'x' | b'X') {
        radix = 16;
        index += 2;
    }
    let digits_start = index;
    let mut value = 0u64;
    while index < bytes.len() {
        let digit = match bytes[index] {
            b'0'..=b'9' => u32::from(bytes[index] - b'0'),
            b'a'..=b'z' => u32::from(bytes[index] - b'a') + 10,
            b'A'..=b'Z' => u32::from(bytes[index] - b'A') + 10,
            _ => break,
        };
        if digit >= radix {
            break;
        }
        value = value
            .wrapping_mul(u64::from(radix))
            .wrapping_add(u64::from(digit));
        index += 1;
    }
    if index == digits_start {
        return None;
    }
    while index < bytes.len() && is_space(bytes[index]) {
        index += 1;
    }
    if index != bytes.len() {
        return None;
    }
    if negate {
        value = 0u64.wrapping_sub(value);
    }
    Some(value as i64)
}

fn integer_unary(args: &[RawValue], op: fn(i64) -> i64) -> Exec<Vec<RawValue>> {
    Ok(integer_result(op(integer_arg(args, 0)?)))
}

fn integer_binary(args: &[RawValue], op: fn(i64, i64) -> i64) -> Exec<Vec<RawValue>> {
    Ok(integer_result(op(
        integer_arg(args, 0)?,
        integer_arg(args, 1)?,
    )))
}

fn integer_div(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let left = integer_arg(args, 0)?;
    let right = integer_arg(args, 1)?;
    if right == 0 {
        return Err(err("division by zero"));
    }
    if left == i64::MIN && right == -1 {
        return Err(err("integer overflow"));
    }
    Ok(integer_result(left / right))
}

fn integer_idiv(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let left = integer_arg(args, 0)?;
    let right = integer_arg(args, 1)?;
    if right == 0 {
        return Err(err("division by zero"));
    }
    if left == i64::MIN && right == -1 {
        return Err(err("integer overflow"));
    }
    Ok(integer_result(integer_floor_div(left, right)))
}

fn integer_udiv(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let left = integer_arg(args, 0)? as u64;
    let right = integer_arg(args, 1)? as u64;
    if right == 0 {
        return Err(err("division by zero"));
    }
    Ok(integer_result((left / right) as i64))
}

fn integer_urem(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let left = integer_arg(args, 0)? as u64;
    let right = integer_arg(args, 1)? as u64;
    if right == 0 {
        return Err(err("division by zero"));
    }
    Ok(integer_result((left % right) as i64))
}

fn integer_mod(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let left = integer_arg(args, 0)?;
    let right = integer_arg(args, 1)?;
    if right == 0 {
        return Err(err("division by zero"));
    }
    if left == i64::MIN && right == -1 {
        return Ok(integer_result(0));
    }
    Ok(integer_result(integer_floor_rem(left, right)))
}

fn integer_rem(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let left = integer_arg(args, 0)?;
    let right = integer_arg(args, 1)?;
    if right == 0 {
        return Err(err("division by zero"));
    }
    if left == i64::MIN && right == -1 {
        return Ok(integer_result(0));
    }
    Ok(integer_result(left % right))
}

fn integer_floor_div(left: i64, right: i64) -> i64 {
    let quotient = left / right;
    let remainder = left % right;
    if remainder != 0 && ((remainder < 0) != (right < 0)) {
        quotient - 1
    } else {
        quotient
    }
}

fn integer_floor_rem(left: i64, right: i64) -> i64 {
    let remainder = left % right;
    if remainder != 0 && ((remainder < 0) != (right < 0)) {
        remainder + right
    } else {
        remainder
    }
}

fn integer_extreme(args: &[RawValue], better: fn(i64, i64) -> bool) -> Exec<Vec<RawValue>> {
    let mut result = integer_arg(args, 0)?;
    for index in 1..args.len() {
        let value = integer_arg(args, index)?;
        if better(value, result) {
            result = value;
        }
    }
    Ok(integer_result(result))
}

fn integer_clamp(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let value = integer_arg(args, 0)?;
    let min = integer_arg(args, 1)?;
    let max = integer_arg(args, 2)?;
    if min > max {
        return Err(err(
            "invalid argument #3 to 'integer.clamp' (max must be greater than or equal to min)",
        ));
    }
    Ok(integer_result(value.clamp(min, max)))
}

fn integer_reduce(
    args: &[RawValue],
    identity: i64,
    op: fn(i64, i64) -> i64,
) -> Exec<Vec<RawValue>> {
    let mut acc = identity;
    for index in 0..args.len() {
        acc = op(acc, integer_arg(args, index)?);
    }
    Ok(integer_result(acc))
}

fn integer_btest(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let mut acc = -1i64;
    for index in 0..args.len() {
        acc &= integer_arg(args, index)?;
    }
    Ok(vec![RawValue::Boolean(acc != 0)])
}

fn integer_compare(args: &[RawValue], op: fn(i64, i64) -> bool) -> Exec<Vec<RawValue>> {
    Ok(vec![RawValue::Boolean(op(
        integer_arg(args, 0)?,
        integer_arg(args, 1)?,
    ))])
}

fn integer_unsigned_compare(args: &[RawValue], op: fn(u64, u64) -> bool) -> Exec<Vec<RawValue>> {
    Ok(vec![RawValue::Boolean(op(
        integer_arg(args, 0)? as u64,
        integer_arg(args, 1)? as u64,
    ))])
}

fn integer_logical_shift(value: i64, disp: i64) -> i64 {
    if !(-63..=63).contains(&disp) {
        0
    } else if disp >= 0 {
        ((value as u64) << disp) as i64
    } else {
        ((value as u64) >> (-disp)) as i64
    }
}

fn integer_lshift(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    Ok(integer_result(integer_logical_shift(
        integer_arg(args, 0)?,
        integer_arg(args, 1)?,
    )))
}

fn integer_rshift(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    Ok(integer_result(integer_logical_shift(
        integer_arg(args, 0)?,
        integer_arg(args, 1)?.saturating_neg(),
    )))
}

fn integer_arshift(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let value = integer_arg(args, 0)?;
    let disp = integer_arg(args, 1)?;
    let result = if disp <= -64 {
        0
    } else if disp < 0 {
        ((value as u64) << (-disp)) as i64
    } else if disp >= 64 {
        if value < 0 { -1 } else { 0 }
    } else {
        value >> disp
    };
    Ok(integer_result(result))
}

fn integer_lrotate(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let amount = integer_arg(args, 1)?.rem_euclid(64) as u32;
    Ok(integer_result(
        (integer_arg(args, 0)? as u64).rotate_left(amount) as i64,
    ))
}

fn integer_rrotate(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let amount = integer_arg(args, 1)?.rem_euclid(64) as u32;
    Ok(integer_result(
        (integer_arg(args, 0)? as u64).rotate_right(amount) as i64,
    ))
}

fn integer_field(args: &[RawValue], field_index: usize) -> Exec<(u32, u32)> {
    let field = integer_arg(args, field_index)?;
    let width = match args.get(field_index + 1).copied() {
        None | Some(RawValue::Nil) => 1,
        _ => integer_arg(args, field_index + 1)?,
    };
    if field < 0 {
        return Err(err("field cannot be negative"));
    }
    if width <= 0 {
        return Err(err("width must be positive"));
    }
    if field >= 64 || width > 64 || field + width > 64 {
        return Err(err("trying to access non-existent bits"));
    }
    Ok((field as u32, width as u32))
}

fn integer_mask(width: u32) -> u64 {
    if width == 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

fn integer_extract(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let value = integer_arg(args, 0)? as u64;
    let (field, width) = integer_field(args, 1)?;
    Ok(integer_result(
        ((value >> field) & integer_mask(width)) as i64,
    ))
}

fn integer_replace(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let value = integer_arg(args, 0)? as u64;
    let replacement = integer_arg(args, 1)? as u64;
    let (field, width) = integer_field(args, 2)?;
    let mask = integer_mask(width);
    let cleared = value & !(mask << field);
    Ok(integer_result(
        (cleared | ((replacement & mask) << field)) as i64,
    ))
}

fn integer_count_zeros(args: &[RawValue], op: fn(u64) -> u32) -> Exec<Vec<RawValue>> {
    Ok(integer_result(i64::from(op(integer_arg(args, 0)? as u64))))
}

/// The permutation table for `math.noise` (upstream `kPerlinHash`, 257 entries:
/// a 256-byte permutation with its first element repeated so the `+ 1` lookups
/// stay in bounds without a wrap).
#[rustfmt::skip]
const PERLIN_HASH: [u8; 257] = [
    151, 160, 137, 91, 90, 15, 131, 13, 201, 95, 96, 53, 194, 233, 7, 225, 140, 36, 103, 30, 69,
    142, 8, 99, 37, 240, 21, 10, 23, 190, 6, 148, 247, 120, 234, 75, 0, 26, 197, 62, 94, 252, 219,
    203, 117, 35, 11, 32, 57, 177, 33, 88, 237, 149, 56, 87, 174, 20, 125, 136, 171, 168, 68, 175,
    74, 165, 71, 134, 139, 48, 27, 166, 77, 146, 158, 231, 83, 111, 229, 122, 60, 211, 133, 230,
    220, 105, 92, 41, 55, 46, 245, 40, 244, 102, 143, 54, 65, 25, 63, 161, 1, 216, 80, 73, 209, 76,
    132, 187, 208, 89, 18, 169, 200, 196, 135, 130, 116, 188, 159, 86, 164, 100, 109, 198, 173, 186,
    3, 64, 52, 217, 226, 250, 124, 123, 5, 202, 38, 147, 118, 126, 255, 82, 85, 212, 207, 206, 59,
    227, 47, 16, 58, 17, 182, 189, 28, 42, 223, 183, 170, 213, 119, 248, 152, 2, 44, 154, 163, 70,
    221, 153, 101, 155, 167, 43, 172, 9, 129, 22, 39, 253, 19, 98, 108, 110, 79, 113, 224, 232, 178,
    185, 112, 104, 218, 246, 97, 228, 251, 34, 242, 193, 238, 210, 144, 12, 191, 179, 162, 241, 81,
    51, 145, 235, 249, 14, 239, 107, 49, 192, 214, 31, 181, 199, 106, 157, 184, 84, 204, 176, 115,
    121, 50, 45, 127, 4, 150, 254, 138, 236, 205, 93, 222, 114, 67, 29, 24, 72, 243, 141, 128, 195,
    78, 66, 215, 61, 156, 180, 151,
];

/// The 16 gradient directions for `math.noise` (upstream `kPerlinGrad`).
#[rustfmt::skip]
const PERLIN_GRAD: [[f32; 3]; 16] = [
    [1.0, 1.0, 0.0], [-1.0, 1.0, 0.0], [1.0, -1.0, 0.0], [-1.0, -1.0, 0.0],
    [1.0, 0.0, 1.0], [-1.0, 0.0, 1.0], [1.0, 0.0, -1.0], [-1.0, 0.0, -1.0],
    [0.0, 1.0, 1.0], [0.0, -1.0, 1.0], [0.0, 1.0, -1.0], [0.0, -1.0, -1.0],
    [1.0, 1.0, 0.0], [0.0, -1.0, 1.0], [-1.0, 1.0, 0.0], [0.0, -1.0, -1.0],
];

fn perlin_fade(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

fn perlin_lerp(t: f32, a: f32, b: f32) -> f32 {
    a + t * (b - a)
}

fn perlin_grad(hash: i32, x: f32, y: f32, z: f32) -> f32 {
    let g = PERLIN_GRAD[(hash & 15) as usize];
    g[0] * x + g[1] * y + g[2] * z
}

/// The Perlin kernel (upstream `perlin`), in `f32` throughout. The pinned VM
/// leaves `FixMathNoisePrecision` off, so large inputs are *not* folded to the
/// 256-unit period — the raw `f32` cast is matched deliberately.
fn perlin(x: f32, y: f32, z: f32) -> f32 {
    let xflr = x.floor();
    let yflr = y.floor();
    let zflr = z.floor();

    let xi = (xflr as i32) & 255;
    let yi = (yflr as i32) & 255;
    let zi = (zflr as i32) & 255;

    let xf = x - xflr;
    let yf = y - yflr;
    let zf = z - zflr;

    let fade_x = perlin_fade(xf);
    let fade_y = perlin_fade(yf);
    let fade_z = perlin_fade(zf);

    let hash = &PERLIN_HASH;
    let hash_at = |i: i32| i32::from(hash[i as usize]);

    let corner_a = (hash_at(xi) + yi) & 255;
    let aa = (hash_at(corner_a) + zi) & 255;
    let ab = (hash_at(corner_a + 1) + zi) & 255;

    let corner_b = (hash_at(xi + 1) + yi) & 255;
    let ba = (hash_at(corner_b) + zi) & 255;
    let bb = (hash_at(corner_b + 1) + zi) & 255;

    let la = perlin_lerp(
        fade_x,
        perlin_grad(hash_at(aa), xf, yf, zf),
        perlin_grad(hash_at(ba), xf - 1.0, yf, zf),
    );
    let lb = perlin_lerp(
        fade_x,
        perlin_grad(hash_at(ab), xf, yf - 1.0, zf),
        perlin_grad(hash_at(bb), xf - 1.0, yf - 1.0, zf),
    );
    let la1 = perlin_lerp(
        fade_x,
        perlin_grad(hash_at(aa + 1), xf, yf, zf - 1.0),
        perlin_grad(hash_at(ba + 1), xf - 1.0, yf, zf - 1.0),
    );
    let lb1 = perlin_lerp(
        fade_x,
        perlin_grad(hash_at(ab + 1), xf, yf - 1.0, zf - 1.0),
        perlin_grad(hash_at(bb + 1), xf - 1.0, yf - 1.0, zf - 1.0),
    );

    perlin_lerp(
        fade_z,
        perlin_lerp(fade_y, la, lb),
        perlin_lerp(fade_y, la1, lb1),
    )
}
