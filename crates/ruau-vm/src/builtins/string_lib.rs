use super::*;

pub(super) fn dispatch(
    builtin: Builtin,
    heap: &mut Heap,
    thread: &mut Thread,
    args: &[RawValue],
    host_entry: crate::scope::HostEntry<'_>,
) -> Exec<Vec<RawValue>> {
    match builtin {
        Builtin::StringLen => string_len(heap, args),
        Builtin::StringSub => string_sub(heap, args),
        Builtin::StringRep => string_rep(heap, args),
        Builtin::StringUpper => string_map_case(heap, args, u8::to_ascii_uppercase),
        Builtin::StringLower => string_map_case(heap, args, u8::to_ascii_lowercase),
        Builtin::StringReverse => string_reverse(heap, args),
        Builtin::StringByte => string_byte(heap, args),
        Builtin::StringChar => string_char(heap, args),
        Builtin::StringFormat => string_format(heap, thread, args, host_entry),
        Builtin::StringFind => string_find(heap, args),
        Builtin::StringMatch => string_match(heap, args),
        Builtin::StringGmatch => string_gmatch(heap, args),
        Builtin::StringGmatchAux => string_gmatch_aux(heap, args),
        Builtin::StringGsub => string_gsub(heap, thread, args, host_entry),
        Builtin::StringSplit => string_split(heap, args),
        Builtin::StringPack => string_pack(heap, args),
        Builtin::StringPacksize => string_packsize(heap, args),
        Builtin::StringUnpack => string_unpack(heap, args),
        _ => unreachable!("non-string builtin routed to string_lib"),
    }
}

/// `string.len(s)`: the byte length. (Returns a number; the integer-tag audit
/// covers the length builtins together.)
fn string_len(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let len = arg_str(args, 0)?.bytes(heap).len();
    Ok(vec![RawValue::Number(len as f64)])
}

/// `string.sub(s, i, j?)`: the substring from `i` to `j`, 1-based with negative
/// positions from the end, Lua-clamped.
fn string_sub(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let source = arg_str(args, 0)?;
    let bytes = source.bytes(heap);
    let len = bytes.len() as i64;
    let mut i = posrelat(arg_int(args, 1).unwrap_or(1), len);
    let mut j = posrelat(arg_int(args, 2).unwrap_or(-1), len);
    if i < 1 {
        i = 1;
    }
    if j > len {
        j = len;
    }
    let sub = if i > j {
        Vec::new()
    } else {
        bytes[(i - 1) as usize..j as usize].to_vec()
    };
    intern_result(heap, &sub)
}

/// `string.rep(s, n)`: `s` repeated `n` times. The result grows through
/// `try_reserve` so a hostile count fails gracefully rather than aborting.
fn string_rep(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let bytes = arg_bytes(heap, args, 0)?;
    let count = usize::try_from(arg_int(args, 1).unwrap_or(0).max(0)).unwrap_or(0);
    let total = bytes
        .len()
        .checked_mul(count)
        .ok_or_else(|| err("string.rep result too large"))?;
    if total > heap.limits().max_string_bytes {
        return Err(err("string.rep result too large"));
    }
    // Pre-charge against the cap: a data-dependent output size must raise before
    // building the temporary, since the safepoint check runs only between
    // instructions and this whole builtin is one instruction.
    if heap.would_exceed_cap(total) {
        return Err(err_memory_limit());
    }
    // Pre-charge the budget for the `total` output bytes too: the memory cap bounds
    // the size but not the CPU to build it (and is a no-op under an unlimited memory
    // cap), so without this a gas-limited tenant could build a huge string for the
    // single tick the `CALL` charged.
    if !heap.charge_gas(total as u64) {
        return Err(err_gas());
    }
    let mut out = Vec::new();
    out.try_reserve(total)
        .map_err(|_| err_memory("not enough memory for 'string.rep'"))?;
    for _ in 0..count {
        out.extend_from_slice(&bytes);
    }
    intern_result(heap, &out)
}

/// `string.upper`/`string.lower`: ASCII case mapping.
fn string_map_case(heap: &mut Heap, args: &[RawValue], map: fn(&u8) -> u8) -> Exec<Vec<RawValue>> {
    let source = arg_str(args, 0)?;
    // One budget unit per mapped byte: the whole map is one bytecode instruction, so
    // without this an `O(len)` case-fold over a large string costs only the single
    // tick the `CALL` charged.
    if !heap.charge_gas(source.bytes(heap).len() as u64) {
        return Err(err_gas());
    }
    let mapped: Vec<u8> = source.bytes(heap).iter().map(map).collect();
    intern_result(heap, &mapped)
}

/// `string.reverse(s)`.
fn string_reverse(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let source = arg_str(args, 0)?;
    // One budget unit per reversed byte, as for the other `O(len)` string builders.
    if !heap.charge_gas(source.bytes(heap).len() as u64) {
        return Err(err_gas());
    }
    let mut bytes = source.bytes(heap).to_vec();
    bytes.reverse();
    intern_result(heap, &bytes)
}

/// `string.byte(s, i?, j?)`: the byte values from `i` to `j` (defaulting to `i`).
fn string_byte(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let source = arg_str(args, 0)?;
    let len = source.bytes(heap).len() as i64;
    let mut i = posrelat(arg_int(args, 1).unwrap_or(1), len);
    let mut j = posrelat(arg_int(args, 2).unwrap_or(i), len);
    if i < 1 {
        i = 1;
    }
    if j > len {
        j = len;
    }
    if i > j {
        return Ok(Vec::new());
    }
    // Charge the `j - i + 1` extracted bytes against the budget upfront: a wide
    // `string.byte(s, 1, #s)` over a large string returns one number per byte as a
    // single bytecode instruction, so without this its `O(n)` work costs only the
    // single tick the `CALL` charged.
    if !heap.charge_gas(u64::try_from(j - i + 1).unwrap_or(u64::MAX)) {
        return Err(err_gas());
    }
    Ok(source.bytes(heap)[(i - 1) as usize..j as usize]
        .iter()
        .map(|&b| RawValue::Number(f64::from(b)))
        .collect())
}

/// `string.char(...)`: a string from its byte-value arguments.
fn string_char(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    // Charge one budget unit per argument byte upfront: a wide
    // `string.char(table.unpack(huge))` builds an `O(n)` string as a single
    // bytecode instruction, so without this its work costs only the single tick the
    // `CALL` charged.
    if !heap.charge_gas(args.len() as u64) {
        return Err(err_gas());
    }
    let mut bytes = Vec::with_capacity(args.len());
    for (index, arg) in args.iter().enumerate() {
        let value = match *arg {
            RawValue::Number(n) => n as i64,
            RawValue::Integer(i) => i,
            _ => {
                return Err(err(format!(
                    "bad argument #{} to 'string.char' (number expected)",
                    index + 1
                )));
            }
        };
        let byte = u8::try_from(value)
            .map_err(|_| err("bad argument to 'string.char' (value out of range)"))?;
        bytes.push(byte);
    }
    intern_result(heap, &bytes)
}

