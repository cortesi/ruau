//! Engine builtins: Rust-backed functions with full VM access (heap + thread +
//! re-entry), unlike leaf `HostFunction`s. They back the base global surface
//! (`assert`/`type`/`tostring`/`tonumber`/`error`/`print`/`setmetatable`/
//! `getmetatable`/`pcall`/`raw*`/`next`/`pairs`/`ipairs`) and the
//! `coroutine`/`string`/`math`/`table`/`bit32` library tables.
//!
//! A builtin is reached through a closure over a native [`Proto`](crate::object::Proto)
//! (`Proto::native`); `precall` dispatches it synchronously without a frame.

use std::{
    sync::Arc,
    task::{Context, Poll},
};

use ruau_vm_api::{RawGc, RawValue, marker};

use crate::{
    call::{
        Exec, RuntimeErrorKind, call_value, err, err_at_level, err_gas, err_handler_failure,
        err_kind, err_memory, err_memory_limit, err_no_location, err_register_stack_oom, err_value,
        materialize, protected_call, reserve_call_entries,
    },
    datetime,
    heap::Heap,
    load::{LoadMode, load_module_with_limits, load_with_limits},
    object::{LuaBuffer, Proto},
    pack, pattern,
    state::{
        CallInfo, CallStackEntry, FrameSnapshot, RequireInfo, SuspendedRequire,
        SuspendedRequireStage, SuspendedTarget, Thread,
    },
    table::{LuaTable, NextStep},
    tm::{self, MetaEvent},
    vmutils,
};

mod base_lib;
mod bit32_lib;
mod buffer_lib;
mod common;
mod conformance_lib;
mod debug_lib;
mod numeric_lib;
mod os_lib;
mod require_load;
mod string_lib;
mod table_lib;
mod utf8_lib;
mod vector_lib;

pub use base_lib::type_name;
use base_lib::{builtin_tostring, metatable_protection};
pub use common::{StrArg, meter_string_growth};
use common::{
    arg_bytes, arg_int, arg_str, intern_result, is_truthy, posrelat, read_array, string_lossy,
};
pub use require_load::{
    RequireBodyStart, RequireCallSite, RequireCallStep, clear_require_loading,
    continue_require_after_resolve, finish_require_read_error, normalize_require_exports,
    release_suspended_require, require_resolve_error, start_require, start_require_body,
};
use table_lib::{get_index, require_writable, set_index};

pub const ASYNC_REQUIRE_SYNC_ENTRY_ERROR: &str =
    "async entry required: use the async driver to run pending require";

