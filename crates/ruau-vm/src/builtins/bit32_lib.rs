use super::*;

/// Converts a `bit32` argument to a 32-bit unsigned value, truncating toward zero
/// and reducing modulo 2^32 (`luaL_checkunsigned`).
pub(super) fn bit_arg(args: &[RawValue], index: usize) -> Exec<u32> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::Integer(i) => Ok(i as u32),
        RawValue::Number(n) => Ok(n.trunc().rem_euclid(4_294_967_296.0) as u32),
        _ => Err(err("bad argument to a bit32 function (number expected)")),
    }
}

/// The shift displacement argument, clamped to `[-32, 32]`. Any larger magnitude
/// already yields zero (or a sign fill for `arshift`), and clamping is what makes
/// negating the displacement (`rshift`/`arshift`) safe — an unclamped `i64::MIN`
/// would overflow on negation and panic.
pub(super) fn shift_disp(args: &[RawValue]) -> Exec<i64> {
    let disp = arg_int(args, 1)
        .ok_or_else(|| err("bad argument #2 to a bit32 shift (number expected)"))?;
    Ok(disp.clamp(-32, 32))
}

/// A `bit32` result is the unsigned 32-bit value as a `number`, matching upstream.
pub(super) fn bit32_result(value: u32) -> RawValue {
    RawValue::Number(f64::from(value))
}

/// Folds `band`/`bor`/`bxor` over their unsigned arguments from `identity`.
pub(super) fn bit32_reduce(
    args: &[RawValue],
    identity: u32,
    op: fn(u32, u32) -> u32,
) -> Exec<Vec<RawValue>> {
    let mut acc = identity;
    for index in 0..args.len() {
        acc = op(acc, bit_arg(args, index)?);
    }
    Ok(vec![bit32_result(acc)])
}

/// Logical shift: `disp >= 0` shifts left, `disp < 0` shifts right; a magnitude of
/// 32 or more yields zero (upstream `bit32.lshift`/`rshift`).
pub(super) fn shift_logical(value: u32, disp: i64) -> u32 {
    if disp <= -32 || disp >= 32 {
        0
    } else if disp >= 0 {
        value << disp
    } else {
        value >> (-disp)
    }
}

/// Arithmetic right shift: sign-extends the 32-bit value; a negative displacement
/// shifts left (upstream `bit32.arshift`).
pub(super) fn shift_arith(value: u32, disp: i64) -> u32 {
    if disp < 0 {
        return shift_logical(value, -disp);
    }
    if disp >= 32 {
        // Saturate to the sign fill, since `i32 >> 32` is undefined.
        if value & 0x8000_0000 != 0 {
            0xFFFF_FFFF
        } else {
            0
        }
    } else {
        ((value as i32) >> disp) as u32
    }
}

/// The rotation amount in `[0, 32)` — `rem_euclid(32)` reduces any displacement
/// (and a left rotation by a negative amount is a right rotation), overflow-free.
pub(super) fn rotate_amount(args: &[RawValue]) -> Exec<u32> {
    let disp = arg_int(args, 1)
        .ok_or_else(|| err("bad argument #2 to a bit32 rotate (number expected)"))?;
    Ok(disp.rem_euclid(32) as u32)
}

/// The `(field, width)` of `bit32.extract`/`replace`, validated to a real bit
/// range (`field >= 0`, `width >= 1`, `field + width <= 32`).
pub(super) fn bit_field(args: &[RawValue], field_index: usize) -> Exec<(u32, u32)> {
    let field = arg_int(args, field_index)
        .ok_or_else(|| err("bad argument to a bit32 field op (number expected)"))?;
    let width = arg_int(args, field_index + 1).unwrap_or(1);
    if field < 0 || width < 1 || field + width > 32 {
        return Err(err("trying to access non-existent bits"));
    }
    Ok((field as u32, width as u32))
}

/// `bit32.extract(n, field, width?)`: the `width` bits of `n` starting at `field`.
pub(super) fn bit32_extract(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let value = bit_arg(args, 0)?;
    let (field, width) = bit_field(args, 1)?;
    let mask = (1u64 << width) - 1;
    Ok(vec![bit32_result(((value >> field) as u64 & mask) as u32)])
}

/// `bit32.replace(n, v, field, width?)`: `n` with its `width` bits at `field`
/// replaced by the low `width` bits of `v`.
pub(super) fn bit32_replace(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let value = bit_arg(args, 0)?;
    let replacement = bit_arg(args, 1)?;
    let (field, width) = bit_field(args, 2)?;
    let mask = ((1u64 << width) - 1) as u32;
    let cleared = value & !(mask << field);
    Ok(vec![bit32_result(
        cleared | ((replacement & mask) << field),
    )])
}
