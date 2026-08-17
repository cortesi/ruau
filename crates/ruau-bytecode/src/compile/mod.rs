//! Minimal AST-to-bytecode compiler entry points.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ruau_syntax::{
    Location, Stat,
    parse::{ParsedModule, parse_module_bytes_with_config, parse_module_with_config},
    visit::{Visitor, WalkControl, walk_stat},
};

use crate::BytecodeChunk;

mod analysis;
mod builtin_folding;
mod context;
mod function_compiler;
mod helpers;
mod inline_cost;
mod options;

use analysis::ConstantValue;
use context::CompileContext;
#[cfg(test)]
use function_compiler::constant_ad_operand;
use function_compiler::{
    BreakBranchKind, FunctionCompiler, LoopControlBranchKind, LoopUnrollPlan, TypeAliasInfo,
};
use helpers::*;
pub use options::{
    CompileOptions, FastFlag, FastInt, KnownMember, KnownMemberValue, UpstreamCompilerOptions,
    UpstreamParseOptions, effective_compile_options, source_compile_options,
};

pub const CONSTANT_STRING_FOLD_LIMIT: usize = 4096;
const EXPORT_TOP_LEVEL_MESSAGE: &str = "'export' may only be applied to top-level statements";
const EXPORT_RETURN_CONFLICT_MESSAGE: &str =
    "Exporting values is not compatible with top-level return (export/return conflict)";

/// Compiler failure.
///
/// Failures carry structured data, not just display text: [`kind`](Self::kind)
/// is the stable category, [`location`](Self::location) the source range when
/// one is known, and [`message`](Self::message) the text without any location
/// prefix. Compilation is chunk-name-agnostic — a chunk name is bound when a
/// compiled chunk is loaded into a VM — so a compile error carries no chunk
/// name; an embedder prefixes its own.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompileError {
    kind: CompileErrorKind,
    location: Option<Location>,
    message: String,
}

/// Stable category of a [`CompileError`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CompileErrorKind {
    /// The source did not parse (or used a rejected construct); `location`
    /// carries the failing range when known.
    Parse,
    /// Compilation was cancelled at a cooperative safepoint.
    Cancelled,
    /// The compiler hit an internal limit or invariant.
    Internal,
}

impl CompileError {
    /// An [`CompileErrorKind::Internal`] error with no location: a compiler
    /// limit or invariant, not a source-text fault. Public so layers above the
    /// compiler (artifact validation of a freshly compiled chunk) can report
    /// their own internal failures in the compiler's error vocabulary.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: CompileErrorKind::Internal,
            location: None,
            message: message.into(),
        }
    }

    /// A cancellation error with no source location.
    pub fn cancelled() -> Self {
        Self {
            kind: CompileErrorKind::Cancelled,
            location: None,
            message: "compilation cancelled".to_owned(),
        }
    }

    fn parse(message: impl Into<String>, location: Location) -> Self {
        Self {
            kind: CompileErrorKind::Parse,
            location: Some(location),
            message: message.into(),
        }
    }

    /// Stable error category.
    #[must_use]
    pub fn kind(&self) -> CompileErrorKind {
        self.kind
    }

    /// Source range associated with the failure, when known.
    ///
    /// Parse failures carry the failing token's full range with zero-based
    /// [`Location`] lines and columns; [`Display`](fmt::Display) renders the
    /// 1-based `begin` position as `line:column: message`. Internal
    /// compiler-limit failures track no source position and return `None`.
    #[must_use]
    pub fn location(&self) -> Option<Location> {
        self.location
    }

    /// Human-readable failure text (without location prefix).
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(location) = self.location {
            write!(
                f,
                "{}:{}: {}",
                location.begin.line + 1,
                location.begin.column + 1,
                self.message
            )
        } else {
            f.write_str(&self.message)
        }
    }
}

impl std::error::Error for CompileError {}

/// Compiles source into a decoded bytecode chunk.
///
/// A malformed program is reported as the wire-compatible
/// `Ok(BytecodeChunk::Error { .. })` chunk, with its `":<line>: <message>"`
/// payload rendered from the structured [`CompileErrorKind::Parse`] failure
/// the strict channel reports.
///
/// # Errors
/// Returns an internal error for compiler limits, or
/// [`CompileErrorKind::Cancelled`] when the `cancel` flag is set at a
/// cooperative compiler safepoint.
pub fn compile_source(
    source: &str,
    options: &CompileOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    chunkify_parse_error(compile_source_strict(source, options, cancel))
}