/// A parsed `%` conversion specifier for [`string_format`].
#[derive(Default)]
struct FormatSpec {
    left: bool,
    plus: bool,
    space: bool,
    alt: bool,
    zero: bool,
    width: usize,
    precision: Option<usize>,
}

impl FormatSpec {
    fn has_modifiers(&self) -> bool {
        self.left
            || self.plus
            || self.space
            || self.alt
            || self.zero
            || self.width != 0
            || self.precision.is_some()
    }
}

/// `string.format(fmt, ...)`: a printf-style formatter. Supports the conversions
/// `d i u o x X c s q f F e E g G %` with the `- + space # 0` flags, width, and
/// precision. Output goes through the interner, so the cap governs the result; a
/// huge width pre-charges to raise before a giant padding allocation.
fn string_format(
    heap: &mut Heap,
    thread: &mut Thread,
    args: &[RawValue],
    host_entry: crate::scope::HostEntry<'_>,
) -> Exec<Vec<RawValue>> {
    let fmt = arg_bytes(heap, args, 0)?;
    let mut out: Vec<u8> = Vec::new();
    let mut arg_index = 1usize;
    let mut i = 0usize;
    while i < fmt.len() {
        let byte = fmt[i];
        if byte != b'%' {
            out.push(byte);
            i += 1;
            continue;
        }
        i += 1;
        let mut spec = FormatSpec::default();
        let mut seen_flags = 0u8;
        while i < fmt.len() {
            let (bit, apply): (u8, fn(&mut FormatSpec)) = match fmt[i] {
                b'-' => (1 << 0, |spec| spec.left = true),
                b'+' => (1 << 1, |spec| spec.plus = true),
                b' ' => (1 << 2, |spec| spec.space = true),
                b'#' => (1 << 3, |spec| spec.alt = true),
                b'0' => (1 << 4, |spec| spec.zero = true),
                _ => break,
            };
            if seen_flags & bit != 0 {
                return Err(err("invalid conversion to 'format' (repeated flags)"));
            }
            seen_flags |= bit;
            apply(&mut spec);
            i += 1;
        }
        // Width and precision are at most two digits each, as upstream
        // `scanformat` requires. This caps every conversion's output at a small
        // bound *regardless of the memory cap*, so a hostile `%.999999999f` cannot
        // build a multi-gigabyte temporary inside this single builtin (there is no
        // safepoint mid-builtin) before it can be metered.
        let mut width_digits = 0u32;
        while i < fmt.len() && fmt[i].is_ascii_digit() {
            spec.width = spec.width * 10 + (fmt[i] - b'0') as usize;
            width_digits += 1;
            i += 1;
        }
        if width_digits > 2 {
            return Err(err("invalid conversion to 'format' (width too long)"));
        }
        if i < fmt.len() && fmt[i] == b'.' {
            i += 1;
            let mut p = 0usize;
            let mut precision_digits = 0u32;
            while i < fmt.len() && fmt[i].is_ascii_digit() {
                p = p * 10 + (fmt[i] - b'0') as usize;
                precision_digits += 1;
                i += 1;
            }
            if precision_digits > 2 {
                return Err(err("invalid conversion to 'format' (precision too long)"));
            }
            spec.precision = Some(p);
        }
        let conv = *fmt
            .get(i)
            .ok_or_else(|| err("invalid conversion '%' to 'format' (no specifier)"))?;
        i += 1;
        if conv == b'%' {
            out.push(b'%');
            continue;
        }
        format_conversion(
            heap,
            thread,
            &mut out,
            conv,
            &spec,
            args,
            &mut arg_index,
            host_entry,
        )?;
        // The width/precision cap bounds a *single* conversion, but a `%s` copies its whole
        // (possibly large) argument and a hostile format can chain many conversions (e.g.
        // `string.format(("%s"):rep(n), table.unpack(strings))`), so the cumulative buffer
        // must be metered inline like `string.gsub` — there is no safepoint mid-builtin.
        meter_string_growth(heap, out.len(), "string.format")?;
    }
    intern_result(heap, &out)
}

/// Formats one conversion, consuming the next argument.
#[allow(
    clippy::too_many_arguments,
    reason = "the formatter keeps the explicit VM host entry beside its existing conversion state"
)]
fn format_conversion(
    heap: &mut Heap,
    thread: &mut Thread,
    out: &mut Vec<u8>,
    conv: u8,
    spec: &FormatSpec,
    args: &[RawValue],
    arg_index: &mut usize,
    host_entry: crate::scope::HostEntry<'_>,
) -> Exec<()> {
    let idx = *arg_index;
    *arg_index += 1;
    match conv {
        b'd' | b'i' => {
            let value = format_int_arg(args, idx)?;
            format_signed(out, spec, value);
        }
        b'u' => format_unsigned(out, spec, format_int_arg(args, idx)? as u64, 10, false, b""),
        b'o' => {
            let prefix: &[u8] = if spec.alt { b"0" } else { b"" };
            format_unsigned(
                out,
                spec,
                format_int_arg(args, idx)? as u64,
                8,
                false,
                prefix,
            );
        }
        b'x' => {
            let prefix: &[u8] = if spec.alt { b"0x" } else { b"" };
            format_unsigned(
                out,
                spec,
                format_int_arg(args, idx)? as u64,
                16,
                false,
                prefix,
            );
        }
        b'X' => {
            let prefix: &[u8] = if spec.alt { b"0X" } else { b"" };
            format_unsigned(
                out,
                spec,
                format_int_arg(args, idx)? as u64,
                16,
                true,
                prefix,
            );
        }
        b'c' => {
            let value = format_int_arg(args, idx)?;
            emit_padded(out, spec, &[], &[value as u8], false);
        }
        b's' => {
            let mut bytes = format_string_arg(heap, args, idx)?;
            if let Some(p) = spec.precision {
                bytes.truncate(p);
            }
            emit_padded(out, spec, &[], &bytes, false);
        }
        b'*' => {
            // `%*` (used by interpolated strings) appends the argument's default string
            // form, like upstream's `luaL_tolstring`. It takes no flags/width/precision and
            // does not pad.
            if spec.has_modifiers() {
                return Err(err("invalid conversion '%*' to 'format'"));
            }
            let value = args
                .get(idx)
                .copied()
                .ok_or_else(|| err("missing argument to 'format'"))?;
            let bytes = format_star_arg(heap, thread, value, host_entry)?;
            out.extend_from_slice(&bytes);
        }
        b'q' => format_quoted(heap, out, args, idx)?,
        b'f' | b'F' => format_float(out, spec, format_float_arg(args, idx)?, false, |x, p| {
            format!("{:.*}", p.unwrap_or(6), x)
        }),
        b'e' | b'E' => format_float(
            out,
            spec,
            format_float_arg(args, idx)?,
            conv == b'E',
            |x, p| format_e_body(x, p.unwrap_or(6)),
        ),
        b'g' | b'G' => {
            let alt = spec.alt;
            format_float(
                out,
                spec,
                format_float_arg(args, idx)?,
                conv == b'G',
                move |x, p| format_g_body(x, p.unwrap_or(6), alt),
            );
        }
        other => {
            return Err(err(format!(
                "invalid conversion '%{}' to 'format'",
                other as char
            )));
        }
    }
    Ok(())
}

