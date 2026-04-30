//! Luau state management.
//!
//! This module provides the main [`Luau`] state handle together with state-specific
//! configuration and garbage collector controls.

use std::{
    any::TypeId,
    cell::{BorrowError, BorrowMutError, Cell, RefCell},
    ffi::CString,
    fmt, future,
    marker::PhantomData,
    mem,
    ops::Deref,
    os::raw::{c_char, c_int, c_void},
    panic::Location,
    ptr::{self, NonNull},
    rc::{Rc, Weak},
    result::Result as StdResult,
    task::Poll,
};

pub use extra::ExtraData;
pub use raw::RawLuau;
pub use util::callback_error_ext;

use crate::{
    buffer::Buffer,
    chunk::{AsChunk, Chunk, ChunkMode, Compiler},
    debug::Debug,
    error::{Error, Result},
    function::Function,
    memory::MemoryState,
    scope::Scope,
    stdlib::StdLib,
    string::LuauString,
    table::Table,
    thread::Thread,
    traits::{FromLuau, FromLuauMulti, IntoLuau, IntoLuauMulti},
    types::{
        AppDataRef, AppDataRefMut, Integer, InterruptCallback, LightUserData, PrimitiveType, RegistryKey,
        ThreadCollectionCallback, VmState, XRc,
    },
    userdata_impl::{AnyUserData, UserData, UserDataProxy, UserDataRegistry, UserDataStorage},
    util::{StackGuard, assert_stack, check_stack, push_string, rawset_field},
    value::{Nil, Value},
};
/// Top level Luau struct which represents an instance of Luau VM.
pub struct Luau {
    pub(self) raw: NonNull<RawLuau>,
    pub(self) live: Rc<Cell<bool>>,
    // Controls whether garbage collection should be run on drop
    pub(self) collect_garbage: bool,
    _not_send_sync: PhantomData<Rc<()>>,
}

struct ChunkInput<T>(T);

impl<T: AsChunk> ChunkInput<T> {
    fn into_chunk<'a>(self, lua: &Luau, location: &'static Location<'static>) -> Chunk<'a>
    where
        T: 'a,
    {
        let chunk = self.0;
        Chunk {
            lua: lua.weak(),
            name: chunk
                .name()
                .unwrap_or_else(|| format!("@{}:{}", location.file(), location.line())),
            env: chunk.environment(lua),
            mode: ChunkMode::Text,
            source: chunk.source(),
            compiler: unsafe { (*lua.raw().extra.get()).compiler.clone() },
        }
    }
}

/// Weak reference to Luau instance.
///
/// This can used to prevent circular references between Luau and Rust objects.
#[derive(Clone)]
pub struct WeakLuau {
    raw: NonNull<RawLuau>,
    live: Weak<Cell<bool>>,
    _not_send_sync: PhantomData<Rc<()>>,
}

pub struct LuauLiveGuard {
    raw: NonNull<RawLuau>,
    live: Weak<Cell<bool>>,
    _not_send_sync: PhantomData<Rc<()>>,
}

/// Tuning parameters for the incremental GC collector.
///
/// These parameters map to Luau's `LUA_GCSETGOAL`, `LUA_GCSETSTEPMUL`, and
/// `LUA_GCSETSTEPSIZE` controls.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default)]
pub struct GcIncParams {
    /// Target heap size as a percentage of live data, controlling how aggressively
    /// the GC reclaims memory (`LUA_GCSETGOAL`).
    pub goal: Option<c_int>,

    /// GC work performed per unit of memory allocated.
    pub step_multiplier: Option<c_int>,

    /// Granularity of each GC step (see Luau reference for details).
    pub step_size: Option<c_int>,
}

impl GcIncParams {
    /// Sets the `goal` parameter.
    pub fn goal(mut self, v: c_int) -> Self {
        self.goal = Some(v);
        self
    }

    /// Sets the `step_multiplier` parameter.
    pub fn step_multiplier(mut self, v: c_int) -> Self {
        self.step_multiplier = Some(v);
        self
    }

    /// Sets the `step_size` parameter.
    pub fn step_size(mut self, v: c_int) -> Self {
        self.step_size = Some(v);
        self
    }
}

/// Luau garbage collector (GC) operating mode.
///
/// Use [`Luau::gc_set_mode`] to switch the collector mode and/or tune its parameters.
#[non_exhaustive]
#[derive(Clone, Copy, Debug)]
pub enum GcMode {
    /// Incremental mark-and-sweep
    Incremental(GcIncParams),
}

/// Thin view over the Luau registry for one [`Luau`] instance.
///
/// Obtain one via [`Luau::registry`]. The registry is a Luau-side, GC-rooted store that any
/// instance sharing the underlying main state can access. Use string keys when you want a
/// stable name and `RegistryKey` when you want a Rust-side handle.
pub struct Registry<'a> {
    lua: &'a Luau,
}

/// RAII guard that restores a replaced app-data value when dropped.
pub struct ScopedAppData<T: 'static> {
    lua: WeakLuau,
    previous: Option<T>,
}

impl<T: 'static> Drop for ScopedAppData<T> {
    fn drop(&mut self) {
        let Some(live) = self.lua.live.upgrade() else {
            return;
        };
        if !live.get() {
            return;
        }

        let raw = unsafe { self.lua.raw.as_ref() };
        let extra = unsafe { &*raw.extra.get() };
        match self.previous.take() {
            Some(previous) => {
                extra.app_data.insert(previous);
            }
            None => {
                extra.app_data.remove::<T>();
            }
        }
    }
}

/// RAII guard that restores the previous interrupt handler when dropped.
pub struct ScopedInterrupt {
    lua: WeakLuau,
    previous: Option<InterruptCallback>,
}

impl Drop for ScopedInterrupt {
    fn drop(&mut self) {
        let Some(live) = self.lua.live.upgrade() else {
            return;
        };
        if !live.get() {
            return;
        }

        let raw = unsafe { self.lua.raw.as_ref() };
        unsafe {
            (*raw.extra.get()).interrupt_callback = self.previous.take();
            (*ffi::lua_callbacks(raw.main_state())).interrupt = (*raw.extra.get())
                .interrupt_callback
                .as_ref()
                .map(|_| Luau::interrupt_proc as unsafe extern "C-unwind" fn(*mut ffi::lua_State, c_int));
        }
    }
}

