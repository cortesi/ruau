use super::*;

pub enum RequireCallStep {
    Ready(Vec<RawValue>),
    WaitForInFlight,
    Suspend(SuspendedRequire),
    BodyStarted,
}

pub struct RequireCallSite {
    pub(crate) result_reg: u32,
    pub(crate) result_count: u8,
    pub(crate) cleanup_end: u32,
}

pub struct RequireBodyStart {
    pub(crate) id: crate::ModuleId,
    pub(crate) instance: crate::InstanceKey,
    pub(crate) epoch: u64,
    pub(crate) loading_key: crate::heap::ModuleCacheKey,
}

enum SourcePoll<T> {
    Ready(crate::ModuleSourceResult<T>),
    Pending(crate::ModuleSourceFuture<T>),
}

fn poll_source_once<T>(mut future: crate::ModuleSourceFuture<T>) -> SourcePoll<T> {
    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(result) => SourcePoll::Ready(result),
        Poll::Pending => SourcePoll::Pending(future),
    }
}

pub(super) fn builtin_loadstring(
    heap: &mut Heap,
    thread: &Thread,
    args: &[RawValue],
) -> Exec<Vec<RawValue>> {
    let Some(RawValue::String(handle)) = args.first().copied() else {
        return Err(err("bad argument #1 to 'loadstring' (string expected)"));
    };
    // Copy the source out: the compile borrows nothing and the load needs `&mut heap`, so the
    // interned source must not stay borrowed across them.
    let Some(source_bytes) = heap.string(handle).map(|s| s.bytes().to_vec()) else {
        return Err(err("loadstring source is not resident"));
    };
    // The chunk name defaults to the source itself (`luaL_optstring(L, 2, s)`), so an unnamed
    // chunk reports `[string "…"]`; an explicit name carries its own `luaO_chunkid` marker.
    let explicit_name = match args.get(1).copied() {
        Some(RawValue::String(name)) => heap.string(name).map(|s| s.bytes().to_vec()),
        _ => None,
    };
    let chunk_name: &[u8] = explicit_name.as_deref().unwrap_or(&source_bytes);
    let limits = heap.limits();
    let compiler = heap.runtime_compiler();
    let context = heap.runtime_compile_context();
    let chunk = match compiler.compile(&source_bytes, context) {
        Ok(chunk) => chunk,
        Err(message) => return Ok(loadstring_error(heap, chunk_name, &message)),
    };
    let module = match load_with_limits(heap, &chunk, LoadMode::Validated, chunk_name, limits) {
        Ok(module) => module,
        Err(error) => {
            return Ok(loadstring_error(
                heap,
                chunk_name,
                format!("{error:?}").as_bytes(),
            ));
        }
    };
    let function = RawValue::Function(module.main);
    if let Some(closure) = heap.closure_mut(module.main) {
        closure.env = thread.globals;
    }
    // Release the loader's GC pin: the returned function value keeps the closure and its
    // proto graph reachable, and no collection runs between here and the builtin return.
    heap.unpin(&module.pin);
    Ok(vec![function])
}

/// `require(name)`: resolves `name` to source through the configured
/// [`ModuleSource`](crate::ModuleSource), compiles and runs the module body
/// once, caches its first return value as the module's exports (Lua's
/// `package.loaded`), and returns it. A later `require` of the same source
/// instance returns the cached value without re-running. A missing module, a
/// compile error, or an error raised by the module body all raise (unlike
/// `loadstring`, which returns `(nil, message)`). A module that requires itself
/// raises a deterministic circular-require error keyed by source instance.
pub(super) fn builtin_require(
    heap: &mut Heap,
    thread: &mut Thread,
    args: &[RawValue],
) -> Exec<Vec<RawValue>> {
    let Some(RawValue::String(handle)) = args.first().copied() else {
        return Err(err("bad argument #1 to 'require' (string expected)"));
    };
    let Some(name) = heap.string(handle).map(|s| s.bytes().to_vec()) else {
        return Err(err("require name is not resident"));
    };
    let requester = current_require_requester(heap, thread);
    if let Some(exports) = native_module_export_for_request(heap, requester.as_ref(), &name)? {
        return Ok(vec![exports]);
    }
    let Some(source_model) = heap.module_source() else {
        return Err(err("require is unavailable (no module source configured)"));
    };
    let id = match poll_source_once(source_model.resolve(requester.as_ref(), &name)) {
        SourcePoll::Ready(Ok(id)) => id,
        SourcePoll::Ready(Err(error)) => return Err(require_resolve_error(&error)),
        SourcePoll::Pending(_) => {
            return Err(err(ASYNC_REQUIRE_SYNC_ENTRY_ERROR));
        }
    };
    continue_require_after_resolve_sync(heap, thread, &source_model, &id, &requester)
}

