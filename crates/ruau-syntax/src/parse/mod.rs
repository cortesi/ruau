//! Public parser-facing API.

use std::{
    str,
    sync::{Arc, OnceLock},
};

use crate::{
    Location, Position,
    json::{JsonDocument, JsonNode, renumber_adjacent_fields},
    lexer::{Lexeme, TokenKind},
    parser::Parser,
    syntax::{Expr, Local, Stat, Type, TypePack},
    visit::{Visitor, WalkControl, walk_stat},
};

/// Parser configuration: the upstream `Luau::ParseOptions` knobs plus the
/// parser-visible syntax posture.
///
/// [`Default`] is the full-Luau posture ([`SyntaxFlags::all_luau`]) with every
/// option off. Upstream conformance harnesses that need upstream's own
/// option and syntax defaults use [`Config::upstream_default`].
///
/// When deserialized, missing fields fall back to [`Default`], so a serialized
/// upstream `parseOptions` sidecar yields its options with full-Luau syntax;
/// override [`Config::syntax`] separately when a sidecar carries flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct Config {
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
    /// Parser-visible syntax flags.
    pub syntax: SyntaxFlags,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            syntax: SyntaxFlags::all_luau(),
            ..Self::upstream_default()
        }
    }
}

impl Config {
    /// Returns the upstream-default posture, matching upstream
    /// `Luau::ParseOptions` and parser-visible fast-flag defaults. Used by
    /// upstream conformance harnesses; ordinary callers want [`Default`].
    #[must_use]
    pub const fn upstream_default() -> Self {
        Self {
            allow_declaration_syntax: false,
            capture_comments: false,
            parse_fragment: false,
            store_cst_data: false,
            no_error_limit: false,
            syntax: SyntaxFlags::upstream_default(),
        }
    }

    /// Returns whether two configurations produce the same compiler-visible
    /// AST.
    ///
    /// Comment capture and CST storage are deliberately ignored because they
    /// do not alter the syntax tree consumed by analysis or compilation.
    #[must_use]
    pub const fn ast_compatible_with(self, other: Self) -> bool {
        self.allow_declaration_syntax == other.allow_declaration_syntax
            && self.parse_fragment == other.parse_fragment
            && self.no_error_limit == other.no_error_limit
            && self.syntax.const_eq(other.syntax)
    }
}

/// Parser-visible syntax flags modeled from upstream fast flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
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
    /// Enables CST-aware attribute lists.
    pub luau_cst_attr: bool,
    /// Omits redundant group nodes from explicit function return type packs.
    pub luau_function_return_type_pack_less_type_groups: bool,
    /// Enables value exports such as `export function`.
    pub luau_export_value_syntax: bool,
    /// Enables user-defined class syntax.
    pub debug_luau_user_defined_classes: bool,
    /// Enables the debug-only `@debugnoinline` attribute.
    pub debug_luau_no_inline: bool,
    /// Allows global declarations to be called class.
    pub luau_allow_global_declaration_to_be_called_class: bool,
    /// Links dotted type-reference prefixes to visible local bindings.
    pub luau_track_prefix_local: bool,
    /// Keeps desugared array type references empty.
    pub desugared_array_type_reference_is_empty: bool,
}

impl SyntaxFlags {
    const fn const_eq(self, other: Self) -> bool {
        self.luau_cst_expr_group == other.luau_cst_expr_group
            && self.luau_cst_type_group == other.luau_cst_type_group
            && self.luau_const2 == other.luau_const2
            && self.luau_integer_type == other.luau_integer_type
            && self.luau_type_functions == other.luau_type_functions
            && self.luau_extern_read_write_attributes == other.luau_extern_read_write_attributes
            && self.luau_cst_attr == other.luau_cst_attr
            && self.luau_function_return_type_pack_less_type_groups
                == other.luau_function_return_type_pack_less_type_groups
            && self.luau_export_value_syntax == other.luau_export_value_syntax
            && self.debug_luau_user_defined_classes == other.debug_luau_user_defined_classes
            && self.debug_luau_no_inline == other.debug_luau_no_inline
            && self.luau_allow_global_declaration_to_be_called_class
                == other.luau_allow_global_declaration_to_be_called_class
            && self.luau_track_prefix_local == other.luau_track_prefix_local
            && self.desugared_array_type_reference_is_empty
                == other.desugared_array_type_reference_is_empty
    }

    /// Returns the upstream-default syntax posture, available in `const`
    /// contexts. Historical fast flags default off here, but `const` syntax is
    /// no longer gated upstream and is therefore enabled.
    #[must_use]
    pub const fn upstream_default() -> Self {
        Self {
            luau_cst_expr_group: false,
            luau_cst_type_group: false,
            luau_const2: true,
            luau_integer_type: false,
            luau_type_functions: false,
            luau_extern_read_write_attributes: false,
            luau_cst_attr: false,
            luau_function_return_type_pack_less_type_groups: false,
            luau_export_value_syntax: false,
            debug_luau_user_defined_classes: false,
            debug_luau_no_inline: false,
            luau_allow_global_declaration_to_be_called_class: false,
            luau_track_prefix_local: false,
            desugared_array_type_reference_is_empty: true,
        }
    }

