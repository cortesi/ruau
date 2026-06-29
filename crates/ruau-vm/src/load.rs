//! The loader: a `BytecodeChunk` becomes a runtime proto graph (port
//! `lvmload.cpp`).
//!
//! Structural verification reuses `ruau_bytecode::validate_chunk` and is
//! a configurable [`LoadMode`]: validated for untrusted or cached bytecode,
//! trusted for bytecode the process compiled itself. Either way every reference
//! is range-checked, so a malformed chunk yields a [`LoadError`], never a panic.

use std::{collections::HashMap, sync::Arc};

use ruau_bytecode::{
    BytecodeChunk, Constant, Instruction, Proto as BytecodeProto, code_word_count,
    instruction_word_offsets, opcodes::Opcode, validate_chunk,
};
use ruau_vm_api::{RawGc, RawValue, RegistryRef, marker};

use crate::{
    func::Closure,
    heap::Heap,
    limits::EffectiveLimits,
    object::{Proto, ProtoBuffers, RuntimeConstant, TableShape},
    runtime_capabilities::RuntimeCapabilities,
};

/// The highest bytecode version the loader understands.
pub const SUPPORTED_BYTECODE_VERSION: u8 = 11;

/// The placeholder chunk name a module reports when the caller names none. The
/// leading `=` is a `luaO_chunkid` marker, so it displays as a bare `?`.
pub const DEFAULT_CHUNK_NAME: &[u8] = b"=?";

/// Whether the loader runs structural verification or trusts the chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadMode {
    /// Run `validate_chunk` first (untrusted or cached bytecode).
    Validated,
    /// Skip validation (bytecode this process compiled in-run). The loader still
    /// range-checks every reference, so it stays panic-free.
    Trusted,
}

/// A loaded module: its main closure, ready to call. Non-`Copy`: it owns a registry
/// pin (`pin`) that roots `main` across a collection, so the module is reclaimable only
/// after [`Vm::unload`](crate::Vm::unload) consumes it and releases the pin. Dropping a
/// module without unloading leaks its pin (the `luaL_unref` model) until the VM drops.
#[derive(Debug)]
pub struct LoadedModule {
    /// The main closure. `pub(crate)`, not `pub`: the call paths trust this handle
    /// directly, so a host must not be able to overwrite it with a fabricated closure
    /// handle after `Vm::load` (the registry `pin` is already `pub(crate)`, so the
    /// struct cannot be constructed from outside the crate either).
    pub(crate) main: RawGc<marker::Closure>,
    /// The registry pin rooting `main`; released by `Vm::unload`.
    pub(crate) pin: RegistryRef,
}

/// A compile-once, instantiate-many artifact: a bytecode chunk validated once
/// at construction, stamped with the [`RuntimeCapabilities`] it was compiled under.
///
/// One artifact feeds any number of VMs ([`Vm::load_compiled`](crate::Vm::load_compiled),
/// [`VmBuilder::preload`](crate::VmBuilder::preload)) without re-running
/// compilation or structural verification per VM: the artifact is
/// host-constructed and immutable (no mutating method exists, and the payload
/// sits behind an `Arc`), so the validation performed here holds for every
/// later load. Each load still range-checks every reference while building the
/// per-VM proto graph, so even a hostile chunk smuggled past validation cannot
/// cause a panic — only a [`LoadError`].
///
/// Cloning is cheap (an `Arc` bump): clones share one chunk, so a fleet of VMs
/// instantiated from the same artifact holds one copy of the compiled program.
/// The artifact's bytes are host-owned and are charged to no VM's memory cap;
/// each VM is charged for what *it* instantiates from the artifact (protos,
/// interned strings, the main closure).
#[derive(Clone, Debug)]
pub struct CompiledModule {
    inner: Arc<CompiledModuleArtifact>,
}

#[derive(Debug)]
struct CompiledModuleArtifact {
    chunk: BytecodeChunk,
    runtime_capabilities: RuntimeCapabilities,
}