/// An engine builtin. Each maps to a global injected by the minimal surface.
// `pub` rather than `pub(crate)`: the conformance-only variants are
// constructed solely under `feature = "conformance"`, and demoting the enum
// makes plain builds flag them dead — gating ~57 construction/match sites
// for a type no public accessor returns isn't worth it. Unnameability is
// fine here: every method touching `Builtin` is crate-private.
#[allow(unnameable_types)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum Builtin {
    Type,
    Typeof,
    ToString,
    Assert,
    Error,
    Print,
    SetMetatable,
    GetMetatable,
    Pcall,
    Xpcall,
    ToNumber,
    RawEqual,
    RawGet,
    RawSet,
    RawLen,
    Select,
    Loadstring,
    Require,
    CollectGarbage,
    GcInfo,
    Next,
    INext,
    Pairs,
    IPairs,
    CoroutineCreate,
    CoroutineResume,
    CoroutineYield,
    CoroutineStatus,
    CoroutineRunning,
    CoroutineClose,
    CoroutineIsYieldable,
    StringLen,
    StringSub,
    StringRep,
    StringUpper,
    StringLower,
    StringReverse,
    StringByte,
    StringChar,
    StringFormat,
    StringFind,
    StringMatch,
    StringGmatch,
    StringGmatchAux,
    StringGsub,
    StringSplit,
    StringPack,
    StringPacksize,
    StringUnpack,
    MathFloor,
    MathCeil,
    MathAbs,
    MathSqrt,
    MathMax,
    MathMin,
    MathExp,
    MathLog,
    MathLog10,
    MathPow,
    MathFmod,
    MathModf,
    MathFrexp,
    MathLdexp,
    MathSin,
    MathCos,
    MathTan,
    MathAsin,
    MathAcos,
    MathAtan,
    MathAtan2,
    MathSinh,
    MathCosh,
    MathTanh,
    MathRad,
    MathDeg,
    MathSign,
    MathRound,
    MathClamp,
    MathLerp,
    MathMap,
    MathIsNan,
    MathIsInf,
    MathIsFinite,
    MathRandom,
    MathRandomseed,
    MathNoise,
    IntegerCreate,
    IntegerFromString,
    IntegerNeg,
    IntegerAdd,
    IntegerSub,
    IntegerMul,
    IntegerDiv,
    IntegerIDiv,
    IntegerUDiv,
    IntegerURem,
    IntegerMod,
    IntegerRem,
    IntegerMin,
    IntegerMax,
    IntegerClamp,
    IntegerBand,
    IntegerBor,
    IntegerBnot,
    IntegerBxor,
    IntegerBtest,
    IntegerLt,
    IntegerLe,
    IntegerUlt,
    IntegerUle,
    IntegerGt,
    IntegerGe,
    IntegerUgt,
    IntegerUge,
    IntegerLshift,
    IntegerRshift,
    IntegerArshift,
    IntegerLrotate,
    IntegerRrotate,
    IntegerExtract,
    IntegerReplace,
    IntegerCountrz,
    IntegerCountlz,
    IntegerBswap,
    TableInsert,
    TableRemove,
    TableConcat,
    TableSort,
    TablePack,
    TableUnpack,
    TableMove,
    TableCreate,
    TableFind,
    TableGetn,
    TableMaxn,
    TableFreeze,
    TableIsFrozen,
    TableClone,
    TableClear,
    TableForeach,
    TableForeachI,
    Bit32Band,
    Bit32Bor,
    Bit32Bxor,
    Bit32Bnot,
    Bit32Lshift,
    Bit32Rshift,
    Bit32Arshift,
    Bit32Btest,
    Bit32Lrotate,
    Bit32Rrotate,
    Bit32Extract,
    Bit32Replace,
    Bit32Countlz,
    Bit32Countrz,
    Bit32Byteswap,
    Utf8Char,
    Utf8Codepoint,
    Utf8Len,
    Utf8Offset,
    Utf8Codes,
    Utf8CodesAux,
    OsTime,
    OsClock,
    OsDate,
    OsDifftime,
    BufferCreate,
    BufferFromString,
    BufferToString,
    BufferLen,
    BufferReadI8,
    BufferReadU8,
    BufferReadI16,
    BufferReadU16,
    BufferReadI32,
    BufferReadU32,
    BufferReadF32,
    BufferReadF64,
    BufferWriteI8,
    BufferWriteU8,
    BufferWriteI16,
    BufferWriteU16,
    BufferWriteI32,
    BufferWriteU32,
    BufferWriteF32,
    BufferWriteF64,
    BufferReadString,
    BufferWriteString,
    BufferReadBits,
    BufferWriteBits,
    BufferReadInteger,
    BufferWriteInteger,
    BufferCopy,
    BufferFill,
    VectorCreate,
    VectorMagnitude,
    VectorNormalize,
    VectorCross,
    VectorDot,
    VectorFloor,
    VectorCeil,
    VectorAbs,
    VectorSign,
    VectorLerp,
    VectorAngle,
    VectorClamp,
    VectorMin,
    VectorMax,
    DebugInfo,
    DebugTraceback,
    CompatGetFenv,
    CompatSetFenv,
    ConformanceGetCoverage,
    ConformanceResumeError,
    ConformanceSetBlockAllocations,
    ConformanceSingleYield,
    ConformanceMultipleYields,
    ConformanceMultipleYieldsWithNestedCall,
    ConformancePassthroughCall,
    ConformancePassthroughCallMoreResults,
    ConformancePassthroughCallArgReuse,
    ConformancePassthroughCallVaradic,
    ConformancePassthroughCallWithState,
}

/// How a builtin was reached. Bytecode calls have a script call site; native
/// re-entry (`pcall(error, ...)`, a metamethod calling a builtin, or a host
/// helper) does not add a Lua frame of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinCallSite {
    Bytecode,
    Native,
}

