//! Lexer-facing Luau token structures, ported from
//! `upstream/luau/Ast/include/Luau/Lexer.h`/`Lexer.cpp`.
//!
//! Beyond the parser itself, the public surface here serves the
//! upstream-extraction fixtures: `tests/lexer_fixtures.rs` replays extracted
//! token streams against [`Lexer`], so [`TokenStream`], [`Lexeme`] (including
//! its upstream `display` string), and the serde token spellings are part of
//! that fixture contract rather than a general embedding API.

#![allow(dead_code)]

use std::{borrow::Cow, error::Error, fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::Location;

/// A Luau token kind.
///
/// This mirrors upstream `Luau::Lexeme::Type`. Single-byte punctuation tokens
/// are represented as `Char`, matching upstream's `1..255` character range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TokenKind {
    /// A single-byte punctuation or operator token.
    Char(char),
    /// End of input.
    Eof,
    /// `==`.
    Equal,
    /// `<=`.
    LessEqual,
    /// `>=`.
    GreaterEqual,
    /// `~=`.
    NotEqual,
    /// `..`.
    Dot2,
    /// `...`.
    Dot3,
    /// `->`.
    SkinnyArrow,
    /// `::`.
    DoubleColon,
    /// `//`.
    FloorDiv,
    /// The first section of an interpolated string.
    InterpStringBegin,
    /// A middle section of an interpolated string.
    InterpStringMid,
    /// The final section of an interpolated string.
    InterpStringEnd,
    /// An interpolated string with no expressions.
    InterpStringSimple,
    /// `+=`.
    AddAssign,
    /// `-=`.
    SubAssign,
    /// `*=`.
    MulAssign,
    /// `/=`.
    DivAssign,
    /// `//=`.
    FloorDivAssign,
    /// `%=`.
    ModAssign,
    /// `^=`.
    PowAssign,
    /// `..=`.
    ConcatAssign,
    /// A long-bracket string.
    RawString,
    /// A single- or double-quoted string.
    QuotedString,
    /// A numeric literal.
    Number,
    /// An identifier.
    Name,
    /// A line comment.
    Comment,
    /// A block comment.
    BlockComment,
    /// An attribute token.
    Attribute,
    /// An attribute opener.
    AttributeOpen,
    /// A malformed quoted string.
    BrokenString,
    /// A malformed block comment.
    BrokenComment,
    /// A malformed Unicode sequence.
    BrokenUnicode,
    /// A double brace inside an interpolated string.
    BrokenInterpDoubleBrace,
    /// Generic lexer error token.
    Error,
    /// `and`.
    ReservedAnd,
    /// `break`.
    ReservedBreak,
    /// `do`.
    ReservedDo,
    /// `else`.
    ReservedElse,
    /// `elseif`.
    ReservedElseif,
    /// `end`.
    ReservedEnd,
    /// `false`.
    ReservedFalse,
    /// `for`.
    ReservedFor,
    /// `function`.
    ReservedFunction,
    /// `if`.
    ReservedIf,
    /// `in`.
    ReservedIn,
    /// `local`.
    ReservedLocal,
    /// `nil`.
    ReservedNil,
    /// `not`.
    ReservedNot,
    /// `or`.
    ReservedOr,
    /// `repeat`.
    ReservedRepeat,
    /// `return`.
    ReservedReturn,
    /// `then`.
    ReservedThen,
    /// `true`.
    ReservedTrue,
    /// `until`.
    ReservedUntil,
    /// `while`.
    ReservedWhile,
}

impl TokenKind {
    /// Returns the upstream token name used in extracted fixtures.
    #[must_use]
    pub(crate) fn as_upstream_str(self) -> String {
        let name = match self {
            Self::Char(ch) => return ch.to_string(),
            Self::Eof => "Eof",
            Self::Equal => "Equal",
            Self::LessEqual => "LessEqual",
            Self::GreaterEqual => "GreaterEqual",
            Self::NotEqual => "NotEqual",
            Self::Dot2 => "Dot2",
            Self::Dot3 => "Dot3",
            Self::SkinnyArrow => "SkinnyArrow",
            Self::DoubleColon => "DoubleColon",
            Self::FloorDiv => "FloorDiv",
            Self::InterpStringBegin => "InterpStringBegin",
            Self::InterpStringMid => "InterpStringMid",
            Self::InterpStringEnd => "InterpStringEnd",
            Self::InterpStringSimple => "InterpStringSimple",
            Self::AddAssign => "AddAssign",
            Self::SubAssign => "SubAssign",
            Self::MulAssign => "MulAssign",
            Self::DivAssign => "DivAssign",
            Self::FloorDivAssign => "FloorDivAssign",
            Self::ModAssign => "ModAssign",
            Self::PowAssign => "PowAssign",
            Self::ConcatAssign => "ConcatAssign",
            Self::RawString => "RawString",
            Self::QuotedString => "QuotedString",
            Self::Number => "Number",
            Self::Name => "Name",
            Self::Comment => "Comment",
            Self::BlockComment => "BlockComment",
            Self::Attribute => "Attribute",
            Self::AttributeOpen => "AttributeOpen",
            Self::BrokenString => "BrokenString",
            Self::BrokenComment => "BrokenComment",
            Self::BrokenUnicode => "BrokenUnicode",
            Self::BrokenInterpDoubleBrace => "BrokenInterpDoubleBrace",
            Self::Error => "Error",
            Self::ReservedAnd => "ReservedAnd",
            Self::ReservedBreak => "ReservedBreak",
            Self::ReservedDo => "ReservedDo",
            Self::ReservedElse => "ReservedElse",
            Self::ReservedElseif => "ReservedElseif",
            Self::ReservedEnd => "ReservedEnd",
            Self::ReservedFalse => "ReservedFalse",
            Self::ReservedFor => "ReservedFor",
            Self::ReservedFunction => "ReservedFunction",
            Self::ReservedIf => "ReservedIf",
            Self::ReservedIn => "ReservedIn",
            Self::ReservedLocal => "ReservedLocal",
            Self::ReservedNil => "ReservedNil",
            Self::ReservedNot => "ReservedNot",
            Self::ReservedOr => "ReservedOr",
            Self::ReservedRepeat => "ReservedRepeat",
            Self::ReservedReturn => "ReservedReturn",
            Self::ReservedThen => "ReservedThen",
            Self::ReservedTrue => "ReservedTrue",
            Self::ReservedUntil => "ReservedUntil",
            Self::ReservedWhile => "ReservedWhile",
        };
        name.to_owned()
    }
}

