//! The engine side of the host-call ABI.
//!
//! [`EngineContext`] is the concrete [`HostContext`] the engine hands a
//! low-level [`HostFunction`] during the synchronous part of a call: it exposes
//! the call arguments as branded [`HostValue`] borrow-views and allocates through
//! the accounted heap. Higher-level scoped host functions run through
//! [`ScopedHostFunction`] and receive the same scoped value model as
//! [`Vm::step`](crate::Vm::step).

use std::{any::Any, future::Future, marker::PhantomData, sync::Mutex};

use ruau_vm_api::{
    HeapId, HostContext, HostError, HostFunction, HostFuture, HostReturn, HostValue,
    HostValueRawExt, OwnedValue, RawValue, RegistryRef, RuntimeErrorKind, marker,
};
use tokio::sync::{mpsc, oneshot};

use crate::{
    heap::Heap,
    scope::{FromLuaMulti, IntoLuaMulti, IntoStash, MultiValue, RuntimeError, Scope, Stashed},
};

/// A synchronous host function that runs inside the engine-owned scoped value
/// model.
///
/// This is the high-level companion to the raw [`HostFunction`] ABI. A scoped
/// function receives typed, scope-branded arguments, can use `Scope` helpers
/// such as app data and the named registry, and returns values through
/// `IntoLuaMulti`-backed materialization without ever receiving raw handles.
pub trait ScopedHostFunction: Send + Sync {
    /// Runs the function for one host call.
    ///
    /// # Errors
    /// Returns a host-facing [`RuntimeError`] that is raised at the Lua call site.
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        args: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError>;
}

/// Wraps a short synchronous Rust function as a scoped host function.
///
/// This adapter is for bounded, non-blocking host work. It converts Lua
/// arguments through [`FromLuaMulti`], calls `f` with the active [`Scope`], and
/// converts the Rust return through [`IntoLuaMulti`]. Heavy work, blocking I/O,
/// and anything that must await belong on the async host path instead.
///
/// The adapter covers owned argument and return shapes that implement the
/// conversion traits for every scope lifetime. If a host must traffic in
/// borrowed handles such as `Table<'s>`, implement [`ScopedHostFunction`]
/// directly so the function can stay generic over `'s`.
pub fn scoped_host_fn<F, A, R>(f: F) -> Box<dyn ScopedHostFunction>
where
    F: for<'s> Fn(&Scope<'s>, A) -> Result<R, RuntimeError> + Send + Sync + 'static,
    A: for<'s> FromLuaMulti<'s> + 'static,
    R: for<'s> IntoLuaMulti<'s> + 'static,
{
    Box::new(ScopedHostFn {
        f,
        _marker: PhantomData,
    })
}

struct ScopedHostFn<F, A, R> {
    f: F,
    _marker: PhantomData<fn(A) -> R>,
}

impl<F, A, R> ScopedHostFunction for ScopedHostFn<F, A, R>
where
    F: for<'s> Fn(&Scope<'s>, A) -> Result<R, RuntimeError> + Send + Sync,
    A: for<'s> FromLuaMulti<'s>,
    R: for<'s> IntoLuaMulti<'s>,
{
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        args: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        let args = A::from_lua_multi(args, scope)?;
        (self.f)(scope, args)?.into_lua_multi(scope)
    }
}

/// The scoped context an async host future uses to request short VM re-entry
/// segments between off-lane awaits.
///
/// `AsyncHostContext` is cloneable and heap-free. Calling [`scope`](AsyncHostContext::scope)
/// sends a synchronous operation back to the async driver; the driver runs it
/// with a fresh [`Scope`] while the suspended host call is resident, then returns
/// an owned result to the host future.
#[derive(Clone)]
pub struct AsyncHostContext {
    request_tx: mpsc::UnboundedSender<HostRequest>,
}

impl AsyncHostContext {
    pub(crate) fn channel() -> (Self, HostRequests) {
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        (Self { request_tx }, request_rx)
    }

