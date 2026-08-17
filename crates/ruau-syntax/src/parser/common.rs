//! Shared parser helpers and lexer utilities.

use std::sync::Arc;

use super::Parser;
use crate::{
    Location, Position,
    lexer::{Lexeme, TokenKind},
    parse::{Error, ErrorKind, HotComment, comment_from_token},
    syntax::{
        ArgumentName, Attribute, BinaryOp, CompoundAssignOp, Expr, GenericType, GenericTypePack,
        IndexOp, Local, LocalId, Name, Number, Stat, SyntaxId, Type, TypeList, TypePack,
        TypeParameter, UnaryOp,
    },
};

impl<'source> Parser<'source> {
    /// Allocates a fresh local binding.
    pub(crate) fn fresh_local(
        &mut self,
        name: Name,
        location: Option<Location>,
        annotation: Option<Box<Type>>,
        is_const: bool,
        function_depth: usize,
    ) -> Local {
        let id = LocalId::new(self.next_local_id);
        self.next_local_id += 1;
        Local {
            id,
            name,
            location,
            annotation: annotation.map(Arc::from),
            is_const,
            function_depth,
        }
    }

    /// Allocates a fresh expression/type syntax id.
    pub(crate) fn fresh_syntax_id(&mut self) -> SyntaxId {
        let id = SyntaxId::new(self.next_syntax_id);
        self.next_syntax_id += 1;
        id
    }

    /// Builds the synthetic number index type used by upstream array shorthand.
    pub(crate) fn number_type_at(&mut self, location: Location) -> Type {
        Type::Reference {
            syntax_id: self.fresh_syntax_id(),
            location: Some(location),
            prefix: None,
            prefix_location: None,
            prefix_local: None,
            name: Name::new("number"),
            name_location: Some(location),
            parameters: Vec::new(),
        }
    }

    /// Builds a recoverable type error node at `location` with a message index.
    pub(crate) fn type_error_at_message(
        &mut self,
        location: Location,
        message_index: usize,
    ) -> Type {
        self.type_error_at_message_optional(location, Some(message_index))
    }

    /// Builds a recoverable type error node at `location`.
    pub(crate) fn type_error_at_message_optional(
        &mut self,
        location: Location,
        message_index: Option<usize>,
    ) -> Type {
        Type::Error {
            syntax_id: self.fresh_syntax_id(),
            location: Some(location),
            types: Vec::new(),
            message_index,
        }
    }

    /// Builds an expression error at a diagnostic location.
    pub(crate) fn error_expr_at(&mut self, location: Location, message_index: usize) -> Expr {
        Expr::Error {
            syntax_id: self.fresh_syntax_id(),
            location: Some(location),
            expressions: Vec::new(),
            message_index: Some(message_index),
        }
    }

    /// Records the recursion-limit error and returns its message index.
    pub(crate) fn record_recursion_limit(&mut self, what: &str) -> usize {
        self.errors.push(Error {
            kind: ErrorKind::MalformedSyntax,
            message: format!("Exceeded allowed recursion depth; simplify your {what} to compile"),
            location: self.current.location,
        })
    }

    /// The recovered expression returned when expression nesting hits the recursion limit.
    pub(crate) fn recursion_limit_error_expr(&mut self) -> Expr {
        let location = self.current.location;
        let message_index = self.record_recursion_limit("expression");
        self.skip_to_eof();
        self.error_expr_at(location, message_index)
    }

    /// The recovered type returned when type nesting hits the recursion limit.
    pub(crate) fn recursion_limit_error_type(&mut self) -> Type {
        let location = self.current.location;
        let message_index = self.record_recursion_limit("type annotation");
        self.skip_to_eof();
        self.type_error_at_message(location, message_index)
    }

    /// The recovered block returned when statement nesting hits the recursion limit.
    pub(crate) fn recursion_limit_error_block(&mut self) -> Stat {
        let location = self.current.location;
        self.record_recursion_limit("program");
        self.skip_to_eof();
        Stat::Block {
            location: Some(location),
            has_end: false,
            is_do: false,
            body: Vec::new(),
        }
    }

    /// Builds the nested expression-error shape used for recovered lvalues.
    pub(crate) fn wrapped_error_expr_at(
        &mut self,
        location: Location,
        message_index: usize,
    ) -> Expr {
        let inner = self.error_expr_at(location, message_index);
        Expr::Error {
            syntax_id: self.fresh_syntax_id(),
            location: Some(location),
            expressions: vec![inner],
            message_index: Some(message_index),
        }
    }

