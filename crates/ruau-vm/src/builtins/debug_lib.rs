use super::*;

pub(super) fn dispatch(
    builtin: Builtin,
    callee: RawValue,
    heap: &mut Heap,
    thread: &mut Thread,
    args: &[RawValue],
) -> Exec<Vec<RawValue>> {
    match builtin {
        Builtin::DebugInfo => debug_info(heap, thread, callee, args),
        Builtin::DebugTraceback => debug_traceback(heap, thread, args),
        Builtin::CompatGetFenv => compat_getfenv(heap, thread, args),
        Builtin::CompatSetFenv => compat_setfenv(heap, thread, args),
        _ => unreachable!("non-debug builtin routed to debug_lib"),
    }
}

struct FrameInfo {
    /// The chunk name (`short_src`); `[C]` for a native/host frame.
    source: Vec<u8>,
    /// The current source line, or `-1` when unavailable.
    currentline: i64,
    /// The function name, or the empty string when unavailable.
    name: Vec<u8>,
    /// The fixed parameter count.
    num_params: i64,
    /// Whether the function is variadic.
    is_vararg: bool,
    /// The frame's function value (`debug.info`'s `f` option).
    function: RawValue,
}

/// Snapshots a thread's frames, innermost last. A failed coroutine has no live
/// call stack, but keeps the abandoned frames for post-mortem debug queries.
fn collect_frames(thread: &Thread) -> Vec<FrameSnapshot> {
    if thread.call_stack.is_empty() {
        return thread.error_frames.clone();
    }
    thread
        .call_stack
        .iter()
        .filter_map(|entry| entry.frame())
        .map(|frame| FrameSnapshot {
            closure: frame.closure,
            savedpc: frame.savedpc,
        })
        .collect()
}

fn global_function_name(
    heap: &Heap,
    globals: Option<RawGc<marker::Table>>,
    function: RawValue,
) -> Option<Vec<u8>> {
    let globals = globals?;
    let mut entries = Vec::new();
    heap.table(globals)
        .map(|table| table.for_each_entry(|key, value| entries.push((key, value))))?;
    entries
        .into_iter()
        .filter_map(|(key, value)| {
            if value != function {
                return None;
            }
            let RawValue::String(key) = key else {
                return None;
            };
            heap.string(key).map(|s| s.bytes().to_vec())
        })
        // Keep tied global aliases deterministic until debug-name resolution
        // grows a richer call-site name model.
        .min()
}

fn current_debug_info_frame(callee: RawValue) -> FrameInfo {
    FrameInfo {
        source: b"[C]".to_vec(),
        currentline: -1,
        name: Builtin::DebugInfo.global_name().to_vec(),
        num_params: 0,
        is_vararg: true,
        function: callee,
    }
}

/// Resolves a frame's introspectable data. `savedpc` is `None` for a function
/// queried directly (`debug.info(f, ...)`), which has no running line.
fn frame_info(
    heap: &Heap,
    closure: RawGc<marker::Closure>,
    savedpc: Option<usize>,
) -> Option<FrameInfo> {
    let proto_handle = heap.closure(closure)?.proto;
    let proto = heap.proto(proto_handle)?;
    let name = if let Some(native) = proto.native {
        native.global_name().to_vec()
    } else {
        proto
            .debug_name
            .and_then(|name| heap.string(name).map(|name| name.bytes().to_vec()))
            .unwrap_or_default()
    };
    let source = proto
        .source
        .and_then(|s| heap.string(s).map(|s| crate::debug::chunk_id(s.bytes())))
        .unwrap_or_else(|| b"[C]".to_vec());
    // `savedpc` points past the current instruction, so the running line is at
    // `savedpc - 1`; an empty line table (a native proto) has no line.
    let currentline = match savedpc {
        Some(pc) if proto.has_line_info() => proto.line(pc.saturating_sub(1)).map_or(-1, i64::from),
        None if proto.line_defined > 0 => i64::from(proto.line_defined),
        _ => -1,
    };
    Some(FrameInfo {
        source,
        currentline,
        name,
        num_params: i64::from(proto.num_params),
        is_vararg: proto.is_vararg,
        function: RawValue::Function(closure),
    })
}

