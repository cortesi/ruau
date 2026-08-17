use super::*;

pub(super) fn dispatch(builtin: Builtin, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    match builtin {
        Builtin::VectorCreate => vector_create(args),
        Builtin::VectorMagnitude => vector_magnitude(args),
        Builtin::VectorNormalize => vector_normalize(args),
        Builtin::VectorCross => vector_cross(args),
        Builtin::VectorDot => vector_dot(args),
        Builtin::VectorFloor => vector_unary(args, "floor", f32::floor),
        Builtin::VectorCeil => vector_unary(args, "ceil", f32::ceil),
        Builtin::VectorAbs => vector_unary(args, "abs", f32::abs),
        Builtin::VectorSign => vector_unary(args, "sign", vector_sign_f),
        Builtin::VectorLerp => vector_lerp(args),
        Builtin::VectorAngle => vector_angle(args),
        Builtin::VectorClamp => vector_clamp(args),
        Builtin::VectorMin => vector_reduce(args, "min", |b, r| b < r),
        Builtin::VectorMax => vector_reduce(args, "max", |b, r| b > r),
        _ => unreachable!("non-vector builtin routed to vector_lib"),
    }
}

/// A vector argument (`luaL_checkvector`).
fn arg_vector(args: &[RawValue], index: usize, name: &str) -> Exec<[f32; 3]> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::Vector(v) => Ok(v),
        RawValue::Nil => Err(err(format!(
            "missing argument #{} to '{name}' (vector expected)",
            index + 1
        ))),
        other => Err(err(format!(
            "invalid argument #{} to '{name}' (vector expected, got {})",
            index + 1,
            String::from_utf8_lossy(type_name(other))
        ))),
    }
}

/// A number argument to a `vector` function (`luaL_checknumber`).
fn vector_num(args: &[RawValue], index: usize, name: &str) -> Exec<f64> {
    num_arg(args, index, |index, value| match value {
        RawValue::Nil => format!(
            "missing argument #{} to '{name}' (number expected)",
            index + 1
        ),
        other => format!(
            "invalid argument #{} to '{name}' (number expected, got {})",
            index + 1,
            String::from_utf8_lossy(type_name(other))
        ),
    })
}

/// `vector.sign` component rule (upstream `luaui_signf`): 1/-1/0, with 0 for NaN.
fn vector_sign_f(v: f32) -> f32 {
    if v > 0.0 {
        1.0
    } else if v < 0.0 {
        -1.0
    } else {
        0.0
    }
}

/// `vector.create(x, y, z?)`: a 3-component vector (`z` defaults to zero).
fn vector_create(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let x = vector_num(args, 0, "create")?;
    let y = vector_num(args, 1, "create")?;
    let z = match args.get(2).copied() {
        None | Some(RawValue::Nil) => 0.0,
        Some(_) => vector_num(args, 2, "create")?,
    };
    Ok(vec![RawValue::Vector([x as f32, y as f32, z as f32])])
}

/// `vector.magnitude(v)`: the Euclidean length (computed in `f32`).
fn vector_magnitude(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let v = arg_vector(args, 0, "magnitude")?;
    let m = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    Ok(vec![RawValue::Number(f64::from(m))])
}

