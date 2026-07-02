//! Recursive-descent parser implementation.

// Split `impl Parser` blocks live in sibling modules; one block per parsing area.
#![allow(clippy::multiple_inherent_impl)]

use std::collections::{BTreeSet, VecDeque};

use crate::{
    Position,
    lexer::{Lexeme, Lexer, TokenKind},
    parse::{
        Comment, Error, ErrorKind, HotComment, ParseConfig, ParseNodeResult, ParseResult,
        SyntaxFlags,
    },
    syntax::{LocalRef, Stat, Type},
};

/// The maximum nesting depth the recursive-descent parser accepts before it raises a catchable
/// parse error. This is deliberately lower than upstream's native-stack profile because
/// adversarial source can reach the parser through `loadstring`, and later AST consumers also
/// recurse through the resulting tree.
const PARSER_RECURSION_LIMIT: usize = 96;

/// Upstream's default parse-error limit (`FInt::LuauErrorLimit`). Once this
/// many distinct-location errors accumulate, the parser records a final
/// `ErrorLimit` diagnostic and stops, unless [`ParseConfig::no_error_limit`]
/// is set.
const PARSER_ERROR_LIMIT: usize = 100;

/// Parse-error sink mirroring `Parser::report` in upstream Luau's `Parser.cpp`.
///
/// Two upstream behaviors live here so every error site shares them: an error at
/// the same location as the immediately preceding one is dropped (collapsing the
/// cascade that incomplete input such as `local a = (((b +` produces), and once
/// the error count reaches the limit a final `ErrorLimit` diagnostic is recorded
/// and further errors are ignored.
pub struct Errors {
    errors: Vec<Error>,
    /// The error-count ceiling, or `None` when the limit is disabled
    /// (`ParseConfig::no_error_limit`).
    limit: Option<usize>,
    /// Set once the `ErrorLimit` diagnostic has been recorded.
    limit_reached: bool,
}

impl Errors {
    pub(crate) fn new(limit: Option<usize>) -> Self {
        Self {
            errors: Vec::new(),
            limit,
            limit_reached: false,
        }
    }

    /// Records `error`, applying consecutive-location dedup and the error limit.
    /// Returns the index of the recorded (or deduplicated) diagnostic so callers
    /// can reference it from an error node's message index.
    pub(crate) fn push(&mut self, error: Error) -> usize {
        if let Some(last) = self.errors.last()
            && last.location == error.location
        {
            return self.errors.len() - 1;
        }
        if self.limit_reached {
            return self.errors.len().saturating_sub(1);
        }
        let index = self.errors.len();
        let location = error.location;
        self.errors.push(error);
        if let Some(limit) = self.limit
            && self.errors.len() >= limit
        {
            self.limit_reached = true;
            // The limit error shares the triggering location but is recorded
            // directly so the dedup above never drops it.
            self.errors.push(Error {
                kind: ErrorKind::ErrorLimit,
                message: format!("Reached error limit ({limit})"),
                location,
            });
        }
        index
    }

    fn extend(&mut self, errors: impl IntoIterator<Item = Error>) {
        for error in errors {
            self.push(error);
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.errors.len()
    }

    fn iter(&self) -> std::slice::Iter<'_, Error> {
        self.errors.iter()
    }

    /// Whether the error limit has been reached; the recursive-descent block
    /// loops poll this to stop once the cap is hit.
    pub(crate) fn limit_reached(&self) -> bool {
        self.limit_reached
    }

    pub(crate) fn into_inner(self) -> Vec<Error> {
        self.errors
    }
}

