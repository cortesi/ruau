//! Luau bytecode codec and compiler entry points.
//!
//! Application embedders usually compile through `ruau::surface::Surface`, or
//! through `ruau::vm::RuntimeCapabilities` when using the lower-level VM API
//! directly. Those paths apply runtime-capability restrictions before calling
//! into this crate.
//!
//! `BuiltinFunction`, `ProtoFlag`, and related opcode-domain constants are
//! deliberately exposed as constant namespaces rather than Rust enums. The
//! serialized bytecode format is byte-oriented and occasionally reserves
//! values that are not meaningful in safe high-level APIs, so callers that
//! inspect or rewrite bytecode need the exact numeric constants without an enum
//! exhaustiveness promise.

mod builder;
mod codec;
mod compile;
mod disassemble;
#[path = "opcodes.rs"]
mod opcodes_inner;
mod types;
mod validate;

pub use builder::{DEFAULT_TYPE_VERSION, DEFAULT_VERSION};
pub use codec::{DecodeError, EncodeError, decode_chunk, encode_chunk};
pub use compile::{
    CompileError, CompileErrorKind, CompileOptions, FastFlag, FastInt, KnownMember,
    KnownMemberValue, UpstreamCompilerOptions, UpstreamParseOptions, chunkify_parse_error,
    compile_source, compile_source_bytes, compile_source_bytes_strict,
    compile_source_bytes_strict_with_upstream_options, compile_source_strict,
    compile_source_strict_with_upstream_options, effective_compile_options,
};
/// Opcode constants and instruction operand helpers.
pub mod opcodes {
    pub use crate::opcodes_inner::{
        BuiltinFunction, CaptureType, FORGLOOP_INEXT_BIT, FORGLOOP_VARS_MASK, FeedbackType,
        IMPORT_PATH_COMPONENT_BITS, IMPORT_PATH_COMPONENT_MASK, IMPORT_PATH_COUNT_SHIFT,
        JUMPX_K_INDEX_MASK, JUMPX_K_NOT_BIT, Opcode, import_component_shift,
    };
    pub(crate) use crate::opcodes_inner::{ConstantTag, ProtoFlag, TypeTag};
}
/// Human-readable bytecode disassembly.
pub mod disasm {
    pub use crate::disassemble::disassemble_chunk;
}
pub use types::{
    BytecodeChunk, ClassShape, Constant, DebugInfo, DebugLocal, FeedbackSlot, Instruction,
    LineInfo, Proto, TableEntry, TypeInfo, UserdataTypeMapping, code_word_count,
    instruction_word_offsets, jump_target_instruction_index,
};
pub use validate::{ValidationError, ValidationErrorKind, validate_chunk};