fn format_star_arg(
    heap: &mut Heap,
    thread: &mut Thread,
    value: RawValue,
    host_entry: crate::scope::HostEntry<'_>,
) -> Exec<Vec<u8>> {
    match builtin_tostring(heap, thread, &[value], host_entry)?.as_slice() {
        [RawValue::String(handle)] => Ok(heap
            .string(*handle)
            .map_or_else(Vec::new, |string| string.bytes().to_vec())),
        _ => Err(err("tostring must return a string")),
    }
}

/// An integer argument for a numeric conversion.
fn format_int_arg(args: &[RawValue], index: usize) -> Exec<i64> {
    arg_int(args, index).ok_or_else(|| {
        err(format!(
            "bad argument #{index} to 'format' (number expected)"
        ))
    })
}

/// A float argument for a float conversion.
fn format_float_arg(args: &[RawValue], index: usize) -> Exec<f64> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::Number(n) => Ok(n),
        RawValue::Integer(i) => Ok(i as f64),
        _ => Err(err(format!(
            "bad argument #{index} to 'format' (number expected)"
        ))),
    }
}

/// A `%s`/`%q` argument as bytes. Like upstream `luaL_checklstring`, only a string
/// or a number (coerced) is accepted — a table/boolean/nil errors rather than
/// rendering its type name.
fn format_string_arg(heap: &Heap, args: &[RawValue], index: usize) -> Exec<Vec<u8>> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::String(handle) => Ok(heap
            .string(handle)
            .map_or_else(Vec::new, |s| s.bytes().to_vec())),
        RawValue::Number(n) => Ok(vmutils::number_to_string(n).into_bytes()),
        RawValue::Integer(i) => Ok(i.to_string().into_bytes()),
        other => Err(err(format!(
            "bad argument #{index} to 'format' (string expected, got {})",
            String::from_utf8_lossy(type_name(other))
        ))),
    }
}

/// Zero-pads `digits` to `precision` (a precision of zero renders a zero value as
/// the empty string, like C).
fn apply_int_precision(digits: &mut Vec<u8>, precision: Option<usize>, is_zero: bool) {
    if let Some(p) = precision {
        if p == 0 && is_zero {
            digits.clear();
        } else if digits.len() < p {
            let mut padded = vec![b'0'; p - digits.len()];
            padded.extend_from_slice(digits);
            *digits = padded;
        }
    }
}

fn format_signed(out: &mut Vec<u8>, spec: &FormatSpec, value: i64) {
    let mut digits = value.unsigned_abs().to_string().into_bytes();
    apply_int_precision(&mut digits, spec.precision, value == 0);
    let mut prefix = Vec::new();
    if value < 0 {
        prefix.push(b'-');
    } else if spec.plus {
        prefix.push(b'+');
    } else if spec.space {
        prefix.push(b' ');
    }
    emit_padded(out, spec, &prefix, &digits, spec.precision.is_none());
}

fn format_unsigned(
    out: &mut Vec<u8>,
    spec: &FormatSpec,
    bits: u64,
    base: u32,
    upper: bool,
    alt_prefix: &[u8],
) {
    let mut digits = match base {
        8 => format!("{bits:o}"),
        16 if upper => format!("{bits:X}"),
        16 => format!("{bits:x}"),
        _ => bits.to_string(),
    }
    .into_bytes();
    apply_int_precision(&mut digits, spec.precision, bits == 0);
    // The `0x`/`0X` prefix only appears for a non-zero value; octal `#` instead
    // forces a leading zero digit.
    let prefix: &[u8] = if bits != 0 && base == 16 {
        alt_prefix
    } else {
        b""
    };
    if base == 8 && !alt_prefix.is_empty() && digits.first() != Some(&b'0') {
        digits.insert(0, b'0');
    }
    emit_padded(out, spec, prefix, &digits, spec.precision.is_none());
}

/// Formats a float conversion: `body` renders the magnitude, then the sign and
/// padding are applied (`inf`/`nan` pad with spaces, never zeros).
fn format_float(
    out: &mut Vec<u8>,
    spec: &FormatSpec,
    value: f64,
    upper: bool,
    body: impl Fn(f64, Option<usize>) -> String,
) {
    let negative = value.is_sign_negative() && !value.is_nan();
    let magnitude = value.abs();
    let mut text = if magnitude.is_nan() {
        "nan".to_string()
    } else if magnitude.is_infinite() {
        "inf".to_string()
    } else {
        body(magnitude, spec.precision)
    };
    if upper {
        text = text.to_uppercase();
    }
    let mut prefix = Vec::new();
    if negative {
        prefix.push(b'-');
    } else if spec.plus {
        prefix.push(b'+');
    } else if spec.space {
        prefix.push(b' ');
    }
    emit_padded(out, spec, &prefix, text.as_bytes(), magnitude.is_finite());
}

/// `%e` body: a C-style exponent (a sign and at least two digits).
fn format_e_body(x: f64, precision: usize) -> String {
    let formatted = format!("{x:.precision$e}");
    if let Some(pos) = formatted.find('e') {
        let mantissa = &formatted[..pos];
        let exponent: i64 = formatted[pos + 1..].parse().unwrap_or(0);
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exponent.abs())
    } else {
        formatted
    }
}

/// `%g` body: shortest of `%e`/`%f` for the value, trailing zeros stripped unless
/// the `#` (alt) flag is set.
fn format_g_body(x: f64, precision: usize, alt: bool) -> String {
    let significant = precision.max(1);
    // The decimal exponent, taken from `%e` so it matches the rounded value.
    let probe = format!("{x:.*e}", significant - 1);
    let exponent: i64 = probe
        .find('e')
        .and_then(|pos| probe[pos + 1..].parse().ok())
        .unwrap_or(0);
    let mut text = if exponent < -4 || exponent >= significant as i64 {
        format_e_body(x, significant - 1)
    } else {
        let frac = (significant as i64 - 1 - exponent).max(0) as usize;
        format!("{x:.frac$}")
    };
    if !alt {
        text = strip_g_zeros(&text);
    }
    text
}

