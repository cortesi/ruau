use ruau_ast::parse::{Options, SyntaxFlags};
use serde::{Deserialize, Serialize};

use crate::builder::DEFAULT_VERSION;

/// Bytecode-visible compiler options.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct CompileOptions {
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
    pub parse_options: Options,
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

impl CompileOptions {
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
        } else if self.fast_flag("LuauIntegerType") {
            8
        } else if self.coverage_level > 0 || self.fast_flag("LuauCompileDuptableConstantPack2") {
            7
        } else {
            DEFAULT_VERSION
        }
    }
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            optimization_level: 1,
            debug_level: 1,
            type_info_level: 0,
            coverage_level: 0,
            syntax_flags: SyntaxFlags::default(),
            parse_options: Options::default(),
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
pub fn effective_compile_options(source: &str, options: &CompileOptions) -> CompileOptions {
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
pub fn source_compile_options(source: &str, options: &CompileOptions) -> CompileOptions {
    let mut effective = effective_compile_options(source, options);
    apply_leading_hot_comments(
        source.lines().skip_while(|line| line.trim().is_empty()),
        &mut effective,
    );
    effective
}

fn apply_leading_hot_comments<'a>(
    lines: impl Iterator<Item = &'a str>,
    options: &mut CompileOptions,
) {
    for line in lines.take_while(|line| line.trim_start().starts_with("--")) {
        apply_hot_comment(line, options);
    }
}

fn apply_hot_comment(line: &str, options: &mut CompileOptions) {
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
        CompileOptions, FastFlag, FastInt, effective_compile_options, source_compile_options,
    };

    #[test]
    fn default_options_match_upstream_defaults() {
        let options = CompileOptions::default();
        assert_eq!(options.optimization_level, 1);
        assert_eq!(options.debug_level, 1);
        assert_eq!(options.type_info_level, 0);
        assert_eq!(options.coverage_level, 0);
    }

    #[test]
    fn fast_flags_default_false_and_sidecars_override() {
        let options = CompileOptions::default();
        assert!(!options.fast_flag("LuauEmitCallFeedback"));

        let options = CompileOptions {
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
        let options = CompileOptions::default();
        assert_eq!(options.fast_int("LuauCompileInlineThreshold"), 25);
        assert_eq!(options.fast_int("LuauCompileInlineThresholdMaxBoost"), 300);
        assert_eq!(options.fast_int("LuauCompileInlineDepth"), 5);
        assert_eq!(options.fast_int("UnknownFastInt"), 0);

        let options = CompileOptions {
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
        let options = CompileOptions::default();
        assert_eq!(options.bytecode_version(), 6);

        let options = CompileOptions {
            fast_flags: vec![FastFlag {
                name: "LuauEmitCallFeedback".to_owned(),
                value: true,
            }],
            ..Default::default()
        };
        assert_eq!(options.bytecode_version(), 11);
    }

    #[test]
    fn source_hot_comments_override_options() {
        let options =
            effective_compile_options("--!optimize 2\nreturn 1", &CompileOptions::default());
        assert_eq!(options.optimization_level, 2);

        let options =
            source_compile_options("\n--!optimize 2\nreturn 1", &CompileOptions::default());
        assert_eq!(options.optimization_level, 2);
    }
}