impl CompiledModule {
    /// Validates `chunk` once and seals it with the [`RuntimeCapabilities`] it was
    /// compiled under. `runtime_capabilities` must be the selector whose compiler
    /// restrictions produced `chunk` ([`RuntimeCapabilities::compile_module`](crate::RuntimeCapabilities::compile_module)
    /// guarantees this pairing); a VM only accepts the artifact when its own
    /// capabilities match, so a chunk compiled with a disabled library's constant
    /// folds can never load into a VM that disabled that library.
    ///
    /// # Errors
    /// Returns a [`LoadError`] for a compile-error chunk, an unsupported
    /// bytecode version, or a chunk structural verification rejects.
    pub fn new(
        chunk: BytecodeChunk,
        runtime_capabilities: RuntimeCapabilities,
    ) -> Result<Self, LoadError> {
        match &chunk {
            BytecodeChunk::Error { message } => {
                return Err(LoadError::CompileError(message.clone()));
            }
            BytecodeChunk::Valid {
                bytecode_version,
                type_version,
                ..
            } => {
                if *bytecode_version == 0 || *bytecode_version > SUPPORTED_BYTECODE_VERSION {
                    return Err(LoadError::UnsupportedVersion {
                        bytecode: *bytecode_version,
                        type_version: *type_version,
                    });
                }
            }
        }
        if let Some(first) = validate_chunk(&chunk).first() {
            return Err(LoadError::Invalid(format!("{first:?}")));
        }
        Ok(Self {
            inner: Arc::new(CompiledModuleArtifact {
                chunk,
                runtime_capabilities,
            }),
        })
    }

    /// The [`RuntimeCapabilities`] this artifact was compiled under. Loading
    /// requires the VM's capabilities to be identical (fail closed on mismatch).
    #[must_use]
    pub fn runtime_capabilities(&self) -> &RuntimeCapabilities {
        &self.inner.runtime_capabilities
    }

    /// The validated chunk, shared by every clone of this artifact.
    #[must_use]
    pub fn chunk(&self) -> &BytecodeChunk {
        &self.inner.chunk
    }
}

/// Why a chunk failed to load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// The chunk encodes a non-throwing compile error, not runnable bytecode.
    CompileError(Vec<u8>),
    /// An unsupported bytecode or type version.
    UnsupportedVersion {
        /// The chunk's bytecode version.
        bytecode: u8,
        /// The chunk's type-encoding version.
        type_version: u8,
    },
    /// Structural verification rejected the chunk.
    Invalid(String),
    /// A feature the VM intentionally does not support (the class runtime).
    Unsupported(&'static str),
    /// A string, proto, or constant reference was out of range.
    BadReference(&'static str),
    /// Allocation failed under the memory cap.
    OutOfMemory,
    /// A [`CompiledModule`] was offered to a VM built under a different
    /// [`RuntimeCapabilities`] than the artifact was compiled under. Fail
    /// closed: capability-restricted compilation (suppressed constant folds,
    /// suppressed imports) is only sound on a VM with the identical capability
    /// surface.
    RuntimeCapabilitiesMismatch {
        /// The capabilities the artifact was compiled under.
        artifact: RuntimeCapabilities,
        /// The capabilities the receiving VM was built with.
        vm: RuntimeCapabilities,
    },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CompileError(message) => {
                write!(
                    f,
                    "chunk is a compile error: {}",
                    String::from_utf8_lossy(message)
                )
            }
            Self::UnsupportedVersion {
                bytecode,
                type_version,
            } => write!(
                f,
                "unsupported chunk version (bytecode {bytecode}, type {type_version})"
            ),
            Self::Invalid(reason) => {
                write!(f, "structural verification rejected the chunk: {reason}")
            }
            Self::Unsupported(feature) => write!(f, "unsupported feature: {feature}"),
            Self::BadReference(what) => write!(f, "out-of-range {what} reference"),
            Self::OutOfMemory => f.write_str("allocation failed under the memory cap"),
            Self::RuntimeCapabilitiesMismatch { artifact, vm } => write!(
                f,
                "compiled module runtime capabilities {artifact:?} do not match the VM runtime capabilities {vm:?}"
            ),
        }
    }
}