/// Strips trailing fractional zeros (and a now-bare decimal point) from a `%g`
/// rendering, leaving any exponent suffix intact.
fn strip_g_zeros(text: &str) -> String {
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(pos) => (&text[..pos], &text[pos..]),
        None => (text, ""),
    };
    let trimmed = if mantissa.contains('.') {
        mantissa.trim_end_matches('0').trim_end_matches('.')
    } else {
        mantissa
    };
    format!("{trimmed}{exponent}")
}

/// `%q`: the value quoted so Lua can read it back, matching the Luau
/// conformance format. Printable bytes are verbatim, common escapes use their
/// named form, and the remaining control bytes use decimal escapes.
fn format_quoted(heap: &Heap, out: &mut Vec<u8>, args: &[RawValue], index: usize) -> Exec<()> {
    let bytes = format_string_arg(heap, args, index)?;
    out.push(b'"');
    for (pos, &byte) in bytes.iter().enumerate() {
        match byte {
            b'"' | b'\\' | b'\n' => {
                out.push(b'\\');
                out.push(byte);
            }
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0 => out.extend_from_slice(b"\\000"),
            _ if byte.is_ascii_control() => {
                let next_is_digit = bytes.get(pos + 1).is_some_and(u8::is_ascii_digit);
                if next_is_digit {
                    out.extend_from_slice(format!("\\{byte:03}").as_bytes());
                } else {
                    out.extend_from_slice(format!("\\{byte}").as_bytes());
                }
            }
            _ => out.push(byte),
        }
    }
    out.push(b'"');
    Ok(())
}

/// Emits `prefix` then `digits`, padded to `spec.width`. `allow_zero` permits the
/// `0` flag to pad with zeros between the prefix and the digits.
fn emit_padded(
    out: &mut Vec<u8>,
    spec: &FormatSpec,
    prefix: &[u8],
    digits: &[u8],
    allow_zero: bool,
) {
    let content = prefix.len() + digits.len();
    if content >= spec.width {
        out.extend_from_slice(prefix);
        out.extend_from_slice(digits);
        return;
    }
    let pad = spec.width - content;
    if spec.left {
        out.extend_from_slice(prefix);
        out.extend_from_slice(digits);
        out.extend(std::iter::repeat_n(b' ', pad));
    } else if spec.zero && allow_zero {
        out.extend_from_slice(prefix);
        out.extend(std::iter::repeat_n(b'0', pad));
        out.extend_from_slice(digits);
    } else {
        out.extend(std::iter::repeat_n(b' ', pad));
        out.extend_from_slice(prefix);
        out.extend_from_slice(digits);
    }
}

/// Whether a pattern contains any special (non-literal) character — if not,
/// `string.find` can do a plain substring search. Mirrors upstream `SPECIALS`
/// (`^$*+?.([%-`); `]` and `)` are not special on their own (a lone `]`/`)` is a
/// literal in a plain search, matching upstream).
fn pattern_has_special(pat: &[u8]) -> bool {
    pat.iter().any(|b| {
        matches!(
            b,
            b'^' | b'$' | b'*' | b'+' | b'?' | b'.' | b'(' | b'[' | b'%' | b'-'
        )
    })
}

fn pattern_limits(heap: &Heap) -> pattern::PatternLimits {
    let limits = heap.limits();
    pattern::PatternLimits {
        max_steps: limits.max_pattern_steps,
        max_depth: limits.max_pattern_depth,
        max_captures: limits.max_pattern_captures,
    }
}

/// The bytes of a string/number value (a pattern/source argument).
fn value_bytes(heap: &Heap, value: RawValue) -> Exec<Vec<u8>> {
    match value {
        RawValue::String(handle) => Ok(heap
            .string(handle)
            .map_or_else(Vec::new, |s| s.bytes().to_vec())),
        RawValue::Number(n) => Ok(vmutils::number_to_string(n).into_bytes()),
        RawValue::Integer(i) => Ok(i.to_string().into_bytes()),
        _ => Err(err("bad argument to a string function (string expected)")),
    }
}

#[derive(Default)]
struct BoundedStringTableResult {
    entries: usize,
    bytes: usize,
    max_piece: usize,
    input_bytes: usize,
}

impl BoundedStringTableResult {
    fn observe_input(&mut self, len: usize) -> Exec<()> {
        self.input_bytes = self
            .input_bytes
            .checked_add(len)
            .ok_or_else(err_memory_limit)?;
        Ok(())
    }

    fn observe_piece(&mut self, len: usize) -> Exec<()> {
        self.entries = self.entries.checked_add(1).ok_or_else(err_memory_limit)?;
        self.bytes = self.bytes.checked_add(len).ok_or_else(err_memory_limit)?;
        self.max_piece = self.max_piece.max(len);
        Ok(())
    }

    fn observe_pieces(&mut self, entries: usize, bytes: usize, max_piece: usize) -> Exec<()> {
        self.entries = self
            .entries
            .checked_add(entries)
            .ok_or_else(err_memory_limit)?;
        self.bytes = self.bytes.checked_add(bytes).ok_or_else(err_memory_limit)?;
        self.max_piece = self.max_piece.max(max_piece);
        Ok(())
    }

    fn preflight_array_strings(&self, heap: &mut Heap, name: &str) -> Exec<()> {
        if self.entries > heap.limits().max_table_elements {
            return Err(err(format!("{name} result too large")));
        }
        if self.max_piece > heap.limits().max_string_bytes
            || self.bytes > heap.limits().max_string_bytes
        {
            return Err(err(format!("{name} result too large")));
        }
        let table_bytes =
            LuaTable::array_capacity_footprint(self.entries).ok_or_else(err_memory_limit)?;
        let interner_entry_bytes = self
            .entries
            .checked_mul(std::mem::size_of::<(Box<[u8]>, RawGc<marker::Str>)>())
            .ok_or_else(err_memory_limit)?;
        let string_bytes = self
            .bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(interner_entry_bytes))
            .ok_or_else(err_memory_limit)?;
        let transient_bytes = self
            .input_bytes
            .checked_add(self.max_piece)
            .ok_or_else(err_memory_limit)?;
        let footprint = table_bytes
            .checked_add(string_bytes)
            .and_then(|bytes| bytes.checked_add(transient_bytes))
            .ok_or_else(err_memory_limit)?;
        if heap.would_exceed_cap(footprint) {
            return Err(err_memory_limit());
        }
        let gas_units = self
            .entries
            .checked_add(self.bytes)
            .map_or(u64::MAX, |units| u64::try_from(units).unwrap_or(u64::MAX));
        if !heap.charge_gas(gas_units) {
            return Err(err_gas());
        }
        Ok(())
    }
}

