//! Per-request resource ceilings.
//!
//! These limits are enforced at dispatch safepoints and while async host calls
//! are parked.

/// Garbage-collector stepping policy for a VM.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum GcPolicy {
    /// Run routine collections after allocation debt crosses the configured threshold.
    #[default]
    Threshold,
    /// Run a collection step at every allocation safepoint.
    CollectOnAllocation,
    /// Run deterministic pseudo-random collection steps at safepoints.
    RandomizedSteps,
}

/// Construction-time seeds and garbage-collector policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct AmbientConfig {
    /// Seed for every key hasher and for handle-identity rendering.
    pub hash_seed: u64,
    /// Seed for `math.random`.
    pub prng_seed: u64,
    /// The collector's step policy.
    pub gc_policy: GcPolicy,
}

/// Clock and cancellation mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum AmbientMode {
    /// Real wall clock and a real cancellation source.
    ///
    /// Not for wasm32-unknown-unknown: `Instant::now`/`SystemTime::now`
    /// panic there. Wasm hosts (e.g. Cloudflare Workers, whose time model is
    /// a frozen per-request timestamp anyway) construct
    /// [`AmbientMode::Deterministic`] from the host clock per request.
    Production,
    /// Deterministic clock/cancel behavior with a frozen wall-clock timestamp,
    /// in seconds since the Unix epoch.
    Deterministic(u64),
}

/// Seeds, policies, clock, and cancellation mode for a VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Ambient {
    /// Construction-time seeds and policies.
    pub config: AmbientConfig,
    /// Clock and cancellation mode.
    pub mode: AmbientMode,
}

impl Ambient {
    /// Real clock and cancellation with caller-supplied per-VM seeds.
    #[must_use]
    pub fn production(seed: u64) -> Self {
        Self {
            config: AmbientConfig {
                hash_seed: seed,
                prng_seed: seed,
                gc_policy: GcPolicy::Threshold,
            },
            mode: AmbientMode::Production,
        }
    }

    /// Fixed seeds with the wall clock frozen at the Unix epoch.
    #[must_use]
    pub fn deterministic(seed: u64) -> Self {
        Self {
            config: AmbientConfig {
                hash_seed: seed,
                prng_seed: seed,
                gc_policy: GcPolicy::Threshold,
            },
            mode: AmbientMode::Deterministic(0),
        }
    }
}

use std::time::Instant;

use crate::{
    cancel::Cancel,
    value_marshal::{DEFAULT_MAX_VALUE_MARSHAL_DEPTH, DEFAULT_MAX_VALUE_MARSHAL_NODES},
};

/// Default maximum Lua call depth.
pub const DEFAULT_MAX_CALL_DEPTH: u32 = 16_384;
/// Default maximum nested native/Rust re-entry depth.
///
/// Native targets default to 200. Targets without stack growth default to 24;
/// raise this only when the host provides and tests a larger stack.
pub const DEFAULT_MAX_NATIVE_DEPTH: u32 =
    if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
        200
    } else {
        24
    };