    /// Runs `f` inside the VM's scoped value model.
    ///
    /// The closure may use ordinary [`Scope`] helpers, including app data, named
    /// registry state, value construction, and synchronous nested calls. The
    /// returned value must satisfy [`IntoStash`], so a borrowed handle such as
    /// `Table<'_>` cannot escape the scoped segment; stash or copy it instead.
    ///
    /// The type gate also prevents holding a `Scope` across a later `.await`:
    ///
    /// ```text
    /// async fn bad(ctx: AsyncHostContext) -> Result<(), RuntimeError> {
    ///     let scope = ctx.scope(|scope| Ok(scope)).await?;
    ///     std::future::ready(()).await;
    ///     let _ = scope;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// Compile-fail coverage for this contract lives under
    /// `crates/ruau-vm/tests/ui/`.
    ///
    /// # Errors
    /// Returns the closure's [`RuntimeError`], or a runtime error if the host
    /// call is no longer attached to a live VM driver.
    pub async fn scope<F, R>(&self, f: F) -> Result<R, RuntimeError>
    where
        F: for<'s> FnOnce(&Scope<'s>) -> Result<R, RuntimeError> + Send + 'static,
        R: IntoStash + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let operation = Box::new(move |scope: &Scope<'_>| {
            f(scope).map(|value| Box::new(value) as Box<dyn Any + Send>)
        });
        self.request_tx
            .send(HostRequest::Scope(HostScopeRequest {
                operation,
                reply: reply_tx,
            }))
            .map_err(|_| RuntimeError::runtime("async host scope request is no longer attached"))?;
        let boxed = reply_rx
            .await
            .map_err(|_| RuntimeError::runtime("async host scope request was cancelled"))??;
        boxed
            .downcast::<R>()
            .map(|value| *value)
            .map_err(|_| RuntimeError::runtime("async host scope request returned the wrong type"))
    }

    /// Calls a registry-rooted Lua callback on the VM's async protected driver.
    ///
    /// The callback is invoked from a short-lived rooted callback thread, so the
    /// suspended caller's registers are not reused. Arguments are converted inside
    /// a scoped segment, and successful results plus catchable error values are
    /// returned as owned [`OwnedValue`]s. Fatal control-flow errors such as
    /// cancellation and deadline return as the outer [`RuntimeError`].
    ///
    /// Re-entry nests: the callback may call sync host functions and async host
    /// functions, including ones whose own `AsyncHostContext` re-enters with
    /// `call_protected` again while this call is pending. Each nesting level
    /// charges one unit of `Limits::max_native_depth`, so unbounded recursive
    /// re-entry fails closed with a catchable
    /// `"stack overflow (async host re-entry)"` error instead of exhausting the
    /// Rust stack. The full nesting matrix, including the unsupported
    /// cross-context case, is documented on the async driver module
    /// (`driver.rs`).
    ///
    /// # Errors
    /// Returns an outer [`RuntimeError`] if argument conversion fails, the
    /// stashed value no longer names a function, the driver is gone, or the
    /// protected callback raises a fatal uncatchable error.
    pub async fn call_protected<A>(
        &self,
        callback: &Stashed<marker::Closure>,
        args: A,
    ) -> HostProtectedCallResult
    where
        A: for<'s> IntoLuaMulti<'s> + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let convert_args = Box::new(move |scope: &Scope<'_>| {
            args.into_lua_multi(scope)
                .map(MultiValue::into_raw_vec)
                .map(ProtectedArgs::from_raw)
        });
        self.request_tx
            .send(HostRequest::ProtectedCall(HostProtectedCallRequest {
                callback: callback.clone(),
                convert_args,
                reply: reply_tx,
            }))
            .map_err(|_| {
                RuntimeError::runtime("async host protected call request is no longer attached")
            })?;
        reply_rx
            .await
            .map_err(|_| RuntimeError::runtime("async host protected call request was cancelled"))?
    }
}

/// A script error caught by [`AsyncHostContext::call_protected`].
#[derive(Clone, Debug)]
pub struct HostScriptError {
    value: OwnedValue,
    kind: RuntimeErrorKind,
    traceback: Option<String>,
}

impl HostScriptError {
    pub(crate) fn new(
        value: OwnedValue,
        kind: RuntimeErrorKind,
        traceback: Option<String>,
    ) -> Self {
        Self {
            value,
            kind,
            traceback,
        }
    }

    /// The owned Lua error value surfaced by the protected call.
    #[must_use]
    pub fn value(&self) -> &OwnedValue {
        &self.value
    }

    /// The failure category carried to runner metrics.
    #[must_use]
    pub fn kind(&self) -> RuntimeErrorKind {
        self.kind
    }

    /// The captured traceback, if available.
    #[must_use]
    pub fn traceback(&self) -> Option<&str> {
        self.traceback.as_deref()
    }
}

pub type HostProtectedCallResult = Result<Result<HostReturn, HostScriptError>, RuntimeError>;

type ProtectedArgsValues = Vec<RawValue>;

pub struct ProtectedArgs {
    values: ProtectedArgsValues,
}

impl ProtectedArgs {
    pub(crate) fn from_raw(values: Vec<RawValue>) -> Self {
        Self { values }
    }

    pub(crate) fn into_raw(self) -> Vec<RawValue> {
        self.values
    }
}

pub type ProtectedArgsOperation =
    Box<dyn for<'s> FnOnce(&Scope<'s>) -> Result<ProtectedArgs, RuntimeError> + Send + 'static>;

