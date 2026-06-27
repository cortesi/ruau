#[cfg(any(test, feature = "conformance"))]
use ruau_bytecode::{CompileOptions, FastFlag};
#[cfg(any(test, feature = "conformance"))]
use ruau_source::InMemorySource;

#[cfg(any(test, feature = "conformance"))]
use crate::{ExecutionFeatures, Limits};

/// Stable post-epoch wall-clock timestamp used by VM conformance harnesses.
/// Ordinary `Ambient::deterministic(0)` remains frozen at the Unix epoch.
#[cfg(any(test, feature = "conformance"))]
pub const CONFORMANCE_WALL_SECS: u64 = 1_700_000_000;
/// Gas budget used by the VM conformance harness.
#[cfg(any(test, feature = "conformance"))]
pub const CONFORMANCE_GAS: u64 = 5_000_000;
/// Conformance-only gas profile for `tables_sparse_boundary.luau`.
///
/// The owned script preserves upstream's 16-bit sparse table boundary smoke
/// loop, which intentionally performs roughly a million table writes.
#[cfg(any(test, feature = "conformance"))]
pub const CONFORMANCE_TABLES_SPARSE_BOUNDARY_GAS: u64 = 20_000_000;
/// Conformance-only gas profile for `pcall.luau`.
///
/// The upstream script deliberately builds a ten-thousand-value protected-call
/// result chain while proving the stack-overflow profile. Result-copy metering
/// makes that work visible, so the ratchet gives this resource-profile script a
/// budget large enough to test depth semantics rather than the generic gas cap.
#[cfg(any(test, feature = "conformance"))]
pub const CONFORMANCE_PCALL_GAS: u64 = 200_000_000;
/// Conformance-only depth profile for `errors.luau`.
///
/// The script only asserts that recursive Lua calls report stack overflow with
/// source locations. A tenant-scale depth cap makes the frontier spend a long
/// time proving the same property repeatedly.
#[cfg(any(test, feature = "conformance"))]
pub const CONFORMANCE_ERRORS_MAX_CALL_DEPTH: u32 = 512;
/// Conformance-only depth profile for `pcall.luau`.
///
/// Ruau's root frame counts toward `Limits::max_call_depth`; upstream's
/// `calllimit = 20000` treats the top-level frame and the coroutine service
/// frame as consumed, so the conformance profile allows the root plus
/// `calllimit - 2` recursive Lua frames. A `pcall`/`xpcall` protected boundary
/// consumes the remaining slot and overflows one recursive frame earlier.
#[cfg(any(test, feature = "conformance"))]
pub const CONFORMANCE_PCALL_MAX_CALL_DEPTH: u32 = 19_999;

/// The conformance-only execution profile for one script.
///
/// This is the first shared path for the feature-gated conformance push:
/// resource limits, compiler switches, and compatibility features travel
/// together instead of being inferred independently by the in-crate regression
/// gate and the `xtask` stress runner.
#[cfg(any(test, feature = "conformance"))]
#[derive(Clone)]
pub struct ConformanceScriptConfig {
    /// VM resource limits for this script.
    pub limits: Limits,
    /// Compiler options for this script.
    pub compile_options: CompileOptions,
    /// Explicit compatibility features this script may use.
    pub features: ExecutionFeatures,
    /// Whether the conformance VM should grant source-backed `require`.
    pub module_source: bool,
}

/// Which conformance suite a script came from.
#[cfg(any(test, feature = "conformance"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConformanceScriptOrigin {
    /// A verbatim export from upstream Luau's conformance corpus.
    UpstreamVerbatim,
    /// An Ruau-owned script under `crates/ruau-vm/conformance-ruau`.
    RuauOwned,
}

const CONFORMANCE_SCOPE: &str = include_str!("../conformance-scope.txt");
const FNV1A64_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A64_PRIME: u64 = 0x0000_0100_0000_01b3;
const CONFORMANCE_SCOPE_REVISION: u64 = fnv1a64(CONFORMANCE_SCOPE.as_bytes());

const fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV1A64_OFFSET;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(FNV1A64_PRIME);
        index += 1;
    }
    hash
}

/// Stable revision of the committed VM conformance scope manifest.
#[must_use]
pub const fn conformance_scope_revision() -> u64 {
    CONFORMANCE_SCOPE_REVISION
}