impl std::error::Error for LoadError {}

pub fn load_with_limits(
    heap: &mut Heap,
    chunk: &BytecodeChunk,
    mode: LoadMode,
    chunk_name: &[u8],
    limits: EffectiveLimits,
) -> Result<LoadedModule, LoadError> {
    load_with_limits_and_module_id(heap, chunk, mode, chunk_name, limits, &None)
}

pub fn load_module_with_limits(
    heap: &mut Heap,
    chunk: &BytecodeChunk,
    mode: LoadMode,
    module_id: crate::ModuleId,
    limits: EffectiveLimits,
) -> Result<LoadedModule, LoadError> {
    let chunk_name = module_id.as_bytes().to_vec();
    load_with_limits_and_module_id(heap, chunk, mode, &chunk_name, limits, &Some(module_id))
}

fn load_with_limits_and_module_id(
    heap: &mut Heap,
    chunk: &BytecodeChunk,
    mode: LoadMode,
    chunk_name: &[u8],
    limits: EffectiveLimits,
    module_id: &Option<crate::ModuleId>,
) -> Result<LoadedModule, LoadError> {
    let chunk = validate_chunk_for_load(chunk, mode, limits)?;

    // Pass 1: allocate a placeholder proto per `Proto` so cross-proto
    // references resolve to stable handles in pass 2.
    let mut handles: Vec<RawGc<Proto>> = Vec::new();
    for bproto in chunk.protos {
        handles.push(
            heap.alloc_proto(placeholder_proto(bproto))
                .ok_or(LoadError::OutOfMemory)?,
        );
    }

    // The chunk name every prototype shares as its error-location prefix, stored
    // raw (with its `luaO_chunkid` marker) and formatted only at display time.
    let source = heap.intern_str(chunk_name).ok_or(LoadError::OutOfMemory)?;

    // Pass 2: resolve constants and wire code and child references.
    for (i, bproto) in chunk.protos.iter().enumerate() {
        let buffers = build_proto_buffers(
            heap,
            bproto,
            chunk.strings,
            &handles,
            source,
            module_id.clone(),
        )?;
        heap.populate_proto(handles[i], buffers)
            .ok_or(LoadError::OutOfMemory)?;
    }

    let main_handle = *handles
        .get(chunk.main_proto as usize)
        .ok_or(LoadError::BadReference("main proto"))?;
    let main = heap
        .alloc_closure(Closure::new(main_handle))
        .ok_or(LoadError::OutOfMemory)?;
    // Pin the main closure as a GC root so the module survives a collection while the
    // host holds it (its proto graph and source string are reachable only through it);
    // `Vm::unload` releases the pin. Without this, a host could load, collect, then call
    // a swept module — a use-after-free.
    let pin = heap
        .pin(RawValue::Function(main))
        .ok_or(LoadError::OutOfMemory)?;
    Ok(LoadedModule { main, pin })
}

struct LoadChunkView<'a> {
    strings: &'a [Vec<u8>],
    protos: &'a [BytecodeProto],
    main_proto: u32,
}

