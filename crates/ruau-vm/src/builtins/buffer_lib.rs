use super::*;

pub(super) fn dispatch(
    builtin: Builtin,
    heap: &mut Heap,
    args: &[RawValue],
) -> Exec<Vec<RawValue>> {
    match builtin {
        Builtin::BufferCreate => buffer_create(heap, args),
        Builtin::BufferFromString => buffer_fromstring(heap, args),
        Builtin::BufferToString => buffer_tostring(heap, args),
        Builtin::BufferLen => buffer_len(heap, args),
        Builtin::BufferReadI8 => buffer_read_int(heap, args, 1, Signedness::Signed),
        Builtin::BufferReadU8 => buffer_read_int(heap, args, 1, Signedness::Unsigned),
        Builtin::BufferReadI16 => buffer_read_int(heap, args, 2, Signedness::Signed),
        Builtin::BufferReadU16 => buffer_read_int(heap, args, 2, Signedness::Unsigned),
        Builtin::BufferReadI32 => buffer_read_int(heap, args, 4, Signedness::Signed),
        Builtin::BufferReadU32 => buffer_read_int(heap, args, 4, Signedness::Unsigned),
        Builtin::BufferReadF32 => buffer_read_fp(heap, args, FloatWidth::F32),
        Builtin::BufferReadF64 => buffer_read_fp(heap, args, FloatWidth::F64),
        Builtin::BufferWriteI8 => buffer_write_int(heap, args, 1),
        Builtin::BufferWriteU8 => buffer_write_int(heap, args, 1),
        Builtin::BufferWriteI16 => buffer_write_int(heap, args, 2),
        Builtin::BufferWriteU16 => buffer_write_int(heap, args, 2),
        Builtin::BufferWriteI32 => buffer_write_int(heap, args, 4),
        Builtin::BufferWriteU32 => buffer_write_int(heap, args, 4),
        Builtin::BufferWriteF32 => buffer_write_fp(heap, args, FloatWidth::F32),
        Builtin::BufferWriteF64 => buffer_write_fp(heap, args, FloatWidth::F64),
        Builtin::BufferReadString => buffer_readstring(heap, args),
        Builtin::BufferWriteString => buffer_writestring(heap, args),
        Builtin::BufferReadBits => buffer_readbits(heap, args),
        Builtin::BufferWriteBits => buffer_writebits(heap, args),
        Builtin::BufferReadInteger => buffer_readinteger(heap, args),
        Builtin::BufferWriteInteger => buffer_writeinteger(heap, args),
        Builtin::BufferCopy => buffer_copy(heap, args),
        Builtin::BufferFill => buffer_fill(heap, args),
        _ => unreachable!("non-buffer builtin routed to buffer_lib"),
    }
}

/// A buffer argument.
fn arg_buffer(args: &[RawValue], index: usize, name: &str) -> Exec<RawGc<marker::Buffer>> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::Buffer(handle) => Ok(handle),
        _ => Err(err(format!(
            "bad argument #{} to 'buffer.{name}' (buffer expected)",
            index + 1
        ))),
    }
}

/// A required integer argument, truncated to a C `int` like `luaL_checkinteger`.
fn buffer_int_arg(args: &[RawValue], index: usize, name: &str) -> Exec<i32> {
    arg_int(args, index).map(|i| i as i32).ok_or_else(|| {
        err(format!(
            "bad argument #{} to 'buffer.{name}' (number expected)",
            index + 1
        ))
    })
}

/// An optional integer argument (`luaL_optinteger`); absent or `nil` is `default`.
fn buffer_opt_int(args: &[RawValue], index: usize, default: i32, name: &str) -> Exec<i32> {
    match args.get(index).copied() {
        None | Some(RawValue::Nil) => Ok(default),
        Some(RawValue::Number(n)) => Ok(n as i64 as i32),
        Some(RawValue::Integer(i)) => Ok(i as i32),
        Some(_) => Err(err(format!(
            "bad argument #{} to 'buffer.{name}' (number expected)",
            index + 1
        ))),
    }
}

/// A 32-bit unsigned value argument (`luaL_checkunsigned`): a number truncates
/// toward zero and reduces modulo 2^32.
fn buffer_uint_arg(args: &[RawValue], index: usize, name: &str) -> Exec<u32> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::Integer(i) => Ok(i as u32),
        RawValue::Number(n) => Ok(n.trunc().rem_euclid(4_294_967_296.0) as u32),
        _ => Err(err(format!(
            "bad argument #{} to 'buffer.{name}' (number expected)",
            index + 1
        ))),
    }
}