/// Recursive-descent parser state.
pub struct Parser<'source> {
    /// Token source.
    lexer: Lexer<'source>,
    /// Current token.
    current: Lexeme<'source>,
    /// Parser configuration.
    options: ParseConfig,
    /// Parser-visible syntax flags, copied out of the configuration.
    syntax_flags: SyntaxFlags,
    /// Captured comments.
    comments: Vec<Comment>,
    /// Captured hot comments.
    hot_comments: Vec<HotComment>,
    /// Whether hot comments still belong to the file header.
    hot_comment_header: bool,
    /// Parse errors, with consecutive-location dedup and the error limit.
    errors: Errors,
    /// Local references visible to the current parser slice.
    locals: Vec<LocalRef>,
    /// User-defined class names already declared in this module.
    class_names: BTreeSet<String>,
    /// Next parser-assigned local identity.
    next_local_id: u32,
    /// Next parser-assigned expression/type identity.
    next_syntax_id: u32,
    /// Current function nesting depth.
    function_depth: usize,
    /// Minimum local function depth visible from the current type function.
    type_function_depth: usize,
    /// Whether each active function accepts varargs.
    function_varargs: Vec<bool>,
    /// Function depths for active loop bodies.
    loop_function_depths: Vec<usize>,
    /// Current nested statement-block depth.
    block_depth: usize,
    /// Recursion depth across the recursive-descent entry points (expressions and blocks),
    /// bounded by [`PARSER_RECURSION_LIMIT`] so adversarial nested input — now reachable via
    /// `loadstring` — raises a catchable parse error instead of overflowing the native stack.
    recursion_depth: usize,
    /// Whether the current expression may absorb a newline-starting call.
    allow_ambiguous_newline_call: bool,
    /// Original source bytes, used for byte-preserving string token values.
    source_bytes: &'source [u8],
    /// Byte offset where each source line starts.
    line_starts: Vec<usize>,
    /// Extra statements produced by one recovery parse step.
    pending_statements: VecDeque<Stat>,
    /// Type recovery extent consumed for the surrounding statement.
    type_recovery_end: Option<Position>,
    /// Statement end to use when a recovered type location extends through a missing delimiter.
    type_statement_end_override: Option<Position>,
}

mod common;
mod decl;
mod expr;
mod stat;
mod types;

#[cfg(any())]
mod tests;

impl<'source> Parser<'source> {
    /// Creates a parser.
    pub fn new(source: &'source str, config: &ParseConfig) -> Self {
        Self::new_with_original_bytes(source, source.as_bytes(), config)
    }

    /// Creates a parser with a normalized UTF-8 source and original source bytes.
    pub fn new_with_original_bytes(
        source: &'source str,
        source_bytes: &'source [u8],
        config: &ParseConfig,
    ) -> Self {
        debug_assert_eq!(source.len(), source_bytes.len());
        let mut lexer = Lexer::new(source);
        let current = lexer.next_token();
        let error_limit = (!config.no_error_limit).then_some(PARSER_ERROR_LIMIT);
        Self {
            lexer,
            current,
            options: *config,
            syntax_flags: config.syntax,
            comments: Vec::new(),
            hot_comments: Vec::new(),
            hot_comment_header: true,
            errors: Errors::new(error_limit),
            locals: Vec::new(),
            class_names: BTreeSet::new(),
            next_local_id: 0,
            next_syntax_id: 0,
            function_depth: 0,
            type_function_depth: 0,
            function_varargs: vec![true],
            loop_function_depths: Vec::new(),
            block_depth: 0,
            recursion_depth: 0,
            allow_ambiguous_newline_call: false,
            source_bytes,
            line_starts: source_line_starts(source.as_bytes()),
            pending_statements: VecDeque::new(),
            type_recovery_end: None,
            type_statement_end_override: None,
        }
    }

    /// Parses a whole file.
    pub fn parse_file(mut self) -> ParseResult {
        let root = self.parse_block();
        self.skip_comments();
        if self.current.kind != TokenKind::Eof {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: format!("Expected <eof>, got {}", self.current.display()),
                location: self.current.location,
            });
        }
        ParseResult {
            root,
            errors: self.errors.into_inner(),
            comments: self.comments,
            hot_comments: self.hot_comments,
            emit_is_const: self.syntax_flags.luau_const2,
        }
    }

    /// Parses a type annotation entry point.
    pub fn parse_type(mut self) -> ParseNodeResult<Type> {
        let root = self.parse_type_expression();
        if self.current.kind != TokenKind::Eof {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected end of type".to_owned(),
                location: self.current.location,
            });
        }
        ParseNodeResult {
            root,
            errors: self.errors.into_inner(),
            emit_is_const: self.syntax_flags.luau_const2,
        }
    }
}

pub use common::source_line_starts;
#[cfg(any())]
pub use common::{parse_integer_literal, parse_number};