impl Registry<'_> {
    /// Sets a value in the registry under a string key.
    pub fn named_set(&self, key: &str, t: impl IntoLuau) -> Result<()> {
        let lua = self.lua.raw();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 5)?;
            lua.push(t)?;
            rawset_field(state, ffi::LUA_REGISTRYINDEX, key)
        }
    }

    /// Gets a value from the registry by its string key.
    pub fn named_get<T: FromLuau>(&self, key: &str) -> Result<T> {
        let lua = self.lua.raw();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 3)?;
            let protect = !lua.unlikely_memory_error();
            push_string(state, key.as_bytes(), protect)?;
            ffi::lua_rawget(state, ffi::LUA_REGISTRYINDEX);
            T::from_stack(-1, &lua.ctx())
        }
    }

    /// Removes a string-keyed registry value (sets it to `nil`).
    #[inline]
    pub fn named_remove(&self, key: &str) -> Result<()> {
        self.named_set(key, Nil)
    }

    /// Stores a value in the registry and returns a [`RegistryKey`] handle to it.
    ///
    /// This value will be available to Rust from all Luau instances which share the same main
    /// state.
    ///
    /// Be warned, garbage collection of values held inside the registry is not automatic, see
    /// [`RegistryKey`] for more details. However, dropped [`RegistryKey`]s are automatically
    /// reused to store new values.
    pub fn insert(&self, t: impl IntoLuau) -> Result<RegistryKey> {
        let lua = self.lua.raw();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 4)?;

            lua.push(t)?;

            let unref_list = (*lua.extra.get()).registry_unref_list.clone();

            // Check if the value is nil (no need to store it in the registry)
            if ffi::lua_isnil(state, -1) != 0 {
                return Ok(RegistryKey::new(ffi::LUA_REFNIL, unref_list));
            }

            // Try to reuse previously allocated slot
            let free_registry_id = unref_list.borrow_mut().as_mut().and_then(|x| x.pop());
            if let Some(registry_id) = free_registry_id {
                // It must be safe to replace the value without triggering memory error
                ffi::lua_rawseti(state, ffi::LUA_REGISTRYINDEX, registry_id as Integer);
                return Ok(RegistryKey::new(registry_id, unref_list));
            }

            // Allocate a new RegistryKey slot
            let registry_id = if lua.unlikely_memory_error() {
                ffi::luaL_ref(state, ffi::LUA_REGISTRYINDEX)
            } else {
                protect_lua!(state, 1, 0, |state| {
                    ffi::luaL_ref(state, ffi::LUA_REGISTRYINDEX)
                })?
            };
            Ok(RegistryKey::new(registry_id, unref_list))
        }
    }

    /// Looks up a registry value by its [`RegistryKey`] handle.
    ///
    /// Any Luau instance which shares the underlying main state may call this method to get a
    /// value previously placed by [`Registry::insert`].
    pub fn get<T: FromLuau>(&self, key: &RegistryKey) -> Result<T> {
        let lua = self.lua.raw();
        if !lua.owns_registry_value(key) {
            return Err(Error::MismatchedRegistryKey);
        }

        let state = lua.state();
        match key.id() {
            ffi::LUA_REFNIL => T::from_luau(Value::Nil, self.lua),
            registry_id => unsafe {
                let _sg = StackGuard::new(state);
                check_stack(state, 1)?;

                ffi::lua_rawgeti(state, ffi::LUA_REGISTRYINDEX, registry_id as Integer);
                T::from_stack(-1, &lua.ctx())
            },
        }
    }

    /// Removes the registry value referenced by the given [`RegistryKey`].
    pub fn remove(&self, key: RegistryKey) -> Result<()> {
        let lua = self.lua.raw();
        if !lua.owns_registry_value(&key) {
            return Err(Error::MismatchedRegistryKey);
        }

        unsafe { ffi::luaL_unref(lua.state(), ffi::LUA_REGISTRYINDEX, key.take()) };
        Ok(())
    }

    /// Replaces the value referenced by `key` in-place.
    ///
    /// The identifier inside [`RegistryKey`] may be changed to a new value.
    pub fn replace(&self, key: &mut RegistryKey, t: impl IntoLuau) -> Result<()> {
        let lua = self.lua.raw();
        if !lua.owns_registry_value(key) {
            return Err(Error::MismatchedRegistryKey);
        }

        let t = t.into_luau(self.lua)?;

        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 2)?;

            match (t, key.id()) {
                (Value::Nil, ffi::LUA_REFNIL) => {
                    // Do nothing, no need to replace nil with nil
                }
                (Value::Nil, registry_id) => {
                    // Remove the value
                    ffi::luaL_unref(state, ffi::LUA_REGISTRYINDEX, registry_id);
                    key.set_id(ffi::LUA_REFNIL);
                }
                (value, ffi::LUA_REFNIL) => {
                    // Allocate a new `RegistryKey`
                    let new_key = self.insert(value)?;
                    key.set_id(new_key.take());
                }
                (value, registry_id) => {
                    // It must be safe to replace the value without triggering memory error
                    lua.push_value(&value)?;
                    ffi::lua_rawseti(state, ffi::LUA_REGISTRYINDEX, registry_id as Integer);
                }
            }
        }
        Ok(())
    }

    /// Returns true if `key` was created by a `Luau` sharing this main state.
    #[inline]
    pub fn owns(&self, key: &RegistryKey) -> bool {
        self.lua.raw().owns_registry_value(key)
    }

    /// Removes any registry values whose [`RegistryKey`]s have been dropped.
    ///
    /// Unlike normal handle values, [`RegistryKey`]s do not auto-clean on `Drop`. Call this
    /// periodically (or after a known burst of `RegistryKey` drops) to reclaim the slots.
    pub fn expire(&self) {
        let lua = self.lua.raw();
        let state = lua.state();
        unsafe {
            let mut unref_list = (*lua.extra.get()).registry_unref_list.borrow_mut();
            let unref_list = unref_list.replace(Vec::new());
            for id in ruau_expect!(unref_list, "unref list is not set") {
                ffi::luaL_unref(state, ffi::LUA_REGISTRYINDEX, id);
            }
        }
    }
}

/// Boxed thread-creation callback invoked by the Luau `userthread` C hook.
pub type ThreadCreateFn = Box<dyn Fn(&crate::Luau, crate::Thread) -> crate::Result<()> + 'static>;
/// Boxed thread-collection callback invoked by the Luau `userthread` C hook.
///
/// Collection runs after the Luau thread is no longer safe to rehydrate as a `Thread`, so the
/// callback receives only the thread's `LightUserData` identity token.
pub type ThreadCollectFn = Box<dyn Fn(LightUserData) + 'static>;

/// Thread lifecycle callbacks installed on the Luau VM's `userthread` C hook.
///
/// Set both fields together via [`Luau::set_thread_callbacks`]. Either field may be left
/// `None` to leave the corresponding slot unset.
#[derive(Default)]
pub struct ThreadCallbacks {
    /// Runs when a new Luau thread is created.
    pub on_create: Option<ThreadCreateFn>,
    /// Runs when a Luau thread is destroyed. Must be non-panicking; panics abort the program.
    pub on_collect: Option<ThreadCollectFn>,
}

/// Controls Luau interpreter behavior such as Rust panics handling.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct LuauOptions {
    /// Catch Rust panics when using [`pcall`] or [`xpcall`].
    ///
    /// If disabled, wraps these functions and automatically resumes panic if found.
    /// If enabled, keeps [`pcall`]/[`xpcall`] unmodified.
    /// Panics are still automatically resumed if returned to the Rust side.
    ///
    /// Default: **true**
    ///
    /// [`pcall`]: https://luau.org/library#global-functions
    /// [`xpcall`]: https://luau.org/library#global-functions
    pub catch_rust_panics: bool,

    /// Max size of thread (coroutine) object pool used to execute asynchronous functions.
    ///
    /// Default: **0** (disabled)
    ///
    /// This maps to Luau's `lua_resetthread` C API.
    pub thread_pool_size: usize,

    /// Enables Luau sandbox mode at construction.
    ///
    /// Sandbox mode marks libraries and globals read-only and routes script writes through a
    /// per-VM proxy. Sandbox state is fixed at construction; it cannot be toggled on a live VM.
    ///
    /// Default: **false**
    pub sandbox: bool,
}

impl Default for LuauOptions {
    fn default() -> Self {
        const { Self::new() }
    }
}

impl LuauOptions {
    /// Returns a new instance of `LuauOptions` with default parameters.
    pub const fn new() -> Self {
        Self {
            catch_rust_panics: true,
            thread_pool_size: 0,
            sandbox: false,
        }
    }

    /// Sets [`catch_rust_panics`] option.
    ///
    /// [`catch_rust_panics`]: #structfield.catch_rust_panics
    #[must_use]
    pub const fn catch_rust_panics(mut self, enabled: bool) -> Self {
        self.catch_rust_panics = enabled;
        self
    }

    /// Sets [`thread_pool_size`] option.
    ///
    /// [`thread_pool_size`]: #structfield.thread_pool_size
    #[must_use]
    pub const fn thread_pool_size(mut self, size: usize) -> Self {
        self.thread_pool_size = size;
        self
    }