impl FromStr for TokenKind {
    type Err = TokenKindError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        if source.len() == 1 {
            return Ok(Self::Char(
                source.chars().next().expect("len checked above"),
            ));
        }

        match source {
            "Eof" => Ok(Self::Eof),
            "Equal" => Ok(Self::Equal),
            "LessEqual" => Ok(Self::LessEqual),
            "GreaterEqual" => Ok(Self::GreaterEqual),
            "NotEqual" => Ok(Self::NotEqual),
            "Dot2" => Ok(Self::Dot2),
            "Dot3" => Ok(Self::Dot3),
            "SkinnyArrow" => Ok(Self::SkinnyArrow),
            "DoubleColon" => Ok(Self::DoubleColon),
            "FloorDiv" => Ok(Self::FloorDiv),
            "InterpStringBegin" => Ok(Self::InterpStringBegin),
            "InterpStringMid" => Ok(Self::InterpStringMid),
            "InterpStringEnd" => Ok(Self::InterpStringEnd),
            "InterpStringSimple" => Ok(Self::InterpStringSimple),
            "AddAssign" => Ok(Self::AddAssign),
            "SubAssign" => Ok(Self::SubAssign),
            "MulAssign" => Ok(Self::MulAssign),
            "DivAssign" => Ok(Self::DivAssign),
            "FloorDivAssign" => Ok(Self::FloorDivAssign),
            "ModAssign" => Ok(Self::ModAssign),
            "PowAssign" => Ok(Self::PowAssign),
            "ConcatAssign" => Ok(Self::ConcatAssign),
            "RawString" => Ok(Self::RawString),
            "QuotedString" => Ok(Self::QuotedString),
            "Number" => Ok(Self::Number),
            "Name" => Ok(Self::Name),
            "Comment" => Ok(Self::Comment),
            "BlockComment" => Ok(Self::BlockComment),
            "Attribute" => Ok(Self::Attribute),
            "AttributeOpen" => Ok(Self::AttributeOpen),
            "BrokenString" => Ok(Self::BrokenString),
            "BrokenComment" => Ok(Self::BrokenComment),
            "BrokenUnicode" => Ok(Self::BrokenUnicode),
            "BrokenInterpDoubleBrace" => Ok(Self::BrokenInterpDoubleBrace),
            "Error" => Ok(Self::Error),
            "ReservedAnd" => Ok(Self::ReservedAnd),
            "ReservedBreak" => Ok(Self::ReservedBreak),
            "ReservedDo" => Ok(Self::ReservedDo),
            "ReservedElse" => Ok(Self::ReservedElse),
            "ReservedElseif" => Ok(Self::ReservedElseif),
            "ReservedEnd" => Ok(Self::ReservedEnd),
            "ReservedFalse" => Ok(Self::ReservedFalse),
            "ReservedFor" => Ok(Self::ReservedFor),
            "ReservedFunction" => Ok(Self::ReservedFunction),
            "ReservedIf" => Ok(Self::ReservedIf),
            "ReservedIn" => Ok(Self::ReservedIn),
            "ReservedLocal" => Ok(Self::ReservedLocal),
            "ReservedNil" => Ok(Self::ReservedNil),
            "ReservedNot" => Ok(Self::ReservedNot),
            "ReservedOr" => Ok(Self::ReservedOr),
            "ReservedRepeat" => Ok(Self::ReservedRepeat),
            "ReservedReturn" => Ok(Self::ReservedReturn),
            "ReservedThen" => Ok(Self::ReservedThen),
            "ReservedTrue" => Ok(Self::ReservedTrue),
            "ReservedUntil" => Ok(Self::ReservedUntil),
            "ReservedWhile" => Ok(Self::ReservedWhile),
            _ => Err(TokenKindError),
        }
    }
}

impl Serialize for TokenKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.as_upstream_str())
    }
}

impl<'de> Deserialize<'de> for TokenKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let source = String::deserialize(deserializer)?;
        source.parse().map_err(|_error| {
            de::Error::unknown_variant(&source, &["upstream Luau token kind or one-byte char"])
        })
    }
}

/// Error returned when a token kind string is not recognized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokenKindError;

impl fmt::Display for TokenKindError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("unknown Luau token kind")
    }
}

impl Error for TokenKindError {}

/// Upstream quote style for quoted strings.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QuoteStyle {
    /// Single-quoted string.
    Single,
    /// Double-quoted string.
    Double,
}

