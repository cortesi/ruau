use ruau_ast::parse::{ParseConfig, SyntaxFlags};
use serde::{Deserialize, Serialize};

use crate::builder::DEFAULT_VERSION;

/// Public compile policy for Ruau VM bytecode.
///
/// This is the ordinary embedder-facing surface: it keeps VM-safe hardening
/// enabled and exposes only the knobs that are meaningful for current pinned
/// Luau compilation. Upstream fixture compatibility uses
/// [`UpstreamCompilerOptions`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct CompileOptions {
    /// Upstream optimization level.
    pub optimization_level: u8,
    /// Upstream debug level.
    pub debug_level: u8,
    /// Upstream coverage level.
    pub coverage_level: u8,
}

impl CompileOptions {
    /// Returns the default VM compile policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns compiler-internal options for ordinary VM execution.
    #[doc(hidden)]
    #[must_use]
    pub fn to_upstream_options(&self) -> UpstreamCompilerOptions {
        let mut options = UpstreamCompilerOptions {
            clear_dead_stack_slots: true,
            ..UpstreamCompilerOptions::default()
        };
        options.optimization_level = self.optimization_level;
        options.debug_level = self.debug_level;
        options.coverage_level = self.coverage_level;
        options
    }
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            optimization_level: 1,
            debug_level: 1,
            coverage_level: 0,
        }
    }
}

/// Upstream-fixture-shaped compiler options.
///
/// This type is public only for repository tooling and compatibility fixtures
/// that need to mirror upstream Luau sidecars. Ordinary embedders should use
/// [`CompileOptions`] through `Surface`, `RuntimeCapabilities`, host, or runner
/// APIs.
#[doc(hidden)]
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct UpstreamCompilerOptions {
    /// Upstream optimization level.
    pub optimization_level: u8,
    /// Upstream debug level.
    pub debug_level: u8,
    /// Upstream type-info level.
    pub type_info_level: u8,
    /// Upstream coverage level.
    pub coverage_level: u8,
    /// Parser syntax flags.
    pub syntax_flags: SyntaxFlags,
    /// Parser options.
    pub parse_options: UpstreamParseOptions,
    /// Bytecode-visible fast flags.
    pub fast_flags: Vec<FastFlag>,
    /// Bytecode-visible fast ints.
    pub fast_ints: Vec<FastInt>,
    /// Alternative vector library.
    pub vector_lib: Option<String>,
    /// Alternative vector constructor.
    pub vector_ctor: Option<String>,
    /// Alternative vector type.
    pub vector_type: Option<String>,
    /// Mutable globals.
    pub mutable_globals: Vec<String>,
    /// Userdata types emitted in type info.
    pub userdata_types: Vec<String>,
    /// Disabled builtin names.
    pub disabled_builtins: Vec<String>,
    /// Reified known-member constants.
    pub known_members: Vec<KnownMember>,
    /// Clear dead stack slots after their last compiler-visible use.
    ///
    /// This is an Ruau VM execution hardening option, not an upstream Luau
    /// compiler option. Keep it out of fixture sidecars so bytecode-oracle
    /// comparisons continue to use upstream-compatible defaults.
    #[serde(skip)]
    pub clear_dead_stack_slots: bool,
    /// Suppress import-path and related fast-path lowering when source uses
    /// `getfenv` or `setfenv`.
    ///
    /// This is an Ruau VM execution correctness option. Upstream bytecode
    /// fixtures still use upstream-compatible defaults, where `getfenv` /
    /// `setfenv` affect some optimizer decisions but do not globally rewrite
    /// ordinary imports into table lookups.
    #[serde(skip)]
    pub preserve_fenv_semantics: bool,
}

impl UpstreamCompilerOptions {
    /// Returns the parser configuration for this fixture posture: the sidecar
    /// parse options combined with the sidecar syntax flags.
    #[must_use]
    pub fn parse_config(&self) -> ParseConfig {
        ParseConfig {
            allow_declaration_syntax: self.parse_options.allow_declaration_syntax,
            capture_comments: self.parse_options.capture_comments,
            parse_fragment: self.parse_options.parse_fragment,
            store_cst_data: self.parse_options.store_cst_data,
            no_error_limit: self.parse_options.no_error_limit,
            syntax: self.syntax_flags,
        }
    }