fn validate_chunk_for_load(
    chunk: &BytecodeChunk,
    mode: LoadMode,
    limits: EffectiveLimits,
) -> Result<LoadChunkView<'_>, LoadError> {
    let (bytecode_version, type_version, strings, protos, main_proto) = match chunk {
        BytecodeChunk::Error { message } => return Err(LoadError::CompileError(message.clone())),
        BytecodeChunk::Valid {
            bytecode_version,
            type_version,
            strings,
            protos,
            main_proto,
            ..
        } => (
            *bytecode_version,
            *type_version,
            strings.as_slice(),
            protos.as_slice(),
            *main_proto,
        ),
    };

    if bytecode_version == 0 || bytecode_version > SUPPORTED_BYTECODE_VERSION {
        return Err(LoadError::UnsupportedVersion {
            bytecode: bytecode_version,
            type_version,
        });
    }

    if protos.len() > limits.max_bytecode_protos {
        return Err(LoadError::Invalid("too many protos".into()));
    }
    for proto in protos {
        let code_words = usize::try_from(code_word_count(&proto.code)).unwrap_or(usize::MAX);
        if code_words > limits.max_bytecode_words
            || proto.constants.len() > limits.max_bytecode_constants
        {
            return Err(LoadError::Invalid(
                "proto exceeds load-time size limit".into(),
            ));
        }
    }

    if mode == LoadMode::Validated
        && let Some(first) = validate_chunk(chunk).first()
    {
        return Err(LoadError::Invalid(format!("{first:?}")));
    }

    for proto in protos {
        if proto
            .constants
            .iter()
            .any(|c| matches!(c, Constant::ClassShape { .. }))
        {
            return Err(LoadError::Unsupported("class shape constant"));
        }
    }

    Ok(LoadChunkView {
        strings,
        protos,
        main_proto,
    })
}

fn build_proto_buffers(
    heap: &mut Heap,
    bproto: &BytecodeProto,
    strings: &[Vec<u8>],
    handles: &[RawGc<Proto>],
    source: RawGc<marker::Str>,
    module_id: Option<crate::ModuleId>,
) -> Result<ProtoBuffers, LoadError> {
    // Reject a prototype whose resolved buffers would push the heap over its memory cap
    // before resolving and cloning them, so a hostile chunk declaring oversized
    // prototypes is rejected up front rather than allocating them past
    // `max_memory_bytes` and only tripping the cap at the next runtime safepoint.
    if heap.would_exceed_cap(estimated_proto_footprint(bproto)) {
        return Err(LoadError::OutOfMemory);
    }
    let constants = resolve_constants(heap, bproto, strings, handles)?;
    let mut children = Vec::with_capacity(bproto.child_protos.len());
    for &id in &bproto.child_protos {
        children.push(
            *handles
                .get(id as usize)
                .ok_or(LoadError::BadReference("child proto"))?,
        );
    }
    let jump_targets = resolve_jump_targets(&bproto.code);
    // `to_line_numbers` yields one line per code *word*; an instruction with an
    // aux word spans two. Re-index to one line per *logical* instruction (the
    // unit a runtime pc counts in) by reading each instruction's start word, so
    // a fault after a two-word opcode reports the right line.
    let lines = bproto
        .line_info
        .as_ref()
        .and_then(|info| info.to_line_numbers())
        .map(|word_lines| {
            instruction_word_offsets(&bproto.code)
                .into_iter()
                .map(|word| {
                    word_lines
                        .get(word as usize)
                        .copied()
                        .map_or(0, |line| u32::try_from(line).unwrap_or(0))
                })
                .collect()
        })
        .unwrap_or_default();
    let debug_name = if bproto.debug_name == 0 {
        None
    } else {
        Some(intern_id(heap, strings, bproto.debug_name)?)
    };
    let coverage_hits = if bproto
        .code
        .iter()
        .any(|instruction| instruction.opcode == Opcode::Coverage)
    {
        vec![0; bproto.code.len()]
    } else {
        Vec::new()
    };
    Ok(ProtoBuffers {
        code: bproto.code.clone(),
        jump_targets,
        constants,
        child_protos: children,
        lines,
        coverage_hits,
        source,
        module_id,
        debug_name,
    })
}

fn placeholder_proto(bproto: &BytecodeProto) -> Proto {
    Proto::placeholder(
        bproto.max_stack_size,
        bproto.num_params,
        bproto.num_upvalues,
        bproto.is_vararg != 0,
        bproto.line_defined,
    )
}

