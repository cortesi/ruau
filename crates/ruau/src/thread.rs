//! Luau thread (coroutine) handling.
//!
//! This module provides types for creating and working with Luau coroutines from Rust.
//! Coroutines allow cooperative multitasking within a single Luau state by suspending and
//! resuming execution at well-defined yield points.
//!
//! # Basic Usage
//!
//! Threads are created via [`Luau::create_thread`] and driven by calling [`Thread::resume`]:
//!
//! ```rust
//! # use ruau::{Luau, Result, Thread};
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<()> {
//! let lua = Luau::new();
//! let thread: Thread = lua.load(r#"
//!     coroutine.create(function(a, b)
//!         coroutine.yield(a + b)
//!         return a * b
//!     end)
//! "#).eval().await?;
//!
//! assert_eq!(thread.resume::<i32>((3, 4))?, 7);
//! assert_eq!(thread.resume::<i32>(())?,    12);
//! # Ok(())
//! # }
//! ```
//!
//! # Async Support
//!
//! A [`Thread`] can be converted into an [`AsyncThread`]
//! via [`Thread::into_async`], which implements both [`Future`] and [`Stream`].
//! This integrates Luau coroutines naturally with Tokio direct local mode.
//!
//! [`Luau::create_thread`]: crate::Luau::create_thread
//! [`Future`]: std::future::Future
//! [`Stream`]: futures_util::stream::Stream

use std::{
    fmt,
    future::Future,
    marker::PhantomData,
    os::raw::{c_int, c_void},
    pin::Pin,
    task::{Context, Poll, Waker},
};

use futures_util::stream::Stream;

use crate::{
    error::{Error, Result},
    function::Function,
    state::RawLuau,
    traits::{FromLuauMulti, IntoLuauMulti},
    types::ValueRef,
    util::{StackGuard, check_stack, error_traceback_thread, pop_error},
};

/// Status of a Luau thread (coroutine).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ThreadStatus {
    /// The thread was just created or is suspended (yielded).
    ///
    /// If a thread is in this state, it can be resumed by calling [`Thread::resume`].
    Resumable,
    /// The thread is currently running.
    Running,
    /// The thread has finished executing.
    Finished,
    /// The thread has raised a Luau error during execution.
    Error,
}

/// Internal representation of a Luau thread status.
///
/// The number in `New` and `Yielded` variants is the number of arguments pushed
/// to the thread stack.
#[derive(Clone, Copy)]
enum ThreadStatusInner {
    New(c_int),
    Running,
    Yielded(c_int),
    Finished,
    Error,
}

impl ThreadStatusInner {
    #[inline(always)]
    fn is_resumable(self) -> bool {
        matches!(self, Self::New(_) | Self::Yielded(_))
    }
    #[inline(always)]
    fn is_yielded(self) -> bool {
        matches!(self, Self::Yielded(_))
    }
}

/// Handle to an internal Luau thread (coroutine).
#[derive(Clone, PartialEq)]
pub struct Thread(pub(crate) ValueRef, pub(crate) *mut ffi::lua_State);

/// Thread (coroutine) representation as an async [`Future`] or [`Stream`].
///
/// [`Future`]: std::future::Future
/// [`Stream`]: futures_util::stream::Stream
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct AsyncThread<R> {
    thread: Thread,
    ret: PhantomData<fn() -> R>,
    /// Provenance gate: only `true` when the thread came out of
    /// [`crate::state::RawLuau::create_recycled_thread`] (driven by
    /// [`Function::call`]) and is therefore safe to return to the pool on drop. User-driven
    /// `Thread::into_async` paths leave this `false` because the thread may have been
    /// sandboxed via [`Thread::sandbox`], reset, or otherwise had its `LUA_GLOBALSINDEX`
    /// modified — `reset_inner` and the global rewrite at checkout do not by themselves prove
    /// such state has been normalised. Removing this flag in favour of the pool-size check
    /// alone would let user-tainted threads into the recycled-thread pool.
    recycle: bool,
}

impl Thread {
    /// Returns reference to the Luau state that this thread is associated with.
    #[inline(always)]
    pub(crate) fn state(&self) -> *mut ffi::lua_State {
        self.1
    }