/// Whether a discovered conformance script is required to pass, or is retained
/// only as explicit permanent non-goal denominator accounting.
#[cfg(any(test, feature = "conformance"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConformanceScopeDisposition<'a> {
    /// The script is in Ruau's declared conformance scope and must return `OK`.
    Required,
    /// The script is intentionally outside Ruau's conformance scope.
    PermanentNonGoal {
        /// The policy disposition that justifies excluding this full script.
        disposition: &'a str,
    },
}

/// One entry in `crates/ruau-vm/conformance-scope.txt`.
#[cfg(any(test, feature = "conformance"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConformanceScopeEntry<'a> {
    /// Script basename, unique across upstream-verbatim and Ruau-owned suites.
    pub name: &'a str,
    /// Required/pass or permanent-non-goal disposition.
    pub disposition: ConformanceScopeDisposition<'a>,
}

/// Result type yielded by [`conformance_scope_entries`].
#[cfg(any(test, feature = "conformance"))]
pub type ConformanceScopeResult = Result<ConformanceScopeEntry<'static>, String>;

/// Parses the committed conformance scope manifest.
///
/// The manifest is the denominator for both the fast regression gate and the
/// stress runner: required scripts must pass; permanent non-goals remain
/// explicitly counted rather than disappearing behind a legacy passing list.
#[cfg(any(test, feature = "conformance"))]
pub fn conformance_scope_entries() -> impl Iterator<Item = ConformanceScopeResult> {
    CONFORMANCE_SCOPE
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                None
            } else {
                Some(parse_conformance_scope_line(index + 1, line))
            }
        })
}

#[cfg(any(test, feature = "conformance"))]
fn parse_conformance_scope_line(
    line_number: usize,
    line: &'static str,
) -> Result<ConformanceScopeEntry<'static>, String> {
    let mut parts = line.split_whitespace();
    let kind = parts.next().expect("non-empty manifest line");
    let name = parts
        .next()
        .ok_or_else(|| format!("conformance-scope.txt:{line_number}: missing script name"))?;
    let disposition = match kind {
        "required" => {
            if parts.next().is_some() {
                return Err(format!(
                    "conformance-scope.txt:{line_number}: required entries take only a script name"
                ));
            }
            ConformanceScopeDisposition::Required
        }
        "permanent-non-goal" => {
            let disposition = parts.next().ok_or_else(|| {
                format!(
                    "conformance-scope.txt:{line_number}: permanent non-goals need a disposition"
                )
            })?;
            if parts.next().is_some() {
                return Err(format!(
                    "conformance-scope.txt:{line_number}: permanent non-goal dispositions cannot contain whitespace"
                ));
            }
            ConformanceScopeDisposition::PermanentNonGoal { disposition }
        }
        other => {
            return Err(format!(
                "conformance-scope.txt:{line_number}: unknown scope kind `{other}`"
            ));
        }
    };
    Ok(ConformanceScopeEntry { name, disposition })
}

/// Per-script conformance profile for regression tests and stress runs.
#[must_use]
#[cfg(any(test, feature = "conformance"))]
pub fn conformance_config_for_script(name: &str) -> ConformanceScriptConfig {
    let mut config = base_conformance_config();
    config.limits = conformance_limits_for_script(name);
    config.compile_options = conformance_compile_options_for_script(name);
    config.features = conformance_features_for_script(name);
    config
}

/// Source-aware conformance profile.
///
/// Upstream-verbatim scripts still use the explicit export-side by-name map.
/// Ruau-owned scripts carry their feature, compiler flag, and conformance-only
/// limit metadata in their header comments, so this parser is the single runtime
/// path that turns those headers into execution config.
#[cfg(any(test, feature = "conformance"))]
pub fn conformance_config_for_script_source(
    name: &str,
    source: &[u8],
    origin: ConformanceScriptOrigin,
) -> Result<ConformanceScriptConfig, String> {
    match origin {
        ConformanceScriptOrigin::UpstreamVerbatim => Ok(conformance_config_for_script(name)),
        ConformanceScriptOrigin::RuauOwned => {
            let text = std::str::from_utf8(source).map_err(|error| {
                format!("{name}: owned conformance script is not UTF-8: {error}")
            })?;
            let mut config = base_conformance_config();
            apply_owned_conformance_metadata(name, text, &mut config)?;
            Ok(config)
        }
    }
}