/// A floating-point value argument (`luaL_checknumber`).
fn buffer_num_arg(args: &[RawValue], index: usize, name: &str) -> Exec<f64> {
    num_arg(args, index, |index, _| {
        format!(
            "bad argument #{} to 'buffer.{name}' (number expected)",
            index + 1
        )
    })
}

/// A 64-bit integer value argument (`buffer.readinteger`/`writeinteger`).
fn buffer_long_arg(args: &[RawValue], index: usize, name: &str) -> Exec<i64> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::Number(n) => Ok(n as i64),
        RawValue::Integer(i) => Ok(i),
        _ => Err(err(format!(
            "bad argument #{} to 'buffer.{name}' (number expected)",
            index + 1
        ))),
    }
}

/// Upstream `isoutofbounds`: a byte access of `size` at `offset` is out of bounds
/// when `unsigned(offset) + size > len`. Reinterpreting a negative `offset` as a
/// huge unsigned value folds the negative-offset rejection into the same check.
fn buffer_oob(offset: i32, size: u64, len: usize) -> bool {
    u64::from(offset as u32) + size > len as u64
}

fn checked_buffer_span(
    heap: &Heap,
    handle: RawGc<marker::Buffer>,
    offset: i32,
    size: usize,
) -> Exec<&[u8]> {
    let buf = heap
        .buffer(handle)
        .ok_or_else(|| err("buffer not resident"))?;
    if buffer_oob(offset, size as u64, buf.len()) {
        return Err(err("buffer access out of bounds"));
    }
    let start = offset as u32 as usize;
    Ok(&buf.bytes()[start..start + size])
}

fn checked_buffer_span_mut(
    heap: &mut Heap,
    handle: RawGc<marker::Buffer>,
    offset: i32,
    size: usize,
) -> Exec<&mut [u8]> {
    let len = heap
        .buffer(handle)
        .ok_or_else(|| err("buffer not resident"))?
        .len();
    if buffer_oob(offset, size as u64, len) {
        return Err(err("buffer access out of bounds"));
    }
    let start = offset as u32 as usize;
    Ok(&mut heap
        .buffer_mut(handle)
        .ok_or_else(|| err("buffer not resident"))?
        .bytes_mut()[start..start + size])
}

fn buffer_string_arg<'h>(
    heap: &'h Heap,
    args: &[RawValue],
    index: usize,
    name: &str,
) -> Exec<&'h [u8]> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::String(handle) => Ok(heap.string(handle).map_or(&[], |s| s.bytes())),
        _ => Err(err(format!(
            "bad argument #{} to 'buffer.{name}' (string expected)",
            index + 1
        ))),
    }
}

fn check_transient_copy(heap: &Heap, len: usize) -> Exec<()> {
    if heap.would_exceed_cap(len) {
        return Err(err_memory_limit());
    }
    Ok(())
}

fn checked_byte_copy(heap: &Heap, bytes: &[u8], name: &str) -> Exec<Vec<u8>> {
    check_transient_copy(heap, bytes.len())?;
    let mut copy = Vec::new();
    copy.try_reserve_exact(bytes.len())
        .map_err(|_| err_memory(format!("out of memory for 'buffer.{name}'")))?;
    copy.extend_from_slice(bytes);
    Ok(copy)
}

fn checked_string_result_copy(heap: &Heap, bytes: &[u8], name: &str) -> Exec<Vec<u8>> {
    if bytes.len() > heap.limits().max_string_bytes {
        return Err(err(format!("buffer.{name} result too large")));
    }
    if heap.would_exceed_cap(bytes.len().saturating_mul(2)) {
        return Err(err_memory_limit());
    }
    checked_byte_copy(heap, bytes, name)
}

/// `buffer.create(size)`: a zero-filled buffer of `size` bytes.
fn buffer_create(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let size = buffer_int_arg(args, 0, "create")?;
    if size < 0 {
        return Err(err("invalid argument #1 to 'create' (size)"));
    }
    let size = size as usize;
    if size > heap.limits().max_buffer_bytes {
        return Err(err("buffer size exceeds the maximum"));
    }
    if heap.would_exceed_cap(size) {
        return Err(err_memory_limit());
    }
    let buffer = LuaBuffer::try_with_size(size)
        .map_err(|_| err_memory("out of memory for 'buffer.create'"))?;
    let handle = heap
        .alloc_buffer(buffer)
        .ok_or_else(|| err_memory("out of memory for 'buffer.create'"))?;
    Ok(vec![RawValue::Buffer(handle)])
}

