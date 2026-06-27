use super::*;

pub(super) fn dispatch(
    builtin: Builtin,
    call_site: BuiltinCallSite,
    heap: &mut Heap,
    thread: &mut Thread,
    args: &[RawValue],
) -> Exec<Vec<RawValue>> {
    match builtin {
        Builtin::Type => builtin_type(heap, args),
        Builtin::Typeof => builtin_typeof(heap, args),
        Builtin::ToString => builtin_tostring(heap, thread, args),
        Builtin::Assert => builtin_assert(heap, args),
        Builtin::Error => builtin_error(heap, args, call_site),
        Builtin::Print => builtin_print(heap, thread, args),
        Builtin::SetMetatable => builtin_setmetatable(heap, args),
        Builtin::GetMetatable => builtin_getmetatable(heap, args),
        Builtin::Pcall => builtin_pcall(heap, thread, args),
        Builtin::Xpcall => builtin_xpcall(heap, thread, args),
        Builtin::ToNumber => builtin_tonumber(heap, args),
        Builtin::RawEqual => builtin_rawequal(args),
        Builtin::RawGet => builtin_rawget(heap, args),
        Builtin::RawSet => builtin_rawset(heap, args),
        Builtin::RawLen => builtin_rawlen(heap, args),
        Builtin::Select => builtin_select(heap, args),
        Builtin::CollectGarbage => builtin_collectgarbage(heap, args),
        Builtin::GcInfo => Ok(vec![RawValue::Number(heap.gcinfo_bytes() as f64 / 1024.0)]),
        Builtin::Next => builtin_next(heap, args),
        Builtin::INext => builtin_inext(heap, args),
        Builtin::Pairs => builtin_pairs(heap, args),
        Builtin::IPairs => builtin_ipairs(heap, args),
        _ => unreachable!("non-base builtin routed to base_lib"),
    }
}

fn builtin_type(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let value = args
        .first()
        .copied()
        .ok_or_else(|| err("missing argument to 'type'"))?;
    let name = heap
        .intern_str(type_name(value))
        .ok_or_else(|| err_memory("out of memory interning a type name"))?;
    Ok(vec![RawValue::String(name)])
}

/// `typeof(v)`: like `type`, but a value whose metatable carries a string `__type` field
/// reports that name (Luau's `luaL_typename`) — how a userdata or a table advertises a custom
/// type. Absent a `__type`, it is the basic type name. The `__type` key is resolved read-only:
/// if it was never interned, no metatable can hold it and the result is the plain type name.
fn builtin_typeof(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let value = args
        .first()
        .copied()
        .ok_or_else(|| err("missing argument to 'typeof'"))?;
    // `luaT_objtypenamestr`: `__type` is honoured from a *userdata's own* metatable or a
    // *global per-type* metatable (e.g. the shared string metatable) — but explicitly NOT from
    // a `table`'s own metatable, so a table cannot spoof its type. `tm::metatable` returns a
    // table's own metatable, so tables are excluded here; the other types resolve correctly
    // (userdata → its own metatable, string → the global string metatable).
    if !matches!(value, RawValue::Table(_))
        && let Some(metatable) = tm::metatable(heap, value)
        && let Some(type_key) = heap.interner.lookup(b"__type")
        && let RawValue::String(name) = heap
            .table(metatable)
            .map_or(RawValue::Nil, |t| t.get(RawValue::String(type_key)))
    {
        return Ok(vec![RawValue::String(name)]);
    }
    let name = heap
        .intern_str(type_name(value))
        .ok_or_else(|| err_memory("out of memory interning a type name"))?;
    Ok(vec![RawValue::String(name)])
}

/// The `type()` name of a value. This revision distinguishes `"integer"` from
/// `"number"`.
pub fn type_name(value: RawValue) -> &'static [u8] {
    match value {
        RawValue::Nil => b"nil",
        RawValue::Boolean(_) => b"boolean",
        RawValue::Number(_) => b"number",
        RawValue::Integer(_) => b"integer",
        RawValue::Vector(_) => b"vector",
        RawValue::String(_) => b"string",
        RawValue::Table(_) => b"table",
        RawValue::Function(_) => b"function",
        RawValue::Userdata(_) | RawValue::LightUserdata { .. } => b"userdata",
        RawValue::Thread(_) => b"thread",
        RawValue::Buffer(_) => b"buffer",
    }
}