    /// Sets [`sandbox`] option.
    ///
    /// [`sandbox`]: #structfield.sandbox
    #[must_use]
    pub const fn sandbox(mut self, enabled: bool) -> Self {
        self.sandbox = enabled;
        self
    }
}

impl Drop for Luau {
    fn drop(&mut self) {
        if self.collect_garbage {
            drop(self.gc_collect());
            self.live.set(false);
            unsafe {
                drop(Box::from_raw(self.raw.as_ptr()));
            }
        }
    }
}

impl fmt::Debug for Luau {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Luau({:p})", self.raw().state())
    }
}

impl Default for Luau {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

unsafe extern "C" fn run_thread_collection_callback(
    callback: *const ThreadCollectionCallback,
    value: *mut ffi::lua_State,
) {
    (*callback)(LightUserData(value as _));
}

impl Luau {
    /// Creates a new Luau state and loads the **safe** subset of the standard libraries.
    ///
    /// # Safety
    /// The created Luau state will have _some_ safety guarantees and will not allow to load unsafe
    /// standard libraries.
    ///
    /// See [`StdLib`] documentation for a list of unsafe modules that cannot be loaded.
    pub fn new() -> Self {
        ruau_expect!(
            Self::new_with(StdLib::ALL_SAFE, LuauOptions::default()),
            "Cannot create a Luau state"
        )
    }

    /// Creates a new Luau state and loads the specified safe subset of the standard libraries.
    ///
    /// Use the [`StdLib`] flags to specify the libraries you want to load.
    ///
    /// # Safety
    /// The created Luau state will have _some_ safety guarantees and will not allow to load unsafe
    /// standard libraries.
    ///
    /// See [`StdLib`] documentation for a list of unsafe modules that cannot be loaded.
    pub fn new_with(libs: StdLib, options: LuauOptions) -> Result<Self> {
        let lua = unsafe { Self::inner_new(libs, options) };

        lua.raw().mark_safe();

        if options.sandbox {
            lua.sandbox(true)?;
        }

        Ok(lua)
    }

    /// Creates a new Luau state with required `libs` and `options`
    unsafe fn inner_new(libs: StdLib, options: LuauOptions) -> Self {
        let (raw, live) = RawLuau::new(libs, &options);
        let lua = Self {
            raw,
            live,
            collect_garbage: true,
            _not_send_sync: PhantomData,
        };

        ruau_expect!(lua.configure_luau(), "Error configuring Luau");

        lua
    }

    /// Loads the specified subset of the standard libraries into an existing Luau state.
    ///
    /// Use the [`StdLib`] flags to specify the libraries you want to load.
    pub fn load_std_libs(&self, libs: StdLib) -> Result<()> {
        unsafe { self.raw().load_std_libs(libs) }
    }

    /// Enables (or disables) sandbox mode on this Luau instance.
    ///
    /// This method, in particular:
    /// - Set all libraries to read-only
    /// - Set all builtin metatables to read-only
    /// - Set globals to read-only (and activates safeenv)
    /// - Setup local environment table that performs writes locally and proxies reads to the global
    ///   environment.
    /// - Allow only `count` mode in `collectgarbage` function.
    ///
    /// Crate-internal: configured at construction via [`LuauOptions::sandbox`].
    pub(crate) fn sandbox(&self, enabled: bool) -> Result<()> {
        let lua = self.raw();
        unsafe {
            if (*lua.extra.get()).sandboxed != enabled {
                let state = lua.main_state();
                check_stack(state, 3)?;
                protect_lua!(state, 0, 0, |state| {
                    if enabled {
                        ffi::luaL_sandbox(state, 1);
                        ffi::luaL_sandboxthread(state);
                    } else {
                        // Restore original `LUA_GLOBALSINDEX`
                        ffi::lua_xpush(lua.ref_thread(), state, ffi::LUA_GLOBALSINDEX);
                        ffi::lua_replace(state, ffi::LUA_GLOBALSINDEX);
                        ffi::luaL_sandbox(state, 0);
                    }
                })?;
                (*lua.extra.get()).sandboxed = enabled;
            }
            Ok(())
        }
    }

    /// Sets an interrupt function that will periodically be called by Luau VM.
    ///
    /// Any Luau code is guaranteed to call this handler "eventually"
    /// (in practice this can happen at any function call or at any loop iteration).
    /// This is similar to `Luau::set_hook` but in more simplified form.
    ///
    /// The provided interrupt function can error, and this error will be propagated through
    /// the Luau code that was executing at the time the interrupt was triggered.
    /// Also this can be used to implement continuous execution limits by instructing Luau VM to
    /// yield by returning [`VmState::Yield`]. The yield will happen only at yieldable points
    /// of execution (not across metamethod/C-call boundaries).
    ///
    /// # Example
    ///
    /// Periodically yield Luau VM to suspend execution.
    ///
    /// ```
    /// # use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
    /// # use ruau::{Luau, Result, ThreadStatus, VmState};
    /// # fn main() -> Result<()> {
    /// let lua = Luau::new();
    /// let count = Arc::new(AtomicU64::new(0));
    /// lua.set_interrupt(move |_| {
    ///     if count.fetch_add(1, Ordering::Relaxed) % 2 == 0 {
    ///         return Ok(VmState::Yield);
    ///     }
    ///     Ok(VmState::Continue)
    /// });
    ///
    /// let co = lua.create_thread(
    ///     lua.load(r#"
    ///         local b = 0
    ///         for _, x in ipairs({1, 2, 3}) do b += x end
    ///     "#)
    ///     .into_function()?,
    /// )?;
    /// while co.status() == ThreadStatus::Resumable {
    ///     co.resume::<()>(())?;
    /// }
    /// # Ok(())
    /// # }
    ///
    /// ```
    pub fn set_interrupt<F>(&self, callback: F)
    where
        F: Fn(&Self) -> Result<VmState> + 'static,
    {
        // Set interrupt callback
        let lua = self.raw();
        unsafe {
            (*lua.extra.get()).interrupt_callback = Some(XRc::new(callback));
            (*ffi::lua_callbacks(lua.main_state())).interrupt = Some(Self::interrupt_proc);
        }
    }

    unsafe extern "C-unwind" fn interrupt_proc(state: *mut ffi::lua_State, gc: c_int) {
        if gc >= 0 {
            // We don't support GC interrupts since they cannot survive Luau exceptions
            return;
        }
        let result = callback_error_ext(state, ptr::null_mut(), false, move |extra, _| {
            let interrupt_cb = (*extra).interrupt_callback.clone();
            let interrupt_cb = ruau_expect!(interrupt_cb, "no interrupt callback set in interrupt_proc");
            if XRc::strong_count(&interrupt_cb) > 2 {
                return Ok(VmState::Continue); // Don't allow recursion
            }
            interrupt_cb((*extra).lua())
        });
        match result {
            VmState::Continue => {}
            VmState::Yield => {
                // We can yield only at yieldable points, otherwise ignore and continue
                if unsafe { ffi::lua_isyieldable(state) } != 0 {
                    unsafe { ffi::lua_yield(state, 0) };
                }
            }
        }
    }

    /// Replaces the current interrupt handler until the returned guard is dropped.
    pub fn scoped_interrupt<F>(&self, callback: F) -> ScopedInterrupt
    where
        F: Fn(&Self) -> Result<VmState> + 'static,
    {
        let lua = self.raw();
        let previous = unsafe { (*lua.extra.get()).interrupt_callback.replace(XRc::new(callback)) };
        unsafe {
            (*ffi::lua_callbacks(lua.main_state())).interrupt = Some(Self::interrupt_proc);
        }
        ScopedInterrupt {
            lua: self.weak(),
            previous,
        }
    }

