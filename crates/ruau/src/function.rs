//! Luau function handling.
//!
//! This module provides types for working with Luau functions from Rust, including
//! both Luau-defined functions and native Rust callbacks.
//!
//! # Calling Functions
//!
//! Use [`Function::call`] to invoke a Luau function:
//!
//! ```
//! # use ruau::{Function, Luau, Result};
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<()> {
//! let lua = Luau::new();
//!
//! // Get a built-in function
//! let print: Function = lua.globals().get("print")?;
//! print.call::<()>("Hello from Rust!").await?;
//!
//! // Call a function that returns values
//! let tonumber: Function = lua.globals().get("tonumber")?;
//! let n: i32 = tonumber.call("42").await?;
//! assert_eq!(n, 42);
//! # Ok(())
//! # }
//! ```
//!
//! # Creating Functions
//!
//! Functions can be created from Rust closures using [`Luau::create_function`]:
//!
//! ```
//! # use ruau::{Luau, Result};
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<()> {
//! let lua = Luau::new();
//!
//! let greet = lua.create_function(|_, name: String| {
//!     Ok(format!("Hello, {}!", name))
//! })?;
//!
//! lua.globals().set("greet", greet)?;
//! let result: String = lua.load(r#"greet("World")"#).eval().await?;
//! assert_eq!(result, "Hello, World!");
//! # Ok(())
//! # }
//! ```
//!
//! # Function Environments
//!
//! Luau functions have an associated environment table that determines how global
//! variables are resolved. Use [`Function::environment`] and [`Function::set_environment`]
//! to inspect or modify this environment.

use std::{
    cell::RefCell,
    mem,
    os::raw::{c_int, c_void},
    ptr,
    result::Result as StdResult,
    slice,
};

use crate::{
    debug,
    error::{Error, Result},
    multi::MultiValue,
    table::Table,
    traits::{FromLuauMulti, IntoLuauMulti},
    types::ValueRef,
    util::{
        StackGuard, assert_stack, check_stack, linenumber_to_usize, pop_error, ptr_to_lossy_str, ptr_to_str,
    },
    value::Value,
};

/// Handle to an internal Luau function.
#[derive(Clone, Debug, PartialEq)]
pub struct Function(pub(crate) ValueRef);

/// Contains information about a function.
///
/// This mirrors the information Luau exposes through its debug API.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct FunctionInfo {
    /// A (reasonable) name of the function (`None` if the name cannot be found).
    pub name: Option<String>,
    /// A string `Lua` if the function is a Luau-defined function (matching Luau's C API label),
    /// `C` if it is a C function, `main` if it is the main part of a chunk.
    pub what: &'static str,
    /// Source of the chunk that created the function.
    pub source: Option<String>,
    /// A "printable" version of `source`, to be used in error messages.
    pub short_src: Option<String>,
    /// The line number where the definition of the function starts.
    pub line_defined: Option<usize>,
    /// The number of upvalues of the function.
    pub num_upvalues: u8,
    /// The number of parameters of the function (always 0 for C).
    pub num_params: u8,
    /// Whether the function is a variadic function (always true for C).
    pub is_vararg: bool,
}

/// Luau function coverage snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageInfo {
    /// Function name, when available.
    pub function: Option<String>,
    /// Line where the function was defined.
    pub line_defined: i32,
    /// Nesting depth in the coverage tree.
    pub depth: i32,
    /// Per-line hit counts.
    pub hits: Vec<i32>,
}

/// Structured failure returned by [`Function::protected_call`].
#[derive(Clone, Debug)]
pub struct ProtectedCallError {
    /// Luau value raised by the script when it can be represented by `ruau`.
    pub error: Value,
    /// Traceback captured by the protected call wrapper.
    pub traceback: String,
}

impl Function {
    /// Calls the function synchronously from crate internals.
    pub(crate) fn call_sync<R: FromLuauMulti>(&self, args: impl IntoLuauMulti) -> Result<R> {
        let lua = self.0.lua.raw();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 2)?;

