use super::*;

pub(super) fn dispatch(
    builtin: Builtin,
    heap: &mut Heap,
    args: &[RawValue],
) -> Exec<Vec<RawValue>> {
    match builtin {
        Builtin::Utf8Char => utf8_char(heap, args),
        Builtin::Utf8Codepoint => utf8_codepoint(heap, args),
        Builtin::Utf8Len => utf8_len(heap, args),
        Builtin::Utf8Offset => utf8_offset(heap, args),
        Builtin::Utf8Codes => utf8_codes(heap, args),
        Builtin::Utf8CodesAux => utf8_codes_aux(heap, args),
        _ => unreachable!("non-utf8 builtin routed to utf8_lib"),
    }
}

/// Whether `byte` is a UTF-8 continuation byte (`10xxxxxx`).
fn is_cont(byte: u8) -> bool {
    byte & 0xC0 == 0x80
}

/// Decodes one codepoint at byte `pos`, returning `(codepoint, byte_len)` for a
/// valid sequence — Luau's strict `utf8_decode`: 1–4 bytes, no overlong encoding,
/// no value above `0x10FFFF`.
fn utf8_decode(bytes: &[u8], pos: usize) -> Option<(u32, usize)> {
    const LIMITS: [u32; 4] = [0xFF, 0x7F, 0x7FF, 0xFFFF];
    let first = u32::from(*bytes.get(pos)?);
    if first < 0x80 {
        return Some((first, 1));
    }
    let mut res = 0u32;
    let mut count = 0usize;
    let mut lead = first;
    while lead & 0x40 != 0 {
        count += 1;
        // A sequence longer than four bytes is invalid; reject before the
        // `count * 5` shift below can exceed `u32`'s width (a panic).
        if count > 3 {
            return None;
        }
        let cont = u32::from(*bytes.get(pos + count)?);
        if cont & 0xC0 != 0x80 {
            return None;
        }
        res = (res << 6) | (cont & 0x3F);
        lead <<= 1;
    }
    res |= (lead & 0x7F) << (count * 5);
    // Reject overlong encodings, out-of-range values, and UTF-16 surrogates, like
    // the pinned `utf8_decode` (`utf8.char` stays permissive; decoding is strict).
    if res > 0x10FFFF || res <= LIMITS[count] || (0xD800..=0xDFFF).contains(&res) {
        return None;
    }
    Some((res, count + 1))
}