    /// Removes any interrupt function previously set by `set_interrupt`.
    ///
    /// This function has no effect if an 'interrupt' was not previously set.
    pub fn remove_interrupt(&self) {
        let lua = self.raw();
        unsafe {
            (*lua.extra.get()).interrupt_callback = None;
            (*ffi::lua_callbacks(lua.main_state())).interrupt = None;
        }
    }

    /// Sets thread lifecycle callbacks installed on the `userthread` C hook.
    ///
    /// `on_create` runs when a new Luau thread is constructed; `on_collect` runs when one is
    /// destroyed. Either field may be left `None` to leave the corresponding slot unset.
    /// Use [`Luau::remove_thread_callbacks`] to clear the hook entirely.
    ///
    /// Luau GC does not support exceptions during collection, so `on_collect` must be
    /// non-panicking. If it panics the program will be aborted.
    pub fn set_thread_callbacks(&self, callbacks: ThreadCallbacks) {
        let lua = self.raw();
        unsafe {
            (*lua.extra.get()).thread_creation_callback = callbacks
                .on_create
                .map(|cb| XRc::from(cb) as XRc<dyn Fn(&Self, Thread) -> Result<()> + 'static>);
            (*lua.extra.get()).thread_collection_callback = callbacks
                .on_collect
                .map(|cb| XRc::from(cb) as XRc<dyn Fn(LightUserData) + 'static>);
            (*ffi::lua_callbacks(lua.main_state())).userthread = Some(Self::userthread_proc);
        }
    }
    unsafe extern "C-unwind" fn userthread_proc(parent: *mut ffi::lua_State, child: *mut ffi::lua_State) {
        let extra = ExtraData::get(child);
        if !parent.is_null() {
            // Thread is created
            let callback = match (*extra).thread_creation_callback {
                Some(ref cb) => cb.clone(),
                None => return,
            };
            if XRc::strong_count(&callback) > 2 {
                return; // Don't allow recursion
            }
            ffi::lua_pushthread(child);
            ffi::lua_xmove(child, (*extra).ref_thread, 1);
            let value = Thread((*extra).raw_luau().pop_ref_thread(), child);
            callback_error_ext(parent, extra, false, move |extra, _| {
                callback((*extra).lua(), value)
            })
        } else {
            // Thread is about to be collected
            let callback = match (*extra).thread_collection_callback {
                Some(ref cb) => cb.clone(),
                None => return,
            };

            (*extra).running_gc = true;
            run_thread_collection_callback(&callback, child);
            (*extra).running_gc = false;
        }
    }

    /// Removes any thread callbacks previously set by [`Luau::set_thread_callbacks`].
    ///
    /// Has no effect if no thread callbacks are currently installed.
    pub fn remove_thread_callbacks(&self) {
        let lua = self.raw();
        unsafe {
            let extra = lua.extra.get();
            (*extra).thread_creation_callback = None;
            (*extra).thread_collection_callback = None;
            (*ffi::lua_callbacks(lua.main_state())).userthread = None;
        }
    }

    /// Gets information about the interpreter runtime stack at the given level.
    ///
    /// Crate-internal: callers use [`crate::debug::inspect_stack`].
    pub(crate) fn inspect_stack<R>(&self, level: usize, f: impl FnOnce(&Debug) -> R) -> Option<R> {
        let lua = self.raw();
        unsafe {
            let mut ar = mem::zeroed::<ffi::lua_Debug>();
            let level = level as c_int;
            if ffi::lua_getinfo(lua.state(), level, cstr!(""), &mut ar) == 0 {
                return None;
            }

            Some(f(&Debug::new(lua, level, &mut ar)))
        }
    }

    /// Creates a traceback of the call stack at the given level.
    ///
    /// Crate-internal: callers use [`crate::debug::traceback`].
    pub(crate) fn traceback(&self, msg: Option<&str>, level: usize) -> Result<LuauString> {
        let lua = self.raw();
        unsafe {
            check_stack(lua.state(), 3)?;
            protect_lua!(lua.state(), 0, 1, |state| {
                let msg = match msg {
                    Some(s) => ffi::lua_pushlstring(state, s.as_ptr() as *const c_char, s.len()),
                    None => ptr::null(),
                };
                // `protect_lua` adds it's own call frame, so we need to increase level by 1
                ffi::luaL_traceback(state, state, msg, (level + 1) as c_int);
            })?;
            Ok(LuauString(lua.pop_ref()))
        }
    }

    /// Returns the amount of memory (in bytes) currently used inside this Luau state.
    pub fn used_memory(&self) -> usize {
        let lua = self.raw();
        let state = lua.main_state();
        unsafe {
            match MemoryState::get(state) {
                mem_state if !mem_state.is_null() => (*mem_state).used_memory(),
                _ => {
                    // Get data from the Luau GC
                    let used_kbytes = ffi::lua_gc(state, ffi::LUA_GCCOUNT, 0);
                    let used_kbytes_rem = ffi::lua_gc(state, ffi::LUA_GCCOUNTB, 0);
                    (used_kbytes as usize) * 1024 + (used_kbytes_rem as usize)
                }
            }
        }
    }

    /// Sets a memory limit (in bytes) on this Luau state.
    ///
    /// Once an allocation occurs that would pass this memory limit, a `Error::MemoryError` is
    /// generated instead.
    /// Returns previous limit (zero means no limit).
    ///
    /// Does not work in module mode where Luau state is managed externally.
    pub fn set_memory_limit(&self, limit: usize) -> Result<usize> {
        let lua = self.raw();
        unsafe {
            match MemoryState::get(lua.state()) {
                mem_state if !mem_state.is_null() => Ok((*mem_state).set_memory_limit(limit)),
                _ => Err(Error::MemoryControlNotAvailable),
            }
        }
    }

    /// Performs a full garbage-collection cycle.
    ///
    /// It may be necessary to call this function twice to collect all currently unreachable
    /// objects. Once to finish the current gc cycle, and once to start and finish the next cycle.
    pub fn gc_collect(&self) -> Result<()> {
        let lua = self.raw();
        let state = lua.main_state();
        unsafe {
            check_stack(state, 2)?;
            protect_lua!(state, 0, 0, fn(state) ffi::lua_gc(state, ffi::LUA_GCCOLLECT, 0))
        }
    }

    /// Switches the GC to the given mode with the provided parameters.
    ///
    /// Luau's C API does not expose a way to read current parameter values back, so this method
    /// only sets — there is no `get`. Pass an `Option<...>` per field on `GcIncParams` to leave
    /// existing values alone.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// lua.gc_set_mode(GcMode::Incremental(
    ///     GcIncParams::default().goal(200).step_multiplier(100)
    /// ));
    /// ```
    pub fn gc_set_mode(&self, mode: GcMode) {
        let lua = self.raw();
        let state = lua.main_state();

        match mode {
            GcMode::Incremental(params) => unsafe {
                if let Some(v) = params.goal {
                    ffi::lua_gc(state, ffi::LUA_GCSETGOAL, v);
                }
                if let Some(v) = params.step_multiplier {
                    ffi::lua_gc(state, ffi::LUA_GCSETSTEPMUL, v);
                }
                if let Some(v) = params.step_size {
                    ffi::lua_gc(state, ffi::LUA_GCSETSTEPSIZE, v);
                }
            },
        }
    }

    /// Sets a default Luau compiler (with custom options).
    ///
    /// This compiler will be used by default to load all Luau chunks
    /// including via `require` function.
    ///
    /// Crate-internal: callers configure the default compiler at VM construction
    /// (e.g. via the worker builder); per-chunk overrides use [`Chunk::compiler`].
    pub(crate) fn set_compiler(&self, compiler: Compiler) {
        let lua = self.raw();
        unsafe { (*lua.extra.get()).compiler = Some(compiler) };
    }

    /// Toggles JIT compilation mode for new chunks of code.
    ///
    /// By default JIT is enabled. Changing this option does not have any effect on
    /// already loaded functions.
    pub fn enable_jit(&self, enable: bool) {
        let lua = self.raw();
        unsafe { (*lua.extra.get()).enable_jit = enable };
    }

    /// Sets Luau feature flag (global setting).
    ///
    /// See https://github.com/luau-lang/luau/blob/master/CONTRIBUTING.md#feature-flags for details.
    #[doc(hidden)]
    #[cfg(test)]
    pub(crate) fn set_fflag(name: &str, enabled: bool) -> bool {
        if let Ok(name) = CString::new(name)
            && unsafe { ffi::luau_setfflag(name.as_ptr(), enabled as c_int) != 0 }
        {
            return true;
        }
        false
    }

    /// Returns Luau source code as a `Chunk` builder type.
    ///
    /// In order to actually compile or run the resulting code, you must call [`Chunk::exec`] or
    /// similar on the returned builder. Code is not even parsed until one of these methods is
    /// called.
    ///
    /// [`Chunk::exec`]: crate::Chunk::exec
    #[track_caller]
    pub fn load<'a>(&self, chunk: impl AsChunk + 'a) -> Chunk<'a> {
        self.load_with_location(chunk, Location::caller())
    }