/// `debug.info([thread,] level | func, options)`: returns the requested fields of
/// a stack frame (or a function). Supported options: `s` (short source), `l`
/// (current/definition line), `n` (debug/native/global name), `f` (the
/// function), and `a` (param count and varargs). A level out of range returns
/// nothing, like upstream.
fn debug_info(
    heap: &mut Heap,
    thread: &Thread,
    callee: RawValue,
    args: &[RawValue],
) -> Exec<Vec<RawValue>> {
    // Resolve the overload and locate the target frame.
    let (mut info, options_index, name_globals) = match args.first().copied() {
        Some(RawValue::Thread(handle)) => {
            let level = debug_level_arg(args, 1)?;
            let target_globals = heap.thread(handle).and_then(|target| target.globals);
            let info = heap.thread(handle).and_then(|target| {
                let frames = collect_frames(target);
                let level = if target.call_stack.is_empty() && !target.error_frames.is_empty() {
                    // A failed coroutine has unwound its transient C/yield
                    // boundary, so level 0 should address the innermost
                    // retained Lua frame; a live suspended coroutine still has
                    // no materialized level-0 frame in this VM.
                    level.checked_add(1)?
                } else {
                    level
                };
                frame_at(heap, &frames, level)
            });
            (info, 2, target_globals)
        }
        Some(RawValue::Function(closure)) => (frame_info(heap, closure, None), 1, thread.globals),
        _ => {
            let level = debug_level_arg(args, 0)?;
            let frames = collect_frames(thread);
            let info = if level == 0 {
                Some(current_debug_info_frame(callee))
            } else {
                frame_at(heap, &frames, level)
            };
            (info, 1, thread.globals)
        }
    };
    let options = match args.get(options_index).copied().unwrap_or(RawValue::Nil) {
        RawValue::String(s) => heap.string(s).map_or_else(Vec::new, |s| s.bytes().to_vec()),
        _ => return Err(err("bad argument to 'debug.info' (string expected)")),
    };
    if let Some(info) = &mut info
        && info.name.is_empty()
        && let Some(name) = global_function_name(heap, name_globals, info.function)
    {
        info.name = name;
    }
    let Some(info) = info else {
        return Ok(Vec::new());
    };
    let mut results = Vec::new();
    let mut seen = [false; 26];
    for &option in &options {
        if option.is_ascii_lowercase() {
            let idx = (option - b'a') as usize;
            if seen[idx] {
                return Err(err("duplicate option"));
            }
            seen[idx] = true;
        }
        match option {
            b's' => {
                let s = heap
                    .intern_str(&info.source)
                    .ok_or_else(|| err_memory("out of memory in 'debug.info'"))?;
                results.push(RawValue::String(s));
            }
            b'l' => results.push(RawValue::Number(info.currentline as f64)),
            b'n' => {
                let s = heap
                    .intern_str(&info.name)
                    .ok_or_else(|| err_memory("out of memory in 'debug.info'"))?;
                results.push(RawValue::String(s));
            }
            b'f' => results.push(info.function),
            b'a' => {
                results.push(RawValue::Number(info.num_params as f64));
                results.push(RawValue::Boolean(info.is_vararg));
            }
            _ => return Err(err("invalid option")),
        }
    }
    Ok(results)
}

/// Harness-only `getcoverage(function)`: returns coverage tables for the target
/// function and nested protos in preorder, matching upstream's conformance
/// helper shape. The helper is installed only by conformance harness setup.
/// Feature-gated `getfenv` compatibility helper. Production runners currently
/// reject `ExecutionFeatures::fenv`; conformance installs this only for scripts
/// that exercise upstream fenv semantics.
fn compat_getfenv(heap: &Heap, thread: &Thread, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let env = match args.first().copied() {
        None | Some(RawValue::Nil) => frame_environment(heap, thread, 1)?,
        Some(RawValue::Number(level)) if level.fract() == 0.0 && level >= 0.0 => {
            let level = level as usize;
            if level == 0 {
                thread.globals
            } else {
                frame_environment(heap, thread, level)?
            }
        }
        Some(RawValue::Integer(level)) if level >= 0 => {
            let level = usize::try_from(level).unwrap_or(usize::MAX);
            if level == 0 {
                thread.globals
            } else {
                frame_environment(heap, thread, level)?
            }
        }
        Some(RawValue::Function(closure)) => heap
            .closure(closure)
            .ok_or_else(|| err("bad argument #1 to 'getfenv' (function expected)"))?
            .env
            .or(thread.globals),
        Some(_) => {
            return Err(err("bad argument #1 to 'getfenv' (number expected)"));
        }
    };
    let env = env.ok_or_else(|| err("no function environment for level"))?;
    Ok(vec![RawValue::Table(env)])
}

fn compat_setfenv(heap: &mut Heap, thread: &mut Thread, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let RawValue::Table(env) = args.get(1).copied().unwrap_or(RawValue::Nil) else {
        return Err(err("bad argument #2 to 'setfenv' (table expected)"));
    };
    if heap.table(env).is_none() {
        return Err(err("bad argument #2 to 'setfenv' (table expected)"));
    }

    match args.first().copied().unwrap_or(RawValue::Nil) {
        RawValue::Function(closure) => {
            set_closure_environment(heap, closure, env)?;
            Ok(vec![RawValue::Function(closure)])
        }
        RawValue::Number(level) if level.fract() == 0.0 && level >= 0.0 => {
            set_level_environment(heap, thread, level as usize, env)
        }
        RawValue::Integer(level) if level >= 0 => set_level_environment(
            heap,
            thread,
            usize::try_from(level).unwrap_or(usize::MAX),
            env,
        ),
        _ => Err(err("bad argument #1 to 'setfenv' (number expected)")),
    }
}

