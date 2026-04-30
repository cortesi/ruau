//! Diagnostic API for inspecting a running Luau VM.
//!
//! This module collects stack inspection, traceback rendering, function metadata, and heap
//! dumping in one place. These operations are not part of the normal embedding workflow; they
//! are tools for error reporting, profiling, and debugging.

mod stack;

pub use stack::{Debug, DebugNames, DebugSource, DebugStack};

use crate::{error::Result, state::Luau, string::LuauString};
pub use crate::{
    function::{CoverageInfo, FunctionInfo},
    runtime::HeapDump,
};

/// Gets information about the interpreter runtime stack at the given level.
///
/// This function calls callback `f`, passing the [`struct@Debug`] structure that can be used to
/// get information about the function executing at a given level. Level `0` is the current
/// running function; level `n+1` is the function that called level `n` (except for tail calls,
/// which do not count in the stack).
pub fn inspect_stack<R>(lua: &Luau, level: usize, f: impl FnOnce(&Debug) -> R) -> Option<R> {
    lua.inspect_stack(level, f)
}

/// Creates a traceback of the call stack at the given level.
///
/// The `msg` parameter, if provided, is added at the beginning of the traceback. The `level`
/// parameter works the same way as in [`inspect_stack`].
pub fn traceback(lua: &Luau, msg: Option<&str>, level: usize) -> Result<LuauString> {
    lua.traceback(msg, level)
}