    /// Builds a function type whose return type is a recoverable parser error.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn function_type_with_error_return(
        &mut self,
        start: Position,
        attributes: Vec<Attribute>,
        generics: Vec<GenericType>,
        generic_packs: Vec<GenericTypePack>,
        arg_types: TypeList,
        arg_names: Vec<Option<ArgumentName>>,
        return_location: Location,
        message_index: usize,
    ) -> Type {
        let return_error = self.type_error_at_message(return_location, message_index);
        Type::Function {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(start, return_location.end)),
            attributes,
            generics,
            generic_packs,
            arg_types,
            arg_names,
            return_types: TypePack::Explicit {
                location: Some(return_location),
                type_list: TypeList::new(vec![return_error]),
            },
        }
    }

    /// Consumes and returns the current token, leaving the next significant
    /// token in `self.current`.
    pub(crate) fn advance(&mut self) -> Lexeme<'source> {
        let previous = std::mem::replace(&mut self.current, self.lexer.next_token());
        while self.capture_or_skip_comment() {
            self.current = self.lexer.next_token();
        }
        previous
    }

    /// Skips or captures current comments.
    pub(crate) fn skip_comments(&mut self) {
        while self.capture_or_skip_comment() {
            self.current = self.lexer.next_token();
        }
        self.hot_comment_header = false;
    }

    /// Consumes the rest of the source after a terminal recovery error.
    pub(crate) fn skip_to_eof(&mut self) {
        while self.current.kind != TokenKind::Eof {
            self.advance();
        }
    }

    /// Captures or skips the current token if it is a comment.
    pub(crate) fn capture_or_skip_comment(&mut self) -> bool {
        match self.current.kind {
            TokenKind::Comment | TokenKind::BlockComment => {
                if self.current.kind == TokenKind::Comment {
                    self.capture_hot_comment();
                }
                if self.options.capture_comments {
                    self.comments.push(comment_from_token(self.current.clone()));
                }
                true
            }
            TokenKind::BrokenComment => false,
            _ => false,
        }
    }

    /// Captures a hot-comment directive from the current line comment.
    pub(crate) fn capture_hot_comment(&mut self) {
        let Some(text) = self.current.text.as_deref() else {
            return;
        };
        let Some(content) = text.strip_prefix('!') else {
            return;
        };

        self.hot_comments.push(HotComment {
            header: self.hot_comment_header,
            location: self.current.location,
            content: content.trim_end().to_owned(),
        });
    }

    /// Consumes a specific character token.
    pub(crate) fn consume_char(&mut self, expected: char) -> Option<Lexeme<'source>> {
        if self.current.kind == TokenKind::Char(expected) {
            let token = self.advance();
            Some(token)
        } else {
            None
        }
    }

    /// Expects a specific character token.
    pub(crate) fn expect_char(&mut self, expected: char) -> Option<Lexeme<'source>> {
        let token = self.consume_char(expected);
        if token.is_none() {
            self.push_expected_token(format!("expected '{expected}'"), self.current.location);
        }
        token
    }

    /// Expects `)` after a function type argument list.
    pub(crate) fn expect_function_type_close(&mut self, open: Position) -> Option<Lexeme<'source>> {
        let token = self.consume_char(')');
        if token.is_none() {
            self.push_expected_token(
                format!(
                    "Expected ')' (to close '(' at {}), got {}",
                    opening_position_description_for(open, &self.current),
                    self.current.display()
                ),
                self.current.location,
            );
        }
        token
    }

    /// Expects a closing character token with an upstream opener reference.
    pub(crate) fn expect_char_to_close(
        &mut self,
        expected: char,
        opener: &str,
        open: Position,
    ) -> Option<Lexeme<'source>> {
        let token = self.consume_char(expected);
        if token.is_none() {
            self.push_expected_token(
                format!(
                    "Expected '{expected}' (to close {opener} at {}), got {}",
                    opening_position_description_for(open, &self.current),
                    self.current.display()
                ),
                self.current.location,
            );
            if self.peek_significant_kind() == TokenKind::Char(expected) {
                self.advance();
                return self.consume_char(expected);
            }
        }
        token
    }

    /// Expects the closing `)` for an expression group.
    pub(crate) fn expect_expression_group_close(
        &mut self,
        open: Position,
    ) -> Option<Lexeme<'source>> {
        let token = self.consume_char(')');
        if token.is_none() {
            let hint = if self.current.kind == TokenKind::Char('=') {
                "; did you mean to use '{' when defining a table?"
            } else {
                ""
            };
            self.push_expected_token(
                format!(
                    "Expected ')' (to close '(' at {}), got {}{hint}",
                    opening_position_description_for(open, &self.current),
                    self.current.display()
                ),
                self.current.location,
            );
            if self.peek_significant_kind() == TokenKind::Char(')') {
                self.advance();
                return self.consume_char(')');
            }
        }
        token
    }

    /// Expects the opening `(` for a function declaration or expression.
    pub(crate) fn expect_function_open_or_skip_extra(&mut self) -> Option<Lexeme<'source>> {
        if let Some(token) = self.consume_char('(') {
            return Some(token);
        }

        let token = self.current.clone();
        self.push_expected_token(
            format!(
                "Expected '(' when parsing function, got {}",
                self.current.display()
            ),
            token.location,
        );

        if self.peek_significant_kind() == TokenKind::Char('(') {
            self.advance();
            self.consume_char('(')
        } else {
            None
        }
    }

    /// Expects a specific token kind.
    pub(crate) fn expect_token(
        &mut self,
        expected: TokenKind,
        display: &str,
    ) -> Option<Lexeme<'source>> {
        if self.current.kind == expected {
            let token = self.advance();
            Some(token)
        } else {
            self.push_expected_token(format!("expected {display}"), self.current.location);
            None
        }
    }

    /// Returns an upstream-style hint for an inner block likely left unclosed.
    pub(crate) fn nesting_hint(&self, opener: &str) -> String {
        let Ok(source) = std::str::from_utf8(self.source_bytes) else {
            return String::new();
        };
        let Some(position) = likely_unclosed_line(source, opener) else {
            return String::new();
        };
        format!(
            "; did you forget to close '{opener}' at {}?",
            opening_position_description(position)
        )
    }

    /// Skips forward to a character token on the same line and consumes it.
    pub(crate) fn recover_to_char_on_line(
        &mut self,
        expected: char,
        line: u32,
    ) -> Option<Lexeme<'source>> {
        while self.current.kind != TokenKind::Eof
            && self.current.location.begin.line == line
            && self.current.kind != TokenKind::Char(expected)
        {
            self.advance();
        }
        self.consume_char(expected)
    }

    /// Returns whether a character token is present before this source line ends.
    pub(crate) fn has_char_on_line(&self, expected: char, line: u32) -> bool {
        let mut current = self.current.clone();
        let mut lexer = self.lexer.clone();
        while current.kind != TokenKind::Eof && current.location.begin.line == line {
            if current.kind == TokenKind::Char(expected) {
                return true;
            }
            current = lexer.next_token();
        }
        false
    }

    /// Pushes an expected-token diagnostic, coalescing EOF cascades at one point.
    pub(crate) fn push_expected_token(&mut self, message: String, location: Location) -> usize {
        if location.begin == location.end
            && self.current.kind == TokenKind::Eof
            && let Some((index, _)) = self.errors.iter().enumerate().find(|(_, error)| {
                error.kind == ErrorKind::ExpectedToken && error.location == location
            })
        {
            return index;
        }

        self.errors.push(Error {
            kind: ErrorKind::ExpectedToken,
            message,
            location,
        })
    }

    /// Records a parse error unless an earlier error used the same source range.
    pub(crate) fn push_error_dedup(&mut self, error: Error) -> usize {
        if let Some(index) = self
            .errors
            .iter()
            .position(|existing| existing.location == error.location)
        {
            return index;
        }
        self.errors.push(error)
    }

    /// Returns the index of an already-recorded diagnostic at `location`.
    pub(crate) fn error_index_at(&self, location: Location) -> Option<usize> {
        self.errors
            .iter()
            .position(|existing| existing.location == location)
    }

    /// Consumes an `end` token or records an error.
    pub(crate) fn consume_end_or_report(&mut self) -> Position {
        if self.current.kind == TokenKind::ReservedEnd {
            let token = self.advance();
            token.location.end
        } else {
            self.push_expected_token("expected 'end'".to_owned(), self.current.location);
            self.current.location.begin
        }
    }

    /// Consumes a class `end` token or records the upstream class-specific error.
    pub(crate) fn consume_class_end_or_report(&mut self) -> Position {
        if self.current.kind == TokenKind::ReservedEnd {
            let token = self.advance();
            token.location.end
        } else {
            self.push_expected_token(
                format!(
                    "Expected 'end' when parsing class, got {}",
                    self.current.display()
                ),
                self.current.location,
            );
            self.current.location.begin
        }
    }

    /// Returns the next non-comment token kind without consuming it.
    pub(crate) fn peek_significant_kind(&self) -> TokenKind {
        self.peek_significant().kind
    }

    /// Returns the next non-comment token name without consuming it.
    pub(crate) fn peek_significant_name(&self) -> Option<String> {
        self.peek_significant()
            .name
            .map(std::borrow::Cow::into_owned)
    }

    /// Returns the next non-comment token without consuming it.
    pub(crate) fn peek_significant(&self) -> Lexeme<'source> {
        let mut lexer = self.lexer.clone();
        loop {
            let token = lexer.next_token();
            if !matches!(token.kind, TokenKind::Comment | TokenKind::BlockComment) {
                return token;
            }
        }
    }

    /// Returns the next raw token without consuming it.
    pub(crate) fn peek_raw(&self) -> Lexeme<'source> {
        let mut lexer = self.lexer.clone();
        lexer.next_token()
    }

    /// Returns whether the next significant token can follow a bare statement.
    pub(crate) fn peek_starts_statement_terminator(&self) -> bool {
        matches!(
            self.peek_significant_kind(),
            TokenKind::Eof
                | TokenKind::ReservedEnd
                | TokenKind::ReservedUntil
                | TokenKind::ReservedElse
                | TokenKind::ReservedElseif
                | TokenKind::Char(';')
        )
    }

    /// Converts a string-like token into upstream AST string text.
    pub(crate) fn string_value_from_token(&self, token: &Lexeme) -> Option<String> {
        match token.kind {
            TokenKind::QuotedString => self
                .original_token_bytes(token, 1, 1)
                .and_then(fixup_quoted_string_bytes)
                .map(|bytes| ast_string_from_bytes(&bytes)),
            TokenKind::RawString => {
                let trim = token.block_depth.unwrap_or(0) as usize + 2;
                self.original_token_bytes(token, trim, trim)
                    .map(fixup_multiline_string_bytes)
                    .map(|bytes| ast_string_from_bytes(&bytes))
            }
            TokenKind::InterpStringBegin
            | TokenKind::InterpStringMid
            | TokenKind::InterpStringEnd
            | TokenKind::InterpStringSimple => string_value_from_token_text(token),
            _ => None,
        }
    }

    /// Returns original source bytes for a token, trimming delimiter bytes.
    pub(crate) fn original_token_bytes(
        &self,
        token: &Lexeme,
        trim_start: usize,
        trim_end: usize,
    ) -> Option<&[u8]> {
        let start = self.position_to_offset(token.location.begin)?;
        let end = self.position_to_offset(token.location.end)?;
        let start = start.checked_add(trim_start)?;
        let end = end.checked_sub(trim_end)?;
        (start <= end && end <= self.source_bytes.len()).then_some(&self.source_bytes[start..end])
    }

    /// Converts an upstream byte-column position into an absolute byte offset.
    pub(crate) fn position_to_offset(&self, position: Position) -> Option<usize> {
        let line_start = *self.line_starts.get(position.line as usize)?;
        line_start.checked_add(position.column as usize)
    }

    /// Returns Luau's diagnostic range for a missing interpolated-string `}`.
    pub(crate) fn missing_interpolation_curly_location(
        &self,
        expr_end: Position,
        current: Location,
        current_kind: TokenKind,
    ) -> Option<Location> {
        if current_kind == TokenKind::Eof || current.begin.line > expr_end.line {
            return self.previous_non_newline_byte_location(current.begin);
        }
        if current.begin.line == expr_end.line
            && current.begin.column == expr_end.column.saturating_add(1)
        {
            return Some(Location::new(
                expr_end,
                Position::new(expr_end.line, expr_end.column.saturating_add(1)),
            ));
        }
        None
    }

    /// Returns the upstream hint for the missing interpolated-string delimiter.
    pub(crate) fn missing_interpolation_delimiter_message(
        &self,
        location: Location,
    ) -> &'static str {
        if self
            .position_to_offset(location.begin)
            .and_then(|offset| self.source_bytes.get(offset))
            == Some(&b'}')
        {
            "Malformed interpolated string; did you forget to add a '`'?"
        } else {
            "Malformed interpolated string; did you forget to add a '}'?"
        }
    }

    /// Returns the one-byte location before `position`, skipping line endings.
    pub(crate) fn previous_non_newline_byte_location(
        &self,
        position: Position,
    ) -> Option<Location> {
        let mut offset = self.position_to_offset(position)?;
        while offset > 0 {
            offset -= 1;
            if matches!(self.source_bytes.get(offset), Some(b'\n' | b'\r')) {
                continue;
            }
            let begin = self.offset_to_position(offset)?;
            let end = Position::new(begin.line, begin.column.saturating_add(1));
            return Some(Location::new(begin, end));
        }
        None
    }

    /// Returns the one-byte location before `position`, skipping ASCII whitespace.
    pub(crate) fn previous_non_whitespace_byte_location(
        &self,
        position: Position,
    ) -> Option<Location> {
        let mut offset = self.position_to_offset(position)?;
        while offset > 0 {
            offset -= 1;
            if matches!(
                self.source_bytes.get(offset),
                Some(b'\n' | b'\r' | b'\t' | b' ')
            ) {
                continue;
            }
            let begin = self.offset_to_position(offset)?;
            let end = Position::new(begin.line, begin.column.saturating_add(1));
            return Some(Location::new(begin, end));
        }
        None
    }

    /// Returns the horizontal whitespace byte immediately before `position`.
    pub(crate) fn previous_horizontal_whitespace_location(
        &self,
        position: Position,
    ) -> Option<Location> {
        let offset = self.position_to_offset(position)?.checked_sub(1)?;
        if !matches!(self.source_bytes.get(offset), Some(b' ' | b'\t')) {
            return None;
        }
        let begin = self.offset_to_position(offset)?;
        (begin.line == position.line).then(|| {
            Location::new(
                begin,
                Position::new(begin.line, begin.column.saturating_add(1)),
            )
        })
    }

    /// Converts an absolute byte offset into an upstream byte-column position.
    pub(crate) fn offset_to_position(&self, offset: usize) -> Option<Position> {
        if offset > self.source_bytes.len() {
            return None;
        }
        let line = match self.line_starts.binary_search(&offset) {
            Ok(line) => line,
            Err(0) => 0,
            Err(line) => line - 1,
        };
        let column = offset.checked_sub(*self.line_starts.get(line)?)?;
        Some(Position::new(
            u32::try_from(line).ok()?,
            u32::try_from(column).ok()?,
        ))
    }
}