    pub(crate) fn load_with_location<'a>(
        &self,
        chunk: impl AsChunk + 'a,
        location: &'static Location<'static>,
    ) -> Chunk<'a> {
        ChunkInput(chunk).into_chunk(self, location)
    }

    /// Loads trusted Luau bytecode into a callable function.
    ///
    /// Luau does not fully validate bytecode before execution. Passing bytes not produced by a
    /// trusted Luau compiler can crash the interpreter.
    ///
    /// Unlike [`Luau::load`], this does not return a [`Chunk`] builder: bytecode cannot be
    /// recompiled and does not support expression-style [`Chunk::eval`] behavior. Set a custom
    /// environment on the returned function with [`Function::set_environment`] if needed.
    ///
    /// # Safety
    ///
    /// The caller must ensure the bytecode came from a trusted Luau compiler and was not modified
    /// by an untrusted source.
    pub unsafe fn load_bytecode(&self, bytecode: impl AsRef<[u8]>) -> Result<Function> {
        let name =
            CString::new("=(bytecode)").expect("static bytecode chunk name must not contain nul bytes");
        self.raw()
            .load_chunk(Some(&name), None, ChunkMode::Binary, bytecode.as_ref())
    }

    /// Creates and returns an interned Luau string.
    ///
    /// Luau strings can be arbitrary `[u8]` data including embedded nulls, so in addition to `&str`
    /// and `&String`, you can also pass plain `&[u8]` here.
    #[inline]
    pub fn create_string(&self, s: impl AsRef<[u8]>) -> Result<LuauString> {
        unsafe { self.raw().create_string(s.as_ref()) }
    }

    /// Creates and returns a Luau [buffer] object from a byte slice of data.
    ///
    /// [buffer]: https://luau.org/library#buffer-library
    pub fn create_buffer(&self, data: impl AsRef<[u8]>) -> Result<Buffer> {
        let lua = self.raw();
        let data = data.as_ref();
        unsafe {
            let (ptr, buffer) = lua.create_buffer_with_capacity(data.len())?;
            ptr.copy_from_nonoverlapping(data.as_ptr(), data.len());
            Ok(buffer)
        }
    }

    /// Creates and returns a Luau [buffer] object with the specified size.
    ///
    /// Size limit is 1GB. All bytes will be initialized to zero.
    ///
    /// [buffer]: https://luau.org/library#buffer-library
    pub fn create_buffer_with_capacity(&self, size: usize) -> Result<Buffer> {
        unsafe { Ok(self.raw().create_buffer_with_capacity(size)?.1) }
    }

    /// Creates and returns a new empty table.
    #[inline]
    pub fn create_table(&self) -> Result<Table> {
        self.create_table_with_capacity(0, 0)
    }

    /// Creates and returns a new empty table, with the specified capacity.
    ///
    /// - `narr` is a hint for how many elements the table will have as a sequence.
    /// - `nrec` is a hint for how many other elements the table will have.
    ///
    /// Luau may use these hints to preallocate memory for the new table.
    pub fn create_table_with_capacity(&self, narr: usize, nrec: usize) -> Result<Table> {
        unsafe { self.raw().create_table_with_capacity(narr, nrec) }
    }

    /// Creates a table and fills it with values from an iterator.
    pub fn create_table_from<K, V>(&self, iter: impl IntoIterator<Item = (K, V)>) -> Result<Table>
    where
        K: IntoLuau,
        V: IntoLuau,
    {
        unsafe { self.raw().create_table_from(iter) }
    }

    /// Creates a table from an iterator of values, using `1..` as the keys.
    pub fn create_sequence_from<T>(&self, iter: impl IntoIterator<Item = T>) -> Result<Table>
    where
        T: IntoLuau,
    {
        unsafe { self.raw().create_sequence_from(iter) }
    }

    /// Wraps a Rust function or closure, creating a callable Luau function handle to it.
    ///
    /// The function's return value is always a `Result`: If the function returns `Err`, the error
    /// is raised as a Luau error, which can be caught using `(x)pcall` or bubble up to the Rust code
    /// that invoked the Luau code. This allows using the `?` operator to propagate errors through
    /// intermediate Luau code.
    ///
    /// If the function returns `Ok`, the contained value will be converted to one or more Luau
    /// values. For details on Rust-to-Luau conversions, refer to the [`IntoLuau`] and
    /// [`IntoLuauMulti`] traits.
    ///
    /// # Examples
    ///
    /// Create a function which prints its argument:
    ///
    /// ```
    /// # use ruau::{Luau, Result};
    /// # fn main() -> Result<()> {
    /// # let lua = Luau::new();
    /// let greet = lua.create_function(|_, name: String| {
    ///     println!("Hello, {}!", name);
    ///     Ok(())
    /// });
    /// # let _ = greet;    // used
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Use tuples to accept multiple arguments:
    ///
    /// ```
    /// # use ruau::{Luau, Result};
    /// # fn main() -> Result<()> {
    /// # let lua = Luau::new();
    /// let print_person = lua.create_function(|_, (name, age): (String, u8)| {
    ///     println!("{} is {} years old!", name, age);
    ///     Ok(())
    /// });
    /// # let _ = print_person;    // used
    /// # Ok(())
    /// # }
    /// ```
    pub fn create_function<F, A, R>(&self, func: F) -> Result<Function>
    where
        F: Fn(&Self, A) -> Result<R> + 'static,
        A: FromLuauMulti,
        R: IntoLuauMulti,
    {
        (self.raw()).create_callback(Box::new(move |rawlua, nargs| unsafe {
            let args = A::from_stack_args(nargs, 1, None, &rawlua.ctx())?;
            func(rawlua.lua(), args)?.push_into_stack_multi(&rawlua.ctx())
        }))
    }

    /// Wraps a Rust mutable closure, creating a callable Luau function handle to it.
    ///
    /// This is a version of [`Luau::create_function`] that accepts a `FnMut` argument.
    pub fn create_function_mut<F, A, R>(&self, func: F) -> Result<Function>
    where
        F: FnMut(&Self, A) -> Result<R> + 'static,
        A: FromLuauMulti,
        R: IntoLuauMulti,
    {
        let func = RefCell::new(func);
        self.create_function(move |lua, args| {
            (*func.try_borrow_mut().map_err(|_| Error::RecursiveMutCallback)?)(lua, args)
        })
    }

    /// Wraps a C function, creating a callable Luau function handle to it.
    ///
    /// # Safety
    /// This function is unsafe because provides a way to execute unsafe C function.
    pub(crate) unsafe fn create_c_function(&self, func: ffi::lua_CFunction) -> Result<Function> {
        let lua = self.raw();
        let state = lua.state();
        {
            let _sg = StackGuard::new(state);
            check_stack(state, 3)?;

            if lua.unlikely_memory_error() {
                ffi::lua_pushcfunction(state, func);
            } else {
                protect_lua!(state, 0, 1, |state| ffi::lua_pushcfunction(state, func))?;
            }
            Ok(Function(lua.pop_ref()))
        }
    }

    /// Wraps a Rust async function or closure, creating a callable Luau function handle to it.
    ///
    /// While executing the function Rust will poll the Future and if the result is not ready,
    /// call `yield()` passing internal representation of a `Poll::Pending` value.
    ///
    /// The function must be called inside Luau coroutine ([`Thread`]) to be able to suspend its
    /// execution. Tokio should be used to poll [`AsyncThread`] and ruau will take a provided Waker
    /// in that case. Otherwise noop waker will be used if try to call the function outside of Rust
    /// executors.
    ///
    /// The family of `call()` functions takes care about creating [`Thread`].
    ///
    /// # Examples
    ///
    /// Non blocking sleep:
    ///
    /// ```
    /// use std::time::Duration;
    /// use ruau::{Luau, Result};
    ///
    /// async fn sleep(_lua: &Luau, n: u64) -> Result<&'static str> {
    ///     tokio::time::sleep(Duration::from_millis(n)).await;
    ///     Ok("done")
    /// }
    ///
    /// #[tokio::main(flavor = "current_thread")]
    /// async fn main() -> Result<()> {
    ///     let lua = Luau::new();
    ///     lua.globals().set("sleep", lua.create_async_function(sleep)?)?;
    ///     let res: String = lua.load("return sleep(...)").call(100).await?; // Sleep 100ms
    ///     assert_eq!(res, "done");
    ///     Ok(())
    /// }
    /// ```
    ///
    /// [`AsyncThread`]: crate::thread::AsyncThread
    pub fn create_async_function<F, A, R>(&self, func: F) -> Result<Function>
    where
        F: AsyncFn(&Self, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti,
    {
        let func = XRc::new(func);
        (self.raw()).create_async_callback(Box::new(move |rawlua, nargs| unsafe {
            let args = match A::from_stack_args(nargs, 1, None, &rawlua.ctx()) {
                Ok(args) => args,
                Err(e) => return Box::pin(future::ready(Err(e))),
            };
            let lua = rawlua.lua();
            let func = XRc::clone(&func);
            Box::pin(async move {
                func(lua, args)
                    .await?
                    .push_into_stack_multi(&lua.raw_luau().ctx())
            })
        }))
    }

    /// Wraps a Luau function into a new thread (or coroutine).
    ///
    /// Equivalent to `coroutine.create`.
    pub fn create_thread(&self, func: Function) -> Result<Thread> {
        let thread = unsafe { self.raw().create_thread(&func) }?;
        drop(func);
        Ok(thread)
    }

    /// Creates a Luau userdata object from a custom userdata type.
    ///
    /// All userdata instances of the same type `T` shares the same metatable.
    #[inline]
    pub fn create_userdata<T>(&self, data: T) -> Result<AnyUserData>
    where
        T: UserData + 'static,
    {
        unsafe { self.raw().make_userdata(UserDataStorage::new(data)) }
    }

    /// Creates a Luau userdata object from a custom Rust type.
    ///
    /// You can register the type using [`Luau::register_userdata_type`] to add fields or methods
    /// _before_ calling this method.
    /// Otherwise, the userdata object will have an empty metatable.
    ///
    /// All userdata instances of the same type `T` shares the same metatable.
    #[inline]
    pub fn create_opaque_userdata<T>(&self, data: T) -> Result<AnyUserData>
    where
        T: 'static,
    {
        unsafe { self.raw().make_any_userdata(UserDataStorage::new(data)) }
    }

    /// Registers a custom Rust type in Luau to use in userdata objects.
    ///
    /// This methods provides a way to add fields or methods to userdata objects of a type `T`.
    pub fn register_userdata_type<T: 'static>(&self, f: impl FnOnce(&mut UserDataRegistry<T>)) -> Result<()> {
        let type_id = TypeId::of::<T>();
        let mut registry = UserDataRegistry::new(self);
        f(&mut registry);

        let lua = self.raw();
        unsafe {
            // Deregister the type if it already registered
            if let Some(table_id) = (*lua.extra.get()).registered_userdata_t.remove(&type_id) {
                (*lua.extra.get()).registered_userdata_tags.remove(&table_id);
                (*lua.extra.get())
                    .registered_userdata_serializers
                    .remove(&type_id);
                ffi::luaL_unref(lua.state(), ffi::LUA_REGISTRYINDEX, table_id);
            }

            // Add to "pending" registration map
            ((*lua.extra.get()).pending_userdata_reg).insert(type_id, registry.into_raw());
        }
        Ok(())
    }

    /// Create a Luau userdata "proxy" object from a custom userdata type.
    ///
    /// Proxy object is an empty userdata object that has `T` metatable attached.
    /// The main purpose of this object is to provide access to static fields and functions
    /// without creating an instance of type `T`.
    ///
    /// You can get or set uservalues on this object but you cannot borrow any Rust type.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ruau::{Luau, Result, UserData, UserDataFields, UserDataMethods};
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> Result<()> {
    /// # let lua = Luau::new();
    /// struct MyUserData(i32);
    ///
    /// impl UserData for MyUserData {
    ///     fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
    ///         fields.add_field_method_get("val", |_, this| Ok(this.0));
    ///     }
    ///
    ///     fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
    ///         methods.add_function("new", |_, value: i32| Ok(MyUserData(value)));
    ///     }
    /// }
    ///
    /// lua.globals().set("MyUserData", lua.create_proxy::<MyUserData>()?)?;
    ///
    /// lua.load("assert(MyUserData.new(321).val == 321)").exec().await?;
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn create_proxy<T>(&self) -> Result<AnyUserData>
    where
        T: UserData + 'static,
    {
        let ud = UserDataProxy::<T>(PhantomData);
        unsafe { self.raw().make_userdata(UserDataStorage::new(ud)) }
    }

    /// Gets the metatable of a Luau built-in (primitive) type.
    ///
    /// The metatable is shared by all values of the given type.
    ///
    /// See [`Luau::set_type_metatable`] for examples.
    pub fn type_metatable(&self, ty: PrimitiveType) -> Option<Table> {
        let lua = self.raw();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            assert_stack(state, 2);

            lua.push_primitive_type(ty);
            if ffi::lua_getmetatable(state, -1) != 0 {
                return Some(Table(lua.pop_ref()));
            }
        }
        None
    }

    /// Sets the metatable for a Luau built-in (primitive) type.
    ///
    /// The metatable will be shared by all values of the given type.
    ///
    /// # Examples
    ///
    /// Change metatable for Luau boolean type:
    ///
    /// ```
    /// # use ruau::{Function, Luau, PrimitiveType, Result};
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> Result<()> {
    /// # let lua = Luau::new();
    /// let mt = lua.create_table()?;
    /// mt.set("__tostring", lua.create_function(|_, b: bool| Ok(if b { "2" } else { "0" }))?)?;
    /// lua.set_type_metatable(PrimitiveType::Boolean, Some(mt));
    /// lua.load("assert(tostring(true) == '2')").exec().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn set_type_metatable(&self, ty: PrimitiveType, metatable: Option<Table>) {
        let lua = self.raw();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            assert_stack(state, 2);

            lua.push_primitive_type(ty);
            match metatable {
                Some(metatable) => lua.push_ref(&metatable.0),
                None => ffi::lua_pushnil(state),
            }
            ffi::lua_setmetatable(state, -2);
        }
    }

    /// Returns a handle to the global environment.
    pub fn globals(&self) -> Table {
        let lua = self.raw();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            assert_stack(state, 1);
            ffi::lua_pushvalue(state, ffi::LUA_GLOBALSINDEX);
            Table(lua.pop_ref())
        }
    }

    /// Returns a handle to the active `Thread`.
    ///
    /// For calls to `Luau` this will be the main Luau thread, for parameters given to a callback,
    /// this will be whatever Luau thread called the callback.
    pub fn current_thread(&self) -> Thread {
        let lua = self.raw();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            assert_stack(state, 1);
            ffi::lua_pushthread(state);
            Thread(lua.pop_ref(), state)
        }
    }

    /// Calls the given function with a [`Scope`] parameter, giving the function the ability to
    /// create userdata and callbacks from Rust types that are `!Send` or non-`'static`.
    ///
    /// The lifetime of any function or userdata created through [`Scope`] lasts only until the
    /// completion of this method call, on completion all such created values are automatically
    /// dropped and Luau references to them are invalidated. If a script accesses a value created
    /// through [`Scope`] outside of this method, a Luau error will result. Since we can ensure the
    /// lifetime of values created through [`Scope`], and we know that [`Luau`] cannot be sent to
    /// another thread while [`Scope`] is live, it is safe to allow `!Send` data types and whose
    /// lifetimes only outlive the scope lifetime.
    pub fn scope<'env, R>(
        &self,
        f: impl for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> Result<R>,
    ) -> Result<R> {
        f(&Scope::new(self.guard()))
    }

    /// Returns a thin view over the Luau registry for this VM.
    ///
    /// All registry access — string-keyed (`named_*`), `RegistryKey`-keyed
    /// (`insert`/`get`/`remove`/`replace`), and bookkeeping (`expire`, `owns`) — lives on the
    /// returned [`Registry`].
    #[inline]
    pub fn registry(&self) -> Registry<'_> {
        Registry { lua: self }
    }

    /// Sets or replaces an application data object of type `T`.
    ///
    /// Application data could be accessed at any time by using [`Luau::app_data_ref`] or
    /// [`Luau::app_data_mut`] methods where `T` is the data type.
    ///
    /// # Panics
    ///
    /// Panics if the app data container is currently borrowed.
    ///
    /// # Examples
    ///
    /// ```
    /// use ruau::{Luau, Result};
    ///
    /// fn hello(lua: &Luau, _: ()) -> Result<()> {
    ///     let mut s = lua.app_data_mut::<&str>().unwrap();
    ///     assert_eq!(*s, "hello");
    ///     *s = "world";
    ///     Ok(())
    /// }
    ///
    /// #[tokio::main(flavor = "current_thread")]
    /// async fn main() -> Result<()> {
    ///     let lua = Luau::new();
    ///     lua.set_app_data("hello");
    ///     lua.create_function(hello)?.call::<()>(()).await?;
    ///     let s = lua.app_data_ref::<&str>().unwrap();
    ///     assert_eq!(*s, "world");
    ///     Ok(())
    /// }
    /// ```
    #[track_caller]
    pub fn set_app_data<T: 'static>(&self, data: T) -> Option<T> {
        let lua = self.raw();
        let extra = unsafe { &*lua.extra.get() };
        extra.app_data.insert(data)
    }

    /// Replaces app data of type `T` until the returned guard is dropped.
    ///
    /// The previous value is restored on drop. If no previous value existed, the scoped value is
    /// removed.
    ///
    /// Use this for retained hosts that install callbacks once and swap request-local state for
    /// one execution. Keep the returned guard alive for the execution and have callbacks borrow
    /// the state briefly with [`Luau::app_data_ref`] or [`Luau::app_data_mut`], clone the data they
    /// need, then release the borrow before awaiting. Prefer this over [`Luau::try_set_app_data`]
    /// for per-exec state because the guard restores the previous value automatically.
    #[track_caller]
    pub fn scoped_app_data<T: 'static>(&self, data: T) -> ScopedAppData<T> {
        let previous = self.set_app_data(data);
        ScopedAppData {
            lua: self.weak(),
            previous,
        }
    }

    /// Tries to set or replace an application data object of type `T`.
    ///
    /// Returns:
    /// - `Ok(Some(old_data))` if the data object of type `T` was successfully replaced.
    /// - `Ok(None)` if the data object of type `T` was successfully inserted.
    /// - `Err(data)` if the data object of type `T` was not inserted because the container is
    ///   currently borrowed.
    ///
    /// See [`Luau::set_app_data`] for examples.
    pub fn try_set_app_data<T: 'static>(&self, data: T) -> StdResult<Option<T>, T> {
        let lua = self.raw();
        let extra = unsafe { &*lua.extra.get() };
        extra.app_data.try_insert(data)
    }

    /// Gets a reference to an application data object stored by [`Luau::set_app_data`] of type
    /// `T`.
    ///
    /// # Panics
    ///
    /// Panics if the data object of type `T` is currently mutably borrowed. Multiple immutable
    /// reads can be taken out at the same time.
    #[track_caller]
    pub fn app_data_ref<T: 'static>(&self) -> Option<AppDataRef<'_, T>> {
        let guard = self.guard();
        let extra = unsafe { &*guard.extra.get() };
        extra.app_data.borrow(Some(guard))
    }

    /// Tries to get a reference to an application data object stored by [`Luau::set_app_data`] of
    /// type `T`.
    pub fn try_app_data_ref<T: 'static>(&self) -> StdResult<Option<AppDataRef<'_, T>>, BorrowError> {
        let guard = self.guard();
        let extra = unsafe { &*guard.extra.get() };
        extra.app_data.try_borrow(Some(guard))
    }

    /// Gets a mutable reference to an application data object stored by [`Luau::set_app_data`] of
    /// type `T`.
    ///
    /// # Panics
    ///
    /// Panics if the data object of type `T` is currently borrowed.
    #[track_caller]
    pub fn app_data_mut<T: 'static>(&self) -> Option<AppDataRefMut<'_, T>> {
        let guard = self.guard();
        let extra = unsafe { &*guard.extra.get() };
        extra.app_data.borrow_mut(Some(guard))
    }

    /// Tries to get a mutable reference to an application data object stored by
    /// [`Luau::set_app_data`] of type `T`.
    pub fn try_app_data_mut<T: 'static>(&self) -> StdResult<Option<AppDataRefMut<'_, T>>, BorrowMutError> {
        let guard = self.guard();
        let extra = unsafe { &*guard.extra.get() };
        extra.app_data.try_borrow_mut(Some(guard))
    }

    /// Removes an application data of type `T`.
    ///
    /// # Panics
    ///
    /// Panics if the app data container is currently borrowed.
    #[track_caller]
    pub fn remove_app_data<T: 'static>(&self) -> Option<T> {
        let lua = self.raw();
        let extra = unsafe { &*lua.extra.get() };
        extra.app_data.remove()
    }

    /// Returns an internal `Poll::Pending` constant used for executing async callbacks.
    ///
    /// Every time when [`Future`] is Pending, Luau corotine is suspended with this constant.
    #[doc(hidden)]
    #[inline(always)]
    pub(crate) fn poll_pending() -> LightUserData {
        static ASYNC_POLL_PENDING: u8 = 0;
        LightUserData(&ASYNC_POLL_PENDING as *const u8 as *mut c_void)
    }
    #[inline(always)]
    pub(crate) fn poll_terminate() -> LightUserData {
        static ASYNC_POLL_TERMINATE: u8 = 0;
        LightUserData(&ASYNC_POLL_TERMINATE as *const u8 as *mut c_void)
    }
    #[inline(always)]
    pub(crate) fn poll_yield() -> LightUserData {
        static ASYNC_POLL_YIELD: u8 = 0;
        LightUserData(&ASYNC_POLL_YIELD as *const u8 as *mut c_void)
    }

    /// Suspends the current async function, returning the provided arguments to caller.
    ///
    /// This function is similar to [`coroutine.yield`] but allow yielding Rust functions
    /// and passing values to the caller.
    /// Please note that you cannot cross [`Thread`] boundaries (e.g. calling `yield_with` on one
    /// thread and resuming on another).
    ///
    /// # Examples
    ///
    /// Async iterator:
    ///
    /// ```
    /// # use ruau::{Luau, Result};
    /// #
    /// async fn generator(lua: &Luau, _: ()) -> Result<()> {
    ///     for i in 0..10 {
    ///         lua.yield_with::<()>(i).await?;
    ///     }
    ///     Ok(())
    /// }
    ///
    /// #[tokio::main(flavor = "current_thread")]
    /// async fn main() -> Result<()> {
    ///     let lua = Luau::new();
    ///     lua.globals().set("generator", lua.create_async_function(generator)?)?;
    ///
    ///     lua.load(r#"
    ///        local n = 0
    ///        for i in coroutine.wrap(generator) do
    ///            n = n + i
    ///        end
    ///        assert(n == 45)
    ///     "#)
    ///     .exec()
    ///     .await
    /// }
    /// ```
    ///
    /// Exchange values on yield:
    ///
    /// ```
    /// # use ruau::{Luau, Result, Value};
    /// #
    /// async fn pingpong(lua: &Luau, mut val: i32) -> Result<()> {
    ///     loop {
    ///         val = lua.yield_with::<i32>(val).await? + 1;
    ///     }
    ///     Ok(())
    /// }
    ///
    /// # fn main() -> Result<()> {
    /// let lua = Luau::new();
    ///
    /// let co = lua.create_thread(lua.create_async_function(pingpong)?)?;
    /// assert_eq!(co.resume::<i32>(1)?, 1);
    /// assert_eq!(co.resume::<i32>(2)?, 3);
    /// assert_eq!(co.resume::<i32>(3)?, 4);
    ///
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// [`coroutine.yield`]: https://luau.org/library#coroutine-library
    pub async fn yield_with<R: FromLuauMulti>(&self, args: impl IntoLuauMulti) -> Result<R> {
        let mut args = Some(args.into_luau_multi(self)?);
        future::poll_fn(move |_cx| match args.take() {
            Some(args) => unsafe {
                let lua = self.raw();
                lua.push(Self::poll_yield())?; // yield marker
                if args.len() <= 1 {
                    lua.push(args.front())?;
                } else {
                    lua.push(lua.create_sequence_from(&args)?)?;
                }
                lua.push(args.len())?;
                Poll::Pending
            },
            None => unsafe {
                let lua = self.raw();
                let state = lua.state();
                let top = ffi::lua_gettop(state);
                if top == 0 || ffi::lua_type(state, 1) != ffi::LUA_TUSERDATA {
                    // This must be impossible scenario if used correctly
                    return Poll::Ready(R::from_stack_multi(0, &lua.ctx()));
                }
                let _sg = StackGuard::with_top(state, 1);
                Poll::Ready(R::from_stack_multi(top - 1, &lua.ctx()))
            },
        })
        .await
    }

    /// Returns a weak reference to the Luau instance.
    ///
    /// This is useful for creating a reference to the Luau instance that does not prevent it from
    /// being deallocated.
    #[inline(always)]
    pub fn weak(&self) -> WeakLuau {
        WeakLuau {
            raw: self.raw,
            live: Rc::downgrade(&self.live),
            _not_send_sync: PhantomData,
        }
    }

    #[inline(always)]
    pub(crate) fn raw(&self) -> &RawLuau {
        assert!(self.live.get(), "Luau instance is destroyed");
        let rawlua = unsafe { self.raw.as_ref() };
        debug_assert!(
            unsafe { !(*rawlua.extra.get()).running_gc },
            "Luau VM is suspended while GC is running"
        );
        rawlua
    }

    #[inline(always)]
    pub(crate) fn guard(&self) -> LuauLiveGuard {
        LuauLiveGuard {
            raw: self.raw,
            live: Rc::downgrade(&self.live),
            _not_send_sync: PhantomData,
        }
    }

    /// Returns a handle to the unprotected Luau state without checking liveness.
    ///
    /// This is useful where callback dispatch already owns a live Luau state.
    #[inline(always)]
    pub(crate) unsafe fn raw_luau(&self) -> &RawLuau {
        self.raw()
    }
}