/// How a lexeme's upstream display string is produced.
///
/// The display string is only read when building diagnostics (and when
/// serializing fixture token streams), so the lexer never renders it eagerly:
/// fixed tokens carry a static string and payload tokens render on demand from
/// their `kind` plus payload fields. `Owned` exists for deserialized fixtures,
/// which keeps fixture equality honest: comparing an extracted upstream token
/// (`Owned`) against a freshly lexed one (`Fixed`/`Derived`) still compares the
/// rendered display text.
#[derive(Clone, Debug)]
enum LexemeDisplay {
    /// Fixed display text known at compile time.
    Fixed(&'static str),
    /// Rendered on demand from the token kind and payload fields.
    Derived,
    /// Owned display text from a deserialized fixture stream.
    Owned(Box<str>),
}

/// A Luau lexeme.
///
/// Payload fields borrow from the tokenized source where possible; only
/// deserialized fixture streams hold owned payloads.
#[derive(Clone, Debug)]
pub struct Lexeme<'source> {
    /// Token kind.
    pub kind: TokenKind,
    /// Source range covered by the token.
    pub location: Location,
    /// Upstream display representation; render with [`Lexeme::display`].
    display: LexemeDisplay,
    /// Source text or decoded token payload for tokens that carry data.
    pub text: Option<Cow<'source, str>>,
    /// Interned name text for `Name` tokens.
    pub name: Option<Cow<'source, str>>,
    /// Long string or block comment separator depth.
    pub block_depth: Option<u32>,
    /// Quote style for quoted strings.
    pub quote_style: Option<QuoteStyle>,
    /// Broken Unicode codepoint, when upstream records one.
    pub codepoint: Option<u32>,
}

impl<'source> Lexeme<'source> {
    /// Creates a token with a fixed display string and no payload fields.
    #[must_use]
    pub fn new(kind: TokenKind, location: Location, display: &'static str) -> Self {
        Self {
            kind,
            location,
            display: LexemeDisplay::Fixed(display),
            text: None,
            name: None,
            block_depth: None,
            quote_style: None,
            codepoint: None,
        }
    }

    /// Creates a token with an eagerly rendered display string. Reserved for
    /// cold paths whose display is not derivable from the retained payload.
    #[must_use]
    fn with_owned_display(kind: TokenKind, location: Location, display: String) -> Self {
        Self {
            display: LexemeDisplay::Owned(display.into_boxed_str()),
            ..Self::derived(kind, location)
        }
    }

    /// Creates a token whose display renders on demand from its payload.
    #[must_use]
    fn derived(kind: TokenKind, location: Location) -> Self {
        Self {
            kind,
            location,
            display: LexemeDisplay::Derived,
            text: None,
            name: None,
            block_depth: None,
            quote_style: None,
            codepoint: None,
        }
    }

    /// Creates a token with a fixed display string and a text payload.
    #[must_use]
    pub fn with_text(
        kind: TokenKind,
        location: Location,
        display: &'static str,
        text: impl Into<Cow<'source, str>>,
    ) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::new(kind, location, display)
        }
    }

    /// Creates a token with a derived display string and a text payload.
    #[must_use]
    fn with_derived_text(
        kind: TokenKind,
        location: Location,
        text: impl Into<Cow<'source, str>>,
    ) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::derived(kind, location)
        }
    }

    /// Creates a name token.
    #[must_use]
    pub fn with_name(location: Location, name: impl Into<Cow<'source, str>>) -> Self {
        Self {
            name: Some(name.into()),
            ..Self::derived(TokenKind::Name, location)
        }
    }

    /// Renders the upstream display string.
    #[must_use]
    pub fn display(&self) -> Cow<'_, str> {
        match &self.display {
            LexemeDisplay::Fixed(display) => Cow::Borrowed(display),
            LexemeDisplay::Owned(display) => Cow::Borrowed(display),
            LexemeDisplay::Derived => Cow::Owned(self.render_display()),
        }
    }

    /// Renders a derived display string from the token kind and payload.
    fn render_display(&self) -> String {
        let text = self.text.as_deref().unwrap_or_default();
        match self.kind {
            TokenKind::Name => format!("'{}'", self.name.as_deref().unwrap_or_default()),
            TokenKind::Attribute => format!("'@{}'", self.name.as_deref().unwrap_or_default()),
            TokenKind::Char(ch) => format!("'{ch}'"),
            TokenKind::Number => format!("'{text}'"),
            TokenKind::QuotedString | TokenKind::RawString => format!("\"{text}\""),
            TokenKind::InterpStringBegin => format!("`{text}{{"),
            TokenKind::InterpStringMid => format!("}}{text}{{"),
            TokenKind::InterpStringEnd => format!("}}{text}`"),
            TokenKind::InterpStringSimple => format!("`{text}`"),
            TokenKind::BrokenUnicode => match self.codepoint {
                Some(codepoint) => format!("Unicode character U+{codepoint:x}"),
                None => "invalid UTF-8 sequence".to_owned(),
            },
            _ => String::new(),
        }
    }
}

impl<'a, 'b> PartialEq<Lexeme<'b>> for Lexeme<'a> {
    fn eq(&self, other: &Lexeme<'b>) -> bool {
        self.kind == other.kind
            && self.location == other.location
            && self.display() == other.display()
            && self.text == other.text
            && self.name == other.name
            && self.block_depth == other.block_depth
            && self.quote_style == other.quote_style
            && self.codepoint == other.codepoint
    }
}

/// Serde mirror of [`Lexeme`] preserving the extracted fixture JSON shape.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LexemeRepr {
    kind: TokenKind,
    location: Location,
    display: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    block_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quote_style: Option<QuoteStyle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codepoint: Option<u32>,
}

impl Serialize for Lexeme<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        LexemeRepr {
            kind: self.kind,
            location: self.location,
            display: self.display().into_owned(),
            text: self.text.as_deref().map(str::to_owned),
            name: self.name.as_deref().map(str::to_owned),
            block_depth: self.block_depth,
            quote_style: self.quote_style,
            codepoint: self.codepoint,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Lexeme<'_> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let repr = LexemeRepr::deserialize(deserializer)?;
        Ok(Self {
            kind: repr.kind,
            location: repr.location,
            display: LexemeDisplay::Owned(repr.display.into_boxed_str()),
            text: repr.text.map(Cow::Owned),
            name: repr.name.map(Cow::Owned),
            block_depth: repr.block_depth,
            quote_style: repr.quote_style,
            codepoint: repr.codepoint,
        })
    }
}

