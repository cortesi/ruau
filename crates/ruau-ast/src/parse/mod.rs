//! Public parser-facing API.

use std::str;

use crate::{
    Location, Position,
    json::{JsonDocument, JsonNode, renumber_adjacent_fields},
    lexer::{Lexeme, TokenKind},
    parser::Parser,
    syntax::{Stat, Type},
};

/// Parser options that mirror upstream `Luau::Options`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct Options {
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

/// Parser-visible syntax flags modeled from upstream fast flags.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct SyntaxFlags {
    /// Enables CST expression groups.
    pub luau_cst_expr_group: bool,
    /// Enables CST type groups.
    pub luau_cst_type_group: bool,
    /// Enables const syntax.
    pub luau_const2: bool,
    /// Enables integer type syntax.
    pub luau_integer_type: bool,
    /// Enables type functions.
    pub luau_type_functions: bool,
    /// Enables extern read/write attributes.
    pub luau_extern_read_write_attributes: bool,
    /// Enables user-defined class syntax.
    pub debug_luau_user_defined_classes: bool,
    /// Enables the debug-only `@debugnoinline` attribute.
    pub debug_luau_no_inline: bool,
    /// Allows global declarations to be called class.
    pub luau_allow_global_declaration_to_be_called_class: bool,
    /// Keeps desugared array type references empty.
    pub desugared_array_type_reference_is_empty: bool,
}

impl SyntaxFlags {
    /// Sets the flag named by its upstream fast-flag spelling, ignoring
    /// unknown names. The one mapping between upstream flag names and
    /// parser-visible syntax flags — fixture tooling reads flag sidecars
    /// through this instead of restating the table.
    pub fn set_by_upstream_name(&mut self, name: &str, value: bool) {
        match name {
            "LuauCstExprGroup" => self.luau_cst_expr_group = value,
            "LuauCstTypeGroup" => self.luau_cst_type_group = value,
            "LuauConst2" => self.luau_const2 = value,
            "LuauIntegerType" => self.luau_integer_type = value,
            "LuauTypeFunctions" => self.luau_type_functions = value,
            "LuauExternReadWriteAttributes" => self.luau_extern_read_write_attributes = value,
            "DebugLuauUserDefinedClasses" => self.debug_luau_user_defined_classes = value,
            "DebugLuauNoInline" => self.debug_luau_no_inline = value,
            "LuauAllowGlobalDeclarationToBeCalledClass" => {
                self.luau_allow_global_declaration_to_be_called_class = value;
            }
            "DesugaredArrayTypeReferenceIsEmpty" => {
                self.desugared_array_type_reference_is_empty = value;
            }
            _ => {}
        }
    }

    /// Returns the broad Luau syntax posture used by upstream `luau-ast`.
    #[must_use]
    pub const fn all_luau() -> Self {
        Self {
            luau_cst_expr_group: true,
            luau_cst_type_group: true,
            luau_const2: true,
            luau_integer_type: true,
            luau_type_functions: true,
            luau_extern_read_write_attributes: true,
            debug_luau_user_defined_classes: true,
            debug_luau_no_inline: false,
            luau_allow_global_declaration_to_be_called_class: true,
            desugared_array_type_reference_is_empty: true,
        }
    }

    /// Returns the syntax posture used by upstream `luau-ast`.
    #[must_use]
    pub const fn luau_ast_cli() -> Self {
        Self {
            debug_luau_user_defined_classes: false,
            ..Self::all_luau()
        }
    }
}

/// A parse result for whole-file parsing.
#[derive(Clone, Debug, PartialEq)]
pub struct ParseResult {
    /// Parsed root block, when one was produced.
    pub root: Option<Stat>,
    /// Parse errors.
    pub errors: Vec<Error>,
    /// Captured comments.
    pub comments: Vec<Comment>,
    /// Captured hot comments.
    pub hot_comments: Vec<HotComment>,
}

impl ParseResult {
    /// Returns whether this parse produced no errors.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// Returns whether a position falls within a captured comment.
    #[must_use]
    pub fn is_within_comment(&self, position: Position) -> bool {
        self.comments
            .iter()
            .any(|comment| comment.location.contains(position))
    }

    /// Converts a successful root block into an AST JSON document.
    #[must_use]
    pub fn into_json_document(self) -> Option<JsonDocument> {
        let root = self.root?;
        let mut document = JsonDocument {
            root: root.into_json(),
            comment_locations: self
                .comments
                .into_iter()
                .map(Comment::into_json_node)
                .collect(),
        };
        renumber_adjacent_fields(&mut document.root);
        Some(document)
    }
}

/// A parse result for node entry points such as type parsing.
#[derive(Clone, Debug, PartialEq)]
pub struct ParseNodeResult<T> {
    /// Parsed node.
    pub root: Option<T>,
    /// Parse errors.
    pub errors: Vec<Error>,
}

/// A parse diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    /// Stable diagnostic category.
    pub kind: ErrorKind,
    /// Human-readable diagnostic text.
    pub message: String,
    /// Source range associated with the diagnostic.
    pub location: Location,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: {}",
            self.location.begin.line + 1,
            self.location.begin.column + 1,
            self.message
        )
    }
}