pub(super) fn builtin_tostring(
    heap: &mut Heap,
    thread: &mut Thread,
    args: &[RawValue],
) -> Exec<Vec<RawValue>> {
    // `luaL_checkany`: a missing argument errors with the bare "missing argument #1"
    // (no function name or "value expected"), distinct from an explicit `nil`.
    let Some(&value) = args.first() else {
        return Err(err("missing argument #1"));
    };
    let bytes = tostring_bytes(heap, thread, value)?;
    let interned = heap
        .intern_str(&bytes)
        .ok_or_else(|| err_memory("out of memory interning a string"))?;
    Ok(vec![RawValue::String(interned)])
}

fn tostring_bytes(heap: &mut Heap, thread: &mut Thread, value: RawValue) -> Exec<Vec<u8>> {
    if let Some(handler) = tm::get_metamethod(heap, value, MetaEvent::ToString)? {
        let result = call_value(heap, thread, handler, &[value])?
            .into_iter()
            .next()
            .unwrap_or(RawValue::Nil);
        // `luaL_tolstring` accepts a string verbatim or coerces a number; anything
        // else raises. (A native integer renders like a plain integer.)
        return match result {
            RawValue::String(handle) => Ok(heap
                .string(handle)
                .map_or_else(Vec::new, |string| string.bytes().to_vec())),
            RawValue::Number(_) | RawValue::Integer(_) => Ok(value_to_string(heap, result)),
            _ => Err(err("'__tostring' must return a string")),
        };
    }
    Ok(value_to_string(heap, value))
}

/// The default `tostring` bytes of a value (no `__tostring`). GC-backed values
/// render with a deterministic handle identity rather than a process pointer;
/// upstream only relies on the `type: 0x...` shape, and avoiding raw addresses
/// is the safer service default.
/// `print(...)`: writes the tab-separated rendering of its arguments, terminated by
/// a newline, to the host's print sink (or discards it when none is installed — the
/// default). Arguments are rendered through `tostring`, so `__tostring`
/// metamethods and host-type tostring hooks are honored.
fn builtin_print(heap: &mut Heap, thread: &mut Thread, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    // No sink: `print` is a no-op, so skip the formatting entirely.
    if !heap.has_print_sink() {
        return Ok(Vec::new());
    }
    let mut line = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            line.push(b'\t');
        }
        line.extend_from_slice(&tostring_bytes(heap, thread, *arg)?);
    }
    line.push(b'\n');
    heap.write_print_output(&line);
    Ok(Vec::new())
}

fn value_to_string(heap: &Heap, value: RawValue) -> Vec<u8> {
    match value {
        RawValue::Nil => b"nil".to_vec(),
        RawValue::Boolean(true) => b"true".to_vec(),
        RawValue::Boolean(false) => b"false".to_vec(),
        RawValue::Number(n) => vmutils::number_to_string(n).into_bytes(),
        RawValue::Integer(i) => i.to_string().into_bytes(),
        RawValue::String(handle) => heap
            .string(handle)
            .map_or_else(Vec::new, |s| s.bytes().to_vec()),
        RawValue::Table(handle) => gc_identity_string(b"table", handle),
        RawValue::Function(handle) => gc_identity_string(b"function", handle),
        RawValue::Thread(handle) => gc_identity_string(b"thread", handle),
        RawValue::Buffer(handle) => gc_identity_string(b"buffer", handle),
        // Each component renders through the same shortest-float formatter as a
        // number, widened from `f32` to `f64` exactly as upstream promotes the
        // float for `luai_num2str`, joined by ", " (`laux.cpp` `luaL_tolstring`).
        RawValue::Vector(v) => {
            let mut out = Vec::new();
            for (i, component) in v.iter().enumerate() {
                if i != 0 {
                    out.extend_from_slice(b", ");
                }
                out.extend_from_slice(vmutils::number_to_string(f64::from(*component)).as_bytes());
            }
            out
        }
        RawValue::Userdata(handle) => gc_identity_string(b"userdata", handle),
        RawValue::LightUserdata { .. } => b"userdata".to_vec(),
    }
}