/// Module source for owned conformance scripts that exercise `require`.
#[cfg(any(test, feature = "conformance"))]
#[must_use]
pub fn conformance_module_source() -> InMemorySource {
    InMemorySource::new()
        .with_module(
            "conformance/counter",
            "__ruau_require_count = (__ruau_require_count or 0) + 1\n\
             return { count = __ruau_require_count }",
        )
        .with_module(
            "conformance/dep",
            "collectgarbage()\n\
             local nested = require('./nested.luau')\n\
             return { value = 42, nested = nested.value }",
        )
        .with_module("conformance/nested", "return { value = 99 }")
        .with_module(
            "conformance/outer",
            "local dep = require('./dep')\n\
             return { value = dep.value + dep.nested }",
        )
        .with_module("conformance/no_return", "local x = 1")
        .with_module("conformance/nil_return", "return nil")
        .with_module(
            "conformance/retry",
            "__ruau_require_retry_count = (__ruau_require_retry_count or 0) + 1\n\
             if __ruau_require_retry_count == 1 then error('retry once') end\n\
             return { value = 7 }",
        )
        .with_module(
            "conformance/cycle_a",
            "local b = require('./cycle_b')\nreturn b",
        )
        .with_module(
            "conformance/cycle_b",
            "local a = require('./cycle_a')\nreturn a",
        )
}

#[cfg(any(test, feature = "conformance"))]
fn base_conformance_config() -> ConformanceScriptConfig {
    ConformanceScriptConfig {
        limits: Limits {
            gas: Some(CONFORMANCE_GAS),
            ..Limits::unlimited()
        },
        compile_options: CompileOptions::for_vm_execution(),
        features: ExecutionFeatures::all_off(),
        module_source: false,
    }
}

/// Per-script VM limits for the upstream conformance harness.
///
/// Service callers should keep using `Limits` / per-call overrides directly.
/// This helper is only the shared profile for Ruau's conformance ratchet.
#[must_use]
#[cfg(any(test, feature = "conformance"))]
pub fn conformance_limits_for_script(name: &str) -> Limits {
    let mut limits = base_conformance_config().limits;
    match name {
        "errors.luau" => limits.max_call_depth = Some(CONFORMANCE_ERRORS_MAX_CALL_DEPTH),
        "pcall.luau" => {
            limits.gas = Some(CONFORMANCE_PCALL_GAS);
            limits.max_call_depth = Some(CONFORMANCE_PCALL_MAX_CALL_DEPTH);
        }
        _ => {}
    }
    limits
}

/// Per-script compiler options for the upstream conformance harness.
///
/// Service callers should pass their own `CompileOptions` through the compiler or production
/// runner. This helper is only the shared feature-flag profile for Ruau's conformance ratchet.
#[must_use]
#[cfg(any(test, feature = "conformance"))]
pub fn conformance_compile_options_for_script(name: &str) -> CompileOptions {
    let mut options = CompileOptions::for_vm_execution();
    if name == "coverage.luau" {
        options.coverage_level = 1;
    }
    if matches!(name, "integers.luau" | "integers_regspill.luau") {
        enable_luau_integer_type(&mut options);
    }
    options
}

/// Per-script compatibility features for the conformance harness.
///
/// `ExecutionFeatures` lives in `ruau-vm`, next to the runtime switches it
/// controls.
/// This conformance profile keeps script-specific compatibility flags explicit
/// until the owned upstream rows carry audited feature metadata in source.
#[must_use]
#[cfg(any(test, feature = "conformance"))]
pub fn conformance_features_for_script(name: &str) -> ExecutionFeatures {
    let mut features = ExecutionFeatures::all_off();
    if matches!(
        name,
        "basic.luau"
            | "buffers.luau"
            | "closure.luau"
            | "events.luau"
            | "iter_fenv.luau"
            | "locals.luau"
            | "safeenv.luau"
            | "tables.luau"
    ) {
        features.fenv = true;
    }
    if matches!(
        name,
        "calls.luau"
            | "closure.luau"
            | "constructs.luau"
            | "errors.luau"
            | "gc.luau"
            | "literals.luau"
            | "locals.luau"
            | "math.luau"
            | "pm.luau"
            | "utf8.luau"
            | "vararg.luau"
    ) {
        features.runtime_compilation = true;
    }
    if matches!(
        name,
        "buffers.luau"
            | "calls.luau"
            | "coroutine.luau"
            | "coverage.luau"
            | "cyield.luau"
            | "errors.luau"
            | "gc.luau"
            | "integers.luau"
            | "iter.luau"
            | "pcall.luau"
            | "types.luau"
            | "vector.luau"
            | "vector_library.luau"
    ) {
        features.harness_mode = true;
    }
    features
}