/// Resolves each instruction's branch target to an absolute instruction index
/// once, at load. A non-branch instruction and an out-of-range target both map
/// to `u32::MAX`; the interpreter turns the latter into a runtime error, keeping
/// even trusted/unverified bytecode panic-free. This replaces the per-jump
/// `O(code length)` word-offset rescan in the dispatch hot loop.
/// An estimate of the heap footprint [`Proto::footprint`] will charge for `bproto`'s resolved
/// buffers, computed from the wire sizes before any buffer is allocated, so the load can reject
/// a hostile chunk declaring oversized prototypes before cloning the buffers rather than after
/// the cap is blown. It mirrors `footprint`'s per-prototype buffer terms (code, jump targets,
/// constants, child handles, lines), counting `len` for each `len == capacity` term. The
/// `lines` term is charged only when the chunk carries line info, since `footprint` charges
/// zero for an empty `lines` vector — counting it unconditionally would falsely reject a
/// no-debug-info chunk near the cap.
///
/// This bounds the per-prototype *buffers*, not the interned string-constant payloads or table
/// shapes those constants reference; those are charged as they are resolved, and the dispatch
/// safepoint's `over_memory_cap` check is the hard enforcement that still stops a chunk whose
/// strings blow the cap (this estimate only makes the common code/constant overflow eager).
fn estimated_proto_footprint(bproto: &BytecodeProto) -> usize {
    use std::mem::size_of;
    let instructions = bproto.code.len();
    let lines = if bproto.line_info.is_some() {
        instructions * size_of::<u32>() // one per code word when line info is present
    } else {
        0
    };
    instructions * size_of::<Instruction>()
        + instructions * size_of::<u32>() // jump_targets: one per instruction
        + lines
        + bproto.constants.len() * size_of::<RuntimeConstant>()
        + bproto.child_protos.len() * size_of::<RawGc<Proto>>()
}

fn resolve_jump_targets(code: &[Instruction]) -> Vec<u32> {
    let offsets = instruction_word_offsets(code);
    let code_words = code
        .last()
        .zip(offsets.last())
        .map(|(instruction, offset)| offset + instruction.word_len());
    let end_target = code_words
        .filter(|_| {
            code.last()
                .is_some_and(|instruction| instruction.opcode == Opcode::Return)
        })
        .and_then(|_| u32::try_from(code.len().saturating_sub(1)).ok());
    let mut word_to_index: HashMap<u32, u32> = HashMap::with_capacity(offsets.len());
    for (index, &word) in offsets.iter().enumerate() {
        word_to_index.insert(word, u32::try_from(index).unwrap_or(u32::MAX));
    }
    code.iter()
        .enumerate()
        .map(|(index, instruction)| {
            instruction
                .jump_target_word(offsets[index])
                .and_then(|target| u32::try_from(target).ok())
                .and_then(|target| {
                    word_to_index
                        .get(&target)
                        .copied()
                        .or_else(|| (Some(target) == code_words).then_some(end_target).flatten())
                })
                .unwrap_or(u32::MAX)
        })
        .collect()
}

fn resolve_constants(
    heap: &mut Heap,
    bproto: &BytecodeProto,
    strings: &[Vec<u8>],
    protos: &[RawGc<Proto>],
) -> Result<Vec<RuntimeConstant>, LoadError> {
    let mut out = Vec::with_capacity(bproto.constants.len());
    for constant in &bproto.constants {
        let resolved = match constant {
            Constant::Nil => RuntimeConstant::Value(RawValue::Nil),
            Constant::Boolean { value } => RuntimeConstant::Value(RawValue::Boolean(*value)),
            Constant::Number { bits } => {
                RuntimeConstant::Value(RawValue::Number(f64::from_bits(*bits)))
            }
            Constant::Integer { value } => RuntimeConstant::Value(RawValue::Integer(*value)),
            Constant::Vector { bits } => {
                // LUA_VECTOR_SIZE == 3: keep the first three lanes, drop the fourth.
                RuntimeConstant::Value(RawValue::Vector([
                    f32::from_bits(bits[0]),
                    f32::from_bits(bits[1]),
                    f32::from_bits(bits[2]),
                ]))
            }
            Constant::String { string } => {
                RuntimeConstant::Value(RawValue::String(intern_id(heap, strings, *string)?))
            }
            Constant::Import { import_id } => RuntimeConstant::Import(*import_id),
            Constant::Closure { proto } => RuntimeConstant::Proto(
                *protos
                    .get(*proto as usize)
                    .ok_or(LoadError::BadReference("closure proto"))?,
            ),
            Constant::Table { keys } => RuntimeConstant::Table(TableShape {
                entries: Vec::new(),
                array_hint: u32::try_from(keys.len()).unwrap_or(u32::MAX),
            }),
            Constant::TableWithConstants { entries } => {
                let mut resolved = Vec::with_capacity(entries.len());
                for entry in entries {
                    let key = constant_value(&out, entry.key)?;
                    let value = if entry.value < 0 {
                        RawValue::Nil
                    } else {
                        constant_value(&out, entry.value as u32)?
                    };
                    resolved.push((key, value));
                }
                RuntimeConstant::Table(TableShape {
                    entries: resolved,
                    array_hint: 0,
                })
            }
            Constant::ClassShape { .. } => {
                return Err(LoadError::Unsupported("class shape constant"));
            }
        };
        out.push(resolved);
    }
    Ok(out)
}