/// Extracted token stream fixture.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TokenStream {
    /// Tokens emitted by upstream (owned payloads after deserialization).
    pub tokens: Vec<Lexeme<'static>>,
}

/// Lexer options that affect the token stream.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LexerOptions {
    /// Whether comments are skipped.
    pub skip_comments: bool,
    /// Whether identifiers are interned and reported as names.
    pub read_names: bool,
}

impl Default for LexerOptions {
    fn default() -> Self {
        Self {
            skip_comments: false,
            read_names: true,
        }
    }
}

/// A Luau lexer.
#[derive(Clone, Debug)]
pub struct Lexer<'source> {
    /// Source text to tokenize.
    source: &'source str,
    /// Source byte length visible to Luau before the first NUL sentinel.
    source_end: usize,
    /// Lexer options.
    options: LexerOptions,
    /// Current byte offset.
    offset: usize,
    /// Current source position.
    position: crate::Position,
    /// Brace nesting while lexing interpolated string expressions.
    brace_stack: Vec<BraceType>,
}

/// Brace stack entries used while lexing interpolated strings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BraceType {
    /// A normal `{...}` nested inside an interpolated expression.
    Normal,
    /// The `{...}` that opened an interpolated expression.
    InterpolatedString,
}

impl<'source> Lexer<'source> {
    /// Creates a lexer with upstream default options.
    #[must_use]
    pub fn new(source: &'source str) -> Self {
        Self::with_options(source, LexerOptions::DEFAULT)
    }

    /// Creates a lexer with explicit options.
    #[must_use]
    pub fn with_options(source: &'source str, options: LexerOptions) -> Self {
        let source_end = source
            .as_bytes()
            .iter()
            .position(|byte| *byte == b'\0')
            .unwrap_or(source.len());
        Self {
            source,
            source_end,
            options,
            offset: 0,
            position: crate::Position::new(0, 0),
            brace_stack: Vec::new(),
        }
    }

