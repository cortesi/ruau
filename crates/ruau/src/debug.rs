//! Luau debugging interface.
//!
//! This module provides access to the Luau debug interface, allowing inspection of the call stack,
//! and function information. The main type is [`struct@Debug`] for accessing debug information.

use std::{borrow::Cow, os::raw::c_int};

use ffi::{lua_Debug, lua_State};

use crate::{
    function::Function,
    state::RawLuau,
    util::{StackGuard, assert_stack, linenumber_to_usize, ptr_to_lossy_str, ptr_to_str},
};

/// Contains information about currently executing Luau code.
///
/// You may call the methods on this structure to retrieve information about the Luau code executing
/// at the specific level. Further information can be found in the Luau [documentation].
///
/// [documentation]: https://www.lua.org/manual/5.4/manual.html#lua_Debug
pub struct Debug<'a> {
    state: *mut lua_State,
    lua: &'a RawLuau,
    level: c_int,
    ar: *mut lua_Debug,
}

impl<'a> Debug<'a> {
    pub(crate) fn new(lua: &'a RawLuau, level: c_int, ar: *mut lua_Debug) -> Self {
        Debug {
            state: lua.state(),
            lua,
            ar,
            level,
        }
    }

    /// Returns the function that is running at the given level.
    ///
    /// Corresponds to the `f` "what" mask.
    pub fn function(&self) -> Function {
        unsafe {
            let _sg = StackGuard::new(self.state);
            assert_stack(self.state, 1);

            ruau_assert!(
                ffi::lua_getinfo(self.state, self.level, cstr!("f"), self.ar) != 0,
                "lua_getinfo failed with `f`"
            );

            ffi::lua_xmove(self.state, self.lua.ref_thread(), 1);
            Function(self.lua.pop_ref_thread())
        }
    }

    /// Corresponds to the `n` "what" mask.
    pub fn names(&self) -> DebugNames<'_> {
        unsafe {
            ruau_assert!(
                ffi::lua_getinfo(self.state, self.level, cstr!("n"), self.ar) != 0,
                "lua_getinfo failed with `n`"
            );

            DebugNames {
                name: ptr_to_lossy_str((*self.ar).name),
                name_what: None,
            }
        }
    }

    /// Corresponds to the `S` "what" mask.
    pub fn source(&self) -> DebugSource<'_> {
        unsafe {
            ruau_assert!(
                ffi::lua_getinfo(self.state, self.level, cstr!("s"), self.ar) != 0,
                "lua_getinfo failed with `s`"
            );

            DebugSource {
                source: ptr_to_lossy_str((*self.ar).source),
                short_src: ptr_to_lossy_str((*self.ar).short_src),
                line_defined: linenumber_to_usize((*self.ar).linedefined),
                last_line_defined: None,
                what: ptr_to_str((*self.ar).what).unwrap_or("main"),
            }
        }
    }

    /// Corresponds to the `l` "what" mask. Returns the current line.
    pub fn current_line(&self) -> Option<usize> {
        unsafe {
            ruau_assert!(
                ffi::lua_getinfo(self.state, self.level, cstr!("l"), self.ar) != 0,
                "lua_getinfo failed with `l`"
            );

            linenumber_to_usize((*self.ar).currentline)
        }
    }

    /// Corresponds to the `u` "what" mask.
    pub fn stack(&self) -> DebugStack {
        unsafe {
            ruau_assert!(
                ffi::lua_getinfo(self.state, self.level, cstr!("au"), self.ar) != 0,
                "lua_getinfo failed with `au`"
            );

            DebugStack {
                num_upvalues: (*self.ar).nupvals,
                num_params: (*self.ar).nparams,
                is_vararg: (*self.ar).isvararg != 0,
            }
        }
    }
}

/// Contains the name information of a function in the call stack.
///
/// Returned by the [`Debug::names`] method.
#[derive(Clone, Debug)]
pub struct DebugNames<'a> {
    /// A (reasonable) name of the function (`None` if the name cannot be found).
    pub name: Option<Cow<'a, str>>,
    /// Explains the `name` field (can be `global`/`local`/`method`/`field`/`upvalue`/etc).
    ///
    /// Always `None` for Luau.
    pub name_what: Option<&'static str>,
}

/// Contains the source information of a function in the call stack.
///
/// Returned by the [`Debug::source`] method.
#[derive(Clone, Debug)]
pub struct DebugSource<'a> {
    /// Source of the chunk that created the function.
    pub source: Option<Cow<'a, str>>,
    /// A "printable" version of `source`, to be used in error messages.
    pub short_src: Option<Cow<'a, str>>,
    /// The line number where the definition of the function starts.
    pub line_defined: Option<usize>,
    /// The line number where the definition of the function ends (not set by Luau).
    pub last_line_defined: Option<usize>,
    /// A string `Lua` if the function is a Luau-defined function, `C` if it is a C function,
    /// `main` if it is the main part of a chunk.
    pub what: &'static str,
}

/// Contains stack information about a function in the call stack.
///
/// Returned by the [`Debug::stack`] method.
#[derive(Copy, Clone, Debug)]
pub struct DebugStack {
    /// The number of upvalues of the function.
    pub num_upvalues: u8,
    /// The number of parameters of the function (always 0 for C).
    pub num_params: u8,
    /// Whether the function is a variadic function (always true for C).
    pub is_vararg: bool,
}