    /// Returns defaults suitable for code that will run inside `ruau-vm`.
    #[must_use]
    pub fn for_vm_execution() -> Self {
        Self {
            clear_dead_stack_slots: true,
            preserve_fenv_semantics: true,
            ..Self::default()
        }
    }

    /// Returns a bytecode-visible fast-flag value with upstream fallback
    /// semantics for flags that are not present in fixture sidecars.
    #[must_use]
    pub fn fast_flag(&self, name: &str) -> bool {
        self.fast_flags
            .iter()
            .rev()
            .find(|flag| flag.name == name)
            .map_or_else(|| default_fast_flag(name), |flag| flag.value)
    }

    /// Returns a bytecode-visible fast-int value with upstream fallback
    /// semantics for ints that are not present in fixture sidecars.
    #[must_use]
    pub fn fast_int(&self, name: &str) -> i32 {
        self.fast_ints
            .iter()
            .rev()
            .find(|fast_int| fast_int.name == name)
            .map_or_else(|| default_fast_int(name), |fast_int| fast_int.value)
    }

    pub(crate) fn bytecode_version(&self) -> u8 {
        if self.fast_flag("LuauEmitCallFeedback") {
            11
        } else if self.fast_flag("DebugLuauUserDefinedClasses") {
            10
        } else if self.fast_flag("LuauCompileUdataDirect") {
            9
        } else if self.fast_flag("LuauIntegerType") || self.fast_flag("LuauIntegerType2") {
            8
        } else if self.coverage_level > 0 || self.fast_flag("LuauCompileDuptableConstantPack2") {
            7
        } else {
            DEFAULT_VERSION
        }
    }
}

impl Default for UpstreamCompilerOptions {
    fn default() -> Self {
        Self {
            optimization_level: 1,
            debug_level: 1,
            type_info_level: 0,
            coverage_level: 0,
            syntax_flags: SyntaxFlags::default(),
            parse_options: UpstreamParseOptions::default(),
            fast_flags: Vec::new(),
            fast_ints: Vec::new(),
            vector_lib: None,
            vector_ctor: None,
            vector_type: None,
            mutable_globals: Vec::new(),
            userdata_types: Vec::new(),
            disabled_builtins: Vec::new(),
            known_members: Vec::new(),
            clear_dead_stack_slots: false,
            preserve_fenv_semantics: false,
        }
    }
}

/// Parser options mirroring upstream `parseOptions` fixture sidecars.
///
/// Kept separate from [`ParseConfig`] so sidecar JSON keeps its exact shape:
/// upstream sidecars carry `parseOptions` and `syntaxFlags` as sibling
/// objects. Combine both via [`UpstreamCompilerOptions::parse_config`].
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct UpstreamParseOptions {
    /// Enables declaration syntax.
    pub allow_declaration_syntax: bool,
    /// Captures comments in parse results.
    pub capture_comments: bool,
    /// Parses a fragment instead of a whole chunk.
    pub parse_fragment: bool,
    /// Stores CST data.
    pub store_cst_data: bool,
    /// Disables upstream's parse-error limit.
    pub no_error_limit: bool,
}

/// Fast-flag override.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FastFlag {
    /// Flag name.
    pub name: String,
    /// Flag value.
    pub value: bool,
}

/// Fast-int override.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FastInt {
    /// Fast-int name.
    pub name: String,
    /// Fast-int value.
    pub value: i32,
}

/// Known library-member constant.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownMember {
    /// Library name.
    pub library: String,
    /// Member name.
    pub member: String,
    /// Constant value.
    pub value: KnownMemberValue,
}

/// Reified known-member constant values.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum KnownMemberValue {
    /// Nil.
    Nil,
    /// Boolean.
    Boolean {
        /// Value.
        value: bool,
    },
    /// Number.
    Number {
        /// Value.
        value: f64,
    },
    /// 64-bit integer.
    Integer {
        /// Value.
        value: i64,
    },
    /// Vector.
    Vector {
        /// Components.
        value: [f32; 4],
    },
    /// String.
    String {
        /// Value.
        value: String,
    },
}

/// Applies source hot comments and attributes to input compiler options.
#[must_use]
pub fn effective_compile_options(
    source: &str,
    options: &UpstreamCompilerOptions,
) -> UpstreamCompilerOptions {
    let mut effective = options.clone();
    apply_leading_hot_comments(source.lines(), &mut effective);
    if source.contains("@native") {
        effective.optimization_level = 2;
        effective.type_info_level = 1;
    }
    effective
}