    /// Returns the source text being tokenized.
    #[must_use]
    pub const fn source(&self) -> &'source str {
        self.source
    }

    /// Returns lexer options.
    #[must_use]
    pub const fn options(&self) -> LexerOptions {
        self.options
    }

    /// Returns the current byte offset.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.offset
    }

    /// Returns the current source position.
    #[must_use]
    pub const fn position(&self) -> crate::Position {
        self.position
    }

    /// Reads the next token.
    pub fn next_token(&mut self) -> Lexeme<'source> {
        loop {
            self.skip_trivia();
            let token = self.read_token();
            if self.options.skip_comments
                && matches!(token.kind, TokenKind::Comment | TokenKind::BlockComment)
            {
                continue;
            }
            return token;
        }
    }

    /// Skips whitespace before a token.
    fn skip_trivia(&mut self) {
        while let Some(byte) = self.peek_byte() {
            if matches!(byte, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r') {
                self.bump();
            } else {
                break;
            }
        }
    }

    /// Reads one token at the current offset.
    fn read_token(&mut self) -> Lexeme<'source> {
        let start = self.position;
        let Some(byte) = self.peek_byte() else {
            return Lexeme::new(TokenKind::Eof, Location::new(start, start), "<eof>");
        };

        if is_name_start(byte) {
            return self.read_name_or_reserved(start);
        }
        if byte.is_ascii_digit()
            || (byte == b'.'
                && self
                    .peek_byte_at(1)
                    .is_some_and(|byte| byte.is_ascii_digit()))
        {
            return self.read_number(start);
        }

        match byte {
            b'@' => self.read_attribute(start),
            b'`' => self.read_interpolated_string_begin(start),
            b'{' => self.read_open_brace(start),
            b'}' if !self.brace_stack.is_empty() => self.read_close_brace(start),
            b'[' if long_separator_depth(self.visible_bytes(), self.offset).is_some() => {
                self.read_long_string(start)
            }
            b'\'' | b'"' => self.read_quoted_string(start),
            b'-' if self.peek_byte_at(1) == Some(b'-') => self.read_comment(start),
            b'-' if self.peek_byte_at(1) == Some(b'>') => {
                self.bump();
                self.bump();
                Lexeme::new(
                    TokenKind::SkinnyArrow,
                    Location::new(start, self.position),
                    "'->'",
                )
            }
            b'=' if self.peek_byte_at(1) == Some(b'=') => {
                self.read_two_byte_token(start, TokenKind::Equal, "'=='")
            }
            b'<' if self.peek_byte_at(1) == Some(b'=') => {
                self.read_two_byte_token(start, TokenKind::LessEqual, "'<='")
            }
            b'>' if self.peek_byte_at(1) == Some(b'=') => {
                self.read_two_byte_token(start, TokenKind::GreaterEqual, "'>='")
            }
            b'~' if self.peek_byte_at(1) == Some(b'=') => {
                self.read_two_byte_token(start, TokenKind::NotEqual, "'~='")
            }
            b':' if self.peek_byte_at(1) == Some(b':') => {
                self.read_two_byte_token(start, TokenKind::DoubleColon, "'::'")
            }
            b'.' if self.peek_byte_at(1) == Some(b'.') && self.peek_byte_at(2) == Some(b'.') => {
                self.read_three_byte_token(start, TokenKind::Dot3, "'...'")
            }
            b'.' if self.peek_byte_at(1) == Some(b'.') && self.peek_byte_at(2) == Some(b'=') => {
                self.read_three_byte_token(start, TokenKind::ConcatAssign, "'..='")
            }
            b'.' if self.peek_byte_at(1) == Some(b'.') => {
                self.read_two_byte_token(start, TokenKind::Dot2, "'..'")
            }
            b'/' if self.peek_byte_at(1) == Some(b'/') && self.peek_byte_at(2) == Some(b'=') => {
                self.read_three_byte_token(start, TokenKind::FloorDivAssign, "'//='")
            }
            b'/' if self.peek_byte_at(1) == Some(b'/') => {
                self.read_two_byte_token(start, TokenKind::FloorDiv, "'//'")
            }
            b'+' if self.peek_byte_at(1) == Some(b'=') => {
                self.read_two_byte_token(start, TokenKind::AddAssign, "'+='")
            }
            b'-' if self.peek_byte_at(1) == Some(b'=') => {
                self.read_two_byte_token(start, TokenKind::SubAssign, "'-='")
            }
            b'*' if self.peek_byte_at(1) == Some(b'=') => {
                self.read_two_byte_token(start, TokenKind::MulAssign, "'*='")
            }
            b'/' if self.peek_byte_at(1) == Some(b'=') => {
                self.read_two_byte_token(start, TokenKind::DivAssign, "'/='")
            }
            b'%' if self.peek_byte_at(1) == Some(b'=') => {
                self.read_two_byte_token(start, TokenKind::ModAssign, "'%='")
            }
            b'^' if self.peek_byte_at(1) == Some(b'=') => {
                self.read_two_byte_token(start, TokenKind::PowAssign, "'^='")
            }
            byte if byte & 0x80 != 0 => self.read_utf8_error(start),
            _ => {
                let ch = self.bump().expect("peeked byte should be present") as char;
                Lexeme::derived(TokenKind::Char(ch), Location::new(start, self.position))
            }
        }
    }

    /// Reads a non-ASCII UTF-8 sequence as upstream's broken-unicode token.
    fn read_utf8_error(&mut self, start: crate::Position) -> Lexeme<'source> {
        let first = self.peek_byte().expect("non-ASCII byte should be present");
        let (size, mut codepoint) = if first & 0b1110_0000 == 0b1100_0000 {
            (2, u32::from(first & 0b0001_1111))
        } else if first & 0b1111_0000 == 0b1110_0000 {
            (3, u32::from(first & 0b0000_1111))
        } else if first & 0b1111_1000 == 0b1111_0000 {
            (4, u32::from(first & 0b0000_0111))
        } else {
            self.bump();
            return Lexeme::new(
                TokenKind::BrokenUnicode,
                Location::new(start, self.position),
                "invalid UTF-8 sequence",
            );
        };

        self.bump();
        for _ in 1..size {
            let Some(byte) = self.peek_byte() else {
                return Lexeme::new(
                    TokenKind::BrokenUnicode,
                    Location::new(start, self.position),
                    "invalid UTF-8 sequence",
                );
            };
            if byte & 0b1100_0000 != 0b1000_0000 {
                return Lexeme::new(
                    TokenKind::BrokenUnicode,
                    Location::new(start, self.position),
                    "invalid UTF-8 sequence",
                );
            }
            codepoint = (codepoint << 6) | u32::from(byte & 0b0011_1111);
            self.bump();
        }

        Lexeme {
            codepoint: Some(codepoint),
            ..Lexeme::derived(
                TokenKind::BrokenUnicode,
                Location::new(start, self.position),
            )
        }
    }

    /// Reads an attribute token such as `@checked`.
    fn read_attribute(&mut self, start: crate::Position) -> Lexeme<'source> {
        self.bump();
        if self.peek_byte() == Some(b'[') {
            self.bump();
            return Lexeme::new(
                TokenKind::AttributeOpen,
                Location::new(start, self.position),
                "'@['",
            );
        }

        let name_start = self.offset;
        while self.peek_byte().is_some_and(is_name_continue) {
            self.bump();
        }
        let name = &self.source[name_start..self.offset];
        Lexeme {
            name: Some(Cow::Borrowed(name)),
            ..Lexeme::derived(TokenKind::Attribute, Location::new(start, self.position))
        }
    }

    /// Reads an identifier or reserved word.
    fn read_name_or_reserved(&mut self, start: crate::Position) -> Lexeme<'source> {
        let start_offset = self.offset;
        while self.peek_byte().is_some_and(is_name_continue) {
            self.bump();
        }

        let text = &self.source[start_offset..self.offset];
        match reserved_word(text) {
            Some((kind, display)) => {
                Lexeme::new(kind, Location::new(start, self.position), display)
            }
            None if self.options.read_names => {
                Lexeme::with_name(Location::new(start, self.position), text)
            }
            // Nameless mode is a fixture-extraction path, so the eager display
            // allocation here is fine.
            None => Lexeme::with_owned_display(
                TokenKind::Name,
                Location::new(start, self.position),
                format!("'{text}'"),
            ),
        }
    }

    /// Reads a decimal number prefix.
    fn read_number(&mut self, start: crate::Position) -> Lexeme<'source> {
        let start_offset = self.offset;
        while self
            .peek_byte()
            .is_some_and(|byte| byte.is_ascii_digit() || byte == b'.' || byte == b'_')
        {
            self.bump();
        }

        if matches!(self.peek_byte(), Some(b'e' | b'E')) {
            self.bump();
            if matches!(self.peek_byte(), Some(b'+' | b'-')) {
                self.bump();
            }
        }

        while self
            .peek_byte()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            self.bump();
        }

        let text = &self.source[start_offset..self.offset];
        Lexeme::with_derived_text(TokenKind::Number, Location::new(start, self.position), text)
    }

    /// Reads a two-byte symbolic token.
    fn read_two_byte_token(
        &mut self,
        start: crate::Position,
        kind: TokenKind,
        display: &'static str,
    ) -> Lexeme<'source> {
        self.bump();
        self.bump();
        Lexeme::new(kind, Location::new(start, self.position), display)
    }

    /// Reads a three-byte symbolic token.
    fn read_three_byte_token(
        &mut self,
        start: crate::Position,
        kind: TokenKind,
        display: &'static str,
    ) -> Lexeme<'source> {
        self.bump();
        self.bump();
        self.bump();
        Lexeme::new(kind, Location::new(start, self.position), display)
    }

    /// Reads a quoted string without applying upstream escape fixups yet.
    fn read_quoted_string(&mut self, start: crate::Position) -> Lexeme<'source> {
        let quote = self.bump().expect("peeked quote should be present");
        let content_start = self.offset;

        while let Some(byte) = self.peek_byte() {
            if byte == b'\\' {
                self.bump_backslash_in_string();
                continue;
            }

            if matches!(byte, b'\n' | b'\r') {
                let text = &self.source[content_start..self.offset];
                return Lexeme::with_text(
                    TokenKind::BrokenString,
                    Location::new(start, self.position),
                    "unfinished string",
                    text,
                );
            }

            self.bump();
            if byte == quote {
                let content_end = self.offset - 1;
                let mut token = Lexeme::with_derived_text(
                    TokenKind::QuotedString,
                    Location::new(start, self.position),
                    &self.source[content_start..content_end],
                );
                token.quote_style = Some(if quote == b'\'' {
                    QuoteStyle::Single
                } else {
                    QuoteStyle::Double
                });
                return token;
            }
        }

        let text = &self.source[content_start..self.offset];
        Lexeme::with_text(
            TokenKind::BrokenString,
            Location::new(start, self.position),
            "unfinished string",
            text,
        )
    }

    /// Reads an opening brace and records normal nesting inside interpolation.
    fn read_open_brace(&mut self, start: crate::Position) -> Lexeme<'source> {
        self.bump();
        if !self.brace_stack.is_empty() {
            self.brace_stack.push(BraceType::Normal);
        }
        Lexeme::new(
            TokenKind::Char('{'),
            Location::new(start, self.position),
            "'{'",
        )
    }

    /// Reads a closing brace, returning an interpolated-string suffix when needed.
    fn read_close_brace(&mut self, start: crate::Position) -> Lexeme<'source> {
        self.bump();
        let Some(brace) = self.brace_stack.pop() else {
            return Lexeme::new(
                TokenKind::Char('}'),
                Location::new(start, self.position),
                "'}'",
            );
        };

        match brace {
            BraceType::Normal => Lexeme::new(
                TokenKind::Char('}'),
                Location::new(start, self.position),
                "'}'",
            ),
            BraceType::InterpolatedString => self.read_interpolated_string_section(
                start,
                TokenKind::InterpStringMid,
                TokenKind::InterpStringEnd,
            ),
        }
    }

    /// Reads a line comment.
    fn read_comment(&mut self, start: crate::Position) -> Lexeme<'source> {
        self.bump();
        self.bump();
        if long_separator_depth(self.visible_bytes(), self.offset).is_some() {
            return self.read_long_body(start, TokenKind::BlockComment, "unfinished comment");
        }

        let content_start = self.offset;
        while self
            .peek_byte()
            .is_some_and(|byte| !matches!(byte, b'\r' | b'\n'))
        {
            self.bump();
        }
        let text = &self.source[content_start..self.offset];
        Lexeme::with_text(
            TokenKind::Comment,
            Location::new(start, self.position),
            "comment",
            text,
        )
    }

    /// Reads the opening section of an interpolated string.
    fn read_interpolated_string_begin(&mut self, start: crate::Position) -> Lexeme<'source> {
        self.bump();
        self.read_interpolated_string_section(
            start,
            TokenKind::InterpStringBegin,
            TokenKind::InterpStringSimple,
        )
    }

    /// Reads an interpolated string section.
    fn read_interpolated_string_section(
        &mut self,
        start: crate::Position,
        format_kind: TokenKind,
        end_kind: TokenKind,
    ) -> Lexeme<'source> {
        let content_start = self.offset;

        while let Some(byte) = self.peek_byte() {
            if byte == b'\\' {
                if self.peek_byte_at(1) == Some(b'u') && self.peek_byte_at(2) == Some(b'{') {
                    self.bump();
                    self.bump();
                    self.bump();
                } else {
                    self.bump_backslash_in_string();
                }
                continue;
            }

            match byte {
                b'`' => {
                    let content_end = self.offset;
                    self.bump();
                    let text = &self.source[content_start..content_end];
                    return Lexeme::with_derived_text(
                        end_kind,
                        Location::new(start, self.position),
                        text,
                    );
                }
                b'{' if self.peek_byte_at(1) == Some(b'{') => {
                    let brace_position = self.position;
                    let text = &self.source[content_start..self.offset];
                    self.brace_stack.push(BraceType::InterpolatedString);
                    self.bump();
                    self.bump();
                    return Lexeme::with_text(
                        TokenKind::BrokenInterpDoubleBrace,
                        Location::new(start, brace_position),
                        "'{{', which is invalid (did you mean '\\{'?)",
                        text,
                    );
                }
                b'{' => {
                    self.brace_stack.push(BraceType::InterpolatedString);
                    self.bump();
                    let text = &self.source[content_start..self.offset - 1];
                    return Lexeme::with_derived_text(
                        format_kind,
                        Location::new(start, self.position),
                        text,
                    );
                }
                b'\n' | b'\r' => {
                    return Lexeme::new(
                        TokenKind::BrokenString,
                        Location::new(start, self.position),
                        "malformed string",
                    );
                }
                _ => {
                    self.bump();
                }
            }
        }

        Lexeme::new(
            TokenKind::BrokenString,
            Location::new(start, self.position),
            "malformed string",
        )
    }

    /// Consumes an escape sequence while scanning a string token.
    fn bump_backslash_in_string(&mut self) {
        self.bump();
        match self.peek_byte() {
            Some(b'\r') => {
                self.bump();
                if self.peek_byte() == Some(b'\n') {
                    self.bump();
                }
            }
            Some(b'z') => {
                self.bump();
                while self.peek_byte().is_some_and(is_space_byte) {
                    self.bump();
                }
            }
            Some(_) => {
                self.bump();
            }
            None => {}
        }
    }

    /// Reads a long-bracket string.
    fn read_long_string(&mut self, start: crate::Position) -> Lexeme<'source> {
        self.read_long_body(start, TokenKind::RawString, "malformed string")
    }

    /// Reads a long-bracket token body.
    fn read_long_body(
        &mut self,
        start: crate::Position,
        kind: TokenKind,
        broken_display: &'static str,
    ) -> Lexeme<'source> {
        let Some(depth) = long_separator_depth(self.visible_bytes(), self.offset) else {
            return Lexeme::new(
                TokenKind::Error,
                Location::new(start, self.position),
                "<error>",
            );
        };
        for _ in 0..depth + 2 {
            self.bump();
        }

        let content_start = self.offset;
        while self.offset < self.visible_bytes().len() {
            if closing_long_separator_at(self.visible_bytes(), self.offset, depth) {
                let content_end = self.offset;
                for _ in 0..depth + 2 {
                    self.bump();
                }
                let text = &self.source[content_start..content_end];
                let mut token = if kind == TokenKind::RawString {
                    Lexeme::with_derived_text(kind, Location::new(start, self.position), text)
                } else {
                    Lexeme::with_text(kind, Location::new(start, self.position), "<unknown>", text)
                };
                token.block_depth = Some(depth as u32);
                return token;
            }
            self.bump();
        }

        Lexeme::new(
            if kind == TokenKind::RawString {
                TokenKind::BrokenString
            } else {
                TokenKind::BrokenComment
            },
            Location::new(start, self.position),
            broken_display,
        )
    }

    /// Returns the current byte.
    fn peek_byte(&self) -> Option<u8> {
        self.visible_bytes().get(self.offset).copied()
    }

    /// Returns a byte ahead of the current offset.
    fn peek_byte_at(&self, lookahead: usize) -> Option<u8> {
        self.visible_bytes()
            .get(self.offset.checked_add(lookahead)?)
            .copied()
    }

    /// Returns source bytes visible to Luau before the first NUL sentinel.
    fn visible_bytes(&self) -> &[u8] {
        &self.source.as_bytes()[..self.source_end]
    }

    /// Consumes one byte and advances the source position.
    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek_byte()?;
        self.offset += 1;
        if byte == b'\n' {
            self.position.line = self.position.line.wrapping_add(1);
            self.position.column = 0;
        } else {
            self.position.column = self.position.column.wrapping_add(1);
        }
        Some(byte)
    }
}