fn gc_identity_string<T>(kind: &[u8], handle: RawGc<T>) -> Vec<u8> {
    let id = (u64::from(handle.generation()) << 32) | u64::from(handle.index());
    let mut out = Vec::with_capacity(kind.len() + 22);
    out.extend_from_slice(kind);
    out.extend_from_slice(b": 0x");
    out.extend_from_slice(format!("{id:x}").as_bytes());
    out
}

/// `assert(v, message?)`: returns all arguments if `v` is truthy, else raises
/// `message`. The message is raised like `error` — a string/number is the located
/// text, any other value passes through unchanged — defaulting to the standard
/// text when absent.
fn builtin_assert(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    // `luaL_checkany(L, 1)`: a missing first argument is distinct from a present falsy one —
    // `assert()` reports the missing argument, `assert(nil)` the assertion failure.
    let Some(&condition) = args.first() else {
        return Err(err("missing argument #1"));
    };
    if vmutils::truthy(condition) {
        return Ok(args.to_vec());
    }
    // The message argument goes through `luaL_optstring` (`luaL_checklstring` →
    // `lua_tolstring`): absent/nil uses the default, a string passes, a number coerces, and
    // any other type raises an argument type error — it is not re-raised as a value.
    match args.get(1).copied() {
        None | Some(RawValue::Nil) => Err(err("assertion failed!")),
        Some(RawValue::String(handle)) => Err(err(heap
            .string(handle)
            .map_or_else(String::new, |s| string_lossy(s.bytes())))),
        Some(RawValue::Number(n)) => Err(err(vmutils::number_to_string(n))),
        Some(RawValue::Integer(i)) => Err(err(i.to_string())),
        Some(other) => Err(err(format!(
            "invalid argument #2 to 'assert' (string expected, got {})",
            core::str::from_utf8(type_name(other)).unwrap_or("value")
        ))),
    }
}

/// `error(value, level?)`: raises `value`. A string or number value with a
/// positive `level` surfaces as a string prefixed with that caller frame's
/// `source:line:` at the protected boundary; `level == 0` adds no location. Any
/// other value surfaces unchanged, so `pcall(function() error({}) end)` returns
/// the table.
fn builtin_error(
    heap: &Heap,
    args: &[RawValue],
    call_site: BuiltinCallSite,
) -> Exec<Vec<RawValue>> {
    let value = args.first().copied().unwrap_or(RawValue::Nil);
    let level = match args.get(1).copied() {
        Some(RawValue::Number(n)) => n as i64,
        Some(RawValue::Integer(i)) => i,
        _ => 1,
    };
    // Upstream prefixes only a string-coercible value, and only at `level > 0`.
    let message = match value {
        _ if level == 0 => None,
        RawValue::String(handle) => heap.string(handle).map(|s| string_lossy(s.bytes())),
        RawValue::Number(n) => Some(vmutils::number_to_string(n)),
        RawValue::Integer(i) => Some(i.to_string()),
        _ => None,
    };
    match (message, level, call_site) {
        (Some(message), level, BuiltinCallSite::Bytecode) if level > 0 => {
            Err(err_at_level(message, level as usize))
        }
        (Some(message), level, BuiltinCallSite::Native) if level > 1 => {
            Err(err_at_level(message, (level - 1) as usize))
        }
        (Some(message), _, _) => Err(err_no_location(message)),
        (None, _, _) => Err(err_value(value)),
    }
}

