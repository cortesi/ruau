//! Opaque VM heap snapshots for sandboxed, quiescent VMs.

use ruau_vm_api::{HeapId, RawGc, RawValue, marker};

use crate::{
    Vm, VmBuildError,
    heap::{Heap, HeapImage},
    limits::{Ambient, EffectiveLimits},
};

const SNAPSHOT_MAGIC: &[u8; 8] = b"RUAUSNP\0";
const SNAPSHOT_VERSION: u32 = 3;
const SNAPSHOT_VERSION_LEN: usize = std::mem::size_of::<u32>();
const SNAPSHOT_FINGERPRINT_LEN: usize = 32;
const SNAPSHOT_STAMP_LEN_LEN: usize = std::mem::size_of::<u32>();
const SNAPSHOT_HEADER_LEN: usize =
    SNAPSHOT_MAGIC.len() + SNAPSHOT_VERSION_LEN + SNAPSHOT_FINGERPRINT_LEN + SNAPSHOT_STAMP_LEN_LEN;
const MAX_SNAPSHOT_STAMP_BYTES: usize = 4096;
const MAX_SNAPSHOT_MSGPACK_DEPTH: usize = 128;
const MAX_SNAPSHOT_MSGPACK_COLLECTION_ITEMS: usize = MAX_SNAPSHOT_BYTES;
const MAX_SNAPSHOT_MSGPACK_SCALAR_BYTES: usize = MAX_SNAPSHOT_BYTES;

/// Maximum accepted encoded snapshot size.
///
/// Restore checks this before decoding the object graph, so hostile bytes cannot
/// ask the snapshot codec to allocate arbitrarily before Ruau has authenticated
/// the small fixed header.
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

/// Opaque bytes produced by [`Vm::snapshot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmSnapshot {
    bytes: Vec<u8>,
}

impl VmSnapshot {
    /// Wraps previously stored snapshot bytes.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// The raw opaque snapshot bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the wrapper and returns the raw bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl AsRef<[u8]> for VmSnapshot {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

/// Why a snapshot could not be created or restored.
#[derive(Debug)]
pub enum SnapshotError {
    /// Building the restore template failed.
    Build(VmBuildError),
    /// The VM was not at a supported quiescent point.
    NotQuiescent(&'static str),
    /// The VM contains state the prototype codec intentionally refuses.
    Unsupported(&'static str),
    /// The bytes are not a valid Ruau snapshot image.
    Decode(String),
    /// The target template does not match the snapshot stamp.
    TemplateMismatch(&'static str),
    /// The decoded image is internally inconsistent.
    Invalid(&'static str),
    /// Rebuilding the image would exceed memory or allocation failed.
    OutOfMemory,
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Build(error) => write!(f, "restore template build failed: {error}"),
            Self::NotQuiescent(reason) => write!(f, "VM is not quiescent: {reason}"),
            Self::Unsupported(reason) => write!(f, "snapshot unsupported: {reason}"),
            Self::Decode(reason) => write!(f, "snapshot decode failed: {reason}"),
            Self::TemplateMismatch(reason) => {
                write!(f, "snapshot template mismatch: {reason}")
            }
            Self::Invalid(reason) => write!(f, "snapshot image is invalid: {reason}"),
            Self::OutOfMemory => f.write_str("snapshot restore ran out of memory"),
        }
    }
}

impl std::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Build(error) => Some(error),
            Self::NotQuiescent(_)
            | Self::Unsupported(_)
            | Self::Decode(_)
            | Self::TemplateMismatch(_)
            | Self::Invalid(_)
            | Self::OutOfMemory => None,
        }
    }
}