fn charge_split_scan_gas(heap: &mut Heap, haystack_len: usize, needle_len: usize) -> Exec<()> {
    let scan_units = if needle_len == 0 {
        haystack_len
    } else if needle_len <= haystack_len {
        haystack_len
            .checked_sub(needle_len)
            .and_then(|span| span.checked_add(1))
            .and_then(|steps| steps.checked_mul(needle_len))
            .unwrap_or(usize::MAX)
    } else {
        0
    };
    let units = haystack_len
        .checked_add(needle_len)
        .and_then(|input| input.checked_add(scan_units))
        .map_or(u64::MAX, |units| u64::try_from(units).unwrap_or(u64::MAX));
    if !heap.charge_gas(units) {
        return Err(err_gas());
    }
    Ok(())
}

fn split_arg(args: &[RawValue], index: usize) -> Exec<StrArg> {
    arg_str(args, index)
}

fn split_separator_arg(args: &[RawValue]) -> Exec<StrArg> {
    match args.get(1).copied() {
        None | Some(RawValue::Nil) => Ok(StrArg::Coerced(b",".to_vec())),
        Some(_) => arg_str(args, 1),
    }
}

fn copy_split_arg(heap: &Heap, arg: StrArg, name: &str) -> Exec<Vec<u8>> {
    match arg {
        StrArg::Coerced(bytes) => Ok(bytes),
        StrArg::Interned(handle) => {
            let bytes = heap.string(handle).map_or(&[][..], |s| s.bytes());
            let mut out = Vec::new();
            out.try_reserve_exact(bytes.len())
                .map_err(|_| err_memory(format!("out of memory for '{name}'")))?;
            out.extend_from_slice(bytes);
            Ok(out)
        }
    }
}

fn split_result_plan(haystack: &[u8], needle: &[u8]) -> Exec<BoundedStringTableResult> {
    let mut plan = BoundedStringTableResult::default();
    plan.observe_input(haystack.len())?;
    plan.observe_input(needle.len())?;
    let len = haystack.len();
    let n = needle.len();
    if n == 0 {
        plan.observe_pieces(len, len, usize::from(len > 0))?;
        return Ok(plan);
    }
    let mut span_start = 0usize;
    let mut iter = 0usize;
    while n <= len && iter <= len - n {
        if haystack[iter..iter + n] == needle[..] {
            plan.observe_piece(iter - span_start)?;
            span_start = iter + n;
            iter += n - 1;
        }
        iter += 1;
    }
    plan.observe_piece(len - span_start)?;
    Ok(plan)
}

fn emit_split_piece(
    heap: &mut Heap,
    table: RawGc<marker::Table>,
    index: usize,
    bytes: &[u8],
) -> Exec<()> {
    let piece = heap
        .intern_str(bytes)
        .ok_or_else(|| err_memory("out of memory for 'string.split'"))?;
    set_index(heap, table, index as i64, RawValue::String(piece))
}

/// `string.split(s, sep?)`: splits `s` on the literal separator `sep` (default
/// `","`), returning an array of the pieces — a port of upstream `str_split`. An
/// empty separator splits into individual bytes.
fn string_split(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let haystack_arg = split_arg(args, 0)?;
    let needle_arg = split_separator_arg(args)?;
    let (haystack_len, needle_len) = {
        let haystack = haystack_arg.bytes(heap);
        let needle = needle_arg.bytes(heap);
        (haystack.len(), needle.len())
    };
    charge_split_scan_gas(heap, haystack_len, needle_len)?;
    let plan = {
        let haystack = haystack_arg.bytes(heap);
        let needle = needle_arg.bytes(heap);
        split_result_plan(haystack, needle)?
    };
    plan.preflight_array_strings(heap, "string.split")?;
    let haystack = copy_split_arg(heap, haystack_arg, "string.split")?;
    let needle = copy_split_arg(heap, needle_arg, "string.split")?;
    let table = heap
        .alloc_table(
            LuaTable::try_with_array_capacity(plan.entries)
                .map_err(|_| err_memory("out of memory for 'string.split'"))?,
        )
        .ok_or_else(|| err_memory("out of memory for 'string.split'"))?;
    let len = haystack.len();
    let n = needle.len();
    let mut count = 0usize;
    let mut span_start = 0usize;
    // An empty separator starts the scan one byte in (upstream `begin++`), so a
    // match at offset 0 doesn't emit a leading empty piece.
    let mut iter = usize::from(n == 0);
    while n <= len && iter <= len - n {
        if haystack[iter..iter + n] == needle[..] {
            count += 1;
            emit_split_piece(heap, table, count, &haystack[span_start..iter])?;
            span_start = iter + n;
            if n > 0 {
                iter += n - 1;
            }
        }
        iter += 1;
    }
    // A non-empty separator emits the trailing span; an empty one already covered
    // every byte.
    if n > 0 {
        count += 1;
        emit_split_piece(heap, table, count, &haystack[span_start..len])?;
    }
    Ok(vec![RawValue::Table(table)])
}

/// A numeric value argument to `string.pack` (`luaL_checknumber`).
fn pack_number_arg(args: &[RawValue], index: usize) -> Exec<f64> {
    num_arg(args, index, |_, _| {
        "bad argument to 'string.pack' (number expected)".to_owned()
    })
}

/// Appends little-endian bytes in the requested byte order.
fn push_endian(out: &mut Vec<u8>, le_bytes: &[u8], little: bool) {
    if little {
        out.extend_from_slice(le_bytes);
    } else {
        out.extend(le_bytes.iter().rev());
    }
}