impl WeakLuau {
    #[inline(always)]
    pub(crate) fn guard(&self) -> LuauLiveGuard {
        LuauLiveGuard {
            raw: self.raw,
            live: self.live.clone(),
            _not_send_sync: PhantomData,
        }
    }

    #[track_caller]
    #[inline(always)]
    pub(crate) fn raw(&self) -> &RawLuau {
        let live = self.live.upgrade().expect("Luau instance is destroyed");
        assert!(live.get(), "Luau instance is destroyed");
        let rawlua = unsafe { self.raw.as_ref() };
        debug_assert!(
            unsafe { !(*rawlua.extra.get()).running_gc },
            "Luau VM is suspended while GC is running"
        );
        rawlua
    }

    #[inline(always)]
    pub(crate) fn try_raw(&self) -> Option<&RawLuau> {
        let live = self.live.upgrade()?;
        if !live.get() {
            return None;
        }
        Some(unsafe { self.raw.as_ref() })
    }

    /// Returns whether the referenced Luau instance is still alive.
    #[inline(always)]
    pub fn is_alive(&self) -> bool {
        self.live.upgrade().is_some_and(|live| live.get())
    }
}

impl PartialEq for WeakLuau {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl Eq for WeakLuau {}

impl Deref for LuauLiveGuard {
    type Target = RawLuau;