impl From<VmBuildError> for SnapshotError {
    fn from(value: VmBuildError) -> Self {
        Self::Build(value)
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct SnapshotEnvelope {
    stamp: SnapshotStamp,
    main_thread: RawGc<marker::Thread>,
    heap: HeapImage,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct SnapshotBody {
    main_thread: RawGc<marker::Thread>,
    heap: HeapImage,
}

#[derive(serde::Serialize)]
struct SnapshotBodyRef<'a> {
    main_thread: RawGc<marker::Thread>,
    heap: &'a HeapImage,
}

struct SnapshotParts<'a> {
    stamp: SnapshotStamp,
    body: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SnapshotStamp {
    ambient: Ambient,
    effective_limits: EffectiveLimits,
    memory_cap: Option<usize>,
    runtime_capability_libraries: u16,
    runtime_capability_compilation: bool,
    host_function_count: usize,
    host_type_count: usize,
    module_source_present: bool,
    preload_count: usize,
}

impl SnapshotStamp {
    pub(crate) fn from_vm(vm: &Vm) -> Self {
        let (runtime_capability_libraries, runtime_capability_compilation) =
            vm.runtime_capabilities.snapshot_bits();
        Self {
            ambient: vm.ambient,
            effective_limits: vm.limits.effective(),
            memory_cap: vm.limits.max_memory_bytes,
            runtime_capability_libraries,
            runtime_capability_compilation,
            host_function_count: vm.heap.host_function_count(),
            host_type_count: vm.heap.host_type_count(),
            module_source_present: vm.heap.module_source_present(),
            preload_count: vm.preloaded.len(),
        }
    }

    pub(crate) fn check(self, other: Self) -> Result<(), SnapshotError> {
        if self.ambient != other.ambient {
            return Err(SnapshotError::TemplateMismatch("ambient"));
        }
        if self.effective_limits != other.effective_limits || self.memory_cap != other.memory_cap {
            return Err(SnapshotError::TemplateMismatch("limits"));
        }
        if self.runtime_capability_libraries != other.runtime_capability_libraries
            || self.runtime_capability_compilation != other.runtime_capability_compilation
        {
            return Err(SnapshotError::TemplateMismatch("runtime capabilities"));
        }
        if self.host_function_count != other.host_function_count {
            return Err(SnapshotError::TemplateMismatch("host function registry"));
        }
        if self.host_type_count != other.host_type_count {
            return Err(SnapshotError::TemplateMismatch("host type registry"));
        }
        if self.module_source_present != other.module_source_present {
            return Err(SnapshotError::TemplateMismatch("module source"));
        }
        if self.preload_count != other.preload_count {
            return Err(SnapshotError::TemplateMismatch("preloads"));
        }
        Ok(())
    }
}

pub fn encode_envelope(envelope: &SnapshotEnvelope) -> Result<VmSnapshot, SnapshotError> {
    let stamp = rmp_serde::to_vec_named(&envelope.stamp)
        .map_err(|error| SnapshotError::Decode(error.to_string()))?;
    if stamp.len() > MAX_SNAPSHOT_STAMP_BYTES {
        return Err(SnapshotError::Decode(format!(
            "snapshot stamp too large: {} bytes exceeds {MAX_SNAPSHOT_STAMP_BYTES}",
            stamp.len()
        )));
    }
    let body = rmp_serde::to_vec_named(&SnapshotBodyRef {
        main_thread: envelope.main_thread,
        heap: &envelope.heap,
    })
    .map_err(|error| SnapshotError::Decode(error.to_string()))?;
    let total_len = SNAPSHOT_HEADER_LEN
        .checked_add(stamp.len())
        .and_then(|len| len.checked_add(body.len()))
        .ok_or_else(|| SnapshotError::Decode("snapshot too large".to_owned()))?;
    if total_len > MAX_SNAPSHOT_BYTES {
        return Err(SnapshotError::Decode(format!(
            "snapshot too large: {total_len} bytes exceeds {MAX_SNAPSHOT_BYTES}"
        )));
    }
    let stamp_len = u32::try_from(stamp.len())
        .map_err(|_| SnapshotError::Decode("snapshot stamp too large".to_owned()))?;
    validate_msgpack_value(&stamp, "stamp")?;
    validate_msgpack_value(&body, "body")?;

    let mut bytes = Vec::with_capacity(total_len);
    bytes.extend_from_slice(SNAPSHOT_MAGIC);
    bytes.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&crate::semantics_fingerprint());
    bytes.extend_from_slice(&stamp_len.to_le_bytes());
    bytes.extend_from_slice(&stamp);
    bytes.extend_from_slice(&body);
    Ok(VmSnapshot::from_bytes(bytes))
}

fn decode_parts(bytes: &[u8]) -> Result<SnapshotParts<'_>, SnapshotError> {
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(SnapshotError::Decode(format!(
            "snapshot too large: {} bytes exceeds {MAX_SNAPSHOT_BYTES}",
            bytes.len()
        )));
    }
    if bytes.len() < SNAPSHOT_HEADER_LEN {
        return Err(SnapshotError::Decode(
            "truncated snapshot header".to_owned(),
        ));
    }
    if &bytes[..SNAPSHOT_MAGIC.len()] != SNAPSHOT_MAGIC {
        return Err(SnapshotError::Decode("bad magic".to_owned()));
    }
    let version_start = SNAPSHOT_MAGIC.len();
    let version_end = version_start + SNAPSHOT_VERSION_LEN;
    let version = u32::from_le_bytes(
        bytes[version_start..version_end]
            .try_into()
            .expect("slice length is checked above"),
    );
    if version != SNAPSHOT_VERSION {
        return Err(SnapshotError::Decode("unsupported version".to_owned()));
    }
    let fingerprint_start = version_end;
    let fingerprint_end = fingerprint_start + SNAPSHOT_FINGERPRINT_LEN;
    if bytes[fingerprint_start..fingerprint_end] != crate::semantics_fingerprint() {
        return Err(SnapshotError::TemplateMismatch("semantics fingerprint"));
    }
    let stamp_len_start = fingerprint_end;
    let stamp_len_end = stamp_len_start + SNAPSHOT_STAMP_LEN_LEN;
    let stamp_len = u32::from_le_bytes(
        bytes[stamp_len_start..stamp_len_end]
            .try_into()
            .expect("slice length is checked above"),
    ) as usize;
    if stamp_len > MAX_SNAPSHOT_STAMP_BYTES {
        return Err(SnapshotError::Decode(format!(
            "snapshot stamp too large: {stamp_len} bytes exceeds {MAX_SNAPSHOT_STAMP_BYTES}"
        )));
    }
    let stamp_start = stamp_len_end;
    let stamp_end = stamp_start
        .checked_add(stamp_len)
        .ok_or_else(|| SnapshotError::Decode("snapshot stamp too large".to_owned()))?;
    if stamp_end > bytes.len() {
        return Err(SnapshotError::Decode("truncated snapshot stamp".to_owned()));
    }
    let stamp = &bytes[stamp_start..stamp_end];
    validate_msgpack_value(stamp, "stamp")?;
    let stamp: SnapshotStamp =
        rmp_serde::from_slice(stamp).map_err(|error| SnapshotError::Decode(error.to_string()))?;
    Ok(SnapshotParts {
        stamp,
        body: &bytes[stamp_end..],
    })
}