/// Parses a finite number token.
pub fn parse_number(text: &str) -> Number {
    let cleaned = text.replace('_', "");
    let number = if let Some(binary) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        number_from_luau_integer(u64::from_str_radix(binary, 2).unwrap_or(u64::MAX))
    } else if let Some(hex) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        number_from_luau_integer(u64::from_str_radix(hex, 16).unwrap_or(u64::MAX))
    } else if cleaned.contains(['.', 'e', 'E']) {
        if let Ok(value) = cleaned.parse::<f64>() {
            if value.is_infinite() && value.is_sign_positive() {
                return Number::Infinity;
            }
            if value.is_infinite() && value.is_sign_negative() {
                return Number::NegativeInfinity;
            }
            serde_json::Number::from_f64(value)
        } else {
            None
        }
    } else {
        cleaned
            .parse::<u64>()
            .ok()
            .and_then(number_from_luau_integer)
            .or_else(|| {
                cleaned
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
            })
    };

    number
        .map(|number| Number::from_json_number(&number))
        .unwrap_or_else(|| Number::finite(0.0).expect("zero is finite"))
}

/// Parses an integer literal token with an upstream `i` suffix.
pub fn parse_integer_literal(text: &str) -> Option<i64> {
    let cleaned = text.replace('_', "");
    let literal = cleaned
        .strip_suffix('i')
        .or_else(|| cleaned.strip_suffix('I'))?;
    let value = if let Some(binary) = literal
        .strip_prefix("0b")
        .or_else(|| literal.strip_prefix("0B"))
    {
        u64::from_str_radix(binary, 2).ok()?
    } else if let Some(hex) = literal
        .strip_prefix("0x")
        .or_else(|| literal.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).ok()?
    } else {
        return literal.parse::<i64>().ok();
    };
    Some(value as i64)
}