    fn deref(&self) -> &Self::Target {
        let live = self.live.upgrade().expect("Luau instance is destroyed");
        assert!(live.get(), "Luau instance is destroyed");
        unsafe { self.raw.as_ref() }
    }
}

mod extra;
mod raw;
mod util;

#[cfg(test)]
mod assertions {
    use std::panic::RefUnwindSafe;

    use super::*;
    use crate::{ObjectLike, Result, Table};

    // Luau has lots of interior mutability, should not be RefUnwindSafe
    static_assertions::assert_not_impl_any!(Luau: RefUnwindSafe);

    // Luau is single-owner and pinned to one thread; both Send and Sync are deliberately
    // excluded so the embedder uses a current-thread Tokio runtime + LocalSet.
    static_assertions::assert_not_impl_any!(Luau: Send, Sync);
    static_assertions::assert_not_impl_any!(RawLuau: Send, Sync);

    #[tokio::test]
    async fn integer64_type_flag_supports_integer_literals() -> Result<()> {
        let lua = Luau::new();
        let _ignored = Luau::set_fflag("LuauIntegerType", true);

        let integer_lib = lua.globals().get::<Table>("integer")?;
        let n = integer_lib.call_function::<i64>("create", 42).await?;
        assert_eq!(n, 42);

        let n: i64 = lua.load("return 42i").eval().await?;
        assert_eq!(n, 42);
        let n: i64 = lua.load("return -42i").eval().await?;
        assert_eq!(n, -42);

        Ok(())
    }
}