fn decode_body(body: &[u8]) -> Result<SnapshotBody, SnapshotError> {
    validate_msgpack_value(body, "body")?;
    rmp_serde::from_slice(body).map_err(|error| SnapshotError::Decode(error.to_string()))
}

#[cfg(any())]
pub fn decode_envelope(bytes: &[u8]) -> Result<SnapshotEnvelope, SnapshotError> {
    let parts = decode_parts(bytes)?;
    let body = decode_body(parts.body)?;
    Ok(SnapshotEnvelope {
        stamp: parts.stamp,
        main_thread: body.main_thread,
        heap: body.heap,
    })
}

fn validate_msgpack_value(bytes: &[u8], label: &'static str) -> Result<(), SnapshotError> {
    if bytes.is_empty() {
        return Err(SnapshotError::Decode(format!("truncated snapshot {label}")));
    }
    let mut cursor = 0;
    let mut stack = vec![1usize];
    while let Some(remaining) = stack.last_mut() {
        if *remaining == 0 {
            stack.pop();
            continue;
        }
        *remaining -= 1;
        let marker = read_u8(bytes, &mut cursor, label)?;
        match marker {
            0x00..=0x7f | 0xc0 | 0xc2 | 0xc3 | 0xe0..=0xff => {}
            0x80..=0x8f => {
                push_msgpack_sequence(&mut stack, usize::from(marker & 0x0f), 2, label)?;
            }
            0x90..=0x9f => {
                push_msgpack_sequence(&mut stack, usize::from(marker & 0x0f), 1, label)?;
            }
            0xa0..=0xbf => {
                skip_msgpack_scalar(bytes, &mut cursor, usize::from(marker & 0x1f), label)?;
            }
            0xc4 | 0xd9 => {
                let len = usize::from(read_u8(bytes, &mut cursor, label)?);
                skip_msgpack_scalar(bytes, &mut cursor, len, label)?;
            }
            0xc5 | 0xda => {
                let len = usize::from(read_u16(bytes, &mut cursor, label)?);
                skip_msgpack_scalar(bytes, &mut cursor, len, label)?;
            }
            0xc6 | 0xdb => {
                let len = read_u32(bytes, &mut cursor, label)? as usize;
                skip_msgpack_scalar(bytes, &mut cursor, len, label)?;
            }
            0xca => skip_msgpack_scalar(bytes, &mut cursor, 4, label)?,
            0xcb | 0xcf | 0xd3 => skip_msgpack_scalar(bytes, &mut cursor, 8, label)?,
            0xcc | 0xd0 => skip_msgpack_scalar(bytes, &mut cursor, 1, label)?,
            0xcd | 0xd1 => skip_msgpack_scalar(bytes, &mut cursor, 2, label)?,
            0xce | 0xd2 => skip_msgpack_scalar(bytes, &mut cursor, 4, label)?,
            0xdc => {
                let len = usize::from(read_u16(bytes, &mut cursor, label)?);
                push_msgpack_sequence(&mut stack, len, 1, label)?;
            }
            0xdd => {
                let len = read_u32(bytes, &mut cursor, label)? as usize;
                push_msgpack_sequence(&mut stack, len, 1, label)?;
            }
            0xde => {
                let len = usize::from(read_u16(bytes, &mut cursor, label)?);
                push_msgpack_sequence(&mut stack, len, 2, label)?;
            }
            0xdf => {
                let len = read_u32(bytes, &mut cursor, label)? as usize;
                push_msgpack_sequence(&mut stack, len, 2, label)?;
            }
            0xc1 | 0xc7..=0xc9 | 0xd4..=0xd8 => {
                return Err(SnapshotError::Decode(format!(
                    "snapshot {label} contains unsupported msgpack marker 0x{marker:02x}"
                )));
            }
        }
    }
    if cursor != bytes.len() {
        return Err(SnapshotError::Decode(format!(
            "snapshot {label} has trailing bytes"
        )));
    }
    Ok(())
}

