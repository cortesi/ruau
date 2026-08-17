use super::*;

/// The bytes of a string-library argument: a string verbatim, a number rendered
/// (Lua coerces a number argument to a string).
pub(super) fn arg_bytes(heap: &Heap, args: &[RawValue], index: usize) -> Exec<Vec<u8>> {
    Ok(match arg_str(args, index)? {
        StrArg::Interned(handle) => heap
            .string(handle)
            .map_or_else(Vec::new, |s| s.bytes().to_vec()),
        StrArg::Coerced(bytes) => bytes,
    })
}

/// A string-or-number argument resolved without copying interned bytes:
/// the interned case stays a handle (re-borrow via [`StrArg::bytes`]), only
/// the number-coercion case allocates.
pub enum StrArg {
    Interned(RawGc<marker::Str>),
    Coerced(Vec<u8>),
}

impl StrArg {
    pub(crate) fn bytes<'h>(&'h self, heap: &'h Heap) -> &'h [u8] {
        match self {
            Self::Interned(handle) => heap.string(*handle).map_or(&[], |s| s.bytes()),
            Self::Coerced(bytes) => bytes,
        }
    }
}

/// [`StrArg`] form of [`arg_bytes`], with the same coercion and error rules.
pub(super) fn arg_str(args: &[RawValue], index: usize) -> Exec<StrArg> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::String(handle) => Ok(StrArg::Interned(handle)),
        RawValue::Number(n) => Ok(StrArg::Coerced(vmutils::number_to_string(n).into_bytes())),
        RawValue::Integer(i) => Ok(StrArg::Coerced(i.to_string().into_bytes())),
        _ => Err(err("bad argument to a string function (string expected)")),
    }
}

/// An integer argument, truncating a number as Lua does.
pub(super) fn arg_int(args: &[RawValue], index: usize) -> Option<i64> {
    match args.get(index).copied() {
        Some(RawValue::Number(n)) => Some(n as i64),
        Some(RawValue::Integer(i)) => Some(i),
        _ => None,
    }
}

/// A numeric argument, preserving the caller's library-specific diagnostic.
pub(super) fn num_arg(
    args: &[RawValue],
    index: usize,
    error: impl FnOnce(usize, RawValue) -> String,
) -> Exec<f64> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::Number(number) => Ok(number),
        RawValue::Integer(integer) => Ok(integer as f64),
        value => Err(err(error(index, value))),
    }
}

/// Lua's `posrelat`: a negative position counts from the end; out-of-range
/// negatives clamp to 0.
pub(super) fn posrelat(pos: i64, len: i64) -> i64 {
    if pos >= 0 {
        pos
    } else if -pos > len {
        0
    } else {
        len + pos + 1
    }
}

pub(super) fn intern_result(heap: &mut Heap, bytes: &[u8]) -> Exec<Vec<RawValue>> {
    let handle = heap
        .intern_str(bytes)
        .ok_or_else(|| err_memory("out of memory interning a string"))?;
    Ok(vec![RawValue::String(handle)])
}

/// Bounds an *incrementally* built string builtin's output buffer against the tenant's
/// string-size and memory caps as it grows. A builtin runs as one bytecode instruction, so
/// the dispatch safepoint never fires inside it; a builder whose output size is
/// data-dependent (`string.gsub`, where replacements can each be large and the match count
/// is unbounded) must therefore check inline, or a capped tenant could drive a large
/// transient `Rust` allocation past its cap before the final `intern_str` would notice.
/// `string.rep`, whose size is known up front, pre-charges instead. The `CONCAT`
/// bytecode op reuses this so `..` honors the same per-string size cap as the builders.
pub fn meter_string_growth(heap: &Heap, len: usize, what: &str) -> Exec<()> {
    if len > heap.limits().max_string_bytes {
        return Err(err(format!("{what} result too large")));
    }
    if heap.would_exceed_cap(len) {
        return Err(err_memory_limit());
    }
    Ok(())
}

/// Whether a value is truthy (anything but `nil`/`false`).
pub(super) fn is_truthy(value: RawValue) -> bool {
    !matches!(value, RawValue::Nil | RawValue::Boolean(false))
}

/// Reads `N` bytes from `data` (which must hold them) into a fixed array.
pub(super) fn read_array<const N: usize>(data: &[u8]) -> [u8; N] {
    let mut bytes = [0u8; N];
    bytes.copy_from_slice(&data[..N]);
    bytes
}

pub(super) fn string_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