/// `pcall(f, ...)`: calls `f` with the remaining arguments in a protected scope.
/// Returns `true` followed by `f`'s results, or `false` and the error value of an
/// uncaught raise (already located). The protected boundary restores the thread,
/// so the caller continues regardless of the outcome.
fn builtin_pcall(heap: &mut Heap, thread: &mut Thread, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let Some((func, call_args)) = args.split_first() else {
        return Err(err("missing value to 'pcall'"));
    };
    match protected_call(heap, thread, *func, call_args) {
        Ok(results) => {
            let mut out = Vec::with_capacity(results.len() + 1);
            out.push(RawValue::Boolean(true));
            out.extend(results);
            Ok(out)
        }
        // A catchable error becomes `false, <error value>`; a fatal one
        // (cancellation, deadline) propagates past `pcall` so it cannot be swallowed.
        Err(error) if error.is_catchable() => {
            Ok(vec![RawValue::Boolean(false), materialize(heap, error)])
        }
        Err(error) => Err(error),
    }
}

/// `xpcall(f, msgh, ...)`: like `pcall`, but a raise from `f` is passed to the
/// message handler `msgh` and its result is returned in place of the raw error.
/// Returns `true` then `f`'s results on success, or `false` then the handler's
/// (first) result on failure. `msgh` must be a function (checked first, as upstream
/// `luaB_xpcall` does). The handler runs in its own protected scope so a failing
/// handler still leaves `xpcall` returning `false` plus that secondary error rather
/// than propagating. (Note: ruau restores the thread at the protected boundary
/// before the handler runs, so a `debug.traceback` handler sees the unwound stack,
/// not the failed frame — a fidelity gap versus upstream's in-place error function.)
fn builtin_xpcall(heap: &mut Heap, thread: &mut Thread, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let func = args.first().copied().unwrap_or(RawValue::Nil);
    let handler = match args.get(1).copied() {
        None => {
            return Err(err_no_location(
                "missing argument #2 to 'xpcall' (function expected)",
            ));
        }
        Some(RawValue::Function(handler)) => RawValue::Function(handler),
        Some(other) => {
            return Err(err_no_location(format!(
                "invalid argument #2 to 'xpcall' (function expected, got {})",
                String::from_utf8_lossy(type_name(other))
            )));
        }
    };
    let call_args = args.get(2..).unwrap_or(&[]);
    match protected_call(heap, thread, func, call_args) {
        Ok(results) => {
            let mut out = Vec::with_capacity(results.len() + 1);
            out.push(RawValue::Boolean(true));
            out.extend(results);
            Ok(out)
        }
        // A fatal error (cancellation, deadline) bypasses the handler and propagates.
        Err(error) if error.is_catchable() => {
            let error_kind = error.kind;
            let error_value = materialize(heap, error);
            let replaced = match protected_call(heap, thread, handler, &[error_value]) {
                Ok(handler_results) => handler_results.into_iter().next().unwrap_or(RawValue::Nil),
                // A handler that raises an ordinary error yields a fixed string. If
                // both the protected function and the handler hit memory failure,
                // preserve the original memory error so the handler cannot mask an
                // OOM with a secondary allocation failure.
                Err(handler_error) if handler_error.is_catchable() => {
                    match (error_kind, handler_error.kind) {
                        (RuntimeErrorKind::Memory, RuntimeErrorKind::Memory) => error_value,
                        _ => materialize(heap, err_handler_failure()),
                    }
                }
                Err(handler_error) => return Err(handler_error),
            };
            Ok(vec![RawValue::Boolean(false), replaced])
        }
        Err(error) => Err(error),
    }
}