fn push_msgpack_sequence(
    stack: &mut Vec<usize>,
    len: usize,
    items_per_entry: usize,
    label: &'static str,
) -> Result<(), SnapshotError> {
    if len > MAX_SNAPSHOT_MSGPACK_COLLECTION_ITEMS {
        return Err(SnapshotError::Decode(format!(
            "snapshot {label} collection too large: {len} entries exceeds \
             {MAX_SNAPSHOT_MSGPACK_COLLECTION_ITEMS}"
        )));
    }
    let items = len
        .checked_mul(items_per_entry)
        .ok_or_else(|| SnapshotError::Decode(format!("snapshot {label} collection too large")))?;
    if items == 0 {
        return Ok(());
    }
    if stack.len() >= MAX_SNAPSHOT_MSGPACK_DEPTH {
        return Err(SnapshotError::Decode(format!(
            "snapshot {label} nesting exceeds {MAX_SNAPSHOT_MSGPACK_DEPTH}"
        )));
    }
    stack.push(items);
    Ok(())
}

fn skip_msgpack_scalar(
    bytes: &[u8],
    cursor: &mut usize,
    len: usize,
    label: &'static str,
) -> Result<(), SnapshotError> {
    if len > MAX_SNAPSHOT_MSGPACK_SCALAR_BYTES {
        return Err(SnapshotError::Decode(format!(
            "snapshot {label} scalar too large: {len} bytes exceeds \
             {MAX_SNAPSHOT_MSGPACK_SCALAR_BYTES}"
        )));
    }
    if bytes.len().saturating_sub(*cursor) < len {
        return Err(SnapshotError::Decode(format!("truncated snapshot {label}")));
    }
    *cursor += len;
    Ok(())
}