/// Default maximum number of varargs captured by one variadic frame.
pub const DEFAULT_MAX_VARARGS: usize = 1 << 20;
/// Default maximum bytes in one data-dependent string result.
pub const DEFAULT_MAX_STRING_BYTES: usize = 1 << 30;
/// Default maximum bytes in one buffer.
pub const DEFAULT_MAX_BUFFER_BYTES: usize = 1 << 30;
/// Default maximum elements/results produced by one data-dependent table operation.
pub const DEFAULT_MAX_TABLE_ELEMENTS: usize = 1 << 26;
/// Default maximum `__index` / `__newindex` chain depth.
pub const DEFAULT_MAX_META_CHAIN: usize = 100;
/// Default maximum bytes produced by one `string.pack` call.
pub const DEFAULT_MAX_PACK_BYTES: usize = 1 << 27;
/// Default pattern-matcher step budget per string pattern call.
pub const DEFAULT_MAX_PATTERN_STEPS: u32 = 10_000_000;
/// Default recursive pattern-matcher depth.
pub const DEFAULT_MAX_PATTERN_DEPTH: u32 = 200;
/// Default captures allowed in one pattern match.
pub const DEFAULT_MAX_PATTERN_CAPTURES: usize = 32;
/// Default maximum prototypes accepted in one bytecode chunk.
pub const DEFAULT_MAX_BYTECODE_PROTOS: usize = 1 << 20;
/// Default maximum instruction words accepted in one prototype.
pub const DEFAULT_MAX_BYTECODE_WORDS: usize = 1 << 24;
/// Default maximum constants accepted in one prototype.
pub const DEFAULT_MAX_BYTECODE_CONSTANTS: usize = 1 << 20;
/// Default maximum source bytes accepted by one runtime compilation request.
pub const DEFAULT_MAX_RUNTIME_COMPILE_SOURCE_BYTES: usize = 1 << 20;
/// Default maximum instruction words produced by one runtime compilation request.
pub const DEFAULT_MAX_RUNTIME_COMPILE_INSTRUCTIONS: usize = 1 << 23;
/// Default maximum encoded bytecode bytes produced by one runtime compilation request.
pub const DEFAULT_MAX_RUNTIME_COMPILE_BYTECODE_BYTES: usize = 1 << 24;

/// Byte and call quotas for a host output sink.
///
/// Quotas count per sink installation. A `None` field is unlimited. When a
/// write would exceed a quota, the sink receives [`SinkQuota::TRUNCATION_MARKER`]
/// once and then no more output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SinkQuota {
    /// Maximum total bytes forwarded to the sink. A write that would push the
    /// total past the quota is dropped whole (never split mid-line) and
    /// triggers truncation.
    pub max_bytes: Option<usize>,
    /// Maximum number of writes forwarded to the sink. Each `print` call is
    /// one write.
    pub max_calls: Option<usize>,
}

impl SinkQuota {
    /// The single line written to the sink when a quota is exceeded. Not
    /// counted against either quota.
    pub const TRUNCATION_MARKER: &'static [u8] = b"[output truncated]\n";

    /// Wraps `sink` in the quota accounting.
    pub(crate) fn apply(self, mut sink: crate::PrintSink) -> crate::PrintSink {
        let mut bytes_left = self.max_bytes.unwrap_or(usize::MAX);
        let mut calls_left = self.max_calls.unwrap_or(usize::MAX);
        let mut truncated = false;
        Box::new(move |line: &[u8]| {
            if truncated {
                return;
            }
            if calls_left == 0 || bytes_left < line.len() {
                truncated = true;
                sink(Self::TRUNCATION_MARKER);
                return;
            }
            calls_left -= 1;
            bytes_left -= line.len();
            sink(line);
        })
    }
}

/// Deadline from the VM's ambient mode.
#[derive(Clone, Copy, Debug)]
pub enum Deadline {
    /// Real wall-clock instant.
    Wall(Instant),
    /// Logical clock value, the gas counter under deterministic mode.
    Logical(u64),
}