    /// Resumes execution of this thread.
    ///
    /// Equivalent to [`coroutine.resume`].
    /// This is the intentionally synchronous coroutine-stepping API; use
    /// [`Thread::into_async`] when the coroutine may run Rust async callbacks.
    ///
    /// Passes `args` as arguments to the thread. If the coroutine has called [`coroutine.yield`],
    /// it will return these arguments. Otherwise, the coroutine wasn't yet started, so the
    /// arguments are passed to its main function.
    ///
    /// If the thread is no longer resumable (meaning it has finished execution or encountered an
    /// error), this will return [`Error::CoroutineUnresumable`], otherwise will return `Ok` as
    /// follows:
    ///
    /// If the thread calls [`coroutine.yield`], returns the values passed to `yield`. If the thread
    /// `return`s values from its main function, returns those.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ruau::{Error, Luau, Result, Thread};
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> Result<()> {
    /// # let lua = Luau::new();
    /// let thread: Thread = lua.load(r#"
    ///     coroutine.create(function(arg)
    ///         assert(arg == 42)
    ///         local yieldarg = coroutine.yield(123)
    ///         assert(yieldarg == 43)
    ///         return 987
    ///     end)
    /// "#).eval().await?;
    ///
    /// assert_eq!(thread.resume::<u32>(42)?, 123);
    /// assert_eq!(thread.resume::<u32>(43)?, 987);
    ///
    /// // The coroutine has now returned, so `resume` will fail
    /// match thread.resume::<u32>(()) {
    ///     Err(Error::CoroutineUnresumable) => {},
    ///     unexpected => panic!("unexpected result {:?}", unexpected),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// [`coroutine.resume`]: https://luau.org/library#coroutine-library
    /// [`coroutine.yield`]: https://luau.org/library#coroutine-library
    pub fn resume<R>(&self, args: impl IntoLuauMulti) -> Result<R>
    where
        R: FromLuauMulti,
    {
        let lua = self.0.lua.raw();
        let mut pushed_nargs = match self.status_inner(lua) {
            ThreadStatusInner::New(nargs) | ThreadStatusInner::Yielded(nargs) => nargs,
            _ => return Err(Error::CoroutineUnresumable),
        };

        let state = lua.state();
        let thread_state = self.state();
        unsafe {
            let _sg = StackGuard::new(state);

            let nargs = args.push_into_stack_multi(&lua.ctx())?;
            if nargs > 0 {
                check_stack(thread_state, nargs)?;
                ffi::lua_xmove(state, thread_state, nargs);
                pushed_nargs += nargs;
            }

            let _thread_sg = StackGuard::with_top(thread_state, 0);
            let (_, nresults) = self.resume_inner(lua, pushed_nargs)?;
            check_stack(state, nresults + 1)?;
            ffi::lua_xmove(thread_state, state, nresults);

            R::from_stack_multi(nresults, &lua.ctx())
        }
    }

    /// Resumes execution of this thread, immediately raising an error.
    ///
    /// This is a Luau specific extension.
    pub fn resume_error<R>(&self, error: impl crate::IntoLuau) -> Result<R>
    where
        R: FromLuauMulti,
    {
        let lua = self.0.lua.raw();
        match self.status_inner(lua) {
            ThreadStatusInner::New(_) | ThreadStatusInner::Yielded(_) => {}
            _ => return Err(Error::CoroutineUnresumable),
        };

        let state = lua.state();
        let thread_state = self.state();
        unsafe {
            let _sg = StackGuard::new(state);

            check_stack(state, 1)?;
            error.push_into_stack(&lua.ctx())?;
            ffi::lua_xmove(state, thread_state, 1);

            let _thread_sg = StackGuard::with_top(thread_state, 0);
            let (_, nresults) = self.resume_inner(lua, ffi::LUA_RESUMEERROR)?;
            check_stack(state, nresults + 1)?;
            ffi::lua_xmove(thread_state, state, nresults);

            R::from_stack_multi(nresults, &lua.ctx())
        }
    }

    /// Resumes execution of this thread.
    ///
    /// It's similar to `resume()` but leaves `nresults` values on the thread stack.
    unsafe fn resume_inner(&self, lua: &RawLuau, nargs: c_int) -> Result<(ThreadStatusInner, c_int)> {
        let state = lua.state();
        let thread_state = self.state();
        let mut nresults = 0;
        let ret = ffi::lua_resumex(thread_state, state, nargs, &mut nresults as *mut c_int);
        match ret {
            ffi::LUA_OK => Ok((ThreadStatusInner::Finished, nresults)),
            ffi::LUA_YIELD => Ok((ThreadStatusInner::Yielded(0), nresults)),
            ffi::LUA_ERRMEM => {
                // Don't call error handler for memory errors
                Err(pop_error(thread_state, ret))
            }
            _ => {
                check_stack(state, 3)?;
                protect_lua!(state, 0, 1, |state| error_traceback_thread(state, thread_state))?;
                Err(pop_error(state, ret))
            }
        }
    }

    /// Gets the status of the thread.
    pub fn status(&self) -> ThreadStatus {
        match self.status_inner(self.0.lua.raw()) {
            ThreadStatusInner::New(_) | ThreadStatusInner::Yielded(_) => ThreadStatus::Resumable,
            ThreadStatusInner::Running => ThreadStatus::Running,
            ThreadStatusInner::Finished => ThreadStatus::Finished,
            ThreadStatusInner::Error => ThreadStatus::Error,
        }
    }

    /// Gets the status of the thread (internal implementation).
    fn status_inner(&self, lua: &RawLuau) -> ThreadStatusInner {
        let thread_state = self.state();
        if thread_state == lua.state() {
            // The thread is currently running
            return ThreadStatusInner::Running;
        }
        let status = unsafe { ffi::lua_status(thread_state) };
        let top = unsafe { ffi::lua_gettop(thread_state) };
        match status {
            ffi::LUA_YIELD => ThreadStatusInner::Yielded(top),
            ffi::LUA_OK if top > 0 => ThreadStatusInner::New(top - 1),
            ffi::LUA_OK => ThreadStatusInner::Finished,
            _ => ThreadStatusInner::Error,
        }
    }

    /// Returns `true` if this thread is resumable (meaning it can be resumed by calling
    /// [`Thread::resume`]).
    #[inline(always)]
    pub fn is_resumable(&self) -> bool {
        self.status() == ThreadStatus::Resumable
    }

    /// Returns `true` if this thread is currently running.
    #[inline(always)]
    pub fn is_running(&self) -> bool {
        self.status() == ThreadStatus::Running
    }

    /// Returns `true` if this thread has finished executing.
    #[inline(always)]
    pub fn is_finished(&self) -> bool {
        self.status() == ThreadStatus::Finished
    }

    /// Returns `true` if this thread has raised a Luau error during execution.
    #[inline(always)]
    pub fn is_error(&self) -> bool {
        self.status() == ThreadStatus::Error
    }

    /// Resets a thread
    ///
    /// Resets to the initial state of a newly created Luau thread.
    /// Luau threads in arbitrary states (like yielded or errored) can be reset properly.
    ///
    /// Sets a Luau function for the thread afterwards.
    #[allow(clippy::needless_pass_by_value)]
    pub fn reset(&self, func: Function) -> Result<()> {
        let lua = self.0.lua.raw();
        let thread_state = self.state();
        unsafe {
            let status = self.status_inner(lua);
            self.reset_inner(status)?;

            // Push function to the top of the thread stack
            ffi::lua_xpush(lua.ref_thread(), thread_state, func.0.index);

            {
                // Inherit `LUA_GLOBALSINDEX` from the main thread
                ffi::lua_xpush(lua.main_state(), thread_state, ffi::LUA_GLOBALSINDEX);
                ffi::lua_replace(thread_state, ffi::LUA_GLOBALSINDEX);
            }

            Ok(())
        }
    }

    unsafe fn reset_inner(&self, status: ThreadStatusInner) -> Result<()> {
        match status {
            ThreadStatusInner::New(_) => {
                // The thread is new, so we can just set the top to 0
                ffi::lua_settop(self.state(), 0);
                Ok(())
            }
            ThreadStatusInner::Running => Err(Error::runtime("cannot reset a running thread")),
            ThreadStatusInner::Finished => Ok(()),
            ThreadStatusInner::Yielded(_) | ThreadStatusInner::Error => {
                let thread_state = self.state();

                ffi::lua_resetthread(thread_state);

                Ok(())
            }
        }
    }

    /// Converts [`Thread`] to an [`AsyncThread`] which implements [`Future`] and [`Stream`] traits.
    ///
    /// Only resumable threads can be converted to [`AsyncThread`].
    ///
    /// `args` are pushed to the thread stack and will be used when the thread is resumed.
    /// The object calls [`resume`] while polling and also allow to run Rust futures
    /// to completion using an executor.
    ///
    /// [`AsyncThread`] is local to the VM and is not `Send`. If it is spawned, use
    /// [`tokio::task::LocalSet`] on a current-thread Tokio runtime.
    ///
    /// Using [`AsyncThread`] as a [`Stream`] allow to iterate through [`coroutine.yield`]
    /// values whereas [`Future`] version discards that values and poll until the final
    /// one (returned from the thread function).
    ///
    /// [`Future`]: std::future::Future
    /// [`Stream`]: futures_util::stream::Stream
    /// [`resume`]: Thread::resume
    /// [`coroutine.yield`]: https://luau.org/library#coroutine-library
    ///
    /// # Examples
    ///
    /// ```
    /// # use ruau::{Luau, Result, Thread};
    /// use futures_util::stream::TryStreamExt;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> Result<()> {
    /// # let lua = Luau::new();
    /// let thread: Thread = lua.load(r#"
    ///     coroutine.create(function (sum)
    ///         for i = 1,10 do
    ///             sum = sum + i
    ///             coroutine.yield(sum)
    ///         end
    ///         return sum
    ///     end)
    /// "#).eval().await?;
    ///
    /// let mut stream = thread.into_async::<i64>(1)?;
    /// let mut sum = 0;
    /// while let Some(n) = stream.try_next().await? {
    ///     sum += n;
    /// }
    ///
    /// assert_eq!(sum, 286);
    ///
    /// # Ok(())
    /// # }
    /// ```
    pub fn into_async<R>(self, args: impl IntoLuauMulti) -> Result<AsyncThread<R>>
    where
        R: FromLuauMulti,
    {
        let lua = self.0.lua.raw();
        if !self.status_inner(lua).is_resumable() {
            return Err(Error::CoroutineUnresumable);
        }

        let state = lua.state();
        let thread_state = self.state();
        unsafe {
            let _sg = StackGuard::new(state);

            let nargs = args.push_into_stack_multi(&lua.ctx())?;
            if nargs > 0 {
                check_stack(thread_state, nargs)?;
                ffi::lua_xmove(state, thread_state, nargs);
            }

            Ok(AsyncThread {
                thread: self,
                ret: PhantomData,
                recycle: false,
            })
        }
    }

    /// Enables sandbox mode on this thread.
    ///
    /// Under the hood replaces the global environment table with a new table,
    /// that performs writes locally and proxies reads to caller's global environment.
    ///
    /// This mode ideally should be used together with the global sandbox mode [`Luau::sandbox`].
    ///
    /// Please note that Luau links environment table with chunk when loading it into Luau state.
    /// Therefore you need to load chunks into a thread to link with the thread environment.
    ///
    /// [`Luau::sandbox`]: crate::Luau::sandbox
    ///
    /// # Examples
    ///
    /// ```
    /// # use ruau::{Luau, Result};
    /// # fn main() -> Result<()> {
    /// let lua = Luau::new();
    /// let thread = lua.create_thread(lua.create_function(|lua2, ()| {
    ///     let chunk = lua2.load("var = 123").into_function()?;
    ///     lua2.create_thread(chunk)?.resume::<()>(())?;
    ///     assert_eq!(lua2.globals().get::<u32>("var")?, 123);
    ///     Ok(())
    /// })?)?;
    /// thread.sandbox()?;
    /// thread.resume::<()>(())?;
    ///
    /// // The global environment should be unchanged
    /// assert_eq!(lua.globals().get::<Option<u32>>("var")?, None);
    /// # Ok(())
    /// # }
    /// ```
    pub fn sandbox(&self) -> Result<()> {
        let lua = self.0.lua.raw();
        let state = lua.state();
        let thread_state = self.state();
        unsafe {
            check_stack(thread_state, 3)?;
            check_stack(state, 3)?;
            protect_lua!(state, 0, 0, |_| ffi::luaL_sandboxthread(thread_state))
        }
    }

    /// Converts this thread to a generic C pointer.
    ///
    /// There is no way to convert the pointer back to its original value.
    ///
    /// Typically this function is used only for hashing and debug information.
    #[inline]
    pub fn to_pointer(&self) -> *const c_void {
        self.0.to_pointer()
    }
}

impl fmt::Debug for Thread {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_tuple("Thread").field(&self.0).finish()
    }
}