/// `vector.normalize(v)`: `v` scaled to unit length.
fn vector_normalize(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let v = arg_vector(args, 0, "normalize")?;
    let inv = 1.0_f32 / (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    Ok(vec![RawValue::Vector([v[0] * inv, v[1] * inv, v[2] * inv])])
}

/// `vector.cross(a, b)`: the cross product.
fn vector_cross(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let a = arg_vector(args, 0, "cross")?;
    let b = arg_vector(args, 1, "cross")?;
    Ok(vec![RawValue::Vector([
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ])])
}

/// `vector.dot(a, b)`: the dot product (a `Number`).
fn vector_dot(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let a = arg_vector(args, 0, "dot")?;
    let b = arg_vector(args, 1, "dot")?;
    let d = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    Ok(vec![RawValue::Number(f64::from(d))])
}

/// The component-wise unary vector ops (`floor`/`ceil`/`abs`/`sign`).
fn vector_unary(args: &[RawValue], name: &str, op: fn(f32) -> f32) -> Exec<Vec<RawValue>> {
    let v = arg_vector(args, 0, name)?;
    Ok(vec![RawValue::Vector([op(v[0]), op(v[1]), op(v[2])])])
}

/// `vector.lerp(a, b, t)`: component-wise lerp, returning `b` exactly at `t == 1`.
fn vector_lerp(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let a = arg_vector(args, 0, "lerp")?;
    let b = arg_vector(args, 1, "lerp")?;
    let t = vector_num(args, 2, "lerp")? as f32;
    let lerp = |a: f32, b: f32| if t == 1.0 { b } else { a + (b - a) * t };
    Ok(vec![RawValue::Vector([
        lerp(a[0], b[0]),
        lerp(a[1], b[1]),
        lerp(a[2], b[2]),
    ])])
}

/// `vector.angle(a, b, axis?)`: the unsigned angle between `a` and `b` (signed by
/// `axis` if given). The cross product is `f32`; the `atan2` is double, like
/// upstream `vector_angle`.
fn vector_angle(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let a = arg_vector(args, 0, "angle")?;
    let b = arg_vector(args, 1, "angle")?;
    let cross = [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ];
    let sin_a = f64::from(cross[0] * cross[0] + cross[1] * cross[1] + cross[2] * cross[2]).sqrt();
    let cos_a = f64::from(a[0] * b[0] + a[1] * b[1] + a[2] * b[2]);
    let mut angle = sin_a.atan2(cos_a);
    // An optional axis (`luaL_optvector`): absent/nil ⇒ unsigned, present must be
    // a vector and flips the sign when the cross faces away from it.
    match args.get(2).copied() {
        None | Some(RawValue::Nil) => {}
        Some(RawValue::Vector(axis)) => {
            if cross[0] * axis[0] + cross[1] * axis[1] + cross[2] * axis[2] < 0.0 {
                angle = -angle;
            }
        }
        Some(_) => return Err(err("bad argument #3 to 'vector.angle' (vector expected)")),
    }
    Ok(vec![RawValue::Number(angle)])
}

/// `vector.clamp(v, min, max)`: component-wise clamp; each `min` component must
/// be `<= max` (upstream `vector_clamp`).
fn vector_clamp(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let v = arg_vector(args, 0, "clamp")?;
    let min = arg_vector(args, 1, "clamp")?;
    let max = arg_vector(args, 2, "clamp")?;
    for (i, axis) in ["x", "y", "z"].iter().enumerate() {
        // A NaN bound is incomparable, so reject it explicitly (upstream's
        // `min <= max` argcheck also fails on NaN).
        if min[i] > max[i] || min[i].is_nan() || max[i].is_nan() {
            return Err(err(format!(
                "bad argument #3 to 'vector.clamp' (max.{axis} must be greater than or equal to min.{axis})"
            )));
        }
    }
    let clamp = |x: f32, lo: f32, hi: f32| {
        let r = if x < lo { lo } else { x };
        if r > hi { hi } else { r }
    };
    Ok(vec![RawValue::Vector([
        clamp(v[0], min[0], max[0]),
        clamp(v[1], min[1], max[1]),
        clamp(v[2], min[2], max[2]),
    ])])
}

/// The variadic component-wise reducers (`vector.min`/`vector.max`): `replace`
/// decides whether a candidate component supersedes the running result.
fn vector_reduce(
    args: &[RawValue],
    name: &str,
    replace: fn(f32, f32) -> bool,
) -> Exec<Vec<RawValue>> {
    let mut result = arg_vector(args, 0, name)?;
    for i in 1..args.len() {
        let b = arg_vector(args, i, name)?;
        for k in 0..3 {
            if replace(b[k], result[k]) {
                result[k] = b[k];
            }
        }
    }
    Ok(vec![RawValue::Vector(result)])
}