pub fn start_require(
    heap: &mut Heap,
    thread: &mut Thread,
    args: &[RawValue],
    site: &RequireCallSite,
) -> Exec<RequireCallStep> {
    let Some(RawValue::String(handle)) = args.first().copied() else {
        return Err(err("bad argument #1 to 'require' (string expected)"));
    };
    // Copy the name out: the resolve/compile borrow nothing and the load needs
    // `&mut heap`, so the interned name must not stay borrowed across them.
    let Some(name) = heap.string(handle).map(|s| s.bytes().to_vec()) else {
        return Err(err("require name is not resident"));
    };
    let requester = current_require_requester(heap, thread);
    if let Some(exports) = native_module_export_for_request(heap, requester.as_ref(), &name)? {
        return Ok(RequireCallStep::Ready(vec![exports]));
    }
    let Some(source_model) = heap.module_source() else {
        return Err(err("require is unavailable (no module source configured)"));
    };
    match poll_source_once(source_model.resolve(requester.as_ref(), &name)) {
        SourcePoll::Ready(Ok(id)) => {
            continue_require_after_resolve(heap, thread, &source_model, id, &requester, site)
        }
        SourcePoll::Ready(Err(error)) => Err(require_resolve_error(&error)),
        SourcePoll::Pending(future) => Ok(RequireCallStep::Suspend(SuspendedRequire {
            stage: SuspendedRequireStage::Resolve {
                source: source_model,
                requester,
                future,
            },
            result_reg: site.result_reg,
            result_count: site.result_count,
            call_pc: 0,
            cleanup_end: site.cleanup_end,
            target: SuspendedTarget::Active,
        })),
    }
}

pub fn continue_require_after_resolve(
    heap: &mut Heap,
    thread: &mut Thread,
    source_model: &Arc<dyn crate::ModuleSource>,
    id: crate::ModuleId,
    requester: &Option<crate::ModuleId>,
    site: &RequireCallSite,
) -> Exec<RequireCallStep> {
    if let Some(exports) =
        native_module_export_after_resolve(heap, source_model, &id, requester.as_ref())?
    {
        return Ok(RequireCallStep::Ready(vec![exports]));
    }
    let epoch = source_model.epoch();
    let read_request = crate::ReadRequest::with_requester(&id, requester.as_ref());
    let instance = source_model.instance_key(read_request);
    let loading_key = crate::heap::ModuleCacheKey::new(instance.clone(), epoch);
    // Cache hit: the module already ran; return its exports without re-running.
    if let Some(cached) = heap.module_cache_get(&instance, epoch) {
        return Ok(RequireCallStep::Ready(vec![cached]));
    }

    if !heap.module_load_begin(&loading_key) {
        if thread_is_loading_module(thread, &loading_key) || thread.entry.is_none() {
            return Err(err(format!("circular require of module '{}'", id)));
        }
        return Ok(RequireCallStep::WaitForInFlight);
    }
    match poll_source_once(source_model.read_request(read_request)) {
        SourcePoll::Ready(Ok(source)) => {
            start_require_body(
                heap,
                thread,
                RequireBodyStart {
                    id,
                    instance,
                    epoch,
                    loading_key,
                },
                &source,
                site,
            )?;
            Ok(RequireCallStep::BodyStarted)
        }
        SourcePoll::Ready(Err(error)) => {
            heap.module_load_end(&loading_key);
            Err(require_read_error(&id, &error))
        }
        SourcePoll::Pending(future) => Ok(RequireCallStep::Suspend(SuspendedRequire {
            stage: SuspendedRequireStage::Read {
                id,
                instance,
                epoch,
                loading_key,
                future,
            },
            result_reg: site.result_reg,
            result_count: site.result_count,
            call_pc: 0,
            cleanup_end: site.cleanup_end,
            target: SuspendedTarget::Active,
        })),
    }
}