/// Compiles arbitrary source bytes into a decoded bytecode chunk.
///
/// Mirrors [`compile_source`] for inputs that are not valid UTF-8: string
/// literals and byte-column locations preserve the original bytes, while a
/// same-length surrogate string drives lexing and leading hot-comment scanning.
///
/// # Errors
/// As [`compile_source`].
pub fn compile_source_bytes(
    source: &[u8],
    options: &CompileOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    chunkify_parse_error(compile_source_bytes_strict(source, options, cancel))
}

/// Compiles source, returning malformed programs as `Err`.
///
/// # Errors
/// Returns a parse error for malformed source, an internal error for compiler
/// limits, or [`CompileErrorKind::Cancelled`] when the `cancel` flag is set at
/// a cooperative compiler safepoint.
pub fn compile_source_strict(
    source: &str,
    options: &CompileOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    let options = options.to_upstream_options();
    compile_source_strict_with_upstream_options(source, &options, cancel)
}

/// Compiles source with the repository's upstream-fixture option shape,
/// returning malformed programs as `Err`.
///
/// Wrap the result in [`chunkify_parse_error`] for the lenient error-chunk
/// behavior of [`compile_source`].
#[doc(hidden)]
pub fn compile_source_strict_with_upstream_options(
    source: &str,
    options: &UpstreamCompilerOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    let effective = source_compile_options(source, options);
    check_compile_cancelled(cancel.as_ref())?;
    let parsed = parse_module_with_config(source, &effective.parse_config());
    compile_parsed_module_with_effective(&parsed, &effective, cancel)
}

/// Byte-preserving form of [`compile_source_strict`].
///
/// # Errors
/// As [`compile_source_strict`].
pub fn compile_source_bytes_strict(
    source: &[u8],
    options: &CompileOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    let options = options.to_upstream_options();
    compile_source_bytes_strict_with_upstream_options(source, &options, cancel)
}

/// Byte-preserving form of [`compile_source_strict_with_upstream_options`].
#[doc(hidden)]
pub fn compile_source_bytes_strict_with_upstream_options(
    source: &[u8],
    options: &UpstreamCompilerOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    // Hot comments and `--!` directives are ASCII at the file head, so a lossy
    // view is sufficient to derive the effective options; the byte-precise
    // content flows through `parse_bytes_with_config`.
    let lossy = String::from_utf8_lossy(source);
    let effective = source_compile_options(&lossy, options);
    check_compile_cancelled(cancel.as_ref())?;
    let parsed = parse_module_bytes_with_config(source, &effective.parse_config());
    compile_parsed_module_with_effective(&parsed, &effective, cancel)
}

/// Compiles an existing shared parse product with the repository's
/// upstream-fixture option shape.
///
/// Comment and CST-capture differences are compatible because they do not
/// change the AST consumed by the compiler. Other parse-option differences are
/// rejected.
///
/// # Errors
///
/// Returns a parse error for malformed source, an internal error when the
/// product's AST-affecting parse options disagree with the effective compiler
/// options, or [`CompileErrorKind::Cancelled`] at a cancellation safepoint.
#[doc(hidden)]
pub fn compile_parsed_module_strict_with_upstream_options(
    parsed: &ParsedModule,
    options: &UpstreamCompilerOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    let lossy = String::from_utf8_lossy(parsed.source());
    let effective = source_compile_options(&lossy, options);
    check_compile_cancelled(cancel.as_ref())?;
    compile_parsed_module_with_effective(parsed, &effective, cancel)
}

fn compile_parsed_module_with_effective(
    parsed: &ParsedModule,
    effective: &UpstreamCompilerOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    if !parsed
        .config()
        .ast_compatible_with(effective.parse_config())
    {
        return Err(CompileError::new(
            "parsed module options do not match effective compiler syntax options",
        ));
    }
    if let Some(error) = parsed.errors().first() {
        // Upstream reports only the *first* parse error (`Compiler.cpp`);
        // recovery may queue more. The error keeps the parser's structured
        // location rather than round-tripping through rendered text.
        return Err(CompileError::parse(error.message.clone(), error.location));
    }
    let mut effective = effective.clone();
    apply_native_attribute_options(parsed.root(), &mut effective);
    compile_ast_with_implicit_return_delta(
        parsed.root(),
        &effective,
        u8::from(parsed.source().ends_with(b"\n")),
        cancel,
    )
}

fn apply_native_attribute_options(root: &Stat, options: &mut UpstreamCompilerOptions) {
    struct NativeAttributeVisitor {
        found: bool,
    }

    impl<'ast> Visitor<'ast> for NativeAttributeVisitor {
        fn visit_expr(&mut self, expr: &'ast ruau_syntax::Expr) -> WalkControl {
            if let ruau_syntax::Expr::Function { attributes, .. } = expr
                && attributes
                    .iter()
                    .any(|attribute| attribute.name.as_str() == "native")
            {
                self.found = true;
                return WalkControl::SkipChildren;
            }
            WalkControl::Continue
        }
    }

    let mut visitor = NativeAttributeVisitor { found: false };
    walk_stat(root, &mut visitor);
    if visitor.found {
        options.optimization_level = 2;
        options.type_info_level = 1;
    }
}