impl LexerOptions {
    /// Upstream default lexer options.
    pub const DEFAULT: Self = Self {
        skip_comments: false,
        read_names: true,
    };
}

/// Returns whether a byte starts an identifier.
fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

/// Returns whether a byte continues an identifier.
fn is_name_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Returns the long-bracket separator depth at an offset.
fn long_separator_depth(bytes: &[u8], offset: usize) -> Option<usize> {
    if bytes.get(offset) != Some(&b'[') {
        return None;
    }

    let mut cursor = offset + 1;
    while bytes.get(cursor) == Some(&b'=') {
        cursor += 1;
    }

    if bytes.get(cursor) == Some(&b'[') {
        Some(cursor - offset - 1)
    } else {
        None
    }
}

/// Returns whether a closing long-bracket separator appears at an offset.
fn closing_long_separator_at(bytes: &[u8], offset: usize, depth: usize) -> bool {
    if bytes.get(offset) != Some(&b']') {
        return false;
    }
    let equals_start = offset + 1;
    let equals_end = equals_start + depth;
    bytes
        .get(equals_start..equals_end)
        .is_some_and(|equals| equals.iter().all(|byte| *byte == b'='))
        && bytes.get(equals_end) == Some(&b']')
}

/// Returns whether a byte is Luau string whitespace.
fn is_space_byte(byte: u8) -> bool {
    matches!(byte, b' ' | b'\n' | b'\r' | b'\t' | 0x0b | 0x0c)
}