fn native_module_export_for_request(
    heap: &Heap,
    requester: Option<&crate::ModuleId>,
    request: &[u8],
) -> Exec<Option<RawValue>> {
    let Some(id) = ruau_source::resolve_request(None, request).ok() else {
        return Ok(None);
    };
    let Some(exports) = heap.native_module_export_get(&id) else {
        return Ok(None);
    };
    if native_source_collision(heap, requester, request, &id)? {
        return Err(native_module_source_collision_error(&id));
    }
    Ok(Some(exports))
}

fn native_module_export_after_resolve(
    heap: &Heap,
    source_model: &Arc<dyn crate::ModuleSource>,
    id: &crate::ModuleId,
    requester: Option<&crate::ModuleId>,
) -> Exec<Option<RawValue>> {
    let Some(exports) = heap.native_module_export_get(id) else {
        return Ok(None);
    };
    if source_has_module(source_model, id, requester)? {
        return Err(native_module_source_collision_error(id));
    }
    Ok(Some(exports))
}

fn native_source_collision(
    heap: &Heap,
    requester: Option<&crate::ModuleId>,
    request: &[u8],
    native_id: &crate::ModuleId,
) -> Exec<bool> {
    let Some(source_model) = heap.module_source() else {
        return Ok(false);
    };
    let resolved = match poll_source_once(source_model.resolve(requester, request)) {
        SourcePoll::Ready(Ok(id)) => id,
        SourcePoll::Ready(Err(crate::ModuleSourceError::MissingModule { .. })) => return Ok(false),
        SourcePoll::Ready(Err(crate::ModuleSourceError::Pending { .. }))
        | SourcePoll::Pending(_) => return Err(native_module_source_collision_error(native_id)),
        SourcePoll::Ready(Err(error)) => return Err(require_resolve_error(&error)),
    };
    if &resolved != native_id {
        return Ok(false);
    }
    source_has_module(&source_model, native_id, requester)
}

fn source_has_module(
    source_model: &Arc<dyn crate::ModuleSource>,
    id: &crate::ModuleId,
    requester: Option<&crate::ModuleId>,
) -> Exec<bool> {
    let request = crate::ReadRequest::with_requester(id, requester);
    match poll_source_once(source_model.read_request(request)) {
        SourcePoll::Ready(Ok(_)) => Ok(true),
        SourcePoll::Ready(Err(crate::ModuleSourceError::MissingModule { .. })) => Ok(false),
        SourcePoll::Ready(Err(crate::ModuleSourceError::Pending { .. }))
        | SourcePoll::Pending(_) => Err(native_module_source_collision_error(id)),
        SourcePoll::Ready(Err(error)) => Err(require_read_error(id, &error)),
    }
}

fn native_module_source_collision_error(id: &crate::ModuleId) -> crate::call::RaisedError {
    err_kind(
        format!("native module '{id}' collides with a configured module source"),
        RuntimeErrorKind::UnresolvedRequire,
    )
}

fn thread_is_loading_module(thread: &Thread, loading_key: &crate::heap::ModuleCacheKey) -> bool {
    thread.call_stack.iter().any(|entry| {
        matches!(
            entry,
            CallStackEntry::Require(require) if &require.loading_key == loading_key
        )
    })
}