/// Returns whether a number token spelling is invalid Luau numeric syntax.
pub fn number_literal_is_malformed(text: &str) -> bool {
    let cleaned = text.replace('_', "");
    if cleaned.is_empty() || cleaned.ends_with(['i', 'I']) {
        return true;
    }

    if let Some(binary) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        return binary.is_empty() || !binary.bytes().all(|byte| matches!(byte, b'0' | b'1'));
    }

    if let Some(hex) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        return hex.is_empty() || !hex.bytes().all(|byte| byte.is_ascii_hexdigit());
    }

    cleaned.parse::<f64>().is_err()
}

/// Returns absolute byte offsets for the start of each source line.
pub fn source_line_starts(source: &[u8]) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in source.iter().enumerate() {
        if *byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

/// Converts token text into upstream AST string bytes encoded as JSON text.
pub fn string_value_from_token_text(token: &Lexeme) -> Option<String> {
    let text = token.text.as_deref().unwrap_or_default();
    match token.kind {
        TokenKind::QuotedString
        | TokenKind::InterpStringBegin
        | TokenKind::InterpStringMid
        | TokenKind::InterpStringEnd
        | TokenKind::InterpStringSimple => {
            fixup_quoted_string_bytes(text.as_bytes()).map(|bytes| ast_string_from_bytes(&bytes))
        }
        TokenKind::RawString => Some(ast_string_from_bytes(&fixup_multiline_string_bytes(
            text.as_bytes(),
        ))),
        _ => None,
    }
}

/// Returns the upstream diagnostic range for malformed string escapes when known.
pub fn malformed_string_escape_location(token: &Lexeme) -> Option<Location> {
    let text = token.text.as_deref()?;
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index + 1 < bytes.len() {
        if bytes[index] != b'\\' {
            index += 1;
            continue;
        }

        if bytes[index + 1] == b'0' {
            let column_offset = u32::try_from(index + 1).unwrap_or(u32::MAX);
            let begin = Position::new(
                token.location.begin.line,
                token.location.begin.column.saturating_add(column_offset),
            );
            let end = Position::new(begin.line, begin.column + 1);
            return Some(Location::new(begin, end));
        }

        index += 2;
    }
    None
}

/// Applies Luau quoted-string escape fixups.
pub fn fixup_quoted_string_bytes(data: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(data.len());
    let mut index = 0usize;

    while index < data.len() {
        let byte = data[index];
        if byte != b'\\' {
            output.push(byte);
            index += 1;
            continue;
        }

        let escape = *data.get(index + 1)?;
        index += 2;
        match escape {
            b'\n' => output.push(b'\n'),
            b'\r' => {
                output.push(b'\n');
                if data.get(index) == Some(&b'\n') {
                    index += 1;
                }
            }
            0 => return None,
            b'x' => {
                let high = hex_value(*data.get(index)?)?;
                let low = hex_value(*data.get(index + 1)?)?;
                output.push(high * 16 + low);
                index += 2;
            }
            b'z' => {
                while data.get(index).is_some_and(|byte| is_luau_space(*byte)) {
                    index += 1;
                }
            }
            b'u' => {
                if data.get(index) != Some(&b'{') {
                    return None;
                }
                index += 1;
                if data.get(index) == Some(&b'}') {
                    return None;
                }

                let mut code = 0u32;
                let mut digits = 0usize;
                while digits < 16 {
                    let byte = *data.get(index)?;
                    if byte == b'}' {
                        break;
                    }
                    code = code
                        .checked_mul(16)?
                        .checked_add(u32::from(hex_value(byte)?))?;
                    index += 1;
                    digits += 1;
                }
                if data.get(index) != Some(&b'}') {
                    return None;
                }
                index += 1;

                encode_luau_codepoint(code, &mut output)?;
            }
            b'0'..=b'9' => {
                let mut code = u32::from(escape - b'0');
                for _ in 0..2 {
                    let Some(byte) = data.get(index).copied() else {
                        break;
                    };
                    if !byte.is_ascii_digit() {
                        break;
                    }
                    code = code * 10 + u32::from(byte - b'0');
                    index += 1;
                }
                if code > u32::from(u8::MAX) {
                    return None;
                }
                output.push(code as u8);
            }
            _ => output.push(unescape_luau_byte(escape)),
        }
    }

    Some(output)
}

/// Applies Luau long-string newline fixups.
pub fn fixup_multiline_string_bytes(data: &[u8]) -> Vec<u8> {
    let mut index = if data.starts_with(b"\r\n") {
        2
    } else if data.starts_with(b"\n") {
        1
    } else {
        0
    };

    let mut output = Vec::with_capacity(data.len().saturating_sub(index));
    while index < data.len() {
        if data.get(index) == Some(&b'\r') && data.get(index + 1) == Some(&b'\n') {
            output.push(b'\n');
            index += 2;
        } else {
            output.push(data[index]);
            index += 1;
        }
    }
    output
}

/// Encodes upstream byte strings into the latest AST JSON string convention.
pub fn ast_string_from_bytes(bytes: &[u8]) -> String {
    let mut remaining = bytes;
    let mut output = String::new();

    while !remaining.is_empty() {
        match str::from_utf8(remaining) {
            Ok(valid) => {
                output.push_str(valid);
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                output.push_str(
                    str::from_utf8(&remaining[..valid_up_to])
                        .expect("valid prefix reported by UTF-8 decoder"),
                );
                let invalid_len = error.error_len().unwrap_or(remaining.len() - valid_up_to);
                for byte in &remaining[valid_up_to..valid_up_to + invalid_len] {
                    output.push('\u{ffff}');
                    output.push_str(&format!("ff{byte:02x}"));
                }
                remaining = &remaining[valid_up_to + invalid_len..];
            }
        }
    }

    output
}

/// Encodes a Luau Unicode escape as bytes, including surrogate codepoints.
pub fn encode_luau_codepoint(code: u32, output: &mut Vec<u8>) -> Option<()> {
    match code {
        0x0000..=0x007f => output.push(code as u8),
        0x0080..=0x07ff => {
            output.push(0xc0 | ((code >> 6) as u8));
            output.push(0x80 | ((code & 0x3f) as u8));
        }
        0x0800..=0xffff => {
            output.push(0xe0 | ((code >> 12) as u8));
            output.push(0x80 | (((code >> 6) & 0x3f) as u8));
            output.push(0x80 | ((code & 0x3f) as u8));
        }
        0x10000..=0x10ffff => {
            output.push(0xf0 | ((code >> 18) as u8));
            output.push(0x80 | (((code >> 12) & 0x3f) as u8));
            output.push(0x80 | (((code >> 6) & 0x3f) as u8));
            output.push(0x80 | ((code & 0x3f) as u8));
        }
        _ => return None,
    }
    Some(())
}

/// Returns a Luau hex-digit value.
pub fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Returns whether a byte is skipped by Luau's `\z` escape.
pub fn is_luau_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\n' | b'\r' | b'\t' | 0x0b | 0x0c)
}

