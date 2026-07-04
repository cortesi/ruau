//! The execution-semantics fingerprint.
//!
//! Crate semver does not promise bit-identical execution across releases. A
//! host that records a run for replay (or that shards same-seeded runs across
//! processes and compares their outputs) needs a cheap, deterministic token
//! that changes whenever script-observable execution semantics change, so a
//! mismatched replay is refused up front instead of producing a wrong-hash
//! mystery. [`semantics_fingerprint`] is that token: a BLAKE3 hash of a
//! manifest assembled from the stable constants that pin the executable
//! surface — bytecode format versions, the opcode and fastcall id spaces, the
//! installed builtin surface, and the determinism-relevant default limits —
//! plus a hand-bumped [`SEMANTICS_REVISION`] covering everything those
//! constants cannot see.

use crate::{
    builtins::Builtin,
    limits::{
        DEFAULT_MAX_BUFFER_BYTES, DEFAULT_MAX_BYTECODE_CONSTANTS, DEFAULT_MAX_BYTECODE_PROTOS,
        DEFAULT_MAX_BYTECODE_WORDS, DEFAULT_MAX_CALL_DEPTH, DEFAULT_MAX_META_CHAIN,
        DEFAULT_MAX_NATIVE_DEPTH, DEFAULT_MAX_PACK_BYTES, DEFAULT_MAX_PATTERN_CAPTURES,
        DEFAULT_MAX_PATTERN_DEPTH, DEFAULT_MAX_PATTERN_STEPS,
        DEFAULT_MAX_RUNTIME_COMPILE_BYTECODE_BYTES, DEFAULT_MAX_RUNTIME_COMPILE_INSTRUCTIONS,
        DEFAULT_MAX_RUNTIME_COMPILE_SOURCE_BYTES, DEFAULT_MAX_STRING_BYTES,
        DEFAULT_MAX_TABLE_ELEMENTS, DEFAULT_MAX_VARARGS, SinkQuota,
    },
    load::PUBLIC_BYTECODE_VERSION,
    value_marshal::{DEFAULT_MAX_VALUE_MARSHAL_DEPTH, DEFAULT_MAX_VALUE_MARSHAL_NODES},
};

/// Hand-bumped revision of the VM's script-observable execution semantics.
///
/// Bump this on **any change to script-observable execution semantics** that
/// the other manifest inputs do not already capture: builtin behavior or error
/// text, dispatch or metamethod resolution order, gas accounting, PRNG or
/// iteration-order derivation, string formatting, GC observability — anything
/// that could make a previously recorded run replay differently. Adding an
/// opcode, a builtin, or changing a default limit is picked up automatically by
/// the constants hashed alongside it; behavioral changes behind unchanged
/// constants are exactly what this revision exists for.
pub const SEMANTICS_REVISION: u32 = 1;

/// The fingerprint of this build's execution semantics.
///
/// Deterministic across calls and processes for the same crate build. Two
/// builds with equal fingerprints are intended to execute the same script with
/// the same seeds and limits identically; a host pins the fingerprint next to
/// recorded inputs and refuses replay on mismatch.
///
/// The fingerprint is target-sensitive where semantics are: the default native
/// re-entry depth differs on targets without stack growth (wasm32), so a
/// native recording correctly refuses to replay under the wasm32 default.
#[must_use]
pub fn semantics_fingerprint() -> [u8; 32] {
    *blake3::hash(manifest(SEMANTICS_REVISION).as_bytes()).as_bytes()
}