pub enum HostRequest {
    Scope(HostScopeRequest),
    ProtectedCall(HostProtectedCallRequest),
}

type ScopeOperation = Box<
    dyn for<'s> FnOnce(&Scope<'s>) -> Result<Box<dyn Any + Send>, RuntimeError> + Send + 'static,
>;

pub struct HostScopeRequest {
    operation: ScopeOperation,
    reply: oneshot::Sender<Result<Box<dyn Any + Send>, RuntimeError>>,
}

impl HostScopeRequest {
    pub(crate) fn run<'s>(self, scope: &Scope<'s>) {
        let result = (self.operation)(scope);
        drop(self.reply.send(result));
    }
}

pub struct HostProtectedCallRequest {
    pub(crate) callback: Stashed<marker::Closure>,
    pub(crate) convert_args: ProtectedArgsOperation,
    pub(crate) reply: oneshot::Sender<HostProtectedCallResult>,
}

pub type HostRequests = mpsc::UnboundedReceiver<HostRequest>;

/// A high-level asynchronous host function.
///
/// The synchronous prelude runs with a [`Scope`] so arguments can be converted
/// and heap values can be copied or stashed before the async future is created.
/// The returned future may await off-lane work and use [`AsyncHostContext::scope`] for
/// short scoped VM segments.
///
/// Files, sockets, off-heap buffers, and other external resources remain host
/// capabilities, not VM-owned heap objects. Own them with RAII guards inside the
/// future or explicit host session state so cancellation/deadline cleanup can
/// release them by dropping the future.
///
/// Borrowed handles are not valid async-host arguments; copy or stash them
/// inside a scoped segment before the future is created:
///
/// ```text
/// let _host = async_host_fn(|_ctx, table: Table<'_>| async move {
///     let _ = table;
///     Ok::<_, RuntimeError>(HostReturn::default())
/// });
/// ```
///
/// Compile-fail coverage for this contract lives under
/// `crates/ruau-vm/tests/ui/`.
pub trait AsyncHostFunction: Send + Sync {
    /// Starts one async host call.
    ///
    /// # Errors
    /// Returns a scoped host error raised at the Lua call site before the async
    /// future starts.
    fn call<'s>(
        &self,
        ctx: AsyncHostContext,
        scope: &Scope<'s>,
        args: MultiValue<'s>,
    ) -> Result<HostFuture, RuntimeError>;
}

/// Wraps an async Rust function as an async scoped host function.
pub fn async_host_fn<F, A, Fut>(f: F) -> Box<dyn AsyncHostFunction>
where
    F: Fn(AsyncHostContext, A) -> Fut + Send + Sync + 'static,
    A: for<'s> FromLuaMulti<'s> + Send + 'static,
    Fut: Future<Output = Result<HostReturn, RuntimeError>> + Send + 'static,
{
    Box::new(AsyncHostFn {
        f,
        _marker: PhantomData,
    })
}

/// Wraps a one-shot async Rust function as an async scoped host function.
///
/// The wrapped function may consume captured resources on its first call. Later
/// calls fail with a catchable runtime error instead of requiring the host to
/// hand-roll an `Arc<Mutex<Option<_>>>` guard.
pub fn async_once_host_fn<F, A, Fut>(f: F) -> Box<dyn AsyncHostFunction>
where
    F: FnOnce(AsyncHostContext, A) -> Fut + Send + 'static,
    A: for<'s> FromLuaMulti<'s> + Send + 'static,
    Fut: Future<Output = Result<HostReturn, RuntimeError>> + Send + 'static,
{
    Box::new(AsyncOnceHostFn {
        f: Mutex::new(Some(f)),
        _marker: PhantomData,
    })
}

struct AsyncHostFn<F, A> {
    f: F,
    _marker: PhantomData<fn(A)>,
}

impl<F, A, Fut> AsyncHostFunction for AsyncHostFn<F, A>
where
    F: Fn(AsyncHostContext, A) -> Fut + Send + Sync,
    A: for<'s> FromLuaMulti<'s> + Send,
    Fut: Future<Output = Result<HostReturn, RuntimeError>> + Send + 'static,
{
    fn call<'s>(
        &self,
        ctx: AsyncHostContext,
        scope: &Scope<'s>,
        args: MultiValue<'s>,
    ) -> Result<HostFuture, RuntimeError> {
        let args = A::from_lua_multi(args, scope)?;
        let future = (self.f)(ctx, args);
        Ok(Box::pin(async move {
            future.await.map_err(|error| {
                let (message, kind, payload, script_fields) = error.into_error_parts();
                HostError {
                    message,
                    kind,
                    payload,
                    script_fields,
                }
            })
        }))
    }
}