/// Applies simple Luau single-character escapes.
pub fn unescape_luau_byte(byte: u8) -> u8 {
    match byte {
        b'a' => b'\x07',
        b'b' => b'\x08',
        b'f' => b'\x0c',
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b'v' => b'\x0b',
        _ => byte,
    }
}

/// Converts a Luau integer literal into the JSON number shape upstream emits.
pub fn number_from_luau_integer(value: u64) -> Option<serde_json::Number> {
    if value <= (1_u64 << 53) {
        Some(serde_json::Number::from(value))
    } else {
        serde_json::Number::from_f64(value as f64)
    }
}

/// Returns the binary operation and precedence for a token.
pub fn binary_op(kind: TokenKind) -> Option<(BinaryOp, u8, bool)> {
    match kind {
        TokenKind::ReservedOr => Some((BinaryOp::Or, 1, false)),
        TokenKind::ReservedAnd => Some((BinaryOp::And, 2, false)),
        TokenKind::Equal => Some((BinaryOp::CompareEq, 3, false)),
        TokenKind::NotEqual => Some((BinaryOp::CompareNe, 3, false)),
        TokenKind::Char('<') => Some((BinaryOp::CompareLt, 3, false)),
        TokenKind::LessEqual => Some((BinaryOp::CompareLe, 3, false)),
        TokenKind::Char('>') => Some((BinaryOp::CompareGt, 3, false)),
        TokenKind::GreaterEqual => Some((BinaryOp::CompareGe, 3, false)),
        TokenKind::Dot2 => Some((BinaryOp::Concat, 4, true)),
        TokenKind::Char('+') => Some((BinaryOp::Add, 5, false)),
        TokenKind::Char('-') => Some((BinaryOp::Sub, 5, false)),
        TokenKind::Char('*') => Some((BinaryOp::Mul, 6, false)),
        TokenKind::Char('/') => Some((BinaryOp::Div, 6, false)),
        TokenKind::FloorDiv => Some((BinaryOp::FloorDiv, 6, false)),
        TokenKind::Char('%') => Some((BinaryOp::Mod, 6, false)),
        TokenKind::Char('^') => Some((BinaryOp::Pow, 8, true)),
        _ => None,
    }
}

/// Returns the unary operation for a token.
pub fn unary_op(kind: TokenKind) -> Option<UnaryOp> {
    match kind {
        TokenKind::ReservedNot => Some(UnaryOp::Not),
        TokenKind::Char('#') => Some(UnaryOp::Len),
        TokenKind::Char('-') => Some(UnaryOp::Minus),
        _ => None,
    }
}

/// Returns the compound assignment operation for a token.
pub fn compound_assign_op(kind: TokenKind) -> Option<CompoundAssignOp> {
    match kind {
        TokenKind::AddAssign => Some(CompoundAssignOp::Add),
        TokenKind::SubAssign => Some(CompoundAssignOp::Sub),
        TokenKind::MulAssign => Some(CompoundAssignOp::Mul),
        TokenKind::DivAssign => Some(CompoundAssignOp::Div),
        TokenKind::FloorDivAssign => Some(CompoundAssignOp::FloorDiv),
        TokenKind::ModAssign => Some(CompoundAssignOp::Mod),
        TokenKind::PowAssign => Some(CompoundAssignOp::Pow),
        TokenKind::ConcatAssign => Some(CompoundAssignOp::Concat),
        _ => None,
    }
}

/// Extracts the source name from a name token.
pub fn token_name(token: &Lexeme) -> String {
    token.name.as_deref().map_or_else(
        || token.display().trim_matches('\'').to_owned(),
        str::to_owned,
    )
}

/// Returns the upstream expression-start diagnostic for an unexpected token.
pub fn expected_expression_message(token: &Lexeme) -> String {
    match token.kind {
        TokenKind::BrokenComment => {
            "Expected identifier when parsing expression, got unfinished comment".to_owned()
        }
        TokenKind::BrokenUnicode => {
            let Some(codepoint) = token.codepoint else {
                return format!(
                    "Expected identifier when parsing expression, got {}",
                    token.display()
                );
            };
            let hint = if codepoint == 0x2024 {
                " (did you mean '.'?)"
            } else {
                ""
            };
            format!(
                "Expected identifier when parsing expression, got Unicode character U+{codepoint:04X}{hint}"
            )
        }
        _ => format!(
            "Expected identifier when parsing expression, got {}",
            token.display()
        ),
    }
}

