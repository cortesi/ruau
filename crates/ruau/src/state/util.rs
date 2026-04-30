use std::{
    os::raw::c_int,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    rc::Rc,
};

use crate::{
    error::{Error, Result},
    state::{ExtraData, RawLuau},
    util::{self, WrappedFailure, get_internal_metatable},
};

struct StateGuard<'a>(&'a RawLuau, *mut ffi::lua_State);

impl<'a> StateGuard<'a> {
    fn new(inner: &'a RawLuau, mut state: *mut ffi::lua_State) -> Self {
        state = inner.state.replace(state);
        Self(inner, state)
    }
}

impl Drop for StateGuard<'_> {
    fn drop(&mut self) {
        self.0.state.set(self.1);
    }
}

enum PreallocatedFailure {
    New(*mut WrappedFailure),
    Reserved,
}

impl PreallocatedFailure {
    unsafe fn reserve(state: *mut ffi::lua_State, extra: *mut ExtraData) -> Self {
        if (*extra).wrapped_failure_top > 0 {
            (*extra).wrapped_failure_top -= 1;
            return Self::Reserved;
        }

        // We need to check stack for Luau in case when callback is called from interrupt
        // See https://github.com/luau-lang/luau/issues/446 and ruau #142 and #153
        ffi::lua_rawcheckstack(state, 2);
        // Place it to the beginning of the stack
        let ud = WrappedFailure::new_userdata(state);
        ffi::lua_insert(state, 1);
        Self::New(ud)
    }

    #[cold]
    unsafe fn r#use(
        &self,
        state: *mut ffi::lua_State,
        extra: *mut ExtraData,
    ) -> *mut WrappedFailure {
        let ref_thread = (*extra).ref_thread;
        match *self {
            Self::New(ud) => {
                ffi::lua_settop(state, 1);
                ud
            }
            Self::Reserved => {
                let index = (*extra).wrapped_failure_pool.pop().unwrap();
                ffi::lua_settop(state, 0);
                ffi::lua_rawcheckstack(state, 2);
                ffi::lua_xpush(ref_thread, state, index);
                ffi::lua_pushnil(ref_thread);
                ffi::lua_replace(ref_thread, index);
                (*extra).ref_free.push(index);
                ffi::lua_touserdata(state, -1) as *mut WrappedFailure
            }
        }
    }

    unsafe fn release(self, state: *mut ffi::lua_State, extra: *mut ExtraData) {
        let ref_thread = (*extra).ref_thread;
        match self {
            Self::New(_) => {
                ffi::lua_rotate(state, 1, -1);
                ffi::lua_xmove(state, ref_thread, 1);
                let index = (*extra).ref_stack_pop();
                (*extra).wrapped_failure_pool.push(index);
                (*extra).wrapped_failure_top += 1;
            }
            Self::Reserved => (*extra).wrapped_failure_top += 1,
        }
    }
}

// An optimized version of `callback_error` that does not allocate `WrappedFailure` userdata
// and instead reuses unused values from previous calls (or allocates new).
pub unsafe fn callback_error_ext<F, R>(
    state: *mut ffi::lua_State,
    mut extra: *mut ExtraData,
    wrap_error: bool,
    f: F,
) -> R
where
    F: FnOnce(*mut ExtraData, c_int) -> Result<R>,
{
    if extra.is_null() {
        extra = ExtraData::get(state);
    }

    let nargs = ffi::lua_gettop(state);

    // We cannot shadow Rust errors with Luau ones, so we need to reserve pre-allocated memory
    // to store a wrapped failure (error or panic) *before* we proceed.
    let prealloc_failure = PreallocatedFailure::reserve(state, extra);

    match catch_unwind(AssertUnwindSafe(|| {
        let rawlua = (*extra).raw_luau();
        let _guard = StateGuard::new(rawlua, state);
        f(extra, nargs)
    })) {
        Ok(Ok(r)) => {
            // Return unused `WrappedFailure` to the pool
            prealloc_failure.release(state, extra);
            r
        }
        Ok(Err(err)) => {
            let wrapped_error = prealloc_failure.r#use(state, extra);

            if !wrap_error {
                ptr::write(wrapped_error, WrappedFailure::Error(err));
                get_internal_metatable::<WrappedFailure>(state);
                ffi::lua_setmetatable(state, -2);
                ffi::lua_error(state)
            }

            // Build `CallbackError` with traceback
            let traceback = if ffi::lua_checkstack(state, ffi::LUA_TRACEBACK_STACK) != 0 {
                ffi::luaL_traceback(state, state, ptr::null(), 0);
                let traceback = util::to_string(state, -1);
                ffi::lua_pop(state, 1);
                traceback
            } else {
                "<not enough stack space for traceback>".to_string()
            };
            let cause = Rc::new(err);
            ptr::write(
                wrapped_error,
                WrappedFailure::Error(Error::CallbackError { traceback, cause }),
            );
            get_internal_metatable::<WrappedFailure>(state);
            ffi::lua_setmetatable(state, -2);

            ffi::lua_error(state)
        }
        Err(p) => {
            let wrapped_panic = prealloc_failure.r#use(state, extra);
            ptr::write(wrapped_panic, WrappedFailure::Panic(Some(p)));
            get_internal_metatable::<WrappedFailure>(state);
            ffi::lua_setmetatable(state, -2);
            ffi::lua_error(state)
        }
    }
}