impl Builtin {
    /// The name this builtin installs under — a flat global for the base surface,
    /// a field name within the `coroutine` table for the coroutine members.
    #[must_use]
    pub fn global_name(self) -> &'static [u8] {
        match self {
            Self::Type => b"type",
            Self::Typeof => b"typeof",
            Self::ToString => b"tostring",
            Self::Assert => b"assert",
            Self::Error => b"error",
            Self::Print => b"print",
            Self::SetMetatable => b"setmetatable",
            Self::GetMetatable => b"getmetatable",
            Self::Pcall => b"pcall",
            Self::Xpcall => b"xpcall",
            Self::ToNumber => b"tonumber",
            Self::RawEqual => b"rawequal",
            Self::RawGet => b"rawget",
            Self::RawSet => b"rawset",
            Self::RawLen => b"rawlen",
            Self::Select => b"select",
            Self::Loadstring => b"loadstring",
            Self::Require => b"require",
            Self::CollectGarbage => b"collectgarbage",
            Self::GcInfo => b"gcinfo",
            Self::Next => b"next",
            Self::INext => b"inext",
            Self::Pairs => b"pairs",
            Self::IPairs => b"ipairs",
            Self::CoroutineCreate => b"create",
            Self::CoroutineResume => b"resume",
            Self::CoroutineYield => b"yield",
            Self::CoroutineStatus => b"status",
            Self::CoroutineRunning => b"running",
            Self::CoroutineIsYieldable => b"isyieldable",
            Self::CoroutineClose => b"close",
            Self::StringLen => b"len",
            Self::StringSub => b"sub",
            Self::StringRep => b"rep",
            Self::StringUpper => b"upper",
            Self::StringLower => b"lower",
            Self::StringReverse => b"reverse",
            Self::StringByte => b"byte",
            Self::StringChar => b"char",
            Self::StringFormat => b"format",
            Self::StringFind => b"find",
            Self::StringMatch => b"match",
            Self::StringSplit => b"split",
            Self::StringPack => b"pack",
            Self::StringPacksize => b"packsize",
            Self::StringUnpack => b"unpack",
            Self::StringGmatch => b"gmatch",
            // The `gmatch` iterator step is internal, never installed by name.
            Self::StringGmatchAux => b"",
            Self::StringGsub => b"gsub",
            Self::MathFloor => b"floor",
            Self::MathCeil => b"ceil",
            Self::MathAbs => b"abs",
            Self::MathSqrt => b"sqrt",
            Self::MathMax => b"max",
            Self::MathMin => b"min",
            Self::MathExp => b"exp",
            Self::MathLog => b"log",
            Self::MathLog10 => b"log10",
            Self::MathPow => b"pow",
            Self::MathFmod => b"fmod",
            Self::MathModf => b"modf",
            Self::MathFrexp => b"frexp",
            Self::MathLdexp => b"ldexp",
            Self::MathSin => b"sin",
            Self::MathCos => b"cos",
            Self::MathTan => b"tan",
            Self::MathAsin => b"asin",
            Self::MathAcos => b"acos",
            Self::MathAtan => b"atan",
            Self::MathAtan2 => b"atan2",
            Self::MathSinh => b"sinh",
            Self::MathCosh => b"cosh",
            Self::MathTanh => b"tanh",
            Self::MathRad => b"rad",
            Self::MathDeg => b"deg",
            Self::MathSign => b"sign",
            Self::MathRound => b"round",
            Self::MathClamp => b"clamp",
            Self::MathLerp => b"lerp",
            Self::MathMap => b"map",
            Self::MathIsNan => b"isnan",
            Self::MathIsInf => b"isinf",
            Self::MathIsFinite => b"isfinite",
            Self::MathRandom => b"random",
            Self::MathRandomseed => b"randomseed",
            Self::MathNoise => b"noise",
            Self::IntegerCreate => b"create",
            Self::IntegerFromString => b"fromstring",
            Self::IntegerNeg => b"neg",
            Self::IntegerAdd => b"add",
            Self::IntegerSub => b"sub",
            Self::IntegerMul => b"mul",
            Self::IntegerDiv => b"div",
            Self::IntegerIDiv => b"idiv",
            Self::IntegerUDiv => b"udiv",
            Self::IntegerURem => b"urem",
            Self::IntegerMod => b"mod",
            Self::IntegerRem => b"rem",
            Self::IntegerMin => b"min",
            Self::IntegerMax => b"max",
            Self::IntegerClamp => b"clamp",
            Self::IntegerBand => b"band",
            Self::IntegerBor => b"bor",
            Self::IntegerBnot => b"bnot",
            Self::IntegerBxor => b"bxor",
            Self::IntegerBtest => b"btest",
            Self::IntegerLt => b"lt",
            Self::IntegerLe => b"le",
            Self::IntegerUlt => b"ult",
            Self::IntegerUle => b"ule",
            Self::IntegerGt => b"gt",
            Self::IntegerGe => b"ge",
            Self::IntegerUgt => b"ugt",
            Self::IntegerUge => b"uge",
            Self::IntegerLshift => b"lshift",
            Self::IntegerRshift => b"rshift",
            Self::IntegerArshift => b"arshift",
            Self::IntegerLrotate => b"lrotate",
            Self::IntegerRrotate => b"rrotate",
            Self::IntegerExtract => b"extract",
            Self::IntegerReplace => b"replace",
            Self::IntegerCountrz => b"countrz",
            Self::IntegerCountlz => b"countlz",
            Self::IntegerBswap => b"bswap",
            Self::TableInsert => b"insert",
            Self::TableRemove => b"remove",
            Self::TableConcat => b"concat",
            Self::TableSort => b"sort",
            Self::TablePack => b"pack",
            Self::TableUnpack => b"unpack",
            Self::TableMove => b"move",
            Self::TableCreate => b"create",
            Self::TableFind => b"find",
            Self::TableGetn => b"getn",
            Self::TableMaxn => b"maxn",
            Self::TableFreeze => b"freeze",
            Self::TableIsFrozen => b"isfrozen",
            Self::TableClone => b"clone",
            Self::TableClear => b"clear",
            Self::TableForeach => b"foreach",
            Self::TableForeachI => b"foreachi",
            Self::Bit32Band => b"band",
            Self::Bit32Bor => b"bor",
            Self::Bit32Bxor => b"bxor",
            Self::Bit32Bnot => b"bnot",
            Self::Bit32Lshift => b"lshift",
            Self::Bit32Rshift => b"rshift",
            Self::Bit32Arshift => b"arshift",
            Self::Bit32Btest => b"btest",
            Self::Bit32Lrotate => b"lrotate",
            Self::Bit32Rrotate => b"rrotate",
            Self::Bit32Extract => b"extract",
            Self::Bit32Replace => b"replace",
            Self::Bit32Countlz => b"countlz",
            Self::Bit32Countrz => b"countrz",
            Self::Bit32Byteswap => b"byteswap",
            Self::Utf8Char => b"char",
            Self::Utf8Codepoint => b"codepoint",
            Self::Utf8Len => b"len",
            Self::Utf8Offset => b"offset",
            Self::Utf8Codes => b"codes",
            // The `codes` iterator function is internal; it is never installed
            // under a name (like `INext` for `ipairs`).
            Self::Utf8CodesAux => b"",
            Self::OsTime => b"time",
            Self::OsClock => b"clock",
            Self::OsDate => b"date",
            Self::OsDifftime => b"difftime",
            Self::BufferCreate => b"create",
            Self::BufferFromString => b"fromstring",
            Self::BufferToString => b"tostring",
            Self::BufferLen => b"len",
            Self::BufferReadI8 => b"readi8",
            Self::BufferReadU8 => b"readu8",
            Self::BufferReadI16 => b"readi16",
            Self::BufferReadU16 => b"readu16",
            Self::BufferReadI32 => b"readi32",
            Self::BufferReadU32 => b"readu32",
            Self::BufferReadF32 => b"readf32",
            Self::BufferReadF64 => b"readf64",
            Self::BufferWriteI8 => b"writei8",
            Self::BufferWriteU8 => b"writeu8",
            Self::BufferWriteI16 => b"writei16",
            Self::BufferWriteU16 => b"writeu16",
            Self::BufferWriteI32 => b"writei32",
            Self::BufferWriteU32 => b"writeu32",
            Self::BufferWriteF32 => b"writef32",
            Self::BufferWriteF64 => b"writef64",
            Self::BufferReadString => b"readstring",
            Self::BufferWriteString => b"writestring",
            Self::BufferReadBits => b"readbits",
            Self::BufferWriteBits => b"writebits",
            Self::BufferReadInteger => b"readinteger",
            Self::BufferWriteInteger => b"writeinteger",
            Self::BufferCopy => b"copy",
            Self::BufferFill => b"fill",
            Self::VectorCreate => b"create",
            Self::VectorMagnitude => b"magnitude",
            Self::VectorNormalize => b"normalize",
            Self::VectorCross => b"cross",
            Self::VectorDot => b"dot",
            Self::VectorFloor => b"floor",
            Self::VectorCeil => b"ceil",
            Self::VectorAbs => b"abs",
            Self::VectorSign => b"sign",
            Self::VectorLerp => b"lerp",
            Self::VectorAngle => b"angle",
            Self::VectorClamp => b"clamp",
            Self::VectorMin => b"min",
            Self::VectorMax => b"max",
            Self::DebugInfo => b"info",
            Self::DebugTraceback => b"traceback",
            Self::CompatGetFenv => b"getfenv",
            Self::CompatSetFenv => b"setfenv",
            Self::ConformanceGetCoverage => b"getcoverage",
            Self::ConformanceResumeError => b"resumeerror",
            Self::ConformanceSetBlockAllocations => b"setblockallocations",
            Self::ConformanceSingleYield => b"singleYield",
            Self::ConformanceMultipleYields => b"multipleYields",
            Self::ConformanceMultipleYieldsWithNestedCall => b"multipleYieldsWithNestedCall",
            Self::ConformancePassthroughCall => b"passthroughCall",
            Self::ConformancePassthroughCallMoreResults => b"passthroughCallMoreResults",
            Self::ConformancePassthroughCallArgReuse => b"passthroughCallArgReuse",
            Self::ConformancePassthroughCallVaradic => b"passthroughCallVaradic",
            Self::ConformancePassthroughCallWithState => b"passthroughCallWithState",
        }
    }

    /// The members of the injected `debug` library table (the safe introspection
    /// surface; no `getupvalue`/`setlocal`/etc.).
    #[must_use]
    pub fn debug_members() -> [Self; 2] {
        [Self::DebugInfo, Self::DebugTraceback]
    }

    /// The members of the injected `vector` library table (`zero`/`one` are vector
    /// constants set directly, like `math`'s numeric constants).
    #[must_use]
    pub fn vector_members() -> [Self; 14] {
        [
            Self::VectorCreate,
            Self::VectorMagnitude,
            Self::VectorNormalize,
            Self::VectorCross,
            Self::VectorDot,
            Self::VectorFloor,
            Self::VectorCeil,
            Self::VectorAbs,
            Self::VectorSign,
            Self::VectorLerp,
            Self::VectorAngle,
            Self::VectorClamp,
            Self::VectorMin,
            Self::VectorMax,
        ]
    }

    /// The members of the injected `os` library table (the time-only surface; no
    /// filesystem, environment, or process control).
    #[must_use]
    pub fn os_members() -> [Self; 4] {
        [Self::OsTime, Self::OsClock, Self::OsDate, Self::OsDifftime]
    }

    /// The members of the injected `buffer` library table.
    #[must_use]
    pub fn buffer_members() -> [Self; 28] {
        [
            Self::BufferCreate,
            Self::BufferFromString,
            Self::BufferToString,
            Self::BufferLen,
            Self::BufferReadI8,
            Self::BufferReadU8,
            Self::BufferReadI16,
            Self::BufferReadU16,
            Self::BufferReadI32,
            Self::BufferReadU32,
            Self::BufferReadF32,
            Self::BufferReadF64,
            Self::BufferWriteI8,
            Self::BufferWriteU8,
            Self::BufferWriteI16,
            Self::BufferWriteU16,
            Self::BufferWriteI32,
            Self::BufferWriteU32,
            Self::BufferWriteF32,
            Self::BufferWriteF64,
            Self::BufferReadString,
            Self::BufferWriteString,
            Self::BufferReadBits,
            Self::BufferWriteBits,
            Self::BufferReadInteger,
            Self::BufferWriteInteger,
            Self::BufferCopy,
            Self::BufferFill,
        ]
    }

    /// The members of the injected `utf8` library table. (`Utf8CodesAux` is the
    /// internal iterator `codes` returns, never installed under a name;
    /// `charpattern` is a string constant set directly.)
    #[must_use]
    pub fn utf8_members() -> [Self; 5] {
        [
            Self::Utf8Char,
            Self::Utf8Codepoint,
            Self::Utf8Len,
            Self::Utf8Offset,
            Self::Utf8Codes,
        ]
    }

    /// The members of the injected `bit32` library table.
    #[must_use]
    pub fn bit32_members() -> [Self; 15] {
        [
            Self::Bit32Band,
            Self::Bit32Bor,
            Self::Bit32Bxor,
            Self::Bit32Bnot,
            Self::Bit32Lshift,
            Self::Bit32Rshift,
            Self::Bit32Arshift,
            Self::Bit32Btest,
            Self::Bit32Lrotate,
            Self::Bit32Rrotate,
            Self::Bit32Extract,
            Self::Bit32Replace,
            Self::Bit32Countlz,
            Self::Bit32Countrz,
            Self::Bit32Byteswap,
        ]
    }

    /// The members of the injected `table` library table.
    #[must_use]
    pub fn table_members() -> [Self; 17] {
        [
            Self::TableInsert,
            Self::TableRemove,
            Self::TableConcat,
            Self::TableSort,
            Self::TablePack,
            Self::TableUnpack,
            Self::TableMove,
            Self::TableCreate,
            Self::TableFind,
            Self::TableGetn,
            Self::TableMaxn,
            Self::TableFreeze,
            Self::TableIsFrozen,
            Self::TableClone,
            Self::TableClear,
            Self::TableForeach,
            Self::TableForeachI,
        ]
    }

    /// The members of the injected `math` library table.
    #[must_use]
    pub fn math_members() -> [Self; 37] {
        [
            Self::MathFloor,
            Self::MathCeil,
            Self::MathAbs,
            Self::MathSqrt,
            Self::MathMax,
            Self::MathMin,
            Self::MathExp,
            Self::MathLog,
            Self::MathLog10,
            Self::MathPow,
            Self::MathFmod,
            Self::MathModf,
            Self::MathFrexp,
            Self::MathLdexp,
            Self::MathSin,
            Self::MathCos,
            Self::MathTan,
            Self::MathAsin,
            Self::MathAcos,
            Self::MathAtan,
            Self::MathAtan2,
            Self::MathSinh,
            Self::MathCosh,
            Self::MathTanh,
            Self::MathRad,
            Self::MathDeg,
            Self::MathSign,
            Self::MathRound,
            Self::MathClamp,
            Self::MathLerp,
            Self::MathMap,
            Self::MathIsNan,
            Self::MathIsInf,
            Self::MathIsFinite,
            Self::MathRandom,
            Self::MathRandomseed,
            Self::MathNoise,
        ]
    }

    /// The members of the injected `integer` library table.
    #[must_use]
    pub fn integer_members() -> [Self; 38] {
        [
            Self::IntegerCreate,
            Self::IntegerFromString,
            Self::IntegerNeg,
            Self::IntegerAdd,
            Self::IntegerSub,
            Self::IntegerMul,
            Self::IntegerDiv,
            Self::IntegerIDiv,
            Self::IntegerUDiv,
            Self::IntegerURem,
            Self::IntegerMod,
            Self::IntegerRem,
            Self::IntegerMin,
            Self::IntegerMax,
            Self::IntegerClamp,
            Self::IntegerBand,
            Self::IntegerBor,
            Self::IntegerBnot,
            Self::IntegerBxor,
            Self::IntegerBtest,
            Self::IntegerLt,
            Self::IntegerLe,
            Self::IntegerUlt,
            Self::IntegerUle,
            Self::IntegerGt,
            Self::IntegerGe,
            Self::IntegerUgt,
            Self::IntegerUge,
            Self::IntegerLshift,
            Self::IntegerRshift,
            Self::IntegerArshift,
            Self::IntegerLrotate,
            Self::IntegerRrotate,
            Self::IntegerExtract,
            Self::IntegerReplace,
            Self::IntegerCountrz,
            Self::IntegerCountlz,
            Self::IntegerBswap,
        ]
    }

    /// The members of the injected `string` library table.
    #[must_use]
    pub fn string_members() -> [Self; 17] {
        [
            Self::StringLen,
            Self::StringSub,
            Self::StringRep,
            Self::StringUpper,
            Self::StringLower,
            Self::StringReverse,
            Self::StringByte,
            Self::StringChar,
            Self::StringFormat,
            Self::StringFind,
            Self::StringMatch,
            Self::StringGmatch,
            Self::StringGsub,
            Self::StringSplit,
            Self::StringPack,
            Self::StringPacksize,
            Self::StringUnpack,
        ]
    }

    /// Every flat base global in install order. (`INext` is internal — it is the
    /// iterator `ipairs` returns, never installed under a name.)
    #[must_use]
    pub fn all() -> [Self; 23] {
        [
            Self::Type,
            Self::Typeof,
            Self::ToString,
            Self::Assert,
            Self::Error,
            Self::Print,
            Self::SetMetatable,
            Self::GetMetatable,
            Self::Pcall,
            Self::Xpcall,
            Self::ToNumber,
            Self::RawEqual,
            Self::RawGet,
            Self::RawSet,
            Self::RawLen,
            Self::Select,
            Self::Loadstring,
            Self::Require,
            Self::CollectGarbage,
            Self::GcInfo,
            Self::Next,
            Self::Pairs,
            Self::IPairs,
        ]
    }

    /// The members of the injected `coroutine` table.
    #[must_use]
    pub fn coroutine_members() -> [Self; 7] {
        [
            Self::CoroutineCreate,
            Self::CoroutineResume,
            Self::CoroutineYield,
            Self::CoroutineStatus,
            Self::CoroutineRunning,
            Self::CoroutineClose,
            Self::CoroutineIsYieldable,
        ]
    }

    /// Whether this is a `bit32` library function. Those coerce a numeric-string argument to
    /// a number (`luaL_checkunsigned`), so the dispatcher resolves such strings before reading
    /// operands.
    fn is_bit32(self) -> bool {
        matches!(
            self,
            Self::Bit32Band
                | Self::Bit32Bor
                | Self::Bit32Bxor
                | Self::Bit32Bnot
                | Self::Bit32Lshift
                | Self::Bit32Rshift
                | Self::Bit32Arshift
                | Self::Bit32Btest
                | Self::Bit32Lrotate
                | Self::Bit32Rrotate
                | Self::Bit32Extract
                | Self::Bit32Replace
                | Self::Bit32Countlz
                | Self::Bit32Countrz
                | Self::Bit32Byteswap
        )
    }

    /// Whether this is a `math` library function, every one of which reads its
    /// numeric arguments through `luaL_checknumber`/`luaL_checkinteger` and so
    /// coerces a numeric-string argument (e.g. `math.abs("-4")`).
    fn is_math(self) -> bool {
        matches!(
            self,
            Self::MathAbs
                | Self::MathAcos
                | Self::MathAsin
                | Self::MathAtan
                | Self::MathAtan2
                | Self::MathCeil
                | Self::MathClamp
                | Self::MathCos
                | Self::MathCosh
                | Self::MathDeg
                | Self::MathExp
                | Self::MathFloor
                | Self::MathFmod
                | Self::MathFrexp
                | Self::MathIsFinite
                | Self::MathIsInf
                | Self::MathIsNan
                | Self::MathLdexp
                | Self::MathLerp
                | Self::MathLog
                | Self::MathLog10
                | Self::MathMap
                | Self::MathMax
                | Self::MathMin
                | Self::MathModf
                | Self::MathNoise
                | Self::MathPow
                | Self::MathRad
                | Self::MathRandom
                | Self::MathRandomseed
                | Self::MathRound
                | Self::MathSign
                | Self::MathSin
                | Self::MathSinh
                | Self::MathSqrt
                | Self::MathTan
                | Self::MathTanh
        )
    }
}