/// Returns Luau's diagnostic for an unexpected token where an identifier is required.
pub fn expected_identifier_message(token: &Lexeme, context: Option<&str>) -> String {
    let got = match token.kind {
        TokenKind::Attribute => token
            .name
            .as_ref()
            .map(|name| format!("'{name}'"))
            .unwrap_or_else(|| token.display().into_owned()),
        TokenKind::BrokenUnicode => token
            .codepoint
            .map(|codepoint| format!("Unicode character U+{codepoint:04X}"))
            .unwrap_or_else(|| token.display().into_owned()),
        _ => token.display().into_owned(),
    };

    if let Some(context) = context {
        format!("Expected identifier when parsing {context}, got {got}")
    } else {
        format!("Expected identifier, got {got}")
    }
}

/// Returns Luau's diagnostic for an unexpected token where a type is required.
pub fn expected_type_message(token: &Lexeme) -> String {
    format!("Expected type, got {}", token.display())
}

/// Returns Luau's diagnostic for a missing function-type arrow.
pub fn expected_function_type_arrow_message(token: &Lexeme) -> String {
    format!(
        "Expected '->' when parsing function type, got {}",
        token.display()
    )
}

/// Returns Luau's diagnostic for a trailing comma in a list.
pub fn expected_after_comma_message(item: &str, token: &Lexeme) -> String {
    format!(
        "Expected {item} after ',' but got {} instead",
        token.display()
    )
}

/// Returns Luau's diagnostic for an unexpected token after `.` or `:`.
pub fn expected_index_name_message(token: &Lexeme, op: IndexOp) -> String {
    let context = match op {
        IndexOp::Colon => Some("method name"),
        IndexOp::Dot => None,
    };
    expected_identifier_message(token, context)
}

/// Returns Luau's diagnostic for a missing function-call argument list.
pub fn expected_call_arguments_message(token: &Lexeme) -> String {
    format!(
        "Expected '(', '{{' or <string> when parsing function call, got {}",
        token.display()
    )
}

/// Returns the user-defined class method-name diagnostic for reserved metamethods.
pub fn class_method_name_error(name: &str) -> Option<String> {
    const ALLOWED: &[&str] = &[
        "__call",
        "__concat",
        "__unm",
        "__add",
        "__sub",
        "__mul",
        "__div",
        "__mod",
        "__pow",
        "__tostring",
        "__eq",
        "__lt",
        "__le",
        "__iter",
        "__len",
        "__idiv",
    ];
    const DISALLOWED: &[&str] = &["__index", "__newindex", "__mode", "__metatable", "__type"];

    if name == "new" {
        return Some(
            "Class methods cannot be named 'new'.  Name it '__init' to define a constructor."
                .to_owned(),
        );
    }

    if name == "__init" {
        return None;
    }

    if !name.starts_with("__") {
        return None;
    }

    if DISALLOWED.contains(&name) {
        Some(format!("Classes cannot define '{name}' as a metamethod"))
    } else if !ALLOWED.contains(&name) {
        Some(format!(
            "Cannot use '{name}' as a method name: names starting with '__' are reserved"
        ))
    } else {
        None
    }
}

/// Formats the opener position in upstream parse-error style.
pub fn opening_position_description(position: Position) -> String {
    if position.line == 0 {
        format!("column {}", position.column + 1)
    } else {
        format!("line {}", position.line + 1)
    }
}

/// Formats an opener position relative to the unexpected token being reported.
pub fn opening_position_description_for(position: Position, unexpected: &Lexeme) -> String {
    if unexpected.location.begin.line == position.line {
        format!("column {}", position.column + 1)
    } else {
        format!("line {}", position.line + 1)
    }
}

/// Returns the likely unclosed line for the narrow upstream EOF nesting hint.
pub fn likely_unclosed_line(source: &str, opener: &str) -> Option<Position> {
    let closer = if opener == "repeat" { "until" } else { "end" };
    let mut candidates = Vec::new();
    let mut closers = Vec::new();
    let mut function_boundaries = Vec::new();

    for (line, text) in source.lines().enumerate() {
        let trimmed = text.trim_start();
        let column = (text.len() - trimmed.len()) as u32;
        if starts_with_word(trimmed, "function")
            || trimmed
                .strip_prefix("local ")
                .is_some_and(|rest| starts_with_word(rest, "function"))
        {
            function_boundaries.push(line as u32);
        }
        if starts_with_word(trimmed, opener) || contains_word(trimmed, opener) {
            candidates.push(Position::new(line as u32, column));
        }
        if starts_with_word(trimmed, closer) {
            closers.push(Position::new(line as u32, column));
        }
    }

    candidates.into_iter().find(|candidate| {
        let boundary = function_boundaries
            .iter()
            .copied()
            .filter(|line| *line > candidate.line)
            .min()
            .unwrap_or(u32::MAX);
        !closers.iter().any(|closer| {
            closer.line > candidate.line
                && closer.line < boundary
                && closer.column >= candidate.column
        })
    })
}

/// Returns whether `text` starts with a standalone keyword.
pub fn starts_with_word(text: &str, word: &str) -> bool {
    let Some(rest) = text.strip_prefix(word) else {
        return false;
    };
    !rest
        .chars()
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// Returns whether `text` contains a standalone keyword.
pub fn contains_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(index, _)| {
        let before = text[..index].chars().next_back();
        let after = text[index + word.len()..].chars().next();
        !before.is_some_and(|ch| ch == '_' || ch.is_ascii_alphanumeric())
            && !after.is_some_and(|ch| ch == '_' || ch.is_ascii_alphanumeric())
    })
}

/// Returns whether this token can be a type name in the supported type slice.
pub fn is_type_name_token(token: &Lexeme) -> bool {
    matches!(
        token.kind,
        TokenKind::Name
            | TokenKind::ReservedNil
            | TokenKind::ReservedTrue
            | TokenKind::ReservedFalse
    )
}