/// `setmetatable(t, mt)`: sets table `t`'s metatable to `mt` (a table) or clears
/// it with `nil`, then returns `t`. A current metatable with a non-nil
/// `__metatable` field is protected and cannot be replaced.
fn builtin_setmetatable(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let target = args.first().copied().unwrap_or(RawValue::Nil);
    let RawValue::Table(handle) = target else {
        return Err(err("bad argument #1 to 'setmetatable' (table expected)"));
    };
    let metatable = match args.get(1).copied() {
        Some(RawValue::Table(mt)) => Some(mt),
        None | Some(RawValue::Nil) => None,
        Some(_) => {
            return Err(err(
                "bad argument #2 to 'setmetatable' (nil or table expected)",
            ));
        }
    };
    let current_metatable = heap
        .table(handle)
        .ok_or_else(|| err("setmetatable on a non-resident table"))?
        .metatable();
    if let Some(current_metatable) = current_metatable
        && metatable_protection(heap, current_metatable)?.is_some()
    {
        return Err(err("cannot change a protected metatable"));
    }
    // A readonly table rejects a metatable change, like every other write
    // (upstream `lua_setmetatable` raises `luaG_readonlyerror`). Without this a
    // sandboxed script could `setmetatable(string, {__call = ...})` and reshape a
    // frozen library.
    require_writable(heap, handle)?;
    heap.table_mut(handle)
        .ok_or_else(|| err("setmetatable on a non-resident table"))?
        .set_metatable(metatable);
    Ok(vec![target])
}

/// `getmetatable(v)`: returns `v`'s raw metatable, its `__metatable` protection
/// value, or `nil`.
fn builtin_getmetatable(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let Some(&value) = args.first() else {
        return Err(err("missing argument #1"));
    };
    let Some(metatable) = tm::metatable(heap, value) else {
        return Ok(vec![RawValue::Nil]);
    };
    Ok(vec![
        metatable_protection(heap, metatable)?.unwrap_or(RawValue::Table(metatable)),
    ])
}

pub(super) fn metatable_protection(
    heap: &mut Heap,
    metatable: RawGc<marker::Table>,
) -> Exec<Option<RawValue>> {
    let key = heap
        .intern_str(b"__metatable")
        .ok_or_else(|| err_memory("out of memory interning __metatable"))?;
    Ok(match heap.table(metatable) {
        Some(table) => match table.get(RawValue::String(key)) {
            RawValue::Nil => None,
            value => Some(value),
        },
        None => None,
    })
}

/// `tonumber(v)`: a number passes through; a string parses (decimal or `0x`
/// hex), failing to `nil`; anything else is `nil`. The optional base argument is
/// implemented for decimal and hexadecimal inputs only.
fn builtin_tonumber(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    // `luaL_optinteger(L, 2, 10)`: the base defaults to 10 and accepts a number or
    // numeric string. Base 10 is the standard conversion; any other base parses an
    // integer in that radix (`luaB_tonumber`).
    let base = match args.get(1).copied() {
        None | Some(RawValue::Nil) => 10,
        Some(RawValue::Number(n)) => n as i64,
        Some(RawValue::Integer(i)) => i,
        Some(RawValue::String(handle)) => heap
            .string(handle)
            .and_then(|s| vmutils::str_to_number(s.bytes()))
            .map(|n| n as i64)
            .ok_or_else(|| err("bad argument #2 to 'tonumber' (number expected)"))?,
        Some(_) => return Err(err("bad argument #2 to 'tonumber' (number expected)")),
    };
    if base == 10 {
        // `luaL_checkany`: a missing argument errors with the bare "missing argument #1"
        // (distinct from an explicit `nil`, a present value that converts to `nil`).
        let Some(&first) = args.first() else {
            return Err(err("missing argument #1"));
        };
        let result = match first {
            value @ RawValue::Number(_) => value,
            // `lua_tonumberx` yields a number even for an integer, so `tonumber`
            // surfaces a number (unlike a passthrough that would keep the integer tag).
            RawValue::Integer(i) => RawValue::Number(i as f64),
            RawValue::String(handle) => heap
                .string(handle)
                .and_then(|s| vmutils::str_to_number(s.bytes()))
                .map_or(RawValue::Nil, RawValue::Number),
            _ => RawValue::Nil,
        };
        return Ok(vec![result]);
    }
    if !(2..=36).contains(&base) {
        return Err(err("bad argument #2 to 'tonumber' (base out of range)"));
    }
    // A non-10 base requires a string argument (`luaL_checkstring`, which coerces a
    // number to its decimal text).
    let bytes = match args.first().copied().unwrap_or(RawValue::Nil) {
        RawValue::String(handle) => heap.string(handle).map(|s| s.bytes().to_vec()),
        RawValue::Number(n) => Some(vmutils::number_to_string(n).into_bytes()),
        RawValue::Integer(i) => Some(i.to_string().into_bytes()),
        _ => return Err(err("bad argument #1 to 'tonumber' (string expected)")),
    };
    let result = bytes
        .and_then(|b| str_to_number_in_base(&b, base as u32))
        .map_or(RawValue::Nil, RawValue::Number);
    Ok(vec![result])
}