fn read_u8(bytes: &[u8], cursor: &mut usize, label: &'static str) -> Result<u8, SnapshotError> {
    let Some(value) = bytes.get(*cursor) else {
        return Err(SnapshotError::Decode(format!("truncated snapshot {label}")));
    };
    *cursor += 1;
    Ok(*value)
}

fn read_u16(bytes: &[u8], cursor: &mut usize, label: &'static str) -> Result<u16, SnapshotError> {
    let end = (*cursor)
        .checked_add(std::mem::size_of::<u16>())
        .ok_or_else(|| SnapshotError::Decode(format!("truncated snapshot {label}")))?;
    let slice = bytes
        .get(*cursor..end)
        .ok_or_else(|| SnapshotError::Decode(format!("truncated snapshot {label}")))?;
    *cursor = end;
    Ok(u16::from_be_bytes(
        slice.try_into().expect("slice length is checked above"),
    ))
}

fn read_u32(bytes: &[u8], cursor: &mut usize, label: &'static str) -> Result<u32, SnapshotError> {
    let end = (*cursor)
        .checked_add(std::mem::size_of::<u32>())
        .ok_or_else(|| SnapshotError::Decode(format!("truncated snapshot {label}")))?;
    let slice = bytes
        .get(*cursor..end)
        .ok_or_else(|| SnapshotError::Decode(format!("truncated snapshot {label}")))?;
    *cursor = end;
    Ok(u32::from_be_bytes(
        slice.try_into().expect("slice length is checked above"),
    ))
}

pub fn restore_snapshot_bytes(vm: Vm, bytes: &[u8]) -> Result<Vm, SnapshotError> {
    let parts = decode_parts(bytes)?;
    parts.stamp.check(SnapshotStamp::from_vm(&vm))?;
    let body = decode_body(parts.body)?;
    restore_heap(
        vm,
        SnapshotEnvelope {
            stamp: parts.stamp,
            main_thread: body.main_thread,
            heap: body.heap,
        },
    )
}

pub fn new_envelope(
    stamp: SnapshotStamp,
    main_thread: RawGc<marker::Thread>,
    heap: HeapImage,
) -> SnapshotEnvelope {
    SnapshotEnvelope {
        stamp,
        main_thread,
        heap,
    }
}

pub fn rebrand_raw<T>(handle: RawGc<T>, heap: HeapId) -> RawGc<T> {
    RawGc::from_parts(handle.index(), handle.generation(), heap)
}