    /// Sets the flag named by its upstream fast-flag spelling, ignoring
    /// unknown names. The one mapping between upstream flag names and
    /// parser-visible syntax flags — fixture tooling reads flag sidecars
    /// through this instead of restating the table.
    pub fn set_by_upstream_name(&mut self, name: &str, value: bool) {
        match name {
            "LuauCstExprGroup" => self.luau_cst_expr_group = value,
            "LuauCstTypeGroup" => self.luau_cst_type_group = value,
            "LuauConst2" => self.luau_const2 = value,
            "LuauIntegerType" | "LuauIntegerType2" => self.luau_integer_type = value,
            "LuauTypeFunctions" => self.luau_type_functions = value,
            "LuauExternReadWriteAttributes" => self.luau_extern_read_write_attributes = value,
            "LuauCstAttr" => self.luau_cst_attr = value,
            "LuauFunctionReturnTypePackLessTypeGroups" => {
                self.luau_function_return_type_pack_less_type_groups = value;
            }
            "LuauExportValueSyntax" => self.luau_export_value_syntax = value,
            "DebugLuauUserDefinedClasses" => self.debug_luau_user_defined_classes = value,
            "DebugLuauNoInline" => self.debug_luau_no_inline = value,
            "LuauAllowGlobalDeclarationToBeCalledClass" => {
                self.luau_allow_global_declaration_to_be_called_class = value;
            }
            "LuauTrackPrefixLocal" => self.luau_track_prefix_local = value,
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
            luau_cst_attr: true,
            luau_function_return_type_pack_less_type_groups: true,
            luau_export_value_syntax: true,
            debug_luau_user_defined_classes: true,
            debug_luau_no_inline: false,
            luau_allow_global_declaration_to_be_called_class: true,
            luau_track_prefix_local: true,
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

impl Default for SyntaxFlags {
    fn default() -> Self {
        Self::upstream_default()
    }
}

/// A parse result for whole-file parsing.
#[derive(Clone, Debug, PartialEq)]
pub struct Result {
    /// Parsed root block. Always present: error recovery produces
    /// `Stat::Error` nodes instead of dropping the root.
    pub root: Stat,
    /// Parse errors.
    pub errors: Vec<Error>,
    /// Captured comments.
    pub comments: Vec<Comment>,
    /// Captured hot comments.
    pub hot_comments: Vec<HotComment>,
}

impl Result {
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

    /// Converts the root block into an AST JSON document.
    #[must_use]
    pub fn into_json_document(self) -> JsonDocument {
        let mut document = JsonDocument {
            root: self.root.into_json(),
            comment_locations: self
                .comments
                .into_iter()
                .map(Comment::into_json_node)
                .collect(),
        };
        renumber_adjacent_fields(&mut document.root);
        document
    }
}

/// Shared whole-module parse product for analysis and compilation.
#[derive(Clone, Debug)]
pub struct ParsedModule {
    root: Arc<Stat>,
    source: Arc<[u8]>,
    config: Config,
    errors: Vec<Error>,
    comments: Vec<Comment>,
    hot_comments: Vec<HotComment>,
    ast_nodes: Arc<OnceLock<usize>>,
}

impl ParsedModule {
    fn new(source: Arc<[u8]>, config: Config, parsed: Result) -> Self {
        Self {
            root: Arc::new(parsed.root),
            source,
            config,
            errors: parsed.errors,
            comments: parsed.comments,
            hot_comments: parsed.hot_comments,
            ast_nodes: Arc::new(OnceLock::new()),
        }
    }

    /// Returns the shared parsed root.
    #[must_use]
    pub const fn root(&self) -> &Arc<Stat> {
        &self.root
    }

    /// Returns the original byte-exact source, including any shebang.
    #[must_use]
    pub const fn source(&self) -> &Arc<[u8]> {
        &self.source
    }

    /// Returns the parser configuration used to build the root.
    #[must_use]
    pub const fn config(&self) -> Config {
        self.config
    }

    /// Returns parse errors in parser order.
    #[must_use]
    pub fn errors(&self) -> &[Error] {
        &self.errors
    }

    /// Returns captured comments.
    #[must_use]
    pub fn comments(&self) -> &[Comment] {
        &self.comments
    }

    /// Returns captured leading hot comments.
    #[must_use]
    pub fn hot_comments(&self) -> &[HotComment] {
        &self.hot_comments
    }

    /// Returns the executor-compatible AST node count.
    ///
    /// The first call measures the tree. Clones of this product share the
    /// cached measurement.
    #[must_use]
    pub fn ast_nodes(&self) -> usize {
        *self
            .ast_nodes
            .get_or_init(|| ast_node_count(self.root.as_ref()))
    }

    /// Returns whether parsing produced no errors.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

impl PartialEq for ParsedModule {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
            && self.source == other.source
            && self.config == other.config
            && self.errors == other.errors
            && self.comments == other.comments
            && self.hot_comments == other.hot_comments
    }
}

fn ast_node_count(root: &Stat) -> usize {
    #[derive(Default)]
    struct Counter {
        nodes: usize,
    }

    impl Visitor<'_> for Counter {
        fn visit_stat(&mut self, _stat: &Stat) -> WalkControl {
            self.nodes += 1;
            WalkControl::Continue
        }

        fn visit_local(&mut self, _local: &Local) -> WalkControl {
            self.nodes += 1;
            WalkControl::Continue
        }

        fn visit_expr(&mut self, _expr: &Expr) -> WalkControl {
            self.nodes += 1;
            WalkControl::Continue
        }

        fn visit_type(&mut self, _luau_type: &Type) -> WalkControl {
            self.nodes += 1;
            WalkControl::Continue
        }

        fn visit_type_pack(&mut self, _type_pack: &TypePack) -> WalkControl {
            self.nodes += 1;
            WalkControl::Continue
        }
    }

    let mut counter = Counter::default();
    walk_stat(root, &mut counter);
    counter.nodes
}

/// A parse result for node entry points such as type parsing.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeResult<T> {
    /// Parsed node. Always present: error recovery produces error nodes
    /// instead of dropping the root.
    pub root: T,
    /// Parse errors.
    pub errors: Vec<Error>,
}

impl NodeResult<Type> {
    /// Converts the parsed type into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        self.root.into_json()
    }
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

/// Parses a whole Luau file with the default [`Config`] (full Luau
/// syntax, every option off).
#[must_use]
pub fn parse(source: &str) -> Result {
    parse_with_config(source, &Config::default())
}

/// Parses a whole Luau file with an explicit parser configuration.
#[must_use]
pub fn parse_with_config(source: &str, config: &Config) -> Result {
    let source = strip_initial_shebang_str(source);
    Parser::new(source, config).parse()
}

/// Parses a whole Luau file into a shared measured module product.
#[must_use]
pub fn parse_module(source: &str) -> ParsedModule {
    parse_module_with_config(source, &Config::default())
}

/// Parses a whole Luau file into a shared measured module product with an
/// explicit parser configuration.
#[must_use]
pub fn parse_module_with_config(source: &str, config: &Config) -> ParsedModule {
    let parser_source = strip_initial_shebang_str(source);
    ParsedModule::new(
        Arc::from(source.as_bytes()),
        *config,
        Parser::new(parser_source, config).parse(),
    )
}

/// Parses a whole Luau file from arbitrary source bytes with the default
/// [`Config`].
#[must_use]
pub fn parse_bytes(source: &[u8]) -> Result {
    parse_bytes_with_config(source, &Config::default())
}

/// Parses a whole Luau file from arbitrary source bytes.
///
/// Invalid UTF-8 bytes are preserved for string-token values and byte-column
/// locations while a same-length UTF-8 surrogate is used for lexing.
#[must_use]
pub fn parse_bytes_with_config(source: &[u8], config: &Config) -> Result {
    let source = strip_initial_shebang_bytes(source);
    let normalized = normalize_source_bytes(source);
    Parser::new_with_original_bytes(&normalized, source, config).parse()
}

/// Parses arbitrary source bytes into a shared measured module product.
#[must_use]
pub fn parse_module_bytes(source: &[u8]) -> ParsedModule {
    parse_module_bytes_with_config(source, &Config::default())
}

/// Parses arbitrary source bytes into a shared measured module product with an
/// explicit parser configuration.
#[must_use]
pub fn parse_module_bytes_with_config(source: &[u8], config: &Config) -> ParsedModule {
    parse_shared_module_bytes_with_config(Arc::from(source), config)
}

/// Parses shared arbitrary source bytes into a shared measured module product
/// with an explicit parser configuration.
///
/// This entry point retains `source` directly so callers that already own
/// shared bytes do not copy them again.
#[must_use]
pub fn parse_shared_module_bytes_with_config(source: Arc<[u8]>, config: &Config) -> ParsedModule {
    let parsed = {
        let parser_source = strip_initial_shebang_bytes(&source);
        let normalized = normalize_source_bytes(parser_source);
        Parser::new_with_original_bytes(&normalized, parser_source, config).parse()
    };
    ParsedModule::new(source, *config, parsed)
}

/// Parses a Luau type annotation with the default [`Config`] (full Luau
/// syntax).
#[must_use]
pub fn parse_type(source: &str) -> NodeResult<Type> {
    parse_type_with_config(source, &Config::default())
}

/// Parses a Luau type annotation with an explicit parser configuration.
#[must_use]
pub fn parse_type_with_config(source: &str, config: &Config) -> NodeResult<Type> {
    Parser::new(source, config).parse_type()
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
        text: token
            .text
            .map(std::borrow::Cow::into_owned)
            .unwrap_or_default(),
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