/// Parses `s` as an integer in `base` (2..=36) like C `strtoull`: skip leading
/// ASCII whitespace and an optional sign, read base digits (case-insensitive),
/// then allow only trailing whitespace. `None` if no digit is read or any other
/// character remains. The accumulator wraps like `unsigned long long`, and a
/// leading `-` negates in that wrapping space (so `tonumber("-1", 36)` is a huge
/// positive double — matching upstream, which leaves that conformance case
/// commented for exactly this reason).
fn str_to_number_in_base(s: &[u8], base: u32) -> Option<f64> {
    let is_space = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r');
    let mut i = 0;
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let mut negate = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        negate = s[i] == b'-';
        i += 1;
    }
    if base == 16 && i + 1 < s.len() && s[i] == b'0' && matches!(s[i + 1], b'x' | b'X') {
        i += 2;
    }
    let digits_start = i;
    let mut value: u64 = 0;
    while i < s.len() {
        let digit = match s[i] {
            b'0'..=b'9' => u32::from(s[i] - b'0'),
            b'a'..=b'z' => u32::from(s[i] - b'a') + 10,
            b'A'..=b'Z' => u32::from(s[i] - b'A') + 10,
            _ => break,
        };
        if digit >= base {
            break;
        }
        value = value
            .wrapping_mul(u64::from(base))
            .wrapping_add(u64::from(digit));
        i += 1;
    }
    if i == digits_start {
        return None; // no valid digit
    }
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    if i != s.len() {
        return None; // invalid trailing characters
    }
    let value = if negate { value.wrapping_neg() } else { value };
    Some(value as f64)
}

/// `rawequal(a, b)`: primitive equality, bypassing `__eq`.
fn builtin_rawequal(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let a = args
        .first()
        .copied()
        .ok_or_else(|| err("missing argument #1"))?;
    let b = args
        .get(1)
        .copied()
        .ok_or_else(|| err("missing argument #2"))?;
    Ok(vec![RawValue::Boolean(vmutils::raw_equal(a, b))])
}

/// `rawget(t, k)`: a table read bypassing `__index`.
fn builtin_rawget(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let RawValue::Table(handle) = args.first().copied().unwrap_or(RawValue::Nil) else {
        return Err(err("bad argument #1 to 'rawget' (table expected)"));
    };
    let key = args.get(1).copied().unwrap_or(RawValue::Nil);
    Ok(vec![
        heap.table(handle).map_or(RawValue::Nil, |t| t.get(key)),
    ])
}

/// `rawset(t, k, v)`: a table write bypassing `__newindex`; returns `t`.
fn builtin_rawset(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let table = args.first().copied().unwrap_or(RawValue::Nil);
    let RawValue::Table(handle) = table else {
        return Err(err("bad argument #1 to 'rawset' (table expected)"));
    };
    let key = args.get(1).copied().unwrap_or(RawValue::Nil);
    if let Some(rejection) = crate::table::key_rejection(key) {
        return Err(err(rejection.message()));
    }
    let value = args.get(2).copied().unwrap_or(RawValue::Nil);
    // `rawset` bypasses metamethods but not the readonly flag (upstream's fast
    // `rawset` raises `luaG_readonlyerror`).
    require_writable(heap, handle)?;
    let set = heap
        .table_mut(handle)
        .ok_or_else(|| err("rawset on a non-resident table"))?
        .set(key, value);
    if !set {
        return Err(err("table index is invalid"));
    }
    Ok(vec![table])
}