/// Resource ceilings for one VM invocation.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Work budget in gas units.
    pub gas: Option<u64>,
    /// Record deterministic per-source gas attribution for this invocation.
    ///
    /// Profiling follows the gas meter; use a gas budget for non-empty output.
    pub gas_profile: bool,
    /// In-VM memory cap.
    pub max_memory_bytes: Option<usize>,
    /// Maximum Lua call depth.
    pub max_call_depth: Option<u32>,
    /// Maximum nested native/Rust re-entry depth.
    pub max_native_depth: Option<u32>,
    /// Maximum varargs captured by one variadic frame.
    pub max_varargs: Option<usize>,
    /// Maximum bytes in one data-dependent string result.
    pub max_string_bytes: Option<usize>,
    /// Maximum bytes in one buffer.
    pub max_buffer_bytes: Option<usize>,
    /// Maximum elements/results produced by one data-dependent table operation.
    pub max_table_elements: Option<usize>,
    /// Maximum `__index` / `__newindex` chain depth.
    pub max_meta_chain: Option<usize>,
    /// Maximum bytes produced by one `string.pack` call.
    pub max_pack_bytes: Option<usize>,
    /// Pattern-matcher step budget per string pattern call.
    pub max_pattern_steps: Option<u32>,
    /// Recursive pattern-matcher depth.
    pub max_pattern_depth: Option<u32>,
    /// Captures allowed in one pattern match.
    pub max_pattern_captures: Option<usize>,
    /// Maximum prototypes accepted in one bytecode chunk.
    pub max_bytecode_protos: Option<usize>,
    /// Maximum instruction words accepted in one prototype.
    pub max_bytecode_words: Option<usize>,
    /// Maximum constants accepted in one prototype.
    pub max_bytecode_constants: Option<usize>,
    /// Maximum source bytes accepted by one `loadstring` runtime compilation.
    pub max_runtime_compile_source_bytes: Option<usize>,
    /// Maximum instruction words produced by one `loadstring` runtime compilation.
    pub max_runtime_compile_instructions: Option<usize>,
    /// Maximum encoded bytecode bytes produced by one `loadstring` runtime compilation.
    pub max_runtime_compile_bytecode_bytes: Option<usize>,
    /// Maximum recursive depth copied by one value-marshal result conversion.
    pub max_value_marshal_depth: Option<usize>,
    /// Maximum number of values copied by one value-marshal result conversion.
    pub max_value_marshal_nodes: Option<usize>,
    /// Gas per scheduling slice before a cooperative yield.
    pub quantum: Option<u64>,
    /// Invocation deadline.
    pub deadline: Option<Deadline>,
    /// Cancellation handle.
    pub cancel: Option<Cancel>,
}

