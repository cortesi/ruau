//! Luau bytecode codec and compiler entry points.
//!
//! Application embedders usually compile through `ruau::surface::Surface`, or
//! through `ruau::vm::RuntimeCapabilities` when using the lower-level VM API
//! directly. Those paths apply runtime-capability restrictions before calling
//! into this crate.

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
    KnownMemberValue, compile_source, compile_source_bytes, compile_source_bytes_strict,
    compile_source_bytes_strict_with_cancel, compile_source_bytes_with_cancel,
    compile_source_strict, compile_source_strict_with_cancel, compile_source_with_cancel,
    effective_compile_options,
};
/// Opcode constants and instruction operand helpers.
pub mod opcodes {
    pub use crate::opcodes_inner::{
        BuiltinFunction, CaptureType, ConstantTag, FORGLOOP_INEXT_BIT, FORGLOOP_VARS_MASK,
        FeedbackType, IMPORT_PATH_COMPONENT_BITS, IMPORT_PATH_COMPONENT_MASK,
        IMPORT_PATH_COUNT_SHIFT, JUMPX_K_INDEX_MASK, JUMPX_K_NOT_BIT, Opcode, ProtoFlag, TypeTag,
        import_component_shift,
    };
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
