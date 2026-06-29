//! Minimal AST-to-bytecode compiler entry points.

use std::{
    fmt,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ruau_ast::{
    Location,
    parse::{parse_file_bytes_with, parse_file_with},
    syntax::Stat,
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
    CompileOptions, CompilerOptions, FastFlag, FastInt, KnownMember, KnownMemberValue,
    effective_compile_options, source_compile_options,
};

pub const CONSTANT_STRING_FOLD_LIMIT: usize = 4096;

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
pub fn compile_source(
    source: &str,
    options: &CompileOptions,
) -> Result<BytecodeChunk, CompileError> {
    compile_source_with_cancel(source, options, None)
}

/// Cancellation-aware form of [`compile_source`].
///
/// # Errors
/// As [`compile_source`], plus [`CompileErrorKind::Cancelled`] when the flag is
/// set at a cooperative compiler safepoint.
pub fn compile_source_with_cancel(
    source: &str,
    options: &CompileOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    let options = options.to_compiler_options();
    compile_source_with_compiler_options_and_cancel(source, &options, cancel)
}

/// Compiles source with the repository's upstream-fixture option shape.
#[doc(hidden)]
pub fn compile_source_with_compiler_options(
    source: &str,
    options: &CompilerOptions,
) -> Result<BytecodeChunk, CompileError> {
    compile_source_with_compiler_options_and_cancel(source, options, None)
}

/// Cancellation-aware form of [`compile_source_with_compiler_options`].
#[doc(hidden)]
pub fn compile_source_with_compiler_options_and_cancel(
    source: &str,
    options: &CompilerOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    chunkify_parse_error(compile_source_strict_with_compiler_options_and_cancel(
        source, options, cancel,
    ))
}

/// Compiles arbitrary source bytes into a decoded bytecode chunk.
///
/// Mirrors [`compile_source`] for inputs that are not valid UTF-8: string
/// literals and byte-column locations preserve the original bytes, while a
/// same-length surrogate string drives lexing and leading hot-comment scanning.
pub fn compile_source_bytes(
    source: &[u8],
    options: &CompileOptions,
) -> Result<BytecodeChunk, CompileError> {
    compile_source_bytes_with_cancel(source, options, None)
}

/// Cancellation-aware form of [`compile_source_bytes`].
///
/// # Errors
/// As [`compile_source_bytes`], plus [`CompileErrorKind::Cancelled`] when the
/// flag is set at a cooperative compiler safepoint.
pub fn compile_source_bytes_with_cancel(
    source: &[u8],
    options: &CompileOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    let options = options.to_compiler_options();
    compile_source_bytes_with_compiler_options_and_cancel(source, &options, cancel)
}

/// Compiles arbitrary source bytes with the repository's upstream-fixture option shape.
#[doc(hidden)]
pub fn compile_source_bytes_with_compiler_options(
    source: &[u8],
    options: &CompilerOptions,
) -> Result<BytecodeChunk, CompileError> {
    compile_source_bytes_with_compiler_options_and_cancel(source, options, None)
}

/// Cancellation-aware form of [`compile_source_bytes_with_compiler_options`].
#[doc(hidden)]
pub fn compile_source_bytes_with_compiler_options_and_cancel(
    source: &[u8],
    options: &CompilerOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    chunkify_parse_error(
        compile_source_bytes_strict_with_compiler_options_and_cancel(source, options, cancel),
    )
}

/// Compiles source, returning malformed programs as `Err`.
///
/// # Errors
/// Returns a parse error for malformed source, or an internal error for
/// compiler limits.
pub fn compile_source_strict(
    source: &str,
    options: &CompileOptions,
) -> Result<BytecodeChunk, CompileError> {
    compile_source_strict_with_cancel(source, options, None)
}

/// Cancellation-aware form of [`compile_source_strict`].
///
/// # Errors
/// As [`compile_source_strict`], plus [`CompileErrorKind::Cancelled`] when the
/// flag is set at a cooperative compiler safepoint.
pub fn compile_source_strict_with_cancel(
    source: &str,
    options: &CompileOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    let options = options.to_compiler_options();
    compile_source_strict_with_compiler_options_and_cancel(source, &options, cancel)
}

/// Compiles source with the repository's upstream-fixture option shape,
/// returning malformed programs as `Err`.
#[doc(hidden)]
pub fn compile_source_strict_with_compiler_options(
    source: &str,
    options: &CompilerOptions,
) -> Result<BytecodeChunk, CompileError> {
    compile_source_strict_with_compiler_options_and_cancel(source, options, None)
}

/// Cancellation-aware form of [`compile_source_strict_with_compiler_options`].
#[doc(hidden)]
pub fn compile_source_strict_with_compiler_options_and_cancel(
    source: &str,
    options: &CompilerOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    let effective = source_compile_options(source, options);
    check_compile_cancelled(cancel.as_ref())?;
    let parse = parse_file_with(source, effective.parse_options, effective.syntax_flags);
    if let Some(error) = parse.errors.first() {
        // Upstream reports only the *first* parse error (`Compiler.cpp`);
        // recovery may queue more. The error keeps the parser's structured
        // location rather than round-tripping through rendered text.
        return Err(CompileError::parse(error.message.clone(), error.location));
    }
    let Some(root) = parse.root else {
        return Err(CompileError::new("parser did not produce a root block"));
    };
    compile_ast_with_implicit_return_delta(
        root,
        &effective,
        u8::from(source.ends_with('\n')),
        cancel,
    )
}

/// Byte-preserving form of [`compile_source_strict`].
///
/// # Errors
/// As [`compile_source_strict`].
pub fn compile_source_bytes_strict(
    source: &[u8],
    options: &CompileOptions,
) -> Result<BytecodeChunk, CompileError> {
    compile_source_bytes_strict_with_cancel(source, options, None)
}

/// Cancellation-aware form of [`compile_source_bytes_strict`].
///
/// # Errors
/// As [`compile_source_bytes_strict`], plus [`CompileErrorKind::Cancelled`]
/// when the flag is set at a cooperative compiler safepoint.
pub fn compile_source_bytes_strict_with_cancel(
    source: &[u8],
    options: &CompileOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    let options = options.to_compiler_options();
    compile_source_bytes_strict_with_compiler_options_and_cancel(source, &options, cancel)
}

/// Byte-preserving form of [`compile_source_strict_with_compiler_options`].
#[doc(hidden)]
pub fn compile_source_bytes_strict_with_compiler_options(
    source: &[u8],
    options: &CompilerOptions,
) -> Result<BytecodeChunk, CompileError> {
    compile_source_bytes_strict_with_compiler_options_and_cancel(source, options, None)
}

/// Cancellation-aware form of [`compile_source_bytes_strict_with_compiler_options`].
#[doc(hidden)]
pub fn compile_source_bytes_strict_with_compiler_options_and_cancel(
    source: &[u8],
    options: &CompilerOptions,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    // Hot comments and `--!` directives are ASCII at the file head, so a lossy
    // view is sufficient to derive the effective options; the byte-precise
    // content flows through `parse_file_bytes_with`.
    let lossy = String::from_utf8_lossy(source);
    let effective = source_compile_options(&lossy, options);
    check_compile_cancelled(cancel.as_ref())?;
    let parse = parse_file_bytes_with(source, effective.parse_options, effective.syntax_flags);
    if let Some(error) = parse.errors.first() {
        return Err(CompileError::parse(error.message.clone(), error.location));
    }
    let Some(root) = parse.root else {
        return Err(CompileError::new("parser did not produce a root block"));
    };
    compile_ast_with_implicit_return_delta(
        root,
        &effective,
        u8::from(source.ends_with(b"\n")),
        cancel,
    )
}

/// Folds a parse-shaped failure into the wire-compatible error chunk: upstream
/// encodes it as ":<line>: <message>" (`Compiler.cpp`); the chunk-id prefix is
/// added by the caller (`luau_load`/`loadstring`), which knows the chunk name.
/// Internal compiler-limit errors stay on the `Err` channel.
fn chunkify_parse_error(
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
    let position = ruau_ast::Position {
        line: line.saturating_sub(1),
        column: 0,
    };
    Location {
        begin: position,
        end: position,
    }
}

fn compile_ast_with_implicit_return_delta(
    root: Stat,
    options: &CompilerOptions,
    implicit_return_line_delta: u8,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BytecodeChunk, CompileError> {
    check_compile_cancelled(cancel.as_ref())?;
    if let Some((line, message)) = repeat_continue_condition_error(&root) {
        return Err(CompileError::parse(message, line_location(line)));
    }

    // Takes the root by value and shares it (`Rc`) between the context and the
    // compile pass — the compile pipeline never deep-copies the module AST.
    let root = Rc::new(root);
    let context = CompileContext::with_cancel(Rc::clone(&root), options, cancel);
    context.check_cancelled()?;
    let mut compiler = FunctionCompiler::new(context, implicit_return_line_delta);
    compiler.compile_registered_functions()?;
    compiler.compile_root(&root)?;
    Ok(compiler.finish())
}

fn check_compile_cancelled(cancel: Option<&Arc<AtomicBool>>) -> Result<(), CompileError> {
    if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
        return Err(CompileError::cancelled());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