impl<R> AsyncThread<R> {
    #[inline(always)]
    pub(crate) fn set_recyclable(&mut self, recyclable: bool) {
        self.recycle = recyclable;
    }
}
impl<R> Drop for AsyncThread<R> {
    fn drop(&mut self) {
        if !self.thread.0.lua.is_alive() {
            return;
        }

        let lua_guard = self.thread.0.lua.guard();
        let lua = &*lua_guard;
        unsafe {
            let mut status = self.thread.status_inner(lua);
            if matches!(status, ThreadStatusInner::Yielded(0)) {
                // The thread is dropped while yielded in the async poller.
                ffi::lua_pushlightuserdata(self.thread.1, crate::Luau::poll_terminate().0);
                if let Ok((new_status, _)) = self.thread.resume_inner(lua, 1) {
                    status = new_status;
                }
            }

            // Recycled threads must be reset before returning to the pool. Non-recycled
            // threads are still cancelled above so pending Rust futures release resources.
            if self.recycle && self.thread.reset_inner(status).is_ok() {
                lua.recycle_thread(&mut self.thread);
            }
        }
    }
}
impl<R: FromLuauMulti> Stream for AsyncThread<R> {
    type Item = Result<R>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let lua = self.thread.0.lua.raw();
        let nargs = match self.thread.status_inner(lua) {
            ThreadStatusInner::New(nargs) | ThreadStatusInner::Yielded(nargs) => nargs,
            _ => return Poll::Ready(None),
        };