/// Encodes `cp` as UTF-8 into `out` (up to `0x10FFFF`; surrogates pass through,
/// as upstream `utf8.char`).
fn utf8_encode(cp: u32, out: &mut Vec<u8>) {
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.push(0xC0 | (cp >> 6) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else if cp < 0x10000 {
        out.push(0xE0 | (cp >> 12) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else {
        out.push(0xF0 | (cp >> 18) as u8);
        out.push(0x80 | ((cp >> 12) & 0x3F) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    }
}

/// `utf8.char(...)`: the UTF-8 string of the given codepoints.
fn utf8_char(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    // One budget unit per encoded codepoint argument: the whole encode is one
    // bytecode instruction, so without this an `O(n)` build costs only the single
    // tick the `CALL` charged.
    if !heap.charge_gas(args.len() as u64) {
        return Err(err_gas());
    }
    let mut out = Vec::new();
    for index in 0..args.len() {
        let cp = arg_int(args, index).ok_or_else(|| {
            err(format!(
                "bad argument #{} to 'utf8.char' (number expected)",
                index + 1
            ))
        })?;
        if !(0..=0x10_FFFF).contains(&cp) {
            return Err(err(format!(
                "bad argument #{} to 'utf8.char' (value out of range)",
                index + 1
            )));
        }
        utf8_encode(cp as u32, &mut out);
    }
    intern_result(heap, &out)
}

/// `utf8.codepoint(s, i?, j?)`: the codepoints whose first byte is in `[i, j]`.
fn utf8_codepoint(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let s = arg_bytes(heap, args, 0)?;
    let len = s.len() as i64;
    let i = posrelat(arg_int(args, 1).unwrap_or(1), len);
    let j = posrelat(arg_int(args, 2).unwrap_or(i), len);
    if i < 1 {
        // Upstream `codepoint` spells these "out of range" (`luaL_argcheck`).
        return Err(err("bad argument #2 to 'utf8.codepoint' (out of range)"));
    }
    if j > len {
        return Err(err("bad argument #3 to 'utf8.codepoint' (out of range)"));
    }
    let mut out = Vec::new();
    let mut pos = (i - 1) as usize;
    let end = j as usize;
    // Charge the decoded byte span upfront: decoding scans `end - pos` bytes as one
    // bytecode instruction, so without this an `O(span)` decode costs only the single
    // tick the `CALL` charged.
    if !heap.charge_gas((end.saturating_sub(pos)) as u64) {
        return Err(err_gas());
    }
    while pos < end {
        let (cp, n) = utf8_decode(&s, pos).ok_or_else(|| err("invalid UTF-8 code"))?;
        out.push(RawValue::Number(f64::from(cp)));
        pos += n;
    }
    Ok(out)
}

/// `utf8.len(s, i?, j?)`: the codepoint count of `s[i..=j]`, or `(nil, badpos)` at
/// the first byte that does not start a valid sequence.
fn utf8_len(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let s = arg_bytes(heap, args, 0)?;
    let len = s.len() as i64;
    let i = posrelat(arg_int(args, 1).unwrap_or(1), len);
    let j = posrelat(arg_int(args, 2).unwrap_or(-1), len);
    if i < 1 || i > len + 1 {
        // Upstream `utflen` spells these "out of string" (`luaL_argcheck`).
        return Err(err(
            "bad argument #2 to 'utf8.len' (initial position out of string)",
        ));
    }
    if j > len {
        return Err(err(
            "bad argument #3 to 'utf8.len' (final position out of string)",
        ));
    }
    let mut count = 0i64;
    let mut pos = (i - 1) as usize;
    let end = j as usize;
    // Charge the scanned byte span upfront, as for `utf8.codepoint`.
    if !heap.charge_gas((end.saturating_sub(pos)) as u64) {
        return Err(err_gas());
    }
    while pos < end {
        match utf8_decode(&s, pos) {
            Some((_, n)) => {
                pos += n;
                count += 1;
            }
            None => return Ok(vec![RawValue::Nil, RawValue::Number((pos + 1) as f64)]),
        }
    }
    Ok(vec![RawValue::Number(count as f64)])
}

/// `utf8.offset(s, n, i?)`: the byte position of the `n`-th codepoint counted from
/// byte `i` (negative `n` counts backward; `n == 0` finds the start of the
/// codepoint containing byte `i`), or `nil` if it falls outside the string.
fn utf8_offset(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let s = arg_bytes(heap, args, 0)?;
    let len = s.len() as i64;
    let mut n = arg_int(args, 1)
        .ok_or_else(|| err("bad argument #2 to 'utf8.offset' (number expected)"))?;
    let default_i = if n >= 0 { 1 } else { len + 1 };
    let i = posrelat(arg_int(args, 2).unwrap_or(default_i), len);
    if i < 1 || i > len + 1 {
        // Upstream `byteoffset` spells this "position out of range" (`luaL_argcheck`).
        return Err(err(
            "bad argument #3 to 'utf8.offset' (position out of range)",
        ));
    }
    let mut p = (i - 1) as i64; // 0-based
    // A non-zero count must start on a codepoint boundary, like upstream.
    if n != 0 && p < len && is_cont(s[p as usize]) {
        return Err(err(
            "bad argument #3 to 'utf8.offset' (initial position is a continuation byte)",
        ));
    }
    match n.cmp(&0) {
        std::cmp::Ordering::Equal => {
            // `i == len + 1` gives `p == len`, one past the end; guard the index.
            while p > 0 && p < len && is_cont(s[p as usize]) {
                p -= 1;
            }
        }
        std::cmp::Ordering::Greater => {
            n -= 1;
            // One budget unit per codepoint advanced: a large `n` scans forward over
            // the string as one bytecode instruction, so this bounds that `O(len)` walk.
            while n > 0 && p < len {
                if !heap.tick_gas() {
                    return Err(err_gas());
                }
                p += 1;
                while p < len && is_cont(s[p as usize]) {
                    p += 1;
                }
                n -= 1;
            }
            if n > 0 {
                return Ok(vec![RawValue::Nil]);
            }
        }
        std::cmp::Ordering::Less => {
            // One budget unit per codepoint walked backward, as for the forward case.
            while n < 0 && p > 0 {
                if !heap.tick_gas() {
                    return Err(err_gas());
                }
                p -= 1;
                while p > 0 && is_cont(s[p as usize]) {
                    p -= 1;
                }
                n += 1;
            }
            if n < 0 {
                return Ok(vec![RawValue::Nil]);
            }
        }
    }
    Ok(vec![RawValue::Number((p + 1) as f64)])
}

/// `utf8.codes(s)`: the iterator triple `(aux, s, 0)` for `for p, c in utf8.codes(s)`.
fn utf8_codes(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let s = match args.first().copied().unwrap_or(RawValue::Nil) {
        value @ RawValue::String(_) => value,
        _ => return Err(err("bad argument #1 to 'utf8.codes' (string expected)")),
    };
    let aux = heap
        .alloc_builtin(Builtin::Utf8CodesAux)
        .ok_or_else(|| err_memory("out of memory for 'utf8.codes'"))?;
    Ok(vec![RawValue::Function(aux), s, RawValue::Number(0.0)])
}

/// The `utf8.codes` iterator step: given the previous codepoint's 1-based byte
/// position (`0` to start), yields the next `(position, codepoint)` or nothing.
fn utf8_codes_aux(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let s = arg_bytes(heap, args, 0)?;
    let prev = arg_int(args, 1).unwrap_or(0);
    // Advance past the previous codepoint: from the byte after its lead, skip
    // continuation bytes to reach the next codepoint's start.
    let pos = if prev <= 0 {
        0usize
    } else {
        let mut i = prev as usize; // byte after the previous lead
        while i < s.len() && is_cont(s[i]) {
            i += 1;
        }
        i
    };
    if pos >= s.len() {
        return Ok(Vec::new());
    }
    let (cp, used) = utf8_decode(&s, pos).ok_or_else(|| err("invalid UTF-8 code"))?;
    // A continuation byte immediately after a complete codepoint is a spurious
    // continuation, which `utf8.codes` rejects — upstream `iter_aux`'s
    // `iscont(next)` check. Without it a stray `0x80`–`0xBF` between codepoints
    // (e.g. "in\x80valid") would be silently skipped instead of raising.
    if pos + used < s.len() && is_cont(s[pos + used]) {
        return Err(err("invalid UTF-8 code"));
    }
    Ok(vec![
        RawValue::Number((pos + 1) as f64),
        RawValue::Number(f64::from(cp)),
    ])
}