/// Replaces each numeric-string argument with its parsed number, leaving other values
/// untouched — the string→number coercion the `bit32` library applies to its arguments
/// (`luaL_checkunsigned`). A string that does not parse stays a string, so the operand reader
/// still raises a typed error.
fn coerce_numeric_args(heap: &Heap, args: &[RawValue]) -> Vec<RawValue> {
    args.iter()
        .map(|&arg| match arg {
            RawValue::String(handle) => heap
                .string(handle)
                .and_then(|s| vmutils::str_to_number(s.bytes()))
                .map_or(arg, RawValue::Number),
            other => other,
        })
        .collect()
}

/// Runs a builtin with `args`, returning its results.
///
/// # Errors
/// Returns whatever the builtin raises (`error`, a failed `assert`, or a nested
/// `__tostring`).
pub fn dispatch(
    builtin: Builtin,
    call_site: BuiltinCallSite,
    callee: RawValue,
    heap: &mut Heap,
    thread: &mut Thread,
    args: &[RawValue],
) -> Exec<Vec<RawValue>> {
    // The bit32 and math functions take their arguments through a string→number coercion;
    // resolve any numeric strings once here so each operand reader sees a number.
    let coerced;
    let args = if builtin.is_bit32() || builtin.is_math() {
        coerced = coerce_numeric_args(heap, args);
        coerced.as_slice()
    } else {
        args
    };
    match builtin {
        Builtin::Type
        | Builtin::Typeof
        | Builtin::ToString
        | Builtin::Assert
        | Builtin::Error
        | Builtin::Print
        | Builtin::SetMetatable
        | Builtin::GetMetatable
        | Builtin::Pcall
        | Builtin::Xpcall
        | Builtin::ToNumber
        | Builtin::RawEqual
        | Builtin::RawGet
        | Builtin::RawSet
        | Builtin::RawLen
        | Builtin::Select
        | Builtin::CollectGarbage
        | Builtin::GcInfo
        | Builtin::Next
        | Builtin::INext
        | Builtin::Pairs
        | Builtin::IPairs => base_lib::dispatch(builtin, call_site, heap, thread, args),
        Builtin::Loadstring => require_load::builtin_loadstring(heap, thread, args),
        Builtin::Require => require_load::builtin_require(heap, thread, args),
        Builtin::CoroutineCreate => crate::coroutine::create(heap, thread, args),
        Builtin::CoroutineResume => crate::coroutine::resume(heap, thread, args),
        Builtin::CoroutineStatus => crate::coroutine::status(heap, args),
        Builtin::CoroutineRunning => crate::coroutine::running(thread),
        Builtin::CoroutineIsYieldable => crate::coroutine::is_yieldable(thread),
        Builtin::CoroutineClose => crate::coroutine::close(heap, thread, args),
        // `coroutine.yield` is intercepted in `precall` (it suspends rather than
        // returning), so dispatch never reaches it.
        Builtin::CoroutineYield => Err(err("'coroutine.yield' must suspend the coroutine")),
        Builtin::StringLen
        | Builtin::StringSub
        | Builtin::StringRep
        | Builtin::StringUpper
        | Builtin::StringLower
        | Builtin::StringReverse
        | Builtin::StringByte
        | Builtin::StringChar
        | Builtin::StringFormat
        | Builtin::StringFind
        | Builtin::StringMatch
        | Builtin::StringGmatch
        | Builtin::StringGmatchAux
        | Builtin::StringGsub
        | Builtin::StringSplit
        | Builtin::StringPack
        | Builtin::StringPacksize
        | Builtin::StringUnpack => string_lib::dispatch(builtin, heap, thread, args),
        Builtin::MathFloor
        | Builtin::MathCeil
        | Builtin::MathAbs
        | Builtin::MathSqrt
        | Builtin::MathMax
        | Builtin::MathMin
        | Builtin::MathExp
        | Builtin::MathLog
        | Builtin::MathLog10
        | Builtin::MathPow
        | Builtin::MathFmod
        | Builtin::MathModf
        | Builtin::MathFrexp
        | Builtin::MathLdexp
        | Builtin::MathSin
        | Builtin::MathCos
        | Builtin::MathTan
        | Builtin::MathAsin
        | Builtin::MathAcos
        | Builtin::MathAtan
        | Builtin::MathAtan2
        | Builtin::MathSinh
        | Builtin::MathCosh
        | Builtin::MathTanh
        | Builtin::MathRad
        | Builtin::MathDeg
        | Builtin::MathSign
        | Builtin::MathRound
        | Builtin::MathClamp
        | Builtin::MathLerp
        | Builtin::MathMap
        | Builtin::MathIsNan
        | Builtin::MathIsInf
        | Builtin::MathIsFinite
        | Builtin::MathRandom
        | Builtin::MathRandomseed
        | Builtin::MathNoise
        | Builtin::IntegerCreate
        | Builtin::IntegerFromString
        | Builtin::IntegerNeg
        | Builtin::IntegerAdd
        | Builtin::IntegerSub
        | Builtin::IntegerMul
        | Builtin::IntegerDiv
        | Builtin::IntegerIDiv
        | Builtin::IntegerUDiv
        | Builtin::IntegerURem
        | Builtin::IntegerMod
        | Builtin::IntegerRem
        | Builtin::IntegerMin
        | Builtin::IntegerMax
        | Builtin::IntegerClamp
        | Builtin::IntegerBand
        | Builtin::IntegerBor
        | Builtin::IntegerBnot
        | Builtin::IntegerBxor
        | Builtin::IntegerBtest
        | Builtin::IntegerLt
        | Builtin::IntegerLe
        | Builtin::IntegerUlt
        | Builtin::IntegerUle
        | Builtin::IntegerGt
        | Builtin::IntegerGe
        | Builtin::IntegerUgt
        | Builtin::IntegerUge
        | Builtin::IntegerLshift
        | Builtin::IntegerRshift
        | Builtin::IntegerArshift
        | Builtin::IntegerLrotate
        | Builtin::IntegerRrotate
        | Builtin::IntegerExtract
        | Builtin::IntegerReplace
        | Builtin::IntegerCountrz
        | Builtin::IntegerCountlz
        | Builtin::IntegerBswap => numeric_lib::dispatch(builtin, heap, args),
        Builtin::TableInsert => table_lib::table_insert(heap, args),
        Builtin::TableRemove => table_lib::table_remove(heap, args),
        Builtin::TableConcat => table_lib::table_concat(heap, args),
        Builtin::TableSort => table_lib::table_sort(heap, thread, args),
        Builtin::TablePack => table_lib::table_pack(heap, args),
        Builtin::TableUnpack => table_lib::table_unpack(heap, args),
        Builtin::TableMove => table_lib::table_move(heap, args),
        Builtin::TableCreate => table_lib::table_create(heap, args),
        Builtin::TableFind => table_lib::table_find(heap, thread, args),
        Builtin::TableGetn => table_lib::table_getn(heap, args),
        Builtin::TableMaxn => table_lib::table_maxn(heap, args),
        Builtin::TableFreeze => table_lib::table_freeze(heap, args),
        Builtin::TableIsFrozen => table_lib::table_isfrozen(heap, args),
        Builtin::TableClone => table_lib::table_clone(heap, args),
        Builtin::TableClear => table_lib::table_clear(heap, args),
        Builtin::TableForeach => table_lib::table_foreach(heap, thread, args),
        Builtin::TableForeachI => table_lib::table_foreachi(heap, thread, args),
        Builtin::Bit32Band => bit32_lib::bit32_reduce(args, u32::MAX, |a, b| a & b),
        Builtin::Bit32Bor => bit32_lib::bit32_reduce(args, 0, |a, b| a | b),
        Builtin::Bit32Bxor => bit32_lib::bit32_reduce(args, 0, |a, b| a ^ b),
        Builtin::Bit32Bnot => Ok(vec![bit32_lib::bit32_result(!bit32_lib::bit_arg(args, 0)?)]),
        Builtin::Bit32Lshift => Ok(vec![bit32_lib::bit32_result(bit32_lib::shift_logical(
            bit32_lib::bit_arg(args, 0)?,
            bit32_lib::shift_disp(args)?,
        ))]),
        Builtin::Bit32Rshift => Ok(vec![bit32_lib::bit32_result(bit32_lib::shift_logical(
            bit32_lib::bit_arg(args, 0)?,
            -bit32_lib::shift_disp(args)?,
        ))]),
        Builtin::Bit32Arshift => Ok(vec![bit32_lib::bit32_result(bit32_lib::shift_arith(
            bit32_lib::bit_arg(args, 0)?,
            bit32_lib::shift_disp(args)?,
        ))]),
        Builtin::Bit32Btest => {
            let mut acc = u32::MAX;
            for index in 0..args.len() {
                acc &= bit32_lib::bit_arg(args, index)?;
            }
            Ok(vec![RawValue::Boolean(acc != 0)])
        }
        Builtin::Bit32Lrotate => Ok(vec![bit32_lib::bit32_result(
            bit32_lib::bit_arg(args, 0)?.rotate_left(bit32_lib::rotate_amount(args)?),
        )]),
        Builtin::Bit32Rrotate => Ok(vec![bit32_lib::bit32_result(
            bit32_lib::bit_arg(args, 0)?.rotate_right(bit32_lib::rotate_amount(args)?),
        )]),
        Builtin::Bit32Extract => bit32_lib::bit32_extract(args),
        Builtin::Bit32Replace => bit32_lib::bit32_replace(args),
        Builtin::Bit32Countlz => Ok(vec![bit32_lib::bit32_result(
            bit32_lib::bit_arg(args, 0)?.leading_zeros(),
        )]),
        Builtin::Bit32Countrz => Ok(vec![bit32_lib::bit32_result(
            bit32_lib::bit_arg(args, 0)?.trailing_zeros(),
        )]),
        Builtin::Bit32Byteswap => Ok(vec![bit32_lib::bit32_result(
            bit32_lib::bit_arg(args, 0)?.swap_bytes(),
        )]),
        Builtin::Utf8Char
        | Builtin::Utf8Codepoint
        | Builtin::Utf8Len
        | Builtin::Utf8Offset
        | Builtin::Utf8Codes
        | Builtin::Utf8CodesAux => utf8_lib::dispatch(builtin, heap, args),
        Builtin::OsTime | Builtin::OsClock | Builtin::OsDate | Builtin::OsDifftime => {
            os_lib::dispatch(builtin, heap, args)
        }
        Builtin::BufferCreate
        | Builtin::BufferFromString
        | Builtin::BufferToString
        | Builtin::BufferLen
        | Builtin::BufferReadI8
        | Builtin::BufferReadU8
        | Builtin::BufferReadI16
        | Builtin::BufferReadU16
        | Builtin::BufferReadI32
        | Builtin::BufferReadU32
        | Builtin::BufferReadF32
        | Builtin::BufferReadF64
        | Builtin::BufferWriteI8
        | Builtin::BufferWriteU8
        | Builtin::BufferWriteI16
        | Builtin::BufferWriteU16
        | Builtin::BufferWriteI32
        | Builtin::BufferWriteU32
        | Builtin::BufferWriteF32
        | Builtin::BufferWriteF64
        | Builtin::BufferReadString
        | Builtin::BufferWriteString
        | Builtin::BufferReadBits
        | Builtin::BufferWriteBits
        | Builtin::BufferReadInteger
        | Builtin::BufferWriteInteger
        | Builtin::BufferCopy
        | Builtin::BufferFill => buffer_lib::dispatch(builtin, heap, args),
        Builtin::VectorCreate
        | Builtin::VectorMagnitude
        | Builtin::VectorNormalize
        | Builtin::VectorCross
        | Builtin::VectorDot
        | Builtin::VectorFloor
        | Builtin::VectorCeil
        | Builtin::VectorAbs
        | Builtin::VectorSign
        | Builtin::VectorLerp
        | Builtin::VectorAngle
        | Builtin::VectorClamp
        | Builtin::VectorMin
        | Builtin::VectorMax => vector_lib::dispatch(builtin, args),
        Builtin::DebugInfo
        | Builtin::DebugTraceback
        | Builtin::CompatGetFenv
        | Builtin::CompatSetFenv => debug_lib::dispatch(builtin, callee, heap, thread, args),
        Builtin::ConformanceGetCoverage
        | Builtin::ConformanceResumeError
        | Builtin::ConformanceSetBlockAllocations
        | Builtin::ConformanceSingleYield
        | Builtin::ConformanceMultipleYields
        | Builtin::ConformanceMultipleYieldsWithNestedCall
        | Builtin::ConformancePassthroughCall
        | Builtin::ConformancePassthroughCallMoreResults
        | Builtin::ConformancePassthroughCallArgReuse
        | Builtin::ConformancePassthroughCallVaradic
        | Builtin::ConformancePassthroughCallWithState => {
            conformance_lib::dispatch(builtin, heap, thread, args)
        }
    }
}