fn continue_require_after_resolve_sync(
    heap: &mut Heap,
    thread: &mut Thread,
    source_model: &Arc<dyn crate::ModuleSource>,
    id: &crate::ModuleId,
    requester: &Option<crate::ModuleId>,
) -> Exec<Vec<RawValue>> {
    if let Some(exports) =
        native_module_export_after_resolve(heap, source_model, id, requester.as_ref())?
    {
        return Ok(vec![exports]);
    }
    let epoch = source_model.epoch();
    let read_request = crate::ReadRequest::with_requester(id, requester.as_ref());
    let instance = source_model.instance_key(read_request);
    let loading_key = crate::heap::ModuleCacheKey::new(instance.clone(), epoch);
    if let Some(cached) = heap.module_cache_get(&instance, epoch) {
        return Ok(vec![cached]);
    }
    if !heap.module_load_begin(&loading_key) {
        return Err(err(format!("circular require of module '{}'", id)));
    }
    let source = match poll_source_once(source_model.read_request(read_request)) {
        SourcePoll::Ready(Ok(source)) => source,
        SourcePoll::Ready(Err(error)) => {
            heap.module_load_end(&loading_key);
            return Err(require_read_error(id, &error));
        }
        SourcePoll::Pending(_) => {
            heap.module_load_end(&loading_key);
            return Err(err(ASYNC_REQUIRE_SYNC_ENTRY_ERROR));
        }
    };
    let exports = require_uncached_module_from_source(heap, thread, id, &source);
    heap.module_load_end(&loading_key);
    let exports = exports?;
    heap.module_cache_set(&instance, epoch, exports)
        .ok_or_else(|| err("out of memory caching a required module"))?;
    Ok(vec![exports])
}

pub fn start_require_body(
    heap: &mut Heap,
    thread: &mut Thread,
    module: RequireBodyStart,
    source: &[u8],
    site: &RequireCallSite,
) -> Exec<()> {
    let RequireBodyStart {
        id,
        instance,
        epoch,
        loading_key,
    } = module;
    let limits = heap.limits();
    let compiler = heap.runtime_compiler();
    let context = heap.runtime_compile_context().with_module_id(id.clone());
    let chunk = match compiler.compile(source, context) {
        Ok(chunk) => chunk,
        Err(message) => {
            heap.module_load_end(&loading_key);
            return Err(err(format!(
                "error compiling module '{}': {}",
                id,
                String::from_utf8_lossy(&message)
            )));
        }
    };
    let module =
        match load_module_with_limits(heap, &chunk, LoadMode::Validated, id.clone(), limits) {
            Ok(module) => module,
            Err(error) => {
                heap.module_load_end(&loading_key);
                return Err(err(format!("error loading module '{}': {error:?}", id)));
            }
        };
    if let Some(closure) = heap.closure_mut(module.main) {
        closure.env = thread.globals;
    }
    let proto = heap
        .closure(module.main)
        .map(|closure| closure.proto)
        .ok_or_else(|| {
            heap.module_load_end(&loading_key);
            heap.unpin(&module.pin);
            err("required module closure is not resident")
        })?;
    let max_stack = heap
        .proto(proto)
        .map(|proto| u32::from(proto.max_stack_size).max(1))
        .ok_or_else(|| {
            heap.module_load_end(&loading_key);
            heap.unpin(&module.pin);
            err("required module has no prototype")
        })?;
    let func_reg = thread
        .call_stack
        .iter()
        .rev()
        .find_map(|entry| entry.frame().map(|frame| frame.frame_top))
        .unwrap_or(thread.top);
    let base = func_reg + 1;
    let frame_top = base + max_stack;
    thread.stacks.ensure(frame_top).map_err(|_| {
        heap.module_load_end(&loading_key);
        heap.unpin(&module.pin);
        err_register_stack_oom()
    })?;
    thread.stacks.set(func_reg, RawValue::Function(module.main));
    reserve_call_entries(heap, thread, 2).inspect_err(|_error| {
        heap.module_load_end(&loading_key);
        heap.unpin(&module.pin);
    })?;
    thread.push_reserved_call_stack_entry(CallStackEntry::Require(RequireInfo {
        result_base: site.result_reg,
        result_count: site.result_count,
        saved_top: thread.top,
        cleanup_end: site.cleanup_end,
        instance,
        epoch,
        loading_key,
        module_pin: module.pin,
    }));
    thread.push_reserved_call_stack_entry(CallStackEntry::Frame(CallInfo {
        closure: module.main,
        proto,
        base,
        result_base: site.result_reg,
        frame_top,
        savedpc: 0,
        nresults: -1,
        varargs: crate::call::empty_varargs(heap),
    }));
    thread.top = frame_top;
    Ok(())
}

pub fn finish_require_read_error(
    heap: &mut Heap,
    id: &crate::ModuleId,
    loading_key: &crate::heap::ModuleCacheKey,
    error: &crate::ModuleSourceError,
) -> crate::call::RaisedError {
    heap.module_load_end(loading_key);
    require_read_error(id, error)
}