fn set_level_environment(
    heap: &mut Heap,
    thread: &mut Thread,
    level: usize,
    env: RawGc<marker::Table>,
) -> Exec<Vec<RawValue>> {
    if level == 0 {
        thread.globals = Some(env);
        return Ok(Vec::new());
    }
    let closure = frame_closure(thread, level)?;
    set_closure_environment(heap, closure, env)?;
    Ok(vec![RawValue::Function(closure)])
}

fn set_closure_environment(
    heap: &mut Heap,
    closure: RawGc<marker::Closure>,
    env: RawGc<marker::Table>,
) -> Exec<()> {
    let proto = heap
        .closure(closure)
        .ok_or_else(|| err("function is not resident"))?
        .proto;
    if heap
        .proto(proto)
        .is_some_and(|proto| proto.native.is_some() || proto.host.is_some())
    {
        return Err(err("cannot change environment of given object"));
    }
    heap.closure_mut(closure)
        .ok_or_else(|| err("function is not resident"))?
        .env = Some(env);
    Ok(())
}

fn frame_environment(
    heap: &Heap,
    thread: &Thread,
    level: usize,
) -> Exec<Option<RawGc<marker::Table>>> {
    let closure = frame_closure(thread, level)?;
    Ok(heap
        .closure(closure)
        .ok_or_else(|| err("function is not resident"))?
        .env
        .or(thread.globals))
}

fn frame_closure(thread: &Thread, level: usize) -> Exec<RawGc<marker::Closure>> {
    thread
        .call_stack
        .iter()
        .rev()
        .filter_map(|entry| entry.frame().map(|frame| frame.closure))
        .nth(level.saturating_sub(1))
        .ok_or_else(|| err("invalid level"))
}

/// The frame at a 1-based `level` (1 = innermost), or `None` when out of range.
fn frame_at(heap: &Heap, frames: &[FrameSnapshot], level: i64) -> Option<FrameInfo> {
    if level < 1 {
        return None;
    }
    let index = frames.len().checked_sub(level as usize)?;
    let frame = *frames.get(index)?;
    frame_info(heap, frame.closure, Some(frame.savedpc))
}

/// The numeric `level` argument to `debug.info`, required and non-negative.
fn debug_level_arg(args: &[RawValue], index: usize) -> Exec<i64> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::Number(n) => {
            if n < 0.0 {
                return Err(err("level can't be negative"));
            }
            Ok(n as i64)
        }
        RawValue::Integer(i) => {
            if i < 0 {
                return Err(err("level can't be negative"));
            }
            Ok(i)
        }
        _ => Err(err(
            "bad argument to 'debug.info' (function or level expected)",
        )),
    }
}

/// `debug.traceback([thread,] message?, level?)`: a stack trace string, one
/// `source:line` frame from `level` (default 1) outward, optionally prefixed by
/// `message`. Names come from proto debug names or global aliases.
fn debug_traceback(heap: &mut Heap, thread: &Thread, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    // The explicit-thread overload defaults `level` to 0 (the target thread's top
    // frame); the current-thread form to 1 (the caller of `traceback`) — upstream
    // `db_traceback`'s `(L == L1) ? 1 : 0`.
    let (frames, globals, base, default_level) = match args.first().copied() {
        Some(RawValue::Thread(handle)) => {
            let target = heap.thread(handle);
            (
                target.map(collect_frames).unwrap_or_default(),
                target.and_then(|thread| thread.globals),
                1,
                0,
            )
        }
        _ => (collect_frames(thread), thread.globals, 0, 1),
    };
    let message = match args.get(base).copied() {
        Some(RawValue::String(s)) => {
            Some(heap.string(s).map_or_else(Vec::new, |s| s.bytes().to_vec()))
        }
        _ => None,
    };
    let level = match args.get(base + 1).copied() {
        None | Some(RawValue::Nil) => default_level,
        Some(RawValue::Number(n)) => n as i64,
        Some(RawValue::Integer(i)) => i,
        Some(_) => default_level,
    };
    if level < 0 {
        return Err(err("level can't be negative"));
    }
    let mut out = Vec::new();
    if let Some(message) = message {
        out.extend_from_slice(&message);
        out.push(b'\n');
    }
    // Frames from `level` (1 = innermost) outward; `frames[len - i]` is level `i`.
    let mut i = level.max(1) as usize;
    while i <= frames.len() {
        if let Some(info) = frame_at(heap, &frames, i as i64) {
            out.extend_from_slice(&info.source);
            if info.currentline > 0 {
                out.push(b':');
                out.extend_from_slice(info.currentline.to_string().as_bytes());
            }
            let name = if info.name.is_empty() {
                global_function_name(heap, globals, info.function)
            } else {
                Some(info.name)
            };
            if let Some(name) = name {
                out.extend_from_slice(b" function ");
                out.extend_from_slice(&name);
            }
            out.push(b'\n');
        }
        i += 1;
    }
    let s = heap
        .intern_str(&out)
        .ok_or_else(|| err_memory("out of memory in 'debug.traceback'"))?;
    Ok(vec![RawValue::String(s)])
}