/// `string.pack(fmt, ...)`: serializes the values per the binary format string.
fn string_pack(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let fmt_bytes = value_bytes(heap, args.first().copied().unwrap_or(RawValue::Nil))?;
    let mut header = pack::Header::new();
    let mut fmt = pack::Fmt::new(&fmt_bytes);
    let mut out: Vec<u8> = Vec::new();
    let mut total: i64 = 0;
    let mut arg = 1usize;
    while !fmt.at_end() {
        let (opt, size, ntoalign) = pack::getdetails(&mut header, total, &mut fmt)?;
        total += i64::from(ntoalign) + i64::from(size);
        if total as usize > heap.limits().max_pack_bytes {
            return Err(err("pack result too large"));
        }
        out.extend(std::iter::repeat_n(0u8, ntoalign as usize));
        match opt {
            pack::KOption::Int => {
                let n = pack_number_arg(args, arg)? as i64;
                if size < 8 {
                    let lim = 1i64 << (size * 8 - 1);
                    if !(-lim <= n && n < lim) {
                        return Err(err("integer overflow"));
                    }
                }
                pack::packint(&mut out, n as u64, header.little, size, n < 0);
                arg += 1;
            }
            pack::KOption::Uint => {
                let n = pack_number_arg(args, arg)? as i64;
                if size < 8 && (n as u64) >= (1u64 << (size as u32 * 8)) {
                    return Err(err("unsigned overflow"));
                }
                pack::packint(&mut out, n as u64, header.little, size, false);
                arg += 1;
            }
            pack::KOption::Float => {
                let n = pack_number_arg(args, arg)?;
                if size == 4 {
                    push_endian(&mut out, &(n as f32).to_le_bytes(), header.little);
                } else {
                    push_endian(&mut out, &n.to_le_bytes(), header.little);
                }
                arg += 1;
            }
            pack::KOption::Char => {
                let s = value_bytes(heap, args.get(arg).copied().unwrap_or(RawValue::Nil))?;
                if s.len() > size as usize {
                    return Err(err("string longer than given size"));
                }
                out.extend_from_slice(&s);
                out.extend(std::iter::repeat_n(0u8, size as usize - s.len()));
                arg += 1;
            }
            pack::KOption::Str => {
                let s = value_bytes(heap, args.get(arg).copied().unwrap_or(RawValue::Nil))?;
                if (size as usize) < 8 && s.len() as u64 >= (1u64 << (size as u32 * 8)) {
                    return Err(err("string length does not fit in given size"));
                }
                // Charge the payload against the absolute bound *before* appending,
                // so a huge string is rejected rather than first copied.
                total += s.len() as i64;
                if total as usize > heap.limits().max_pack_bytes {
                    return Err(err("pack result too large"));
                }
                pack::packint(&mut out, s.len() as u64, header.little, size, false);
                out.extend_from_slice(&s);
                arg += 1;
            }
            pack::KOption::Zstr => {
                let s = value_bytes(heap, args.get(arg).copied().unwrap_or(RawValue::Nil))?;
                if s.contains(&0) {
                    return Err(err("string contains zeros"));
                }
                total += s.len() as i64 + 1;
                if total as usize > heap.limits().max_pack_bytes {
                    return Err(err("pack result too large"));
                }
                out.extend_from_slice(&s);
                out.push(0);
                arg += 1;
            }
            pack::KOption::Padding => out.push(0),
            pack::KOption::PaddAlign | pack::KOption::Nop => {}
        }
    }
    let interned = heap
        .intern_str(&out)
        .ok_or_else(|| err_memory("out of memory for 'string.pack'"))?;
    Ok(vec![RawValue::String(interned)])
}

/// `string.packsize(fmt)`: the byte size of a fixed-format pack (no `s`/`z`).
fn string_packsize(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let fmt_bytes = value_bytes(heap, args.first().copied().unwrap_or(RawValue::Nil))?;
    let mut header = pack::Header::new();
    let mut fmt = pack::Fmt::new(&fmt_bytes);
    let mut total: i64 = 0;
    while !fmt.at_end() {
        let (opt, size, ntoalign) = pack::getdetails(&mut header, total, &mut fmt)?;
        if matches!(opt, pack::KOption::Str | pack::KOption::Zstr) {
            return Err(err("variable-length format"));
        }
        let add = i64::from(size) + i64::from(ntoalign);
        // `packsize` allocates nothing (it only returns the size), so it is bounded
        // by upstream's `MAXSSIZE`, not the tighter allocation-output cap.
        if total > pack::MAXSSIZE - add {
            return Err(err("format result too large"));
        }
        total += add;
    }
    Ok(vec![RawValue::Number(total as f64)])
}

/// `string.unpack(fmt, data, pos?)`: reads values per the format from `data`,
/// returning them followed by the next read position.
fn string_unpack(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let fmt_bytes = value_bytes(heap, args.first().copied().unwrap_or(RawValue::Nil))?;
    let data = value_bytes(heap, args.get(1).copied().unwrap_or(RawValue::Nil))?;
    let ld = data.len() as i64;
    let init = arg_int(args, 2).unwrap_or(1);
    let mut pos = posrelat(init, ld) - 1;
    if pos < 0 {
        pos = 0;
    }
    if pos > ld {
        return Err(err("initial position out of string"));
    }
    let mut header = pack::Header::new();
    let mut fmt = pack::Fmt::new(&fmt_bytes);
    let mut results: Vec<RawValue> = Vec::new();
    while !fmt.at_end() {
        // Bound the result count absolutely: a pathological format string would
        // otherwise grow an unmetered result vector without limit on a no-cap VM.
        if results.len() >= heap.limits().max_table_elements {
            return Err(err("too many results to unpack"));
        }
        let (opt, size, ntoalign) = pack::getdetails(&mut header, pos, &mut fmt)?;
        if i64::from(ntoalign) + i64::from(size) > ld - pos {
            return Err(err("data string too short"));
        }
        pos += i64::from(ntoalign);
        let at = pos as usize;
        match opt {
            pack::KOption::Int => {
                let res = pack::unpackint(&data[at..], header.little, size, true)?;
                results.push(RawValue::Number(res as f64));
            }
            pack::KOption::Uint => {
                let res = pack::unpackint(&data[at..], header.little, size, false)?;
                results.push(RawValue::Number((res as u64) as f64));
            }
            pack::KOption::Float => {
                let value = if size == 4 {
                    let bytes = read_endian::<4>(&data[at..], header.little);
                    f64::from(f32::from_le_bytes(bytes))
                } else {
                    f64::from_le_bytes(read_endian::<8>(&data[at..], header.little))
                };
                results.push(RawValue::Number(value));
            }
            pack::KOption::Char => {
                let s = heap
                    .intern_str(&data[at..at + size as usize])
                    .ok_or_else(|| err_memory("out of memory for 'string.unpack'"))?;
                results.push(RawValue::String(s));
            }
            pack::KOption::Str => {
                let len = pack::unpackint(&data[at..], header.little, size, false)? as i64;
                if len < 0 || len > ld - pos - i64::from(size) {
                    return Err(err("data string too short"));
                }
                let start = at + size as usize;
                let s = heap
                    .intern_str(&data[start..start + len as usize])
                    .ok_or_else(|| err_memory("out of memory for 'string.unpack'"))?;
                results.push(RawValue::String(s));
                pos += len;
            }
            pack::KOption::Zstr => {
                let rest = &data[at..];
                let Some(zlen) = rest.iter().position(|&b| b == 0) else {
                    return Err(err("unfinished string for format 'z'"));
                };
                let s = heap
                    .intern_str(&rest[..zlen])
                    .ok_or_else(|| err_memory("out of memory for 'string.unpack'"))?;
                results.push(RawValue::String(s));
                pos += zlen as i64 + 1;
            }
            pack::KOption::Padding | pack::KOption::PaddAlign | pack::KOption::Nop => {}
        }
        pos += i64::from(size);
    }
    results.push(RawValue::Number((pos + 1) as f64));
    Ok(results)
}

fn read_endian<const N: usize>(data: &[u8], little: bool) -> [u8; N] {
    let mut bytes = read_array(data);
    if !little {
        bytes.reverse();
    }
    bytes
}