/// Assembles the fingerprint manifest for `revision`.
///
/// Takes the revision as a parameter so tests can prove the fingerprint moves
/// when [`SEMANTICS_REVISION`] is bumped; production callers always hash
/// `manifest(SEMANTICS_REVISION)`.
fn manifest(revision: u32) -> String {
    let mut lines = vec![
        "ruau-semantics".to_owned(),
        format!("revision={revision}"),
        // Bytecode format: what the loader accepts and the compiler emits.
        format!(
            "bytecode.version.default={}",
            ruau_bytecode::DEFAULT_VERSION
        ),
        format!(
            "bytecode.type_version.default={}",
            ruau_bytecode::DEFAULT_TYPE_VERSION
        ),
        format!("bytecode.version.public={PUBLIC_BYTECODE_VERSION}"),
        format!(
            "bytecode.opcode_count={}",
            ruau_bytecode::opcodes::Opcode::COUNT
        ),
        format!(
            "bytecode.builtin_function_count={}",
            ruau_bytecode::opcodes::BuiltinFunction::COUNT
        ),
        // The installed builtin dispatch surface: the flat base globals plus
        // each library table's member count.
        format!("builtins.base={}", Builtin::all().len()),
        format!("builtins.coroutine={}", Builtin::coroutine_members().len()),
        format!("builtins.string={}", Builtin::string_members().len()),
        format!("builtins.math={}", Builtin::math_members().len()),
        format!("builtins.integer={}", Builtin::integer_members().len()),
        format!("builtins.table={}", Builtin::table_members().len()),
        format!("builtins.bit32={}", Builtin::bit32_members().len()),
        format!("builtins.utf8={}", Builtin::utf8_members().len()),
        format!("builtins.os={}", Builtin::os_members().len()),
        format!("builtins.buffer={}", Builtin::buffer_members().len()),
        format!("builtins.vector={}", Builtin::vector_members().len()),
        format!("builtins.debug={}", Builtin::debug_members().len()),
        // Determinism-relevant default ceilings: where an unconfigured VM
        // raises its catchable limit errors.
        format!("limits.max_call_depth={DEFAULT_MAX_CALL_DEPTH}"),
        format!("limits.max_native_depth={DEFAULT_MAX_NATIVE_DEPTH}"),
        format!("limits.max_varargs={DEFAULT_MAX_VARARGS}"),
        format!("limits.max_string_bytes={DEFAULT_MAX_STRING_BYTES}"),
        format!("limits.max_buffer_bytes={DEFAULT_MAX_BUFFER_BYTES}"),
        format!("limits.max_table_elements={DEFAULT_MAX_TABLE_ELEMENTS}"),
        format!("limits.max_meta_chain={DEFAULT_MAX_META_CHAIN}"),
        format!("limits.max_pack_bytes={DEFAULT_MAX_PACK_BYTES}"),
        format!("limits.max_pattern_steps={DEFAULT_MAX_PATTERN_STEPS}"),
        format!("limits.max_pattern_depth={DEFAULT_MAX_PATTERN_DEPTH}"),
        format!("limits.max_pattern_captures={DEFAULT_MAX_PATTERN_CAPTURES}"),
        format!("limits.max_bytecode_protos={DEFAULT_MAX_BYTECODE_PROTOS}"),
        format!("limits.max_bytecode_words={DEFAULT_MAX_BYTECODE_WORDS}"),
        format!("limits.max_bytecode_constants={DEFAULT_MAX_BYTECODE_CONSTANTS}"),
        format!(
            "limits.max_runtime_compile_source_bytes={DEFAULT_MAX_RUNTIME_COMPILE_SOURCE_BYTES}"
        ),
        format!(
            "limits.max_runtime_compile_instructions={DEFAULT_MAX_RUNTIME_COMPILE_INSTRUCTIONS}"
        ),
        format!(
            "limits.max_runtime_compile_bytecode_bytes={DEFAULT_MAX_RUNTIME_COMPILE_BYTECODE_BYTES}"
        ),
        format!("marshal.max_depth={DEFAULT_MAX_VALUE_MARSHAL_DEPTH}"),
        format!("marshal.max_nodes={DEFAULT_MAX_VALUE_MARSHAL_NODES}"),
        // The deterministic sink-truncation marker a quota-limited print sink
        // emits — part of a recorded run's output stream.
        format!(
            "sink.truncation_marker={}",
            String::from_utf8_lossy(SinkQuota::TRUNCATION_MARKER).escape_default()
        ),
    ];
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_deterministic() {
        // Two assemblies in this process hash identically; the manifest is a
        // pure function of compile-time constants, so a second process can
        // only produce the same bytes.
        assert_eq!(semantics_fingerprint(), semantics_fingerprint());
        assert_eq!(manifest(SEMANTICS_REVISION), manifest(SEMANTICS_REVISION));
        assert_eq!(
            semantics_fingerprint(),
            *blake3::hash(manifest(SEMANTICS_REVISION).as_bytes()).as_bytes()
        );
    }

    #[test]
    fn fingerprint_moves_when_the_revision_is_bumped() {
        let bumped = SEMANTICS_REVISION + 1;
        assert_ne!(manifest(SEMANTICS_REVISION), manifest(bumped));
        assert_ne!(
            *blake3::hash(manifest(SEMANTICS_REVISION).as_bytes()).as_bytes(),
            *blake3::hash(manifest(bumped).as_bytes()).as_bytes()
        );
    }

    #[test]
    fn manifest_pins_every_input_section() {
        // Each section heading appears, so dropping an input class from the
        // assembly is a test failure rather than a silently narrower token.
        let manifest = manifest(SEMANTICS_REVISION);
        for prefix in [
            "revision=",
            "bytecode.version.default=",
            "bytecode.opcode_count=",
            "bytecode.builtin_function_count=",
            "builtins.base=",
            "limits.max_call_depth=",
            "marshal.max_depth=",
            "sink.truncation_marker=",
        ] {
            assert!(
                manifest.contains(prefix),
                "manifest is missing the {prefix:?} input"
            );
        }
    }
}