/// Returns a reserved-word token kind.
fn reserved_word(word: &str) -> Option<(TokenKind, &'static str)> {
    match word {
        "and" => Some((TokenKind::ReservedAnd, "'and'")),
        "break" => Some((TokenKind::ReservedBreak, "'break'")),
        "do" => Some((TokenKind::ReservedDo, "'do'")),
        "else" => Some((TokenKind::ReservedElse, "'else'")),
        "elseif" => Some((TokenKind::ReservedElseif, "'elseif'")),
        "end" => Some((TokenKind::ReservedEnd, "'end'")),
        "false" => Some((TokenKind::ReservedFalse, "'false'")),
        "for" => Some((TokenKind::ReservedFor, "'for'")),
        "function" => Some((TokenKind::ReservedFunction, "'function'")),
        "if" => Some((TokenKind::ReservedIf, "'if'")),
        "in" => Some((TokenKind::ReservedIn, "'in'")),
        "local" => Some((TokenKind::ReservedLocal, "'local'")),
        "nil" => Some((TokenKind::ReservedNil, "'nil'")),
        "not" => Some((TokenKind::ReservedNot, "'not'")),
        "or" => Some((TokenKind::ReservedOr, "'or'")),
        "repeat" => Some((TokenKind::ReservedRepeat, "'repeat'")),
        "return" => Some((TokenKind::ReservedReturn, "'return'")),
        "then" => Some((TokenKind::ReservedThen, "'then'")),
        "true" => Some((TokenKind::ReservedTrue, "'true'")),
        "until" => Some((TokenKind::ReservedUntil, "'until'")),
        "while" => Some((TokenKind::ReservedWhile, "'while'")),
        _ => None,
    }
}