pub fn clear_require_loading(heap: &mut Heap, loading_key: &crate::heap::ModuleCacheKey) {
    heap.module_load_end(loading_key);
}

fn require_uncached_module_from_source(
    heap: &mut Heap,
    thread: &mut Thread,
    id: &crate::ModuleId,
    source: &[u8],
) -> Exec<RawValue> {
    let limits = heap.limits();
    let compiler = heap.runtime_compiler();
    let context = heap.runtime_compile_context().with_module_id(id.clone());
    let chunk = match compiler.compile(source, context) {
        Ok(chunk) => chunk,
        Err(message) => {
            return Err(err(format!(
                "error compiling module '{}': {}",
                id,
                String::from_utf8_lossy(&message)
            )));
        }
    };
    let module =
        match load_module_with_limits(heap, &chunk, LoadMode::Validated, id.clone(), limits) {
            Ok(module) => module,
            Err(error) => {
                return Err(err(format!("error loading module '{}': {error:?}", id)));
            }
        };
    // The module body resolves globals through the running thread's environment.
    if let Some(closure) = heap.closure_mut(module.main) {
        closure.env = thread.globals;
    }
    // Run the module body once; its first return value is the module's exports.
    let func = RawValue::Function(module.main);
    let outcome = protected_call(heap, thread, func, &[]);
    // The loader pin kept `module.main` rooted across the run; release it now. No
    // collection runs between here and the cache pin below on this synchronous path,
    // so the still-unpinned exports value stays live until it is cached.
    heap.unpin(&module.pin);
    let exports = {
        let values = outcome?;
        normalize_require_exports(values.first().copied())
    };
    Ok(exports)
}

pub fn normalize_require_exports(first: Option<RawValue>) -> RawValue {
    match first {
        Some(RawValue::Nil) | None => RawValue::Boolean(true),
        Some(value) => value,
    }
}

pub fn release_suspended_require(heap: &mut Heap, require: SuspendedRequire) {
    if let SuspendedRequireStage::Read { loading_key, .. } = require.stage {
        clear_require_loading(heap, &loading_key);
    }
}

pub fn require_resolve_error(error: &crate::ModuleSourceError) -> crate::call::RaisedError {
    err_kind(
        format!("error resolving module source: {error}"),
        RuntimeErrorKind::UnresolvedRequire,
    )
}

fn require_read_error(
    id: &crate::ModuleId,
    error: &crate::ModuleSourceError,
) -> crate::call::RaisedError {
    match error {
        crate::ModuleSourceError::MissingModule { .. } => {
            err_kind(error.to_string(), RuntimeErrorKind::UnresolvedRequire)
        }
        _ => err(format!("error reading module '{}': {error}", id)),
    }
}

fn current_require_requester(heap: &Heap, thread: &Thread) -> Option<crate::ModuleId> {
    let frame = thread
        .call_stack
        .iter()
        .rev()
        .filter_map(|entry| entry.frame())
        .next()?;
    let proto = heap.closure(frame.closure)?.proto;
    let proto = heap.proto(proto)?;
    if let Some(id) = &proto.module_id {
        return Some(id.clone());
    }
    let source = proto.source?;
    let bytes = heap.string(source)?.bytes();
    if matches!(bytes.first(), Some(b'=' | b'@')) {
        return None;
    }
    Some(std::str::from_utf8(bytes).map_or_else(
        |_| crate::ModuleId::from(bytes),
        crate::ModuleId::canonicalized,
    ))
}

/// The `(nil, message)` failure shape of `loadstring`; a non-resident interner yields a bare
/// `nil` rather than raising on the caller. The message gets the chunk's location prefix —
/// `chunk_id(name)` ahead of the compiler's `:line: text` — matching `luau_load`.
fn loadstring_error(heap: &mut Heap, chunk_name: &[u8], body: &[u8]) -> Vec<RawValue> {
    let mut message = crate::debug::chunk_id(chunk_name);
    message.extend_from_slice(body);
    match heap.intern_str(&message) {
        Some(handle) => vec![RawValue::Nil, RawValue::String(handle)],
        None => vec![RawValue::Nil],
    }
}