/// `rawlen(v)`: the length of a table (its border) or a string, bypassing
/// `__len`.
fn builtin_rawlen(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let len = match args.first().copied().unwrap_or(RawValue::Nil) {
        RawValue::Table(handle) => heap.table(handle).map_or(0, |t| t.length()),
        RawValue::String(handle) => heap.string(handle).map_or(0, |s| s.len() as u64),
        _ => {
            return Err(err(
                "bad argument #1 to 'rawlen' (table or string expected)",
            ));
        }
    };
    Ok(vec![RawValue::Number(len as f64)])
}

/// `select('#', ...)`: the number of values after the selector. `select(n, ...)`:
/// all values from the n-th on; a negative `n` counts from the end (`-1` is the
/// last). Port of `luaB_select` (`lbaselib.cpp`): the selector and values are
/// ordinary arguments here, so `...` is already expanded by the caller.
fn builtin_select(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let selector = args.first().copied().unwrap_or(RawValue::Nil);
    // `args` is the selector plus the values; `n` mirrors upstream's `lua_gettop`.
    let n = args.len() as i64;
    if let RawValue::String(handle) = selector
        && heap
            .string(handle)
            .is_some_and(|s| s.bytes().first() == Some(&b'#'))
    {
        return Ok(vec![RawValue::Number((n - 1) as f64)]);
    }
    let mut index = match selector {
        RawValue::Integer(i) => i,
        RawValue::Number(f) if f.fract() == 0.0 => f as i64,
        // `luaL_checkinteger` coerces a numeric string to the index (a non-`#` string).
        RawValue::String(handle) => heap
            .string(handle)
            .and_then(|s| vmutils::str_to_number(s.bytes()))
            .filter(|f| f.fract() == 0.0)
            .map(|f| f as i64)
            .ok_or_else(|| err("bad argument #1 to 'select' (number expected)"))?,
        _ => return Err(err("bad argument #1 to 'select' (number expected)")),
    };
    if index < 0 {
        index += n;
    } else if index > n {
        index = n;
    }
    if index < 1 {
        return Err(err("bad argument #1 to 'select' (index out of range)"));
    }
    Ok(args[index as usize..].to_vec())
}