/// Returns whether a token is a reserved keyword.
pub fn is_reserved_keyword_token(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::ReservedAnd
            | TokenKind::ReservedBreak
            | TokenKind::ReservedDo
            | TokenKind::ReservedElse
            | TokenKind::ReservedElseif
            | TokenKind::ReservedEnd
            | TokenKind::ReservedFalse
            | TokenKind::ReservedFor
            | TokenKind::ReservedFunction
            | TokenKind::ReservedIf
            | TokenKind::ReservedIn
            | TokenKind::ReservedLocal
            | TokenKind::ReservedNil
            | TokenKind::ReservedNot
            | TokenKind::ReservedOr
            | TokenKind::ReservedRepeat
            | TokenKind::ReservedReturn
            | TokenKind::ReservedThen
            | TokenKind::ReservedTrue
            | TokenKind::ReservedUntil
            | TokenKind::ReservedWhile
    )
}

/// Flattens the matching type sequence for parser output.
pub fn flatten_type_sequence(luau_type: Type, union: bool) -> Vec<Type> {
    match (union, luau_type) {
        (true, Type::Union { types, .. }) | (false, Type::Intersection { types, .. }) => types,
        (_, luau_type) => vec![luau_type],
    }
}

/// Flattens a mixed union/intersection sequence into its leaf type parts.
pub fn flatten_any_type_sequence(types: Vec<Type>) -> Vec<Type> {
    types
        .into_iter()
        .flat_map(|luau_type| match luau_type {
            Type::Union { types, .. } | Type::Intersection { types, .. } => {
                flatten_any_type_sequence(types)
            }
            other => vec![other],
        })
        .collect()
}

/// Extends the recovered type span through an invalid trailing type-pack suffix.
pub fn extend_type_for_unexpected_pack_suffix(luau_type: &mut Type, end: Position) {
    match luau_type {
        Type::Reference { location, .. } => {
            if let Some(location) = location {
                location.end = end;
            }
        }
        Type::Union {
            location, types, ..
        }
        | Type::Intersection {
            location, types, ..
        } => {
            if let Some(location) = location {
                location.end = end;
            }
            if let Some(last) = types.last_mut() {
                extend_type_for_unexpected_pack_suffix(last, end);
            }
        }
        Type::Function { .. }
        | Type::Group { .. }
        | Type::Optional { .. }
        | Type::Table { .. }
        | Type::Typeof { .. }
        | Type::SingletonString { .. }
        | Type::SingletonBool { .. }
        | Type::Error { .. } => {}
    }
}

/// Returns local binding end position.
pub fn local_end(local: &Local) -> Position {
    if let Some(annotation) = &local.annotation {
        if let Type::Error {
            location: Some(location),
            types,
            ..
        } = annotation.as_ref()
            && types.is_empty()
        {
            return location.begin;
        }
        return type_deep_end(annotation);
    }
    local.location.unwrap_or_default().end
}

/// Returns type location.
pub fn type_location(luau_type: &Type) -> Location {
    luau_type.location().unwrap_or_default()
}

/// Returns the furthest syntactic end owned by a type.
pub fn type_deep_end(luau_type: &Type) -> Position {
    let mut end = type_location(luau_type).end;
    match luau_type {
        Type::Typeof { expr, .. } => end = end.max(expr_end(expr)),
        Type::Group { inner, .. } => end = end.max(type_deep_end(inner)),
        Type::Union { types, .. } | Type::Intersection { types, .. } => {
            for item in types {
                end = end.max(type_deep_end(item));
            }
        }
        Type::Function {
            arg_types,
            return_types,
            ..
        } => {
            if let Some(args_end) = type_list_end(arg_types) {
                end = end.max(args_end);
            }
            end = end.max(type_pack_deep_end(return_types));
        }
        Type::Table { props, indexer, .. } => {
            for prop in props {
                end = end.max(prop.location.unwrap_or_default().end);
                end = end.max(type_deep_end(&prop.prop_type));
            }
            if let Some(indexer) = indexer {
                end = end.max(indexer.location.unwrap_or_default().end);
                end = end.max(type_deep_end(&indexer.index_type));
                end = end.max(type_deep_end(&indexer.result_type));
            }
        }
        Type::Error { types, .. } => {
            for item in types {
                end = end.max(type_deep_end(item));
            }
        }
        Type::Reference { .. }
        | Type::Optional { .. }
        | Type::SingletonString { .. }
        | Type::SingletonBool { .. } => {}
    }
    end
}

/// Returns the diagnostic range and recovered error-node range for a missing type.
pub fn unexpected_type_locations(token: &Lexeme) -> (Location, Location) {
    let point = Location::new(token.location.begin, token.location.begin);
    match token.kind {
        TokenKind::Eof | TokenKind::ReservedEnd | TokenKind::Char('=') => (token.location, point),
        TokenKind::Number | TokenKind::SkinnyArrow => {
            let start = Position::new(
                token.location.begin.line,
                token.location.begin.column.saturating_sub(1),
            );
            (
                Location::new(start, token.location.end),
                Location::new(start, token.location.begin),
            )
        }
        _ => (token.location, token.location),
    }
}

/// Returns type-pack location.
pub fn type_pack_location(type_pack: &TypePack) -> Location {
    type_pack.location().unwrap_or_default()
}

/// Returns the furthest syntactic end owned by a type pack.
pub fn type_pack_deep_end(type_pack: &TypePack) -> Position {
    match type_pack {
        TypePack::Explicit {
            location,
            type_list,
        } => {
            let mut end = location.unwrap_or_default().end;
            if let Some(last_type) = type_list.types.last() {
                end = end.max(type_location(last_type).end);
            }
            if let Some(tail) = &type_list.tail_type {
                end = end.max(type_pack_deep_end(tail));
            }
            end
        }
        TypePack::Generic { location, .. } | TypePack::Variadic { location, .. } => {
            location.unwrap_or_default().end
        }
    }
}

/// Returns the end position of a type list.
pub fn type_list_end(type_list: &TypeList) -> Option<Position> {
    type_list
        .tail_type
        .as_deref()
        .map(|tail| type_pack_location(tail).end)
        .or_else(|| type_list.types.last().map(|ty| type_location(ty).end))
}

/// Returns expression location.
pub fn expr_location(expression: &Expr) -> Location {
    expression.location().unwrap_or_default()
}

/// Returns type-parameter end position.
pub fn type_parameter_end(parameter: &TypeParameter) -> Position {
    match parameter {
        TypeParameter::Type(luau_type) => type_location(luau_type).end,
        TypeParameter::Pack(type_pack) => type_pack_location(type_pack).end,
    }
}

/// Returns expression end position.
pub fn expr_end(expression: &Expr) -> Position {
    expr_location(expression).end
}