/// Interns a string by its one-based table id; id zero is the empty string.
fn intern_id(
    heap: &mut Heap,
    strings: &[Vec<u8>],
    id: u32,
) -> Result<RawGc<marker::Str>, LoadError> {
    let bytes: &[u8] = if id == 0 {
        b""
    } else {
        strings
            .get((id - 1) as usize)
            .map(Vec::as_slice)
            .ok_or(LoadError::BadReference("string id"))?
    };
    heap.intern_str(bytes).ok_or(LoadError::OutOfMemory)
}

/// Loads a chunk into a runnable module.
///
/// # Errors
/// Returns a [`LoadError`] for a compile-error chunk, an unsupported version or
/// feature, a failed structural check, an out-of-range reference, or OOM.
#[cfg(any())]
pub fn load(
    heap: &mut Heap,
    chunk: &BytecodeChunk,
    mode: LoadMode,
    chunk_name: &[u8],
) -> Result<LoadedModule, LoadError> {
    load_with_limits(heap, chunk, mode, chunk_name, EffectiveLimits::default())
}

/// Reads an already-resolved scalar constant by id (table-shape keys and values
/// reference earlier constants).
fn constant_value(resolved: &[RuntimeConstant], id: u32) -> Result<RawValue, LoadError> {
    match resolved.get(id as usize) {
        Some(RuntimeConstant::Value(value)) => Ok(*value),
        _ => Err(LoadError::BadReference("table entry constant")),
    }
}

#[cfg(any())]
mod tests {
    use std::path::PathBuf;

    use ruau_bytecode::{CompileOptions, compile_source};

    use super::*;
    use crate::{Ambient, HeapId};

    fn heap() -> Heap {
        Heap::new(HeapId(1), Ambient::deterministic(0).config)
    }

    fn compile(source: &str) -> BytecodeChunk {
        compile_source(source, &CompileOptions::default()).expect("compile")
    }

    fn minimal_proto() -> BytecodeProto {
        BytecodeProto {
            max_stack_size: 1,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: 0,
            flags: 0,
            type_info: ruau_bytecode::TypeInfo { raw: Vec::new() },
            code: vec![Instruction::abc(Opcode::Return, 0, 1, 0)],
            constants: Vec::new(),
            child_protos: Vec::new(),
            line_defined: 0,
            debug_name: 0,
            line_info: None,
            debug_info: None,
            feedback_slots: Vec::new(),
        }
    }

    fn chunk_with_proto(proto: BytecodeProto) -> BytecodeChunk {
        BytecodeChunk::Valid {
            bytecode_version: SUPPORTED_BYTECODE_VERSION,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            userdata_mapping_terminator: 0,
            protos: vec![proto],
            main_proto: 0,
        }
    }

    #[test]
    fn jump_to_proto_end_resolves_to_final_return() {
        let code = vec![
            Instruction::ad(Opcode::JumpIf, 0, 1),
            Instruction::abc(Opcode::Return, 0, 0, 0),
        ];

        assert_eq!(resolve_jump_targets(&code), vec![1, u32::MAX]);
    }