impl std::error::Error for Error {}

/// Stable parse diagnostic category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    /// The parser has not implemented this syntax yet.
    UnsupportedSyntax,
    /// The parser expected a token that was not present.
    ExpectedToken,
    /// The parser saw malformed syntax.
    MalformedSyntax,
    /// The parser reached an error limit.
    ErrorLimit,
}

/// A captured comment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Comment {
    /// Source range.
    pub location: Location,
    /// Comment kind.
    pub kind: CommentKind,
    /// Comment text.
    pub text: String,
}

/// A captured hot comment directive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HotComment {
    /// Whether this directive appeared before the first non-comment token.
    pub header: bool,
    /// Source range.
    pub location: Location,
    /// Directive content after the leading `!`.
    pub content: String,
}

impl Comment {
    /// Converts this comment into the AST JSON comment-location shape.
    #[must_use]
    pub fn into_json_node(self) -> JsonNode {
        use std::collections::BTreeMap;

        use crate::json::{JsonKind, KnownJsonKind};

        JsonNode {
            kind: JsonKind::Known(match self.kind {
                CommentKind::Line => KnownJsonKind::Comment,
                CommentKind::Block => KnownJsonKind::BlockComment,
                CommentKind::BrokenBlock => KnownJsonKind::BrokenComment,
            }),
            location: Some(self.location),
            fields: BTreeMap::new(),
        }
    }
}

/// Captured comment kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommentKind {
    /// Line comment.
    Line,
    /// Block comment.
    Block,
    /// Broken block comment.
    BrokenBlock,
}

/// Parses a whole Luau file with default options and syntax flags.
#[must_use]
pub fn parse_file(source: &str) -> ParseResult {
    parse_file_with(source, Options::default(), SyntaxFlags::default())
}

/// Parses a whole Luau file with explicit options and syntax flags.
#[must_use]
pub fn parse_file_with(source: &str, options: Options, syntax_flags: SyntaxFlags) -> ParseResult {
    let source = strip_initial_shebang_str(source);
    Parser::new(source, options, syntax_flags).parse_file()
}

/// Parses a whole Luau file from arbitrary source bytes.
///
/// Invalid UTF-8 bytes are preserved for string-token values and byte-column
/// locations while a same-length UTF-8 surrogate is used for lexing.
#[must_use]
pub fn parse_file_bytes_with(
    source: &[u8],
    options: Options,
    syntax_flags: SyntaxFlags,
) -> ParseResult {
    let source = strip_initial_shebang_bytes(source);
    let normalized = normalize_source_bytes(source);
    Parser::new_with_original_bytes(&normalized, source, options, syntax_flags).parse_file()
}

/// Parses a Luau type annotation.
#[must_use]
pub fn parse_type(source: &str) -> ParseNodeResult<Type> {
    parse_type_with(source, SyntaxFlags::default())
}

/// Parses a Luau type annotation with explicit syntax flags.
#[must_use]
pub fn parse_type_with(source: &str, syntax_flags: SyntaxFlags) -> ParseNodeResult<Type> {
    Parser::new(source, Options::default(), syntax_flags).parse_type()
}

/// Converts a comment token into a parse comment.
pub(crate) fn comment_from_token(token: Lexeme) -> Comment {
    Comment {
        location: token.location,
        kind: match token.kind {
            TokenKind::Comment => CommentKind::Line,
            TokenKind::BlockComment => CommentKind::Block,
            TokenKind::BrokenComment => CommentKind::BrokenBlock,
            _ => unreachable!("only comment tokens are converted"),
        },
        text: token.text.unwrap_or_default(),
    }
}

/// Builds a valid UTF-8 source string with the same byte length as the input.
fn normalize_source_bytes(source: &[u8]) -> String {
    let mut normalized = String::with_capacity(source.len());
    let mut offset = 0usize;

    while offset < source.len() {
        match str::from_utf8(&source[offset..]) {
            Ok(valid) => {
                normalized.push_str(valid);
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    let valid = str::from_utf8(&source[offset..offset + valid_up_to])
                        .expect("valid_up_to is guaranteed to split valid UTF-8");
                    normalized.push_str(valid);
                    offset += valid_up_to;
                }

                let invalid_len = error.error_len().unwrap_or(1);
                for _ in 0..invalid_len {
                    normalized.push('\u{1a}');
                }
                offset += invalid_len;
            }
        }
    }

    normalized
}

/// Strips an initial executable shebang in the same posture as upstream file reads.
fn strip_initial_shebang_str(source: &str) -> &str {
    if !source.as_bytes().starts_with(b"#!") {
        return source;
    }

    source
        .as_bytes()
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or("", |newline| &source[newline..])
}

/// Strips an initial executable shebang from arbitrary source bytes.
fn strip_initial_shebang_bytes(source: &[u8]) -> &[u8] {
    if !source.starts_with(b"#!") {
        return source;
    }

    source
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(&[], |newline| &source[newline..])
}

#[cfg(any())]
mod tests;