        let state = lua.state();
        let thread_state = self.thread.state();
        unsafe {
            let _sg = StackGuard::new(state);
            let _thread_sg = StackGuard::with_top(thread_state, 0);
            let _wg = WakerGuard::new(lua, cx.waker());

            let (status, nresults) = (self.thread).resume_inner(lua, nargs)?;

            if status.is_yielded() {
                if nresults == 1 && is_poll_pending(thread_state) {
                    return Poll::Pending;
                }
                // Continue polling
                cx.waker().wake_by_ref();
            }

            check_stack(state, nresults + 1)?;
            ffi::lua_xmove(thread_state, state, nresults);

            Poll::Ready(Some(R::from_stack_multi(nresults, &lua.ctx())))
        }
    }
}
impl<R: FromLuauMulti> Future for AsyncThread<R> {
    type Output = Result<R>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let lua = self.thread.0.lua.raw();
        let nargs = match self.thread.status_inner(lua) {
            ThreadStatusInner::New(nargs) | ThreadStatusInner::Yielded(nargs) => nargs,
            _ => return Poll::Ready(Err(Error::CoroutineUnresumable)),
        };

        let state = lua.state();
        let thread_state = self.thread.state();
        unsafe {
            let _sg = StackGuard::new(state);
            let _thread_sg = StackGuard::with_top(thread_state, 0);
            let _wg = WakerGuard::new(lua, cx.waker());

            let (status, nresults) = self.thread.resume_inner(lua, nargs)?;

            if status.is_yielded() {
                if !(nresults == 1 && is_poll_pending(thread_state)) {
                    // Ignore values returned via yield()
                    cx.waker().wake_by_ref();
                }
                return Poll::Pending;
            }

            check_stack(state, nresults + 1)?;
            ffi::lua_xmove(thread_state, state, nresults);

            Poll::Ready(R::from_stack_multi(nresults, &lua.ctx()))
        }
    }
}
#[inline(always)]
unsafe fn is_poll_pending(state: *mut ffi::lua_State) -> bool {
    ffi::lua_tolightuserdata(state, -1) == crate::Luau::poll_pending().0
}
struct WakerGuard<'lua> {
    lua: &'lua RawLuau,
    prev: Waker,
}
impl<'lua> WakerGuard<'lua> {
    #[inline]
    pub fn new(lua: &'lua RawLuau, waker: &Waker) -> Self {
        let prev = lua.set_waker(waker);
        Self { lua, prev }
    }
}
impl Drop for WakerGuard<'_> {
    fn drop(&mut self) {
        self.lua.set_waker(&self.prev);
    }
}

#[cfg(test)]
mod assertions {
    use super::*;

    static_assertions::assert_not_impl_any!(Thread: Send);
    static_assertions::assert_not_impl_any!(AsyncThread<()>: Send);
}