#[cfg(any(test, feature = "conformance"))]
fn apply_owned_conformance_metadata(
    name: &str,
    source: &str,
    config: &mut ConformanceScriptConfig,
) -> Result<(), String> {
    let header = source
        .lines()
        .take_while(|line| line.trim().is_empty() || line.starts_with("--"))
        .collect::<Vec<_>>();
    let feature_line = owned_header_value(&header, "Execution features:")
        .ok_or_else(|| format!("{name}: missing `Execution features:` header"))?;
    let compiler_line = owned_header_value(&header, "Compiler flags:")
        .ok_or_else(|| format!("{name}: missing `Compiler flags:` header"))?;
    let limit_line = owned_header_value(&header, "Conformance-only limits:")
        .ok_or_else(|| format!("{name}: missing `Conformance-only limits:` header"))?;

    apply_owned_features(name, feature_line, config)?;
    apply_owned_compiler_flags(name, compiler_line, &mut config.compile_options)?;
    apply_owned_limits(name, limit_line, &mut config.limits)
}

#[cfg(any(test, feature = "conformance"))]
fn owned_header_value<'a>(header: &'a [&'a str], prefix: &str) -> Option<&'a str> {
    header.iter().find_map(|line| {
        let line = line.trim_start_matches("--").trim();
        let start = line.find(prefix)? + prefix.len();
        let mut value = line[start..].trim();
        for next_field in [
            " Execution features:",
            " Compiler flags:",
            " Profile:",
            " Conformance-only limits:",
        ] {
            if let Some((before, _)) = value.split_once(next_field) {
                value = before.trim();
            }
        }
        Some(value.trim_end_matches('.').trim())
    })
}

#[cfg(any(test, feature = "conformance"))]
fn apply_owned_features(
    name: &str,
    value: &str,
    config: &mut ConformanceScriptConfig,
) -> Result<(), String> {
    for feature in value
        .split(',')
        .map(str::trim)
        .filter(|feature| !feature.is_empty())
    {
        match feature {
            "none" => {}
            "harness mode" => config.features.harness_mode = true,
            "fenv" => config.features.fenv = true,
            "runtime compilation" => config.features.runtime_compilation = true,
            "module source" => config.module_source = true,
            other => return Err(format!("{name}: unknown execution feature `{other}`")),
        }
    }
    Ok(())
}

#[cfg(any(test, feature = "conformance"))]
fn apply_owned_compiler_flags(
    name: &str,
    value: &str,
    options: &mut CompileOptions,
) -> Result<(), String> {
    for flag in value
        .split(',')
        .map(str::trim)
        .filter(|flag| !flag.is_empty())
    {
        match flag {
            "none" => {}
            "LuauIntegerType" => enable_luau_integer_type(options),
            other => return Err(format!("{name}: unknown compiler flag `{other}`")),
        }
    }
    Ok(())
}

#[cfg(any(test, feature = "conformance"))]
fn apply_owned_limits(name: &str, value: &str, limits: &mut Limits) -> Result<(), String> {
    for limit in value
        .split(',')
        .map(str::trim)
        .filter(|limit| !limit.is_empty())
    {
        match limit {
            "none" => {}
            "max memory bytes = 1 MiB" => limits.max_memory_bytes = Some(1 << 20),
            "quantum = 25" => limits.quantum = Some(25),
            "CONFORMANCE_TABLES_SPARSE_BOUNDARY_GAS" => {
                limits.gas = Some(CONFORMANCE_TABLES_SPARSE_BOUNDARY_GAS);
            }
            other => return Err(format!("{name}: unknown conformance-only limit `{other}`")),
        }
    }
    Ok(())
}

/// Enables Luau integer literal/type compilation for conformance scripts that opt in.
#[cfg(any(test, feature = "conformance"))]
pub fn enable_luau_integer_type(options: &mut CompileOptions) {
    options.syntax_flags.luau_integer_type = true;
    if !options.fast_flag("LuauIntegerType") {
        options.fast_flags.push(FastFlag {
            name: "LuauIntegerType".to_owned(),
            value: true,
        });
    }
}