/// Returns whether an expression can accept call-style suffixes.
pub fn expr_can_be_called(expression: &Expr) -> bool {
    matches!(
        expression,
        Expr::Global { .. }
            | Expr::Local { .. }
            | Expr::Call { .. }
            | Expr::Function { .. }
            | Expr::IndexName { .. }
            | Expr::IndexExpr { .. }
            | Expr::Group { .. }
            | Expr::Instantiate { .. }
            | Expr::Error { .. }
    )
}

/// Returns whether a newline `(` should use Luau's ambiguous-call diagnostic.
pub fn starts_ambiguous_newline_call(expression: &Expr, current: &Lexeme) -> bool {
    current.kind == TokenKind::Char('(')
        && current.location.begin.line > expr_end(expression).line
        && expr_can_be_called(expression)
}

/// Returns whether a method-index expression should use upstream's terse
/// terminator diagnostic for a missing call argument list.
pub fn uses_missing_method_call_arguments_message(expression: &Expr, current: &Lexeme) -> bool {
    method_index_was_consumed(expression) && current.location.begin.line > expr_end(expression).line
        || method_index_was_consumed(expression)
            && current.kind == TokenKind::Eof
            && expr_location(expression).begin.line > 0
}

/// Returns whether a method-index expression consumed an index token after `:`.
pub fn method_index_was_consumed(expression: &Expr) -> bool {
    matches!(
        expression,
        Expr::IndexName {
            index_location: Some(location),
            ..
        } if location.end > location.begin
    )
}

/// Returns the recovery range for a method index missing call arguments.
pub fn missing_call_args_location(expression: &Expr, current: &Lexeme) -> Location {
    let expression_location = expr_location(expression);
    let end = if current.location.begin.line == expression_location.end.line {
        if current.kind == TokenKind::Eof {
            current.location.end
        } else {
            current.location.begin
        }
    } else {
        expression_location.end
    };
    Location::new(expression_location.begin, end)
}

/// Returns the source identifier represented by a simple expression.
pub fn expression_identifier_name(expression: &Expr) -> Option<String> {
    match expression {
        Expr::Local { local, .. } => Some(local.name.as_str().to_owned()),
        Expr::Global { name, .. } => Some(name.as_str().to_owned()),
        _ => None,
    }
}

/// Returns whether an expression is a direct reference to a const local.
pub fn expr_is_const_local(expression: &Expr) -> bool {
    matches!(expression, Expr::Local { local, .. } if local.is_const)
}

/// Returns whether an expression is a valid assignment target.
pub fn expr_is_assignable(expression: &Expr) -> bool {
    matches!(
        expression,
        Expr::Local { .. }
            | Expr::Global { .. }
            | Expr::IndexName { .. }
            | Expr::IndexExpr { .. }
            | Expr::Error { .. }
    )
}

/// Returns whether an expression can provide multiple assignment values.
pub fn expr_may_return_multiple(expression: &Expr) -> bool {
    matches!(expression, Expr::Call { .. } | Expr::Varargs { .. })
}

/// Returns a call close extent, preserving upstream's error-argument recovery.
pub fn call_close_end(args: &[Expr], close: &Lexeme) -> Position {
    if let Some(Expr::Error { location, .. }) = args.last() {
        if *location == Some(close.location) {
            close.location.end
        } else {
            close.location.begin
        }
    } else {
        close.location.end
    }
}

/// Returns whether an attribute name is followed by an argument payload.
pub fn attribute_starts_arguments(token: &Lexeme) -> bool {
    matches!(
        token.kind,
        TokenKind::RawString
            | TokenKind::QuotedString
            | TokenKind::Char('{')
            | TokenKind::Char('(')
    )
}

/// Returns whether a token can begin a table constructor element.
pub fn starts_table_item(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Name
            | TokenKind::Char('[')
            | TokenKind::Char('{')
            | TokenKind::Char('(')
            | TokenKind::ReservedFunction
            | TokenKind::ReservedIf
            | TokenKind::ReservedNil
            | TokenKind::ReservedTrue
            | TokenKind::ReservedFalse
            | TokenKind::Dot3
            | TokenKind::Number
            | TokenKind::QuotedString
            | TokenKind::RawString
            | TokenKind::InterpStringSimple
            | TokenKind::InterpStringBegin
    )
}

/// Builds the upstream recovery statement for an expression that cannot stand
/// alone as a statement.
pub fn stat_error_from_expression(expression: Expr, location: Location) -> Stat {
    Stat::Error {
        location: Some(location),
        expressions: vec![expression],
        statements: Vec::new(),
    }
}

/// Returns whether a statement terminates the current block.
pub fn statement_is_last(statement: &Stat) -> bool {
    matches!(
        statement,
        Stat::Break { .. } | Stat::Continue { .. } | Stat::Return { .. }
    )
}

/// Returns whether the last class member is a function with a missing `end`.
pub fn last_class_member_has_missing_function_end(members: &[Stat]) -> bool {
    let Some(Stat::TypeFunction { func, .. }) = members.last() else {
        return false;
    };
    let Expr::Function { body, .. } = func.as_ref() else {
        return false;
    };
    matches!(body.as_ref(), Stat::Block { has_end: false, .. })
}

/// Returns whether an invalid nested class has JSON-visible member payloads.
pub fn class_has_json_visible_members(class: &Stat) -> bool {
    let Stat::Class {
        super_class,
        members,
        ..
    } = class
    else {
        return false;
    };
    super_class.is_some()
        || members.iter().any(|member| {
            matches!(
                member,
                Stat::ClassProperty {
                    declared_type: Some(_),
                    ..
                } | Stat::TypeFunction { .. }
            )
        })
}

/// Returns statement end position.
pub fn stat_end(statement: &Stat) -> Position {
    match statement {
        Stat::Block { location, .. }
        | Stat::Return { location, .. }
        | Stat::Expr { location, .. }
        | Stat::Local { location, .. }
        | Stat::Assign { location, .. }
        | Stat::CompoundAssign { location, .. }
        | Stat::If { location, .. }
        | Stat::Break { location }
        | Stat::Continue { location }
        | Stat::While { location, .. }
        | Stat::Repeat { location, .. }
        | Stat::For { location, .. }
        | Stat::ForIn { location, .. }
        | Stat::Function { location, .. }
        | Stat::LocalFunction { location, .. }
        | Stat::DeclareGlobal { location, .. }
        | Stat::DeclareFunction { location, .. }
        | Stat::DeclareClass { location, .. }
        | Stat::TypeAlias { location, .. }
        | Stat::TypeFunction { location, .. }
        | Stat::Class { location, .. }
        | Stat::ClassProperty { location, .. }
        | Stat::Error { location, .. } => location.unwrap_or_default().end,
    }
}