struct AsyncOnceHostFn<F, A> {
    f: Mutex<Option<F>>,
    _marker: PhantomData<fn(A)>,
}

impl<F, A, Fut> AsyncHostFunction for AsyncOnceHostFn<F, A>
where
    F: FnOnce(AsyncHostContext, A) -> Fut + Send,
    A: for<'s> FromLuaMulti<'s> + Send,
    Fut: Future<Output = Result<HostReturn, RuntimeError>> + Send + 'static,
{
    fn call<'s>(
        &self,
        ctx: AsyncHostContext,
        scope: &Scope<'s>,
        args: MultiValue<'s>,
    ) -> Result<HostFuture, RuntimeError> {
        let args = A::from_lua_multi(args, scope)?;
        let f = self
            .f
            .lock()
            .map_err(|_| RuntimeError::poisoned())?
            .take()
            .ok_or_else(|| {
                RuntimeError::runtime("one-shot async host function was already called")
            })?;
        let future = f(ctx, args);
        Ok(Box::pin(async move {
            future.await.map_err(|error| {
                let (message, kind, payload, script_fields) = error.into_error_parts();
                HostError {
                    message,
                    kind,
                    payload,
                    script_fields,
                }
            })
        }))
    }
}

/// The two host-call implementations the engine can install behind a Lua
/// closure.
pub enum HostCallable {
    /// The stable low-level ABI from `ruau-vm-api`.
    Raw(Box<dyn HostFunction>),
    /// The engine-owned scoped embedding API.
    Scoped(Box<dyn ScopedHostFunction>),
    /// The engine-owned async scoped embedding API.
    Async(Box<dyn AsyncHostFunction>),
}

/// A high-level callable registered through [`ruau_vm_api::ModuleBuilder`].
///
/// The stable ABI only sees this as an opaque value; the engine downcasts it
/// during module installation and allocates the matching host closure.
#[allow(private_interfaces, unnameable_types)]
pub enum ModuleHostCallable {
    /// The engine-owned scoped embedding API.
    Scoped(Box<dyn ScopedHostFunction>),
    /// The engine-owned async scoped embedding API.
    Async(Box<dyn AsyncHostFunction>),
}

/// Boxes a scoped host function as the opaque module-callable payload expected
/// by [`ruau_vm_api::ModuleBuilder::host_callable`].
pub fn scoped_module_host_callable(
    f: Box<dyn ScopedHostFunction>,
) -> Box<dyn std::any::Any + Send + Sync> {
    Box::new(ModuleHostCallable::Scoped(f))
}

/// Boxes an async host function as the opaque module-callable payload expected
/// by [`ruau_vm_api::ModuleBuilder::host_callable`].
pub fn async_module_host_callable(
    f: Box<dyn AsyncHostFunction>,
) -> Box<dyn std::any::Any + Send + Sync> {
    Box::new(ModuleHostCallable::Async(f))
}

/// The borrow a host function receives for the synchronous part of its call.
///
/// It holds the VM borrow (`&mut Heap`) and the call arguments. A `HostCall`
/// the function returns is unbranded (`Ready`) or `'static` (`Pending`), so it
/// cannot smuggle this borrow — or a heap handle read through it — past the
/// call.
pub struct EngineContext<'a> {
    heap: &'a mut Heap,
    args: &'a [RawValue],
    pins: Vec<RegistryRef>,
}

impl<'a> EngineContext<'a> {
    /// Builds the context for a host call with the given arguments.
    pub(crate) fn new(heap: &'a mut Heap, args: &'a [RawValue]) -> Self {
        Self {
            heap,
            args,
            pins: Vec::new(),
        }
    }

    /// Returns the registry pins minted during this host call. The async driver
    /// owns these leases and releases any unconsumed pins when the call finishes.
    pub fn into_pins(mut self) -> Vec<RegistryRef> {
        std::mem::take(&mut self.pins)
    }
}

impl Drop for EngineContext<'_> {
    fn drop(&mut self) {
        for reference in self.pins.drain(..) {
            self.heap.unpin(&reference);
        }
    }
}

impl HostContext for EngineContext<'_> {
    fn heap_id(&self) -> HeapId {
        self.heap.id
    }

    fn arg_count(&self) -> usize {
        self.args.len()
    }

    fn arg(&self, index: usize) -> Option<HostValue<'_>> {
        self.args.get(index).copied().map(HostValue::from_raw)
    }

    fn pin_arg(&mut self, index: usize) -> Option<RegistryRef> {
        let value = self.args.get(index).copied()?;
        let reference = self.heap.pin(value)?;
        self.pins.push(reference.clone());
        Some(reference)
    }
}