#[cfg(any())]
mod tests {
    use super::{Lexer, LexerOptions, QuoteStyle, TokenKind};
    use crate::{Location, Position};

    #[test]
    fn token_kind_round_trips_named_and_char_tokens() {
        assert_eq!("Eof".parse::<TokenKind>(), Ok(TokenKind::Eof));
        assert_eq!(",".parse::<TokenKind>(), Ok(TokenKind::Char(',')));
        assert_eq!(TokenKind::ReservedNil.as_upstream_str(), "ReservedNil");
        assert_eq!(TokenKind::Char('{').as_upstream_str(), "{");
    }

    #[test]
    fn lexer_keeps_source_and_options() {
        let lexer = Lexer::with_options(
            "return nil",
            LexerOptions {
                skip_comments: true,
                read_names: false,
            },
        );

        assert_eq!(lexer.source(), "return nil");
        assert_eq!(
            lexer.options(),
            LexerOptions {
                skip_comments: true,
                read_names: false
            }
        );
    }

    #[test]
    fn lexer_reads_names_reserved_words_numbers_and_chars() {
        let mut lexer = Lexer::new("local answer = 42");

        assert_eq!(lexer.next_token().kind, TokenKind::ReservedLocal);

        let name = lexer.next_token();
        assert_eq!(name.kind, TokenKind::Name);
        assert_eq!(name.name.as_deref(), Some("answer"));

        assert_eq!(lexer.next_token().kind, TokenKind::Char('='));

        let number = lexer.next_token();
        assert_eq!(number.kind, TokenKind::Number);
        assert_eq!(number.text.as_deref(), Some("42"));

        assert_eq!(lexer.next_token().kind, TokenKind::Eof);
    }

    #[test]
    fn lexer_preserves_upstream_number_literal_spelling() {
        let source = concat!(
            "return ",
            "1, .5, 1.5, 1e-5, 1.5e-5, 12_345.1_25, ",
            "0xab, 0XAB05, 0xff_ff, 0b101010, ",
            "1i, 0xabi, 0b101i",
        );

        assert_eq!(
            number_texts(source),
            [
                "1",
                ".5",
                "1.5",
                "1e-5",
                "1.5e-5",
                "12_345.1_25",
                "0xab",
                "0XAB05",
                "0xff_ff",
                "0b101010",
                "1i",
                "0xabi",
                "0b101i",
            ]
        );
    }

    #[test]
    fn lexer_preserves_malformed_number_spelling_as_number_tokens() {
        let source = concat!(
            "return ",
            "0b123, 123x, 0xg, 0x0x123, ",
            "0xffffffffffffffffffffllllllg, 123ii, 1e+, 1e-",
        );

        assert_eq!(
            number_texts(source),
            [
                "0b123",
                "123x",
                "0xg",
                "0x0x123",
                "0xffffffffffffffffffffllllllg",
                "123ii",
                "1e+",
                "1e-",
            ]
        );
    }

    #[test]
    fn lexer_tracks_byte_columns_through_utf8_string_source() {
        let mut lexer = Lexer::new("\"こんにちは\"");
        let token = lexer.next_token();

        assert_eq!(token.kind, TokenKind::QuotedString);
        assert_eq!(token.quote_style, Some(QuoteStyle::Double));
        assert_eq!(
            token.location,
            Location::new(Position::new(0, 0), Position::new(0, 17))
        );
    }

    #[test]
    fn lexer_can_skip_or_keep_comments() {
        let mut keep = Lexer::new("-- hello\nnil");
        assert_eq!(keep.next_token().kind, TokenKind::Comment);
        assert_eq!(keep.next_token().kind, TokenKind::ReservedNil);

        let mut skip = Lexer::with_options(
            "-- hello\nnil",
            LexerOptions {
                skip_comments: true,
                read_names: true,
            },
        );
        assert_eq!(skip.next_token().kind, TokenKind::ReservedNil);
    }

    #[test]
    fn lexer_treats_nul_as_source_end() {
        let mut lexer = Lexer::new("nil\0return true");
        assert_eq!(lexer.next_token().kind, TokenKind::ReservedNil);

        let eof = lexer.next_token();
        assert_eq!(eof.kind, TokenKind::Eof);
        assert_eq!(
            eof.location,
            Location::new(Position::new(0, 3), Position::new(0, 3))
        );
    }

    #[test]
    fn lexer_stops_line_comments_at_nul() {
        let mut lexer = Lexer::new("-- hello\0\nreturn true");
        let comment = lexer.next_token();

        assert_eq!(comment.kind, TokenKind::Comment);
        assert_eq!(comment.text.as_deref(), Some(" hello"));
        assert_eq!(lexer.next_token().kind, TokenKind::Eof);
    }

    fn number_texts(source: &str) -> Vec<String> {
        let mut lexer = Lexer::new(source);
        let mut numbers = Vec::new();

        loop {
            let token = lexer.next_token();
            if token.kind == TokenKind::Number {
                numbers.push(
                    token
                        .text
                        .expect("number tokens carry source text")
                        .into_owned(),
                );
            }
            if token.kind == TokenKind::Eof {
                break;
            }
        }

        numbers
    }
}