/// `loadstring(source, chunkname?)`: compiles `source` at runtime and returns the
/// resulting function. Under the fenv compatibility profile, the loaded chunk
/// receives the running thread's global environment, including changes made by
/// `setfenv(0, env)`. A parse or compile error returns `(nil, message)` rather
/// than raising. The optional `chunkname` is accepted, but compile-error
/// prefixes still use the loader's default chunk name.
fn builtin_collectgarbage(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    // Classify the option against the *borrowed* bytes — never copy the argument string
    // (an adversary-supplied option could be arbitrarily long, and an infallible copy here
    // would breach the collector's never-abort-the-process discipline). The borrow ends
    // with this `match`, before the `&mut` `request_gc` call below.
    enum Action {
        Count,
        Collect,
        Stop,
        Restart,
        Step,
        IsRunning,
        Unsupported(&'static str),
        InvalidOption,
        TypeError,
    }
    let action = match args.first().copied().unwrap_or(RawValue::Nil) {
        RawValue::Nil => Action::Collect,
        RawValue::String(handle) => match heap.string(handle).map(|s| s.bytes()) {
            Some(b"count") => Action::Count,
            Some(b"collect") => Action::Collect,
            Some(b"stop") => Action::Stop,
            Some(b"restart") => Action::Restart,
            Some(b"step") => Action::Step,
            Some(b"isrunning") => Action::IsRunning,
            Some(b"setpause") => Action::Unsupported("setpause"),
            Some(b"setstepmul") => Action::Unsupported("setstepmul"),
            Some(b"generational") => Action::Unsupported("generational"),
            Some(b"incremental") => Action::Unsupported("incremental"),
            _ => Action::InvalidOption,
        },
        _ => Action::TypeError,
    };
    match action {
        Action::Count => {
            let kilobytes = heap.gcinfo_bytes() as f64 / 1024.0;
            Ok(vec![RawValue::Number(kilobytes)])
        }
        Action::Collect => {
            heap.request_gc();
            Ok(vec![RawValue::Number(0.0)])
        }
        Action::Stop => {
            heap.stop_gc();
            Ok(vec![RawValue::Number(0.0)])
        }
        Action::Restart => {
            heap.restart_gc();
            Ok(vec![RawValue::Number(0.0)])
        }
        Action::Step => {
            let size = match args.get(1).copied().unwrap_or(RawValue::Nil) {
                RawValue::Nil => 0,
                RawValue::Number(value) if value.is_finite() && value > 0.0 => value as usize,
                RawValue::Integer(value) if value > 0 => value as usize,
                RawValue::Number(_) | RawValue::Integer(_) => 0,
                _ => return Err(err("bad argument #2 to 'collectgarbage' (number expected)")),
            };
            Ok(vec![RawValue::Boolean(heap.request_gc_step(size))])
        }
        Action::IsRunning => Ok(vec![RawValue::Boolean(heap.gc_running())]),
        Action::Unsupported(option) => Err(err(format!(
            "collectgarbage option '{option}' is not supported"
        ))),
        Action::InvalidOption => Err(err("invalid option")),
        Action::TypeError => Err(err("bad argument #1 to 'collectgarbage' (string expected)")),
    }
}

/// `next(t, k)`: the key/value after `k` in `t`'s traversal order, or `nil` at
/// the end. The stateless iterator `pairs` returns.
fn builtin_next(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let RawValue::Table(handle) = args.first().copied().unwrap_or(RawValue::Nil) else {
        return Err(err("bad argument #1 to 'next' (table expected)"));
    };
    let key = args.get(1).copied().unwrap_or(RawValue::Nil);
    match heap.table(handle).map(|t| t.next(key)) {
        Some(NextStep::Pair(key, value)) => Ok(vec![key, value]),
        Some(NextStep::Done) => Ok(vec![RawValue::Nil]),
        Some(NextStep::InvalidKey) => Err(err("invalid key to 'next'")),
        None => Err(err("'next' on a non-resident table")),
    }
}

/// The integer iterator `ipairs` returns: from index `i`, yield `(i + 1,
/// t[i + 1])` until the value is `nil`.
fn builtin_inext(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let RawValue::Table(handle) = args.first().copied().unwrap_or(RawValue::Nil) else {
        return Err(err("bad argument #1 to 'ipairs' iterator (table expected)"));
    };
    let index = match args.get(1).copied().unwrap_or(RawValue::Nil) {
        RawValue::Number(n) => n,
        RawValue::Integer(i) => i as f64,
        _ => 0.0,
    };
    let next_index = index + 1.0;
    let value = heap
        .table(handle)
        .map_or(RawValue::Nil, |t| t.get(RawValue::Number(next_index)));
    if matches!(value, RawValue::Nil) {
        // End of the array: zero results, terminating the generic-for.
        Ok(Vec::new())
    } else {
        Ok(vec![RawValue::Number(next_index), value])
    }
}

/// `pairs(t)`: the iterator triple `(next, t, nil)` a generic `for` consumes.
fn builtin_pairs(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let table = args.first().copied().unwrap_or(RawValue::Nil);
    if !matches!(table, RawValue::Table(_)) {
        return Err(err("bad argument #1 to 'pairs' (table expected)"));
    }
    let next = heap
        .alloc_builtin(Builtin::Next)
        .ok_or_else(|| err_memory("out of memory creating an iterator"))?;
    Ok(vec![RawValue::Function(next), table, RawValue::Nil])
}

/// `ipairs(t)`: the iterator triple `(inext, t, 0)` for an array traversal.
fn builtin_ipairs(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let table = args.first().copied().unwrap_or(RawValue::Nil);
    if !matches!(table, RawValue::Table(_)) {
        return Err(err("bad argument #1 to 'ipairs' (table expected)"));
    }
    let inext = heap
        .alloc_builtin(Builtin::INext)
        .ok_or_else(|| err_memory("out of memory creating an iterator"))?;
    Ok(vec![
        RawValue::Function(inext),
        table,
        RawValue::Number(0.0),
    ])
}