/// Applies source directives that the bytecode compiler itself observes.
#[must_use]
pub fn source_compile_options(
    source: &str,
    options: &UpstreamCompilerOptions,
) -> UpstreamCompilerOptions {
    let mut effective = effective_compile_options(source, options);
    apply_leading_hot_comments(
        source.lines().skip_while(|line| line.trim().is_empty()),
        &mut effective,
    );
    effective
}

fn apply_leading_hot_comments<'a>(
    lines: impl Iterator<Item = &'a str>,
    options: &mut UpstreamCompilerOptions,
) {
    for line in lines.take_while(|line| line.trim_start().starts_with("--")) {
        apply_hot_comment(line, options);
    }
}

fn apply_hot_comment(line: &str, options: &mut UpstreamCompilerOptions) {
    let trimmed = line.trim_start();
    if let Some(value) = trimmed.strip_prefix("--!optimize") {
        let level = value
            .trim()
            .parse::<u8>()
            .map_or(options.optimization_level, |level| level.min(2));
        options.optimization_level = level;
    }
    if trimmed.starts_with("--!native") {
        options.optimization_level = 2;
        options.type_info_level = 1;
    }
}

fn default_fast_flag(_name: &str) -> bool {
    false
}

fn default_fast_int(name: &str) -> i32 {
    match name {
        "LuauCompileLoopUnrollThreshold" => 25,
        "LuauCompileLoopUnrollThresholdMaxBoost" => 300,
        "LuauCompileInlineThreshold" => 25,
        "LuauCompileInlineThresholdMaxBoost" => 300,
        "LuauCompileInlineDepth" => 5,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FastFlag, FastInt, UpstreamCompilerOptions, effective_compile_options,
        source_compile_options,
    };

    #[test]
    fn default_options_match_upstream_defaults() {
        let options = UpstreamCompilerOptions::default();
        assert_eq!(options.optimization_level, 1);
        assert_eq!(options.debug_level, 1);
        assert_eq!(options.type_info_level, 0);
        assert_eq!(options.coverage_level, 0);
    }

    #[test]
    fn fast_flags_default_false_and_sidecars_override() {
        let options = UpstreamCompilerOptions::default();
        assert!(!options.fast_flag("LuauEmitCallFeedback"));

        let options = UpstreamCompilerOptions {
            fast_flags: vec![FastFlag {
                name: "LuauEmitCallFeedback".to_owned(),
                value: true,
            }],
            ..Default::default()
        };
        assert!(options.fast_flag("LuauEmitCallFeedback"));
    }

    #[test]
    fn fast_ints_use_upstream_defaults_and_sidecars_override() {
        let options = UpstreamCompilerOptions::default();
        assert_eq!(options.fast_int("LuauCompileInlineThreshold"), 25);
        assert_eq!(options.fast_int("LuauCompileInlineThresholdMaxBoost"), 300);
        assert_eq!(options.fast_int("LuauCompileInlineDepth"), 5);
        assert_eq!(options.fast_int("UnknownFastInt"), 0);

        let options = UpstreamCompilerOptions {
            fast_ints: vec![FastInt {
                name: "LuauCompileInlineThreshold".to_owned(),
                value: 10,
            }],
            ..Default::default()
        };
        assert_eq!(options.fast_int("LuauCompileInlineThreshold"), 10);
    }

    #[test]
    fn bytecode_version_uses_fast_flag_helpers() {
        let options = UpstreamCompilerOptions::default();
        assert_eq!(options.bytecode_version(), 7);

        let options = UpstreamCompilerOptions {
            fast_flags: vec![FastFlag {
                name: "LuauEmitCallFeedback".to_owned(),
                value: true,
            }],
            ..Default::default()
        };
        assert_eq!(options.bytecode_version(), 11);

        let options = UpstreamCompilerOptions {
            fast_flags: vec![FastFlag {
                name: "LuauIntegerType2".to_owned(),
                value: true,
            }],
            ..Default::default()
        };
        assert_eq!(options.bytecode_version(), 8);
    }

    #[test]
    fn source_hot_comments_override_options() {
        let options = effective_compile_options(
            "--!optimize 2\nreturn 1",
            &UpstreamCompilerOptions::default(),
        );
        assert_eq!(options.optimization_level, 2);

        let options = source_compile_options(
            "\n--!optimize 2\nreturn 1",
            &UpstreamCompilerOptions::default(),
        );
        assert_eq!(options.optimization_level, 2);
    }
}