            // Push error handler
            lua.push_error_traceback();
            let stack_start = ffi::lua_gettop(state);
            // Push function and the arguments
            lua.push_ref(&self.0);
            let nargs = args.push_into_stack_multi(&lua.ctx())?;
            // Call the function
            let ret = ffi::lua_pcall(state, nargs, ffi::LUA_MULTRET, stack_start);
            if ret != ffi::LUA_OK {
                return Err(pop_error(state, ret));
            }
            // Get the results
            let nresults = ffi::lua_gettop(state) - stack_start;
            R::from_stack_multi(nresults, &lua.ctx())
        }
    }

    /// Returns a future that, when polled, calls `self`, passing `args` as function arguments,
    /// and drives the execution.
    ///
    /// Internally it wraps the function to an [`AsyncThread`]. The returned type implements
    /// `Future<Output = Result<R>>` and can be awaited.
    ///
    /// The returned future is local to the VM and is not `Send`. If it is spawned, use
    /// [`LocalSet`] on a current-thread Tokio runtime. Use
    /// [`crate::LuauWorkerHandle`] when the caller needs a `Send` handle from multi-thread Tokio
    /// tasks.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// # use ruau::{Luau, Result};
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> Result<()> {
    /// # let lua = Luau::new();
    ///
    /// let sleep = lua.create_async_function(async move |_lua, n: u64| {
    ///     tokio::time::sleep(Duration::from_millis(n)).await;
    ///     Ok(())
    /// })?;
    ///
    /// sleep.call::<()>(10).await?;
    ///
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// [`AsyncThread`]: crate::thread::AsyncThread
    pub async fn call<R>(&self, args: impl IntoLuauMulti) -> Result<R>
    where
        R: FromLuauMulti,
    {
        let lua = self.0.lua.raw();
        let thread = unsafe {
            let th = lua.create_recycled_thread(self)?;
            let mut th = th.into_async(args)?;
            th.set_recyclable(true);
            th
        };
        thread.await
    }

    /// Calls the function and returns script-thrown errors as data.
    pub async fn protected_call(
        &self,
        args: impl IntoLuauMulti,
    ) -> Result<StdResult<MultiValue, ProtectedCallError>> {
        let lua = self.0.lua.raw().lua();
        let error_handler = lua.create_function(|lua, error: Value| {
            let message = error.to_string()?;
            let traceback = debug::traceback(lua, Some(&message), 1)?.to_string_lossy();
            let captured_error = match error {
                Value::Nil
                | Value::Boolean(_)
                | Value::Integer(_)
                | Value::Number(_)
                | Value::String(_)
                | Value::Table(_) => error,
                _ => Value::String(lua.create_string(&message)?),
            };
            let captured = lua.create_table()?;
            captured.raw_set("error", captured_error)?;
            captured.raw_set("traceback", traceback)?;
            Ok(captured)
        })?;
        let wrapper: Self = lua
            .load(
                r#"
return function(on_error, fn, ...)
    local result = table.pack(xpcall(fn, on_error, ...))
    if result[1] == false then
        local captured = result[2]
        return {
            ok = false,
            error = captured.error,
            traceback = captured.traceback,
        }
    end
    result.ok = true
    return result
end
"#,
            )
            .try_cache()
            .name("=__ruau_protected_call")
            .eval()
            .await?;

        let mut values = args.into_luau_multi(lua)?;
        values.push_front(Value::Function(self.clone()));
        values.push_front(Value::Function(error_handler));
        let packed: Table = wrapper.call(values).await?;
        let ok: bool = packed.raw_get("ok")?;
        if !ok {
            return Ok(Err(ProtectedCallError {
                error: packed.raw_get("error")?,
                traceback: packed.raw_get("traceback")?,
            }));
        }

        let count = packed.raw_get::<usize>("n").unwrap_or(1).saturating_sub(1);
        let mut returns = MultiValue::new();
        for index in 2..=(count + 1) {
            returns.push_back(packed.raw_get(index)?);
        }
        Ok(Ok(returns))
    }

    /// Returns a function that, when called, calls `self`, passing `args` as the first set of
    /// arguments.
    ///
    /// If any arguments are passed to the returned function, they will be passed after `args`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ruau::{Function, Luau, Result};
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> Result<()> {
    /// # let lua = Luau::new();
    /// let sum: Function = lua.load(
    ///     r#"
    ///         function(a, b)
    ///             return a + b
    ///         end
    /// "#).eval().await?;
    ///
    /// let bound_a = sum.bind(1)?;
    /// assert_eq!(bound_a.call::<u32>(2).await?, 1 + 2);
    ///
    /// let bound_a_and_b = sum.bind(13)?.bind(57)?;
    /// assert_eq!(bound_a_and_b.call::<u32>(()).await?, 13 + 57);
    ///
    /// # Ok(())
    /// # }
    /// ```
    pub fn bind(&self, args: impl IntoLuauMulti) -> Result<Self> {
        unsafe extern "C-unwind" fn args_wrapper_impl(state: *mut ffi::lua_State) -> c_int {
            let nargs = ffi::lua_gettop(state);
            let nbinds = ffi::lua_tointeger(state, ffi::lua_upvalueindex(1)) as c_int;
            ffi::luaL_checkstack(state, nbinds, ptr::null());

            for i in 0..nbinds {
                ffi::lua_pushvalue(state, ffi::lua_upvalueindex(i + 2));
            }
            if nargs > 0 {
                ffi::lua_rotate(state, 1, nbinds);
            }

            nargs + nbinds
        }

        let lua = self.0.lua.raw();
        let state = lua.state();

        let args = args.into_luau_multi(lua.lua())?;

        if args.is_empty() {
            return Ok(self.clone());
        }

        if args.len() >= ffi::LUA_MAX_UPVALUES as usize {
            return Err(Error::BindError);
        }

        let nargs: c_int = args.len().try_into().map_err(|_| Error::BindError)?;
        let args_wrapper = unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, nargs + 3)?;

            ffi::lua_pushinteger(state, nargs as ffi::lua_Integer);
            for arg in &args {
                lua.push_value(arg)?;
            }
            protect_lua!(state, nargs + 1, 1, fn(state) {
                ffi::lua_pushcclosure(state, args_wrapper_impl, ffi::lua_gettop(state));
            })?;

            Self(lua.pop_ref())
        };

        let lua = lua.lua();
        lua.load(
            r#"
            local func, args_wrapper = ...
            return function(...)
                return func(args_wrapper(...))
            end
            "#,
        )
        .try_cache()
        .name("=__ruau_bind")
        .call_sync((self, args_wrapper))
    }

    /// Returns the environment of the Luau function.
    ///
    /// By default Luau functions shares a global environment.
    ///
    /// This function always returns `None` for Rust/C functions.
    pub fn environment(&self) -> Option<Table> {
        let lua = self.0.lua.raw();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            assert_stack(state, 1);

            lua.push_ref(&self.0);
            if ffi::lua_iscfunction(state, -1) != 0 {
                return None;
            }

            ffi::lua_getfenv(state, -1);

            if ffi::lua_type(state, -1) != ffi::LUA_TTABLE {
                return None;
            }
            Some(Table(lua.pop_ref()))
        }
    }

    /// Sets the environment of the Luau function.
    ///
    /// The environment is a table that is used as the global environment for the function.
    /// Returns `true` if environment successfully changed, `false` otherwise.
    ///
    /// This function does nothing for Rust/C functions.
    pub fn set_environment(&self, env: Table) -> Result<bool> {
        let lua = self.0.lua.raw();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 2)?;

            lua.push_ref(&self.0);
            if ffi::lua_iscfunction(state, -1) != 0 {
                drop(env);
                return Ok(false);
            }

            {
                lua.push_ref(&env.0);
                ffi::lua_setfenv(state, -2);
            }

            drop(env);
            Ok(true)
        }
    }

    /// Returns information about the function.
    ///
    /// Corresponds to the `>Snu` (`>Sn` for Luau) what mask for
    /// Luau's `lua_getinfo` when applied to the function.
    pub fn info(&self) -> FunctionInfo {
        let lua = self.0.lua.raw();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            assert_stack(state, 1);

            let mut ar: ffi::lua_Debug = mem::zeroed();
            lua.push_ref(&self.0);

            let res = ffi::lua_getinfo(state, -1, cstr!("snau"), &mut ar);
            ruau_assert!(res != 0, "lua_getinfo failed with `snau`");

            FunctionInfo {
                name: ptr_to_lossy_str(ar.name).map(|s| s.into_owned()),
                what: ptr_to_str(ar.what).unwrap_or("main"),
                source: ptr_to_lossy_str(ar.source).map(|s| s.into_owned()),
                short_src: ptr_to_lossy_str(ar.short_src).map(|s| s.into_owned()),
                line_defined: linenumber_to_usize(ar.linedefined),
                num_upvalues: ar.nupvals,
                num_params: ar.nparams,
                is_vararg: ar.isvararg != 0,
            }
        }
    }

    /// Retrieves recorded coverage information about this Luau function including inner calls.
    ///
    /// This function takes a callback as an argument and calls it providing [`CoverageInfo`]
    /// snapshot per each executed inner function.
    ///
    /// Recording of coverage information is controlled by [`Compiler::coverage_level`] option.
    ///
    /// [`Compiler::coverage_level`]: crate::chunk::Compiler::coverage_level
    pub fn coverage<F>(&self, func: F)
    where
        F: FnMut(CoverageInfo),
    {
        use std::{ffi::CStr, os::raw::c_char};

        unsafe extern "C-unwind" fn callback<F: FnMut(CoverageInfo)>(
            data: *mut c_void,
            function: *const c_char,
            line_defined: c_int,
            depth: c_int,
            hits: *const c_int,
            size: usize,
        ) {
            let function = if !function.is_null() {
                Some(CStr::from_ptr(function).to_string_lossy().to_string())
            } else {
                None
            };
            let rust_callback = &*(data as *const RefCell<F>);
            if let Ok(mut rust_callback) = rust_callback.try_borrow_mut() {
                // Call the Rust callback with CoverageInfo
                rust_callback(CoverageInfo {
                    function,
                    line_defined,
                    depth,
                    hits: slice::from_raw_parts(hits, size).to_vec(),
                });
            }
        }

        let lua = self.0.lua.raw();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            assert_stack(state, 1);

            lua.push_ref(&self.0);
            let func = RefCell::new(func);
            let func_ptr = &func as *const RefCell<F> as *mut c_void;
            ffi::lua_getcoverage(state, -1, func_ptr, callback::<F>);
        }
    }

    /// Converts this function to a generic C pointer.
    ///
    /// There is no way to convert the pointer back to its original value.
    ///
    /// Typically this function is used only for hashing and debug information.
    #[inline]
    pub fn to_pointer(&self) -> *const c_void {
        self.0.to_pointer()
    }

    /// Creates a deep clone of the Luau function.
    ///
    /// Copies the function prototype and all its upvalues to the
    /// newly created function.
    /// This function returns shallow clone (same handle) for Rust/C functions.
    pub fn deep_clone(&self) -> Result<Self> {
        let lua = self.0.lua.raw();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 2)?;

            lua.push_ref(&self.0);
            if ffi::lua_iscfunction(state, -1) != 0 {
                return Ok(self.clone());
            }

            if lua.unlikely_memory_error() {
                ffi::lua_clonefunction(state, -1);
            } else {
                protect_lua!(state, 1, 1, fn(state) ffi::lua_clonefunction(state, -1))?;
            }
            Ok(Self(lua.pop_ref()))
        }
    }
}

#[cfg(test)]
mod assertions {
    use super::*;

    static_assertions::assert_not_impl_any!(Function: Send);
}