impl Limits {
    /// Fully unmetered limits: every ceiling unset.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            gas: None,
            gas_profile: false,
            max_memory_bytes: None,
            max_call_depth: None,
            max_native_depth: None,
            max_varargs: None,
            max_string_bytes: None,
            max_buffer_bytes: None,
            max_table_elements: None,
            max_meta_chain: None,
            max_pack_bytes: None,
            max_pattern_steps: None,
            max_pattern_depth: None,
            max_pattern_captures: None,
            max_bytecode_protos: None,
            max_bytecode_words: None,
            max_bytecode_constants: None,
            max_runtime_compile_source_bytes: None,
            max_runtime_compile_instructions: None,
            max_runtime_compile_bytecode_bytes: None,
            max_value_marshal_depth: None,
            max_value_marshal_nodes: None,
            quantum: None,
            deadline: None,
            cancel: None,
        }
    }

    /// Metered limits derived from `gas` and `max_memory_bytes`.
    #[must_use]
    pub fn production(gas: u64, max_memory_bytes: usize) -> Self {
        let memory = max_memory_bytes.max(64 * 1024);
        let quarter = (memory / 4).max(16 * 1024);
        Self {
            gas: Some(gas),
            max_memory_bytes: Some(max_memory_bytes),
            max_string_bytes: Some(quarter),
            max_buffer_bytes: Some(quarter),
            max_pack_bytes: Some(quarter),
            // A table element slot costs at least ~16 bytes, so this cap is
            // reachable only by a table that already approaches the heap cap.
            max_table_elements: Some((memory / 16).max(1024)),
            max_runtime_compile_source_bytes: Some(quarter),
            max_runtime_compile_instructions: Some((memory / 8).max(16 * 1024)),
            max_runtime_compile_bytecode_bytes: Some(quarter),
            ..Self::unlimited()
        }
    }

    #[must_use]
    pub(crate) fn overlay(&self, overrides: &Self) -> Self {
        Self {
            gas: overrides.gas.or(self.gas),
            gas_profile: overrides.gas_profile || self.gas_profile,
            max_memory_bytes: overrides.max_memory_bytes.or(self.max_memory_bytes),
            max_call_depth: overrides.max_call_depth.or(self.max_call_depth),
            max_native_depth: overrides.max_native_depth.or(self.max_native_depth),
            max_varargs: overrides.max_varargs.or(self.max_varargs),
            max_string_bytes: overrides.max_string_bytes.or(self.max_string_bytes),
            max_buffer_bytes: overrides.max_buffer_bytes.or(self.max_buffer_bytes),
            max_table_elements: overrides.max_table_elements.or(self.max_table_elements),
            max_meta_chain: overrides.max_meta_chain.or(self.max_meta_chain),
            max_pack_bytes: overrides.max_pack_bytes.or(self.max_pack_bytes),
            max_pattern_steps: overrides.max_pattern_steps.or(self.max_pattern_steps),
            max_pattern_depth: overrides.max_pattern_depth.or(self.max_pattern_depth),
            max_pattern_captures: overrides.max_pattern_captures.or(self.max_pattern_captures),
            max_bytecode_protos: overrides.max_bytecode_protos.or(self.max_bytecode_protos),
            max_bytecode_words: overrides.max_bytecode_words.or(self.max_bytecode_words),
            max_bytecode_constants: overrides
                .max_bytecode_constants
                .or(self.max_bytecode_constants),
            max_runtime_compile_source_bytes: overrides
                .max_runtime_compile_source_bytes
                .or(self.max_runtime_compile_source_bytes),
            max_runtime_compile_instructions: overrides
                .max_runtime_compile_instructions
                .or(self.max_runtime_compile_instructions),
            max_runtime_compile_bytecode_bytes: overrides
                .max_runtime_compile_bytecode_bytes
                .or(self.max_runtime_compile_bytecode_bytes),
            max_value_marshal_depth: overrides
                .max_value_marshal_depth
                .or(self.max_value_marshal_depth),
            max_value_marshal_nodes: overrides
                .max_value_marshal_nodes
                .or(self.max_value_marshal_nodes),
            quantum: overrides.quantum.or(self.quantum),
            deadline: overrides.deadline.or(self.deadline),
            cancel: overrides.cancel.clone().or_else(|| self.cancel.clone()),
        }
    }

    #[must_use]
    pub(crate) fn effective(&self) -> EffectiveLimits {
        EffectiveLimits {
            max_call_depth: self.max_call_depth.unwrap_or(DEFAULT_MAX_CALL_DEPTH),
            max_native_depth: self.max_native_depth.unwrap_or(DEFAULT_MAX_NATIVE_DEPTH),
            max_varargs: self.max_varargs.unwrap_or(DEFAULT_MAX_VARARGS),
            max_string_bytes: self.max_string_bytes.unwrap_or(DEFAULT_MAX_STRING_BYTES),
            max_buffer_bytes: self.max_buffer_bytes.unwrap_or(DEFAULT_MAX_BUFFER_BYTES),
            max_table_elements: self
                .max_table_elements
                .unwrap_or(DEFAULT_MAX_TABLE_ELEMENTS),
            max_meta_chain: self.max_meta_chain.unwrap_or(DEFAULT_MAX_META_CHAIN),
            max_pack_bytes: self.max_pack_bytes.unwrap_or(DEFAULT_MAX_PACK_BYTES),
            max_pattern_steps: self.max_pattern_steps.unwrap_or(DEFAULT_MAX_PATTERN_STEPS),
            max_pattern_depth: self.max_pattern_depth.unwrap_or(DEFAULT_MAX_PATTERN_DEPTH),
            max_pattern_captures: self
                .max_pattern_captures
                .unwrap_or(DEFAULT_MAX_PATTERN_CAPTURES),
            max_bytecode_protos: self
                .max_bytecode_protos
                .unwrap_or(DEFAULT_MAX_BYTECODE_PROTOS),
            max_bytecode_words: self
                .max_bytecode_words
                .unwrap_or(DEFAULT_MAX_BYTECODE_WORDS),
            max_bytecode_constants: self
                .max_bytecode_constants
                .unwrap_or(DEFAULT_MAX_BYTECODE_CONSTANTS),
            max_runtime_compile_source_bytes: self
                .max_runtime_compile_source_bytes
                .unwrap_or(DEFAULT_MAX_RUNTIME_COMPILE_SOURCE_BYTES),
            max_runtime_compile_instructions: self
                .max_runtime_compile_instructions
                .unwrap_or(DEFAULT_MAX_RUNTIME_COMPILE_INSTRUCTIONS),
            max_runtime_compile_bytecode_bytes: self
                .max_runtime_compile_bytecode_bytes
                .unwrap_or(DEFAULT_MAX_RUNTIME_COMPILE_BYTECODE_BYTES),
            max_value_marshal_depth: self
                .max_value_marshal_depth
                .unwrap_or(DEFAULT_MAX_VALUE_MARSHAL_DEPTH),
            max_value_marshal_nodes: self
                .max_value_marshal_nodes
                .unwrap_or(DEFAULT_MAX_VALUE_MARSHAL_NODES),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct EffectiveLimits {
    pub max_call_depth: u32,
    pub max_native_depth: u32,
    pub max_varargs: usize,
    pub max_string_bytes: usize,
    pub max_buffer_bytes: usize,
    pub max_table_elements: usize,
    pub max_meta_chain: usize,
    pub max_pack_bytes: usize,
    pub max_pattern_steps: u32,
    pub max_pattern_depth: u32,
    pub max_pattern_captures: usize,
    pub max_bytecode_protos: usize,
    pub max_bytecode_words: usize,
    pub max_bytecode_constants: usize,
    pub max_runtime_compile_source_bytes: usize,
    pub max_runtime_compile_instructions: usize,
    pub max_runtime_compile_bytecode_bytes: usize,
    pub max_value_marshal_depth: usize,
    pub max_value_marshal_nodes: usize,
}

impl Default for EffectiveLimits {
    fn default() -> Self {
        Limits::unlimited().effective()
    }
}

#[cfg(any())]
mod sink_quota_tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Runs `source` on a fresh deterministic VM with a quota-wrapped capture
    /// sink, returning everything the sink received.
    fn run_with_quota(seed: u64, quota: SinkQuota, source: &str) -> Vec<u8> {
        let chunk = ruau_bytecode::compile_source(
            source,
            &ruau_bytecode::CompileOptions::for_vm_execution(),
        )
        .expect("compile");
        let mut vm = crate::Vm::builder()
            .ambient(Ambient::deterministic(seed))
            .build_for_test();
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink_out = Arc::clone(&out);
        vm.set_print_sink_with_quota(
            Box::new(move |bytes| sink_out.lock().expect("sink lock").extend_from_slice(bytes)),
            quota,
        );
        let module = vm.load(&chunk).expect("load");
        vm.call(&module, Default::default()).expect("script runs");
        let captured = out.lock().expect("sink lock");
        captured.clone()
    }

    fn count_markers(output: &[u8]) -> usize {
        output
            .windows(SinkQuota::TRUNCATION_MARKER.len())
            .filter(|window| *window == SinkQuota::TRUNCATION_MARKER)
            .count()
    }

    #[test]
    fn byte_quota_truncates_whole_lines_with_one_marker() {
        // Each `print("line", i)` writes "line\t<i>\n": 7 bytes for one-digit
        // `i`. A 20-byte quota admits two lines (14 bytes); the third would
        // reach 21, so it is dropped whole and replaced by the marker, and the
        // remaining seven prints stay silent.
        let output = run_with_quota(
            0,
            SinkQuota {
                max_bytes: Some(20),
                max_calls: None,
            },
            "for i = 1, 10 do print(\"line\", i) end",
        );
        let expected = [
            b"line\t1\nline\t2\n".as_slice(),
            SinkQuota::TRUNCATION_MARKER,
        ]
        .concat();
        assert_eq!(output, expected);
        assert_eq!(count_markers(&output), 1);
    }

    #[test]
    fn call_quota_truncates_with_one_marker() {
        let output = run_with_quota(
            0,
            SinkQuota {
                max_bytes: None,
                max_calls: Some(3),
            },
            "for i = 1, 10 do print(i) end",
        );
        let expected = [b"1\n2\n3\n".as_slice(), SinkQuota::TRUNCATION_MARKER].concat();
        assert_eq!(output, expected);
        assert_eq!(count_markers(&output), 1);
    }

    #[test]
    fn zero_quota_emits_only_the_marker_once() {
        // Both quotas exhausted before the first write: the first print
        // triggers the marker, every later print is silent — the marker never
        // repeats even though output keeps arriving.
        let output = run_with_quota(
            0,
            SinkQuota {
                max_bytes: Some(0),
                max_calls: Some(0),
            },
            "for i = 1, 10 do print(i) end",
        );
        assert_eq!(output, SinkQuota::TRUNCATION_MARKER);
    }

    #[test]
    fn quota_counts_across_invocations_until_the_sink_is_reinstalled() {
        // The quota is per sink installation (VM-lifetime state), so a second
        // invocation keeps drawing on the same budget; reinstalling the sink
        // is the documented per-run reset.
        let chunk = ruau_bytecode::compile_source(
            "print(\"x\")",
            &ruau_bytecode::CompileOptions::for_vm_execution(),
        )
        .expect("compile");
        let mut vm = crate::test_vm();
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink_out = Arc::clone(&out);
        vm.set_print_sink_with_quota(
            Box::new(move |bytes| sink_out.lock().expect("sink lock").extend_from_slice(bytes)),
            SinkQuota {
                max_bytes: None,
                max_calls: Some(1),
            },
        );
        let module = vm.load(&chunk).expect("load");
        vm.call(&module, Default::default()).expect("first run");
        vm.call(&module, Default::default()).expect("second run");
        let expected = [b"x\n".as_slice(), SinkQuota::TRUNCATION_MARKER].concat();
        assert_eq!(out.lock().expect("sink lock").as_slice(), expected);
    }

    #[test]
    fn truncation_is_deterministic_across_same_seeded_runs() {
        // Seed-dependent output volume: the same seed must truncate at the
        // same point with byte-identical output, marker included.
        let source = "for i = 1, 8 do print(math.random()) end";
        let quota = SinkQuota {
            max_bytes: Some(64),
            max_calls: None,
        };
        let first = run_with_quota(7, quota, source);
        let second = run_with_quota(7, quota, source);
        assert_eq!(first, second);
        assert_eq!(count_markers(&first), 1);
    }
}

#[cfg(any())]
mod production_preset_tests {
    use super::*;

    #[test]
    fn production_preset_derives_finite_caps_from_memory() {
        let memory = 64 * 1024 * 1024;
        let limits = Limits::production(5_000_000, memory);

        assert_eq!(limits.gas, Some(5_000_000));
        assert_eq!(limits.max_memory_bytes, Some(memory));
        assert_eq!(limits.max_string_bytes, Some(memory / 4));
        assert_eq!(limits.max_buffer_bytes, Some(memory / 4));
        assert_eq!(limits.max_pack_bytes, Some(memory / 4));
        assert_eq!(limits.max_table_elements, Some(memory / 16));
        assert_eq!(limits.max_runtime_compile_source_bytes, Some(memory / 4));
        assert_eq!(limits.max_runtime_compile_instructions, Some(memory / 8));
        assert_eq!(limits.max_runtime_compile_bytecode_bytes, Some(memory / 4));

        // A tiny memory cap still produces sane non-zero floors.
        let tiny = Limits::production(1, 0);
        assert!(tiny.max_string_bytes.unwrap() >= 16 * 1024);
        assert!(tiny.max_table_elements.unwrap() >= 1024);
    }
}