    #[test]
    fn load_error_display_renders_each_variant() {
        assert_eq!(
            LoadError::CompileError(b"bad syntax".to_vec()).to_string(),
            "chunk is a compile error: bad syntax"
        );
        assert_eq!(
            LoadError::UnsupportedVersion {
                bytecode: 99,
                type_version: 3,
            }
            .to_string(),
            "unsupported chunk version (bytecode 99, type 3)"
        );
        assert_eq!(
            LoadError::Unsupported("class shape constant").to_string(),
            "unsupported feature: class shape constant"
        );
        assert_eq!(
            LoadError::Invalid("too many protos".into()).to_string(),
            "structural verification rejected the chunk: too many protos"
        );
        assert_eq!(
            LoadError::BadReference("constant").to_string(),
            "out-of-range constant reference"
        );
        assert_eq!(
            LoadError::OutOfMemory.to_string(),
            "allocation failed under the memory cap"
        );
        assert!(
            LoadError::RuntimeCapabilitiesMismatch {
                artifact: RuntimeCapabilities::from_libraries([]).enable_runtime_compilation(),
                vm: RuntimeCapabilities::default().enable_runtime_compilation(),
            }
            .to_string()
            .contains("do not match the VM runtime capabilities")
        );
    }

    #[test]
    fn load_limits_run_before_structural_validation() {
        let mut proto = minimal_proto();
        proto.code = vec![Instruction::ad(Opcode::Jump, 0, i16::MAX)];
        let chunk = chunk_with_proto(proto);
        let limits = EffectiveLimits {
            max_bytecode_protos: 0,
            ..EffectiveLimits::default()
        };

        let Err(error) = validate_chunk_for_load(&chunk, LoadMode::Validated, limits) else {
            panic!("load should reject the chunk before validation")
        };
        assert_eq!(error, LoadError::Invalid("too many protos".into()));
    }

    #[test]
    fn load_word_limit_counts_aux_words() {
        let mut proto = minimal_proto();
        proto.constants = vec![Constant::Nil];
        proto.code = vec![Instruction::abc_with_aux(Opcode::LoadKx, 0, 0, 0, Some(0))];
        let chunk = chunk_with_proto(proto);
        let limits = EffectiveLimits {
            max_bytecode_words: 1,
            ..EffectiveLimits::default()
        };

        let Err(error) = validate_chunk_for_load(&chunk, LoadMode::Validated, limits) else {
            panic!("load should reject bytecode over the word limit")
        };
        assert_eq!(
            error,
            LoadError::Invalid("proto exceeds load-time size limit".into())
        );
    }

    #[test]
    fn compiled_module_validates_once_at_construction() {
        // A compile-error chunk is rejected at artifact build, not at load.
        assert!(matches!(
            CompiledModule::new(
                BytecodeChunk::Error {
                    message: b"boom".to_vec(),
                },
                RuntimeCapabilities::default().enable_runtime_compilation(),
            ),
            Err(LoadError::CompileError(message)) if message == b"boom".to_vec()
        ));

        // An unsupported bytecode version is rejected at artifact build.
        let future_version = match compile("return 1\n") {
            BytecodeChunk::Valid {
                type_version,
                strings,
                userdata_type_mappings,
                userdata_mapping_terminator,
                protos,
                main_proto,
                ..
            } => BytecodeChunk::Valid {
                bytecode_version: SUPPORTED_BYTECODE_VERSION + 1,
                type_version,
                strings,
                userdata_type_mappings,
                userdata_mapping_terminator,
                protos,
                main_proto,
            },
            other => other,
        };
        assert!(matches!(
            CompiledModule::new(
                future_version,
                RuntimeCapabilities::default().enable_runtime_compilation()
            ),
            Err(LoadError::UnsupportedVersion { .. })
        ));

        // A well-formed chunk seals into an artifact stamped with its runtime
        // capabilities; clones share it.
        let module = CompiledModule::new(
            compile("return 1\n"),
            RuntimeCapabilities::default().enable_runtime_compilation(),
        )
        .expect("validates");
        assert_eq!(
            module.runtime_capabilities(),
            &RuntimeCapabilities::default().enable_runtime_compilation()
        );
        let clone = module.clone();
        assert!(std::ptr::eq(module.chunk(), clone.chunk()));
    }