/// `buffer.fromstring(s)`: a buffer initialized from a string's bytes.
fn buffer_fromstring(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let buffer = {
        let bytes = buffer_string_arg(heap, args, 0, "fromstring")?;
        if bytes.len() > heap.limits().max_buffer_bytes {
            return Err(err("buffer size exceeds the maximum"));
        }
        if heap.would_exceed_cap(bytes.len()) {
            return Err(err_memory_limit());
        }
        LuaBuffer::try_from_bytes(bytes)
            .map_err(|_| err_memory("out of memory for 'buffer.fromstring'"))?
    };
    let handle = heap
        .alloc_buffer(buffer)
        .ok_or_else(|| err_memory("out of memory for 'buffer.fromstring'"))?;
    Ok(vec![RawValue::Buffer(handle)])
}

/// `buffer.tostring(b)`: the buffer's bytes as a string.
fn buffer_tostring(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_buffer(args, 0, "tostring")?;
    let bytes = {
        let bytes = heap.buffer(handle).map_or(&[][..], LuaBuffer::bytes);
        checked_string_result_copy(heap, bytes, "tostring")?
    };
    let s = heap
        .intern_str(&bytes)
        .ok_or_else(|| err_memory("out of memory for 'buffer.tostring'"))?;
    Ok(vec![RawValue::String(s)])
}

/// `buffer.len(b)`: the buffer length in bytes.
fn buffer_len(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_buffer(args, 0, "len")?;
    let len = heap.buffer(handle).map_or(0, LuaBuffer::len);
    Ok(vec![RawValue::Number(len as f64)])
}

/// The little-endian integer reads (`readi8`..`readu32`): `width` bytes, sign-
/// extended when `signed`. The result is a `Number`, like the rest of the stdlib.
/// Whether a buffer integer access sign-extends the stored bytes.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Signedness {
    Signed,
    Unsigned,
}

/// Width of a buffer floating-point access.
#[derive(Clone, Copy, Eq, PartialEq)]
enum FloatWidth {
    F32,
    F64,
}

fn buffer_read_int(
    heap: &Heap,
    args: &[RawValue],
    width: usize,
    signedness: Signedness,
) -> Exec<Vec<RawValue>> {
    let handle = arg_buffer(args, 0, "read")?;
    let offset = buffer_int_arg(args, 1, "read")?;
    let mut raw = 0u64;
    for (i, &b) in checked_buffer_span(heap, handle, offset, width)?
        .iter()
        .enumerate()
    {
        raw |= u64::from(b) << (8 * i);
    }
    let value = match signedness {
        Signedness::Signed => {
            let shift = 64 - width * 8;
            (((raw << shift) as i64) >> shift) as f64
        }
        Signedness::Unsigned => raw as f64,
    };
    Ok(vec![RawValue::Number(value)])
}

/// The little-endian integer writes (`writei8`..`writeu32`): the value's low
/// `width` bytes (`luaL_checkunsigned` then truncation), little-endian.
fn buffer_write_int(heap: &mut Heap, args: &[RawValue], width: usize) -> Exec<Vec<RawValue>> {
    let handle = arg_buffer(args, 0, "write")?;
    let offset = buffer_int_arg(args, 1, "write")?;
    let value = buffer_uint_arg(args, 2, "write")?;
    let bytes = value.to_le_bytes();
    checked_buffer_span_mut(heap, handle, offset, width)?.copy_from_slice(&bytes[..width]);
    Ok(Vec::new())
}

/// `buffer.readf32`/`readf64`: a little-endian IEEE-754 read.
fn buffer_read_fp(heap: &Heap, args: &[RawValue], fp: FloatWidth) -> Exec<Vec<RawValue>> {
    let handle = arg_buffer(args, 0, "read")?;
    let offset = buffer_int_arg(args, 1, "read")?;
    let width = if fp == FloatWidth::F64 { 8 } else { 4 };
    let slice = checked_buffer_span(heap, handle, offset, width)?;
    let value = if fp == FloatWidth::F64 {
        f64::from_le_bytes(read_array(slice))
    } else {
        f64::from(f32::from_le_bytes(read_array(slice)))
    };
    Ok(vec![RawValue::Number(value)])
}