/// Pushes one capture (a substring or a position) onto a result list.
fn push_capture(
    heap: &mut Heap,
    out: &mut Vec<RawValue>,
    src: &[u8],
    cap: pattern::Capture,
) -> Exec<()> {
    match cap {
        pattern::Capture::Position(p) => out.push(RawValue::Number(p as f64)),
        pattern::Capture::Bytes { start, len } => {
            let handle = heap
                .intern_str(&src[start..start + len])
                .ok_or_else(|| err_memory("out of memory interning a capture"))?;
            out.push(RawValue::String(handle));
        }
    }
    Ok(())
}

/// `string.find(s, pattern, init?, plain?)`.
fn string_find(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let src = arg_bytes(heap, args, 0)?;
    let pat = arg_bytes(heap, args, 1)?;
    let len = src.len() as i64;
    let init = posrelat(arg_int(args, 2).unwrap_or(1), len).max(1);
    if init > len + 1 {
        return Ok(vec![RawValue::Nil]);
    }
    let init0 = (init - 1) as usize;
    let plain = args.get(3).copied().is_some_and(is_truthy);
    if plain || !pattern_has_special(&pat) {
        if pat.is_empty() {
            return Ok(vec![
                RawValue::Number(init as f64),
                RawValue::Number((init - 1) as f64),
            ]);
        }
        let found = src[init0..]
            .windows(pat.len())
            .position(|window| window == pat.as_slice());
        return match found {
            Some(rel) => {
                let start = init0 + rel;
                Ok(vec![
                    RawValue::Number((start + 1) as f64),
                    RawValue::Number((start + pat.len()) as f64),
                ])
            }
            None => Ok(vec![RawValue::Nil]),
        };
    }
    let mut steps = 0u32;
    match pattern::find(&src, &pat, init0, &mut steps, pattern_limits(heap))? {
        Some(m) => {
            let mut out = vec![
                RawValue::Number((m.start + 1) as f64),
                RawValue::Number(m.end as f64),
            ];
            for cap in m.captures {
                push_capture(heap, &mut out, &src, cap)?;
            }
            Ok(out)
        }
        None => Ok(vec![RawValue::Nil]),
    }
}

/// `string.match(s, pattern, init?)`.
fn string_match(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let src = arg_bytes(heap, args, 0)?;
    let pat = arg_bytes(heap, args, 1)?;
    let len = src.len() as i64;
    let init = posrelat(arg_int(args, 2).unwrap_or(1), len).max(1);
    if init > len + 1 {
        return Ok(vec![RawValue::Nil]);
    }
    let mut steps = 0u32;
    match pattern::find(
        &src,
        &pat,
        (init - 1) as usize,
        &mut steps,
        pattern_limits(heap),
    )? {
        Some(m) => Ok(match_results(heap, &src, &m)?),
        None => Ok(vec![RawValue::Nil]),
    }
}

/// The results of a match: its captures, or the whole match when there are none.
fn match_results(heap: &mut Heap, src: &[u8], m: &pattern::MatchResult) -> Exec<Vec<RawValue>> {
    let mut out = Vec::new();
    if m.captures.is_empty() {
        let handle = heap
            .intern_str(&src[m.start..m.end])
            .ok_or_else(|| err_memory("out of memory interning a match"))?;
        out.push(RawValue::String(handle));
    } else {
        for &cap in &m.captures {
            push_capture(heap, &mut out, src, cap)?;
        }
    }
    Ok(out)
}

/// Resolves a string-library argument to one rooted interned string. Numeric
/// coercion is paid once when an iterator is created, not on every step.
fn interned_string_arg(
    heap: &mut Heap,
    args: &[RawValue],
    index: usize,
) -> Exec<RawGc<marker::Str>> {
    match arg_str(args, index)? {
        StrArg::Interned(handle) => Ok(handle),
        StrArg::Coerced(bytes) => heap
            .intern_str(&bytes)
            .ok_or_else(|| err_memory("out of memory for 'gmatch'")),
    }
}

/// Interns a range of an existing heap string after copying only that result
/// range across the mutable heap borrow.
fn intern_string_range(
    heap: &mut Heap,
    source: RawGc<marker::Str>,
    start: usize,
    end: usize,
) -> Exec<RawValue> {
    let bytes = heap
        .string(source)
        .map_or_else(Vec::new, |string| string.bytes()[start..end].to_vec());
    let handle = heap
        .intern_str(&bytes)
        .ok_or_else(|| err_memory("out of memory interning a match"))?;
    Ok(RawValue::String(handle))
}

/// Materializes only the returned ranges of a match against a rooted source.
fn match_results_from_string(
    heap: &mut Heap,
    source: RawGc<marker::Str>,
    matched: &pattern::MatchResult,
) -> Exec<Vec<RawValue>> {
    if matched.captures.is_empty() {
        return Ok(vec![intern_string_range(
            heap,
            source,
            matched.start,
            matched.end,
        )?]);
    }
    let mut out = Vec::with_capacity(matched.captures.len());
    for capture in &matched.captures {
        match *capture {
            pattern::Capture::Bytes { start, len } => {
                out.push(intern_string_range(heap, source, start, start + len)?);
            }
            pattern::Capture::Position(position) => {
                out.push(RawValue::Number(position as f64));
            }
        }
    }
    Ok(out)
}

/// `string.gmatch(s, pattern)`: an iterator over the matches. The iterator state
/// is a table `{interned_source, interned_pattern, pos}` the aux step advances.
fn string_gmatch(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let source = interned_string_arg(heap, args, 0)?;
    let pattern = interned_string_arg(heap, args, 1)?;
    let state = heap
        .alloc_table(LuaTable::new())
        .ok_or_else(|| err_memory("out of memory for 'gmatch'"))?;
    set_index(heap, state, 1, RawValue::String(source))?;
    set_index(heap, state, 2, RawValue::String(pattern))?;
    set_index(heap, state, 3, RawValue::Number(0.0))?;
    let aux = heap
        .alloc_builtin(Builtin::StringGmatchAux)
        .ok_or_else(|| err_memory("out of memory for 'gmatch'"))?;
    Ok(vec![
        RawValue::Function(aux),
        RawValue::Table(state),
        RawValue::Nil,
    ])
}