pub fn rebrand_value(value: RawValue, heap: HeapId) -> RawValue {
    match value {
        RawValue::String(handle) => RawValue::String(rebrand_raw(handle, heap)),
        RawValue::Table(handle) => RawValue::Table(rebrand_raw(handle, heap)),
        RawValue::Function(handle) => RawValue::Function(rebrand_raw(handle, heap)),
        RawValue::Userdata(handle) => RawValue::Userdata(rebrand_raw(handle, heap)),
        RawValue::Thread(handle) => RawValue::Thread(rebrand_raw(handle, heap)),
        RawValue::Buffer(handle) => RawValue::Buffer(rebrand_raw(handle, heap)),
        RawValue::Nil
        | RawValue::Boolean(_)
        | RawValue::Number(_)
        | RawValue::Integer(_)
        | RawValue::Vector(_)
        | RawValue::LightUserdata { .. } => value,
    }
}

pub fn rebrand_thread(handle: RawGc<marker::Thread>, heap: HeapId) -> RawGc<marker::Thread> {
    rebrand_raw(handle, heap)
}

pub fn restore_heap(mut vm: Vm, envelope: SnapshotEnvelope) -> Result<Vm, SnapshotError> {
    envelope.stamp.check(SnapshotStamp::from_vm(&vm))?;
    let heap_id = vm.heap.id;
    let main_thread = rebrand_thread(envelope.main_thread, heap_id);
    if vm
        .heap
        .total_exceeds_memory_cap(envelope.heap.min_restore_bytes())
    {
        return Err(SnapshotError::OutOfMemory);
    }
    let heap = Heap::from_snapshot_image(
        std::mem::replace(&mut vm.heap, Heap::new(heap_id, vm.ambient.config)),
        envelope.heap,
    )?;
    if heap.over_memory_cap() {
        return Err(SnapshotError::OutOfMemory);
    }
    vm.heap = heap;
    vm.main_thread = main_thread;
    vm.preloaded.clear();
    vm.validate()
        .map_err(|_| SnapshotError::Invalid("restored heap validation failed"))?;
    Ok(vm)
}

#[cfg(any())]
mod tests {
    use ruau_bytecode::{CompileOptions, compile_source};

    use super::*;
    use crate::{Ambient, Limits, RuntimeCapabilities, object::HostId};

    fn snapshot_vm(memory_cap: Option<usize>) -> Vm {
        let mut vm = Vm::builder()
            .ambient(Ambient::deterministic(77))
            .limits(Limits {
                gas: Some(10_000),
                max_memory_bytes: memory_cap,
                ..Limits::unlimited()
            })
            .runtime_capabilities(RuntimeCapabilities::default())
            .build()
            .expect("snapshot vm builds");
        let chunk = compile_source(
            "STATE = { one = 1, two = 2, three = 3 }\nreturn STATE.one",
            &CompileOptions::default(),
        )
        .expect("snapshot fixture compiles");
        let module = vm.load_named(&chunk, b"=snapshot-test").expect("load");
        vm.call(&module, Default::default()).expect("run fixture");
        vm.sandbox_for_untrusted().expect("sandbox");
        vm
    }

    fn snapshot_template_with_seed(seed: u64, memory_cap: Option<usize>) -> Vm {
        Vm::builder()
            .ambient(Ambient::deterministic(seed))
            .limits(Limits {
                gas: Some(10_000),
                max_memory_bytes: memory_cap,
                ..Limits::unlimited()
            })
            .runtime_capabilities(RuntimeCapabilities::default())
            .build()
            .expect("snapshot template builds")
    }

    fn snapshot_template(memory_cap: Option<usize>) -> Vm {
        snapshot_template_with_seed(77, memory_cap)
    }