/// `buffer.writef32`/`writef64`: a little-endian IEEE-754 write.
fn buffer_write_fp(heap: &mut Heap, args: &[RawValue], fp: FloatWidth) -> Exec<Vec<RawValue>> {
    let handle = arg_buffer(args, 0, "write")?;
    let offset = buffer_int_arg(args, 1, "write")?;
    let value = buffer_num_arg(args, 2, "write")?;
    let width = if fp == FloatWidth::F64 { 8 } else { 4 };
    let out = checked_buffer_span_mut(heap, handle, offset, width)?;
    match fp {
        FloatWidth::F64 => out.copy_from_slice(&value.to_le_bytes()),
        FloatWidth::F32 => out.copy_from_slice(&(value as f32).to_le_bytes()),
    }
    Ok(Vec::new())
}

/// `buffer.readinteger(b, offset)`: a little-endian signed 64-bit integer.
fn buffer_readinteger(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_buffer(args, 0, "readinteger")?;
    let offset = buffer_int_arg(args, 1, "readinteger")?;
    let raw = i64::from_le_bytes(read_array(checked_buffer_span(heap, handle, offset, 8)?));
    Ok(vec![RawValue::Integer(raw)])
}

/// `buffer.writeinteger(b, offset, value)`: a little-endian 64-bit integer write.
fn buffer_writeinteger(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_buffer(args, 0, "writeinteger")?;
    let offset = buffer_int_arg(args, 1, "writeinteger")?;
    let value = buffer_long_arg(args, 2, "writeinteger")?;
    checked_buffer_span_mut(heap, handle, offset, 8)?.copy_from_slice(&value.to_le_bytes());
    Ok(Vec::new())
}

/// `buffer.readbits(b, bitOffset, bitCount)`: a little-endian bit-field read of
/// `bitCount` bits (0..=32) as an unsigned `Number`.
fn buffer_readbits(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_buffer(args, 0, "readbits")?;
    let bitoffset = buffer_long_arg(args, 1, "readbits")?;
    let bitcount = buffer_int_arg(args, 2, "readbits")?;
    let buf = heap
        .buffer(handle)
        .ok_or_else(|| err("buffer not resident"))?;
    let (startbyte, endbyte, subbyteoffset) = bit_range(bitoffset, bitcount, buf.len())?;
    let mut data = 0u64;
    for (i, &b) in buf.bytes()[startbyte..endbyte].iter().enumerate() {
        data |= u64::from(b) << (8 * i);
    }
    let mask = ((1u64 << (bitcount as u32)) - 1) << subbyteoffset;
    let result = (data & mask) >> subbyteoffset;
    Ok(vec![RawValue::Number(result as f64)])
}

/// `buffer.writebits(b, bitOffset, bitCount, value)`: a little-endian bit-field
/// write of the low `bitCount` bits (0..=32) of `value`.
fn buffer_writebits(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_buffer(args, 0, "writebits")?;
    let bitoffset = buffer_long_arg(args, 1, "writebits")?;
    let bitcount = buffer_int_arg(args, 2, "writebits")?;
    let value = buffer_uint_arg(args, 3, "writebits")?;
    let len = heap.buffer(handle).map_or(0, LuaBuffer::len);
    let (startbyte, endbyte, subbyteoffset) = bit_range(bitoffset, bitcount, len)?;
    let span = endbyte - startbyte;
    let buf = heap
        .buffer_mut(handle)
        .ok_or_else(|| err("buffer not resident"))?;
    let mut data = 0u64;
    for (i, &b) in buf.bytes()[startbyte..endbyte].iter().enumerate() {
        data |= u64::from(b) << (8 * i);
    }
    let mask = ((1u64 << (bitcount as u32)) - 1) << subbyteoffset;
    data = (data & !mask) | ((u64::from(value) << subbyteoffset) & mask);
    let bytes = data.to_le_bytes();
    buf.bytes_mut()[startbyte..endbyte].copy_from_slice(&bytes[..span]);
    Ok(Vec::new())
}

/// Validates a bit-field access and returns its `(startbyte, endbyte,
/// subbyteoffset)`, mirroring the shared prologue of upstream
/// `buffer_readbits`/`writebits`.
fn bit_range(bitoffset: i64, bitcount: i32, len: usize) -> Exec<(usize, usize, u32)> {
    if bitoffset < 0 {
        return Err(err("buffer access out of bounds"));
    }
    if bitcount as u32 > 32 {
        return Err(err("bit count is out of range of [0; 32]"));
    }
    if bitoffset as u64 + bitcount as u64 > len as u64 * 8 {
        return Err(err("buffer access out of bounds"));
    }
    let startbyte = (bitoffset / 8) as usize;
    let endbyte = ((bitoffset + i64::from(bitcount) + 7) / 8) as usize;
    let subbyteoffset = (bitoffset & 7) as u32;
    Ok((startbyte, endbyte, subbyteoffset))
}