    #[test]
    fn loads_a_compiled_chunk() {
        let mut h = heap();
        let chunk = compile("local x = 1\nreturn x + 2\n");
        let module = load(&mut h, &chunk, LoadMode::Validated, DEFAULT_CHUNK_NAME).expect("load");
        let closure = h.closure(module.main).expect("main closure resident");
        assert!(h.proto(closure.proto).is_some());
    }

    #[test]
    fn rejects_a_compile_error_chunk() {
        let mut h = heap();
        let chunk = BytecodeChunk::Error {
            message: b"boom".to_vec(),
        };
        assert!(matches!(
            load(&mut h, &chunk, LoadMode::Validated, DEFAULT_CHUNK_NAME),
            Err(LoadError::CompileError(message)) if message == b"boom".to_vec()
        ));
    }

    #[test]
    fn rejects_an_unsupported_version() {
        let mut h = heap();
        let chunk = match compile("return 1\n") {
            BytecodeChunk::Valid {
                type_version,
                strings,
                userdata_type_mappings,
                userdata_mapping_terminator,
                protos,
                main_proto,
                ..
            } => BytecodeChunk::Valid {
                bytecode_version: SUPPORTED_BYTECODE_VERSION + 1,
                type_version,
                strings,
                userdata_type_mappings,
                userdata_mapping_terminator,
                protos,
                main_proto,
            },
            other => other,
        };
        assert!(matches!(
            load(&mut h, &chunk, LoadMode::Validated, DEFAULT_CHUNK_NAME),
            Err(LoadError::UnsupportedVersion { .. })
        ));
    }

    fn conformance_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../upstream/conformance")
    }

    #[test]
    fn loads_every_conformance_chunk() {
        let dir = conformance_dir();
        let mut seen = 0;
        let mut skipped_non_utf8 = 0;
        let mut skipped_compile_error = 0;
        let mut skipped_error_chunk = 0;
        let mut loaded = 0;
        for entry in std::fs::read_dir(&dir).expect("conformance dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("luau") {
                continue;
            }
            seen += 1;
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let source = match std::fs::read_to_string(&path) {
                Ok(source) => source,
                // `compile_source` takes `&str`; a non-UTF-8 source is not a loader concern.
                Err(_) => {
                    skipped_non_utf8 += 1;
                    continue;
                }
            };
            let chunk = match compile_source(&source, &CompileOptions::default()) {
                Ok(chunk) => chunk,
                // A source the compiler itself rejects is not a loader concern.
                Err(_) => {
                    skipped_compile_error += 1;
                    continue;
                }
            };
            // Sources the compiler cannot yet turn into valid bytecode (compiler
            // gaps such as some integer-literal syntax) are not loader concerns.
            if matches!(chunk, BytecodeChunk::Error { .. }) {
                skipped_error_chunk += 1;
                continue;
            }
            let mut h = heap();
            let result = load(&mut h, &chunk, LoadMode::Validated, DEFAULT_CHUNK_NAME);
            if name == "classes.luau" {
                // The class runtime is out of scope: the loader rejects the
                // class-shape constant, so a valid classes chunk must not load.
                assert!(
                    result.is_err(),
                    "classes.luau should not load, got {result:?}"
                );
            } else {
                assert!(result.is_ok(), "loading {name} failed: {result:?}");
                loaded += 1;
            }
        }
        assert!(seen > 0, "expected at least one conformance script");
        assert!(
            loaded > seen / 2,
            "loader smoke is not full-corpus coverage: seen {seen}, loaded {loaded}, \
             skipped non-UTF-8 {skipped_non_utf8}, compile errors {skipped_compile_error}, \
             error chunks {skipped_error_chunk}"
        );
    }
}