    fn snapshot_bytes_with_stamp_and_body(stamp: SnapshotStamp, body: &[u8]) -> Vec<u8> {
        let stamp = rmp_serde::to_vec_named(&stamp).expect("stamp encodes");
        let mut bytes = Vec::with_capacity(SNAPSHOT_HEADER_LEN + stamp.len() + body.len());
        bytes.extend_from_slice(SNAPSHOT_MAGIC);
        bytes.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&crate::semantics_fingerprint());
        bytes.extend_from_slice(&(stamp.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&stamp);
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn decode_rejects_hostile_header_bytes_before_body_decode() {
        assert!(matches!(
            decode_envelope(&vec![0; MAX_SNAPSHOT_BYTES + 1]),
            Err(SnapshotError::Decode(message)) if message.contains("too large")
        ));
        assert!(matches!(
            decode_envelope(b"short"),
            Err(SnapshotError::Decode(message)) if message.contains("header")
        ));

        let mut vm = snapshot_vm(None);
        let mut bytes = vm.snapshot().expect("snapshot").into_bytes();

        bytes[0] = b'X';
        assert!(matches!(
            decode_envelope(&bytes),
            Err(SnapshotError::Decode(message)) if message == "bad magic"
        ));

        let mut bytes = vm.snapshot().expect("snapshot").into_bytes();
        bytes[SNAPSHOT_MAGIC.len()] = SNAPSHOT_VERSION.wrapping_add(1) as u8;
        assert!(matches!(
            decode_envelope(&bytes),
            Err(SnapshotError::Decode(message)) if message == "unsupported version"
        ));

        let mut bytes = vm.snapshot().expect("snapshot").into_bytes();
        bytes[SNAPSHOT_MAGIC.len() + SNAPSHOT_VERSION_LEN] ^= 0xff;
        assert!(matches!(
            decode_envelope(&bytes),
            Err(SnapshotError::TemplateMismatch("semantics fingerprint"))
        ));
    }

    #[test]
    fn decode_rejects_huge_declared_body_length_cleanly() {
        let vm = snapshot_vm(None);
        let mut body = vec![0xc6];
        body.extend_from_slice(&u32::MAX.to_be_bytes());
        let bytes = snapshot_bytes_with_stamp_and_body(SnapshotStamp::from_vm(&vm), &body);

        assert!(matches!(
            decode_envelope(&bytes),
            Err(SnapshotError::Decode(message)) if !message.is_empty()
        ));
    }

    #[test]
    fn restore_rejects_template_mismatch_before_body_decode() {
        let vm = snapshot_vm(None);
        let mut body = vec![0xc6];
        body.extend_from_slice(&u32::MAX.to_be_bytes());
        let bytes = snapshot_bytes_with_stamp_and_body(SnapshotStamp::from_vm(&vm), &body);

        assert!(matches!(
            restore_snapshot_bytes(snapshot_template_with_seed(78, None), &bytes),
            Err(SnapshotError::TemplateMismatch("ambient"))
        ));
    }

    #[test]
    fn restore_rejects_heap_that_exceeds_template_memory_cap() {
        let mut vm = snapshot_vm(None);
        let mut envelope =
            decode_envelope(vm.snapshot().expect("snapshot").as_bytes()).expect("snapshot decodes");
        envelope.stamp.memory_cap = Some(0);
        let snapshot = encode_envelope(&envelope).expect("mutated snapshot encodes");

        let restored = restore_snapshot_bytes(snapshot_template(Some(0)), snapshot.as_bytes());
        assert!(matches!(restored, Err(SnapshotError::OutOfMemory)));
    }

    #[test]
    fn restore_rejects_forged_host_proto() {
        let mut vm = snapshot_vm(None);
        let mut envelope =
            decode_envelope(vm.snapshot().expect("snapshot").as_bytes()).expect("snapshot decodes");
        assert!(
            envelope.heap.test_forge_first_proto_host(HostId(0)),
            "snapshot fixture should contain at least one proto to forge"
        );
        let snapshot = encode_envelope(&envelope).expect("mutated snapshot encodes");

        let restored = restore_snapshot_bytes(snapshot_template(None), snapshot.as_bytes());
        assert!(matches!(
            restored,
            Err(SnapshotError::Invalid(message)) if message.contains("host prototypes")
        ));
    }

    #[test]
    fn restore_accepts_trusted_snapshot_with_native_protos() {
        let mut vm = snapshot_vm(None);
        let envelope =
            decode_envelope(vm.snapshot().expect("snapshot").as_bytes()).expect("snapshot decodes");
        assert!(
            envelope.heap.test_has_native_proto(),
            "default-capability snapshot fixture should contain native builtin protos"
        );
        let snapshot = encode_envelope(&envelope).expect("snapshot re-encodes");

        restore_snapshot_bytes(snapshot_template(None), snapshot.as_bytes())
            .expect("trusted native proto snapshot restores");
    }

    fn restore_rejects_forged_arena(
        forge: impl FnOnce(&mut HeapImage) -> bool,
        expected: &'static str,
    ) {
        let mut vm = snapshot_vm(None);
        let mut envelope =
            decode_envelope(vm.snapshot().expect("snapshot").as_bytes()).expect("snapshot decodes");
        assert!(
            forge(&mut envelope.heap),
            "snapshot fixture should be forgeable"
        );
        let snapshot = encode_envelope(&envelope).expect("mutated snapshot encodes");

        let restored = restore_snapshot_bytes(snapshot_template(None), snapshot.as_bytes());
        assert!(matches!(
            restored,
            Err(SnapshotError::Invalid(message)) if message.contains(expected)
        ));
    }

    #[test]
    fn restore_rejects_arena_free_list_live_slot() {
        restore_rejects_forged_arena(
            HeapImage::test_forge_string_live_slot_as_free,
            "free index references live slot",
        );
    }

    #[test]
    fn restore_rejects_arena_duplicate_free_entry() {
        restore_rejects_forged_arena(
            HeapImage::test_forge_string_duplicate_free_entry,
            "duplicate free index",
        );
    }

    #[test]
    fn restore_rejects_arena_missing_generation() {
        restore_rejects_forged_arena(
            HeapImage::test_forge_string_missing_generation,
            "generation missing",
        );
    }

    #[test]
    fn restore_rejects_arena_out_of_range_free_index() {
        restore_rejects_forged_arena(
            HeapImage::test_forge_string_out_of_range_free_index,
            "free index out of range",
        );
    }

    fn restore_normalized_forged_gc_metadata(forge: impl FnOnce(&mut HeapImage) -> bool) -> Vm {
        let mut vm = snapshot_vm(None);
        let mut envelope =
            decode_envelope(vm.snapshot().expect("snapshot").as_bytes()).expect("snapshot decodes");
        assert!(
            forge(&mut envelope.heap),
            "snapshot fixture should be forgeable"
        );
        let snapshot = encode_envelope(&envelope).expect("mutated snapshot encodes");

        restore_snapshot_bytes(snapshot_template(None), snapshot.as_bytes())
            .expect("restore normalizes derived GC metadata")
    }

    #[test]
    fn restore_rebuilds_missing_arena_young_entry() {
        let mut restored =
            restore_normalized_forged_gc_metadata(HeapImage::test_forge_string_missing_young_entry);

        assert!(matches!(
            restored.collect_routine(),
            crate::CollectionOutcome::Completed { .. }
        ));
        restored
            .validate()
            .expect("normalized young list survives routine collection");
    }

    #[test]
    fn restore_normalizes_forged_gc_metadata_before_collection() {
        let mut restored = restore_normalized_forged_gc_metadata(
            HeapImage::test_forge_gc_metadata_for_normalization,
        );

        assert!(matches!(
            restored.collect_routine(),
            crate::CollectionOutcome::Completed { .. }
        ));
        restored
            .validate()
            .expect("normalized metadata survives a routine collection");
        assert!(matches!(
            restored.collect(),
            crate::CollectionOutcome::Completed { .. }
        ));
        restored
            .validate()
            .expect("normalized metadata survives a full collection");
    }
}