/// `buffer.readstring(b, offset, size)`: `size` bytes as a string.
fn buffer_readstring(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_buffer(args, 0, "readstring")?;
    let offset = buffer_int_arg(args, 1, "readstring")?;
    let size = buffer_int_arg(args, 2, "readstring")?;
    if size < 0 {
        return Err(err("invalid argument #3 to 'readstring' (size)"));
    }
    let bytes = {
        let bytes = checked_buffer_span(heap, handle, offset, size as usize)?;
        checked_string_result_copy(heap, bytes, "readstring")?
    };
    let s = heap
        .intern_str(&bytes)
        .ok_or_else(|| err_memory("out of memory for 'buffer.readstring'"))?;
    Ok(vec![RawValue::String(s)])
}

/// `buffer.writestring(b, offset, s, count?)`: writes the first `count` bytes of
/// `s` (default: all of `s`).
fn buffer_writestring(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_buffer(args, 0, "writestring")?;
    let offset = buffer_int_arg(args, 1, "writestring")?;
    let value = buffer_string_arg(heap, args, 2, "writestring")?;
    let default_count = i32::try_from(value.len()).unwrap_or(i32::MAX);
    let count = buffer_opt_int(args, 3, default_count, "writestring")?;
    if count < 0 {
        return Err(err("invalid argument #4 to 'writestring' (count)"));
    }
    if count as usize > value.len() {
        return Err(err("string length overflow"));
    }
    let count = count as usize;
    let value = checked_byte_copy(heap, &value[..count], "writestring")?;
    checked_buffer_span_mut(heap, handle, offset, count)?.copy_from_slice(&value);
    Ok(Vec::new())
}

/// `buffer.copy(target, targetOffset, source, sourceOffset?, count?)`: copies
/// bytes between buffers (handling overlap when they are the same buffer).
fn buffer_copy(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let target = arg_buffer(args, 0, "copy")?;
    let toffset = buffer_int_arg(args, 1, "copy")?;
    let source = arg_buffer(args, 2, "copy")?;
    let soffset = buffer_opt_int(args, 3, 0, "copy")?;
    let slen = heap.buffer(source).map_or(0, LuaBuffer::len);
    // The default count is `slen - soffset`; compute it in i64 (and clamp) so a
    // `soffset` near i32::MIN cannot overflow the subtraction.
    let default =
        (slen as i64 - i64::from(soffset)).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    let size = buffer_opt_int(args, 4, default, "copy")?;
    if size < 0 {
        return Err(err("buffer access out of bounds"));
    }
    let size = size as usize;
    let target_len = heap.buffer(target).map_or(0, LuaBuffer::len);
    if buffer_oob(toffset, size as u64, target_len) {
        return Err(err("buffer access out of bounds"));
    }
    if target == source {
        let source_len = heap.buffer(source).map_or(0, LuaBuffer::len);
        if buffer_oob(soffset, size as u64, source_len) {
            return Err(err("buffer access out of bounds"));
        }
        let source_start = soffset as u32 as usize;
        let target_start = toffset as u32 as usize;
        heap.buffer_mut(target)
            .ok_or_else(|| err("buffer not resident"))?
            .bytes_mut()
            .copy_within(source_start..source_start + size, target_start);
        return Ok(Vec::new());
    }
    let chunk = {
        let bytes = checked_buffer_span(heap, source, soffset, size)?;
        checked_byte_copy(heap, bytes, "copy")?
    };
    checked_buffer_span_mut(heap, target, toffset, size)?.copy_from_slice(&chunk);
    Ok(Vec::new())
}

/// `buffer.fill(b, offset, value, count?)`: sets `count` bytes (default: to the
/// end) to the low byte of `value`.
fn buffer_fill(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_buffer(args, 0, "fill")?;
    let offset = buffer_int_arg(args, 1, "fill")?;
    let value = buffer_uint_arg(args, 2, "fill")?;
    let len = heap.buffer(handle).map_or(0, LuaBuffer::len);
    // Default count `len - offset` computed in i64 (and clamped) to avoid an
    // i32 subtraction overflow for an `offset` near i32::MIN.
    let default =
        (len as i64 - i64::from(offset)).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    let size = buffer_opt_int(args, 3, default, "fill")?;
    if size < 0 {
        return Err(err("buffer access out of bounds"));
    }
    let byte = (value & 0xff) as u8;
    checked_buffer_span_mut(heap, handle, offset, size as usize)?.fill(byte);
    Ok(Vec::new())
}