/// The `gmatch` iterator step.
fn string_gmatch_aux(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let RawValue::Table(state) = args.first().copied().unwrap_or(RawValue::Nil) else {
        return Ok(Vec::new());
    };
    let RawValue::String(source) = get_index(heap, state, 1) else {
        return Ok(Vec::new());
    };
    let RawValue::String(pattern) = get_index(heap, state, 2) else {
        return Ok(Vec::new());
    };
    let pos = match get_index(heap, state, 3) {
        RawValue::Number(n) if n >= 0.0 => n as usize,
        _ => 0,
    };
    let source_len = heap.string(source).map_or(0, |string| string.bytes().len());
    if pos > source_len {
        return Ok(Vec::new());
    }
    let mut steps = 0u32;
    let matched = {
        let source_bytes = heap.string(source).map_or(&[][..], |string| string.bytes());
        let pattern_bytes = heap
            .string(pattern)
            .map_or(&[][..], |string| string.bytes());
        pattern::find(
            source_bytes,
            pattern_bytes,
            pos,
            &mut steps,
            pattern_limits(heap),
        )?
    };
    match matched {
        Some(matched) => {
            let next = matched.end.max(matched.start + 1);
            set_index(heap, state, 3, RawValue::Number(next as f64))?;
            match_results_from_string(heap, source, &matched)
        }
        None => {
            set_index(heap, state, 3, RawValue::Number((source_len + 1) as f64))?;
            Ok(Vec::new())
        }
    }
}

/// `string.gsub(s, pattern, repl, n?)`: replaces matches, returning the result
/// and the replacement count.
fn string_gsub(
    heap: &mut Heap,
    thread: &mut Thread,
    args: &[RawValue],
    host_entry: crate::scope::HostEntry<'_>,
) -> Exec<Vec<RawValue>> {
    let src = arg_bytes(heap, args, 0)?;
    let pat = arg_bytes(heap, args, 1)?;
    let repl = args.get(2).copied().unwrap_or(RawValue::Nil);
    let max_n = arg_int(args, 3).unwrap_or(i64::MAX);
    let anchored = pat.first() == Some(&b'^');
    let pat_body = if anchored { &pat[1..] } else { &pat[..] };
    let mut out: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    let mut count = 0i64;
    // One budget across every match attempt of this gsub call.
    let mut steps = 0u32;
    while count < max_n {
        match pattern::match_at(&src, pat_body, pos, &mut steps, pattern_limits(heap))? {
            Some(m) => {
                count += 1;
                gsub_append(heap, thread, &mut out, &src, &m, repl, host_entry)?;
                // Meter the growing output inline: a replacement (especially a `__index`
                // table lookup or a function result) can be large and the match count is
                // unbounded, so the buffer must not outgrow the cap before the final intern.
                meter_string_growth(heap, out.len(), "string.gsub")?;
                if m.end > pos {
                    pos = m.end;
                } else {
                    if pos < src.len() {
                        out.push(src[pos]);
                    }
                    pos += 1;
                }
            }
            None => {
                if pos < src.len() {
                    out.push(src[pos]);
                    pos += 1;
                } else {
                    break;
                }
            }
        }
        if anchored || pos > src.len() {
            break;
        }
    }
    if pos < src.len() {
        out.extend_from_slice(&src[pos..]);
        meter_string_growth(heap, out.len(), "string.gsub")?;
    }
    let handle = heap
        .intern_str(&out)
        .ok_or_else(|| err_memory("out of memory for 'gsub'"))?;
    Ok(vec![
        RawValue::String(handle),
        RawValue::Number(count as f64),
    ])
}

/// The `idx`-th capture as a value (the whole match when there are no explicit
/// captures and `idx == 0`).
fn capture_value(
    heap: &mut Heap,
    src: &[u8],
    m: &pattern::MatchResult,
    idx: usize,
) -> Exec<RawValue> {
    if m.captures.is_empty() {
        if idx == 0 {
            return Ok(RawValue::String(
                heap.intern_str(&src[m.start..m.end])
                    .ok_or_else(|| err_memory("out of memory"))?,
            ));
        }
        return Err(err("invalid capture index in replacement"));
    }
    let cap = *m
        .captures
        .get(idx)
        .ok_or_else(|| err("invalid capture index in replacement"))?;
    match cap {
        pattern::Capture::Position(p) => Ok(RawValue::Number(p as f64)),
        pattern::Capture::Bytes { start, len } => Ok(RawValue::String(
            heap.intern_str(&src[start..start + len])
                .ok_or_else(|| err_memory("out of memory"))?,
        )),
    }
}

/// Appends a `gsub` replacement value, falling back to the whole match for a
/// `nil`/`false` result (the upstream contract).
fn append_repl_value(heap: &Heap, out: &mut Vec<u8>, value: RawValue, whole: &[u8]) -> Exec<()> {
    match value {
        RawValue::Nil | RawValue::Boolean(false) => out.extend_from_slice(whole),
        RawValue::String(handle) => {
            if let Some(s) = heap.string(handle) {
                out.extend_from_slice(s.bytes());
            }
        }
        RawValue::Number(n) => out.extend_from_slice(vmutils::number_to_string(n).as_bytes()),
        RawValue::Integer(i) => out.extend_from_slice(i.to_string().as_bytes()),
        _ => return Err(err("invalid replacement value (a string is expected)")),
    }
    Ok(())
}

/// Appends the replacement for one match (string template, table lookup, or
/// function call).
fn gsub_append(
    heap: &mut Heap,
    thread: &mut Thread,
    out: &mut Vec<u8>,
    src: &[u8],
    m: &pattern::MatchResult,
    repl: RawValue,
    host_entry: crate::scope::HostEntry<'_>,
) -> Exec<()> {
    let whole = &src[m.start..m.end];
    match repl {
        RawValue::String(_) | RawValue::Number(_) | RawValue::Integer(_) => {
            let template = value_bytes(heap, repl)?;
            let mut i = 0;
            while i < template.len() {
                let c = template[i];
                if c == b'%' && i + 1 < template.len() {
                    let d = template[i + 1];
                    if d == b'%' {
                        out.push(b'%');
                    } else if d == b'0' {
                        out.extend_from_slice(whole);
                    } else if d.is_ascii_digit() {
                        let value = capture_value(heap, src, m, (d - b'1') as usize)?;
                        append_repl_value(heap, out, value, whole)?;
                    } else {
                        return Err(err("invalid use of '%' in replacement string"));
                    }
                    i += 2;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
        }
        RawValue::Table(table) => {
            let key = capture_value(heap, src, m, 0)?;
            // Metatable-aware lookup, like upstream's `lua_gettable`: a gsub
            // replacement table may answer through `__index`.
            let value =
                crate::execute::index_value(heap, thread, RawValue::Table(table), key, host_entry)?;
            append_repl_value(heap, out, value, whole)?;
        }
        RawValue::Function(_) => {
            let mut call_args = Vec::new();
            let n = m.captures.len().max(1);
            for idx in 0..n {
                call_args.push(capture_value(heap, src, m, idx)?);
            }
            let results = call_value(heap, thread, repl, &call_args, host_entry)?;
            let value = results.into_iter().next().unwrap_or(RawValue::Nil);
            append_repl_value(heap, out, value, whole)?;
        }
        _ => {
            return Err(err(
                "bad argument #3 to 'gsub' (string/function/table expected)",
            ));
        }
    }
    Ok(())
}