/// Folds a parse-shaped failure into the wire-compatible error chunk: upstream
/// encodes it as ":<line>: <message>" (`Compiler.cpp`); the chunk-id prefix is
/// added by the caller (`luau_load`/`loadstring`), which knows the chunk name.
/// Internal compiler-limit errors stay on the `Err` channel.
///
/// Public (but hidden) so fixture tooling and the VM's `loadstring` path can
/// combine it with the `*_with_upstream_options` strict entry points to get
/// the lenient behavior of [`compile_source`]/[`compile_source_bytes`].
#[doc(hidden)]
pub fn chunkify_parse_error(
    result: Result<BytecodeChunk, CompileError>,
) -> Result<BytecodeChunk, CompileError> {
    match result {
        Err(error) if error.kind() == CompileErrorKind::Parse => {
            let message = match error.location() {
                Some(location) => {
                    format!(":{}: {}", location.begin.line + 1, error.message())
                }
                None => error.message().to_owned(),
            };
            Ok(BytecodeChunk::Error {
                message: message.into_bytes(),
            })
        }
        other => other,
    }
}

/// The single-position [`Location`] of a 1-based source line whose column is
/// not tracked, for compile-stage failures that know only their line.
fn line_location(line: u32) -> Location {
    let position = ruau_syntax::Position {
        line: line.saturating_sub(1),
        column: 0,
    };
    Location {
        begin: position,
        end: position,
    }
}

fn compile_ast_with_implicit_return_delta(
    root: &Arc<Stat>,
    options: &UpstreamCompilerOptions,
    implicit_return_line_delta: u8,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    check_compile_cancelled(cancel.as_ref())?;
    if let Some((line, message)) = repeat_continue_condition_error(root) {
        return Err(CompileError::parse(message, line_location(line)));
    }
    if let Some((location, message)) = export_value_ast_error(root) {
        return Err(CompileError::parse(message, location));
    }

    let context = CompileContext::with_cancel(Arc::clone(root), options, cancel);
    context.check_cancelled()?;
    let mut compiler = FunctionCompiler::new(context, implicit_return_line_delta);
    compiler.compile_registered_functions_for_root(root)?;
    compiler.compile_root(root)?;
    Ok(compiler.finish())
}

fn check_compile_cancelled(cancel: Option<&Arc<AtomicBool>>) -> Result<(), CompileError> {
    if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
        return Err(CompileError::cancelled());
    }
    Ok(())
}

fn export_value_ast_error(root: &Stat) -> Option<(Location, &'static str)> {
    let Stat::Block { body, .. } = root else {
        return exported_value_location(root).map(|location| (location, EXPORT_TOP_LEVEL_MESSAGE));
    };

    let mut has_export = false;
    let mut has_return = false;
    for stat in body {
        if let Some(location) = nested_exported_value_location(stat) {
            return Some((location, EXPORT_TOP_LEVEL_MESSAGE));
        }
        if let Some(location) = exported_value_location(stat) {
            if has_return {
                return Some((location, EXPORT_RETURN_CONFLICT_MESSAGE));
            }
            has_export = true;
        }
        if let Stat::Return {
            location: Some(location),
            ..
        } = stat
        {
            if has_export {
                return Some((*location, EXPORT_RETURN_CONFLICT_MESSAGE));
            }
            has_return = true;
        }
    }

    None
}

fn nested_exported_value_location(stat: &Stat) -> Option<Location> {
    struct NestedExportFinder {
        skip_root: bool,
        location: Option<Location>,
    }

    impl<'ast> Visitor<'ast> for NestedExportFinder {
        fn visit_stat(&mut self, stat: &'ast Stat) -> WalkControl {
            if self.skip_root {
                self.skip_root = false;
                return WalkControl::Continue;
            }
            if self.location.is_none() {
                self.location = exported_value_location(stat);
            }
            if self.location.is_some() {
                WalkControl::SkipChildren
            } else {
                WalkControl::Continue
            }
        }
    }

    let mut finder = NestedExportFinder {
        skip_root: true,
        location: None,
    };
    walk_stat(stat, &mut finder);
    finder.location
}

fn exported_value_location(stat: &Stat) -> Option<Location> {
    match stat {
        Stat::Local {
            exported: true,
            location,
            ..
        }
        | Stat::LocalFunction {
            exported: true,
            location,
            ..
        }
        | Stat::Class {
            exported: true,
            location,
            ..
        } => *location,
        _ => None,
    }
}

#[cfg(test)]
mod tests;
