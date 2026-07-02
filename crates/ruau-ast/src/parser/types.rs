//! Parser types parsing.

use super::{
    Parser,
    common::{
        expected_after_comma_message, expected_function_type_arrow_message,
        expected_identifier_message, expected_type_message, expr_end, expr_location,
        extend_type_for_unexpected_pack_suffix, flatten_any_type_sequence, flatten_type_sequence,
        is_reserved_keyword_token, is_type_name_token, malformed_string_escape_location,
        opening_position_description, opening_position_description_for, token_name, type_deep_end,
        type_list_end, type_location, type_pack_location, unexpected_type_locations,
    },
};
use crate::{
    Location, Position,
    lexer::TokenKind,
    parse::{Error, ErrorKind},
    syntax::{
        ArgumentName, Attribute, GenericType, GenericTypePack, Name, TableIndexer, TableProp, Type,
        TypeList, TypePack, TypeParameter,
    },
};

impl<'source> Parser<'source> {
    /// Parses an optional local or argument type annotation.
    pub(crate) fn parse_optional_type_annotation(&mut self) -> Option<Box<Type>> {
        let colon = self.consume_char(':')?;
        if self.current.kind == TokenKind::Eof {
            let location = Location::new(colon.location.end, self.current.location.end);
            let message_index = self.errors.len();
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: expected_type_message(&self.current),
                location,
            });
            return Some(Box::new(
                self.type_error_at_message(location, message_index),
            ));
        }
        Some(Box::new(self.parse_type_expression()))
    }

    /// Parses the currently supported type grammar slice.
    pub(crate) fn parse_type_expression(&mut self) -> Type {
        if self.current.kind == TokenKind::Char('<') {
            let start = self.current.location.begin;
            let (generics, generic_packs) = self.parse_generic_parameters();
            let function_type =
                self.parse_function_type_after_generics(start, generics, generic_packs);
            return self.reject_unexpected_type_pack_suffix(function_type);
        }

        if self.current.kind == TokenKind::Char('|') {
            let luau_type = self.parse_leading_type_sequence(TokenKind::Char('|'), true);
            return self.reject_unexpected_type_pack_suffix(luau_type);
        }

        let left = self.parse_type_intersection();
        let luau_type = self.parse_type_sequence(left, TokenKind::Char('|'), true);
        self.reject_unexpected_type_pack_suffix(luau_type)
    }

    /// Reports a type-pack marker in a context that expects a normal type.
    pub(crate) fn reject_unexpected_type_pack_suffix(&mut self, mut luau_type: Type) -> Type {
        if self.current.kind != TokenKind::Dot3 {
            return luau_type;
        }

        let dots = self.current.clone();
        let message = if matches!(luau_type, Type::Group { .. }) {
            "Unexpected '...' after type annotation"
        } else {
            "Unexpected '...' after type name; type pack is not allowed in this context"
        };
        self.errors.push(Error {
            kind: ErrorKind::ExpectedToken,
            message: message.to_owned(),
            location: dots.location,
        });
        self.advance();

        extend_type_for_unexpected_pack_suffix(&mut luau_type, dots.location.end);

        luau_type
    }

    /// Parses intersection types.
    pub(crate) fn parse_type_intersection(&mut self) -> Type {
        if self.current.kind == TokenKind::Char('&') {
            return self.parse_leading_type_sequence(TokenKind::Char('&'), false);
        }

        let left = self.parse_type_primary_with_optional();
        self.parse_type_sequence(left, TokenKind::Char('&'), false)
    }

    /// Parses a type sequence that starts with its separator.
    pub(crate) fn parse_leading_type_sequence(
        &mut self,
        separator: TokenKind,
        union: bool,
    ) -> Type {
        let start = self.current.location.begin;
        let mut types = Vec::new();

        loop {
            self.advance();
            let item = if union {
                self.parse_type_intersection()
            } else {
                self.parse_type_primary_with_optional()
            };
            types.extend(flatten_type_sequence(item, union));

            if self.current.kind != separator {
                break;
            }
        }

        let end = types
            .last()
            .map_or(start, |luau_type| type_location(luau_type).end);

        if let Some(message_index) = self.report_mixed_type_sequence(start, end, union, &types) {
            return Type::Error {
                syntax_id: self.fresh_syntax_id(),
                location: Some(Location::new(start, end)),
                types: flatten_any_type_sequence(types),
                message_index: Some(message_index),
            };
        }

        if union {
            Type::Union {
                syntax_id: self.fresh_syntax_id(),
                location: Some(Location::new(start, end)),
                types,
            }
        } else {
            Type::Intersection {
                syntax_id: self.fresh_syntax_id(),
                location: Some(Location::new(start, end)),
                types,
            }
        }
    }

    /// Parses a `|` or `&` type sequence.
    pub(crate) fn parse_type_sequence(
        &mut self,
        first: Type,
        separator: TokenKind,
        union: bool,
    ) -> Type {
        if self.current.kind != separator {
            return first;
        }

        let start = type_location(&first).begin;
        let mut types = flatten_type_sequence(first, union);
        while self.current.kind == separator {
            self.advance();
            let item = if union {
                self.parse_type_intersection()
            } else {
                self.parse_type_primary_with_optional()
            };
            types.extend(flatten_type_sequence(item, union));
        }
        let end = types
            .last()
            .map_or(start, |luau_type| type_location(luau_type).end);

        if let Some(message_index) = self.report_mixed_type_sequence(start, end, union, &types) {
            return Type::Error {
                syntax_id: self.fresh_syntax_id(),
                location: Some(Location::new(start, end)),
                types: flatten_any_type_sequence(types),
                message_index: Some(message_index),
            };
        }

        if union {
            Type::Union {
                syntax_id: self.fresh_syntax_id(),
                location: Some(Location::new(start, end)),
                types,
            }
        } else {
            Type::Intersection {
                syntax_id: self.fresh_syntax_id(),
                location: Some(Location::new(start, end)),
                types,
            }
        }
    }

    /// Reports unparenthesized mixes of `|` and `&`.
    pub(crate) fn report_mixed_type_sequence(
        &mut self,
        start: Position,
        end: Position,
        union: bool,
        types: &[Type],
    ) -> Option<usize> {
        let mixed = if union {
            types
                .iter()
                .any(|luau_type| matches!(luau_type, Type::Intersection { .. }))
        } else {
            types
                .iter()
                .any(|luau_type| matches!(luau_type, Type::Union { .. }))
        };
        if mixed {
            let message_index = self.errors.len();
            self.push_error_dedup(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "Mixing union and intersection types is not allowed; consider wrapping in parentheses.".to_owned(),
                location: Location::new(start, end),
            });
            Some(message_index)
        } else {
            None
        }
    }

    /// Parses a primary type followed by Luau optional markers.
    pub(crate) fn parse_type_primary_with_optional(&mut self) -> Type {
        let primary = self.parse_type_primary();
        self.parse_optional_type_suffix(primary)
    }

    pub(crate) fn parse_optional_type_suffix(&mut self, mut primary: Type) -> Type {
        if self.current.kind != TokenKind::Char('?') {
            return primary;
        }

        let start = type_location(&primary).begin;
        if let Type::Group {
            location, inner, ..
        } = &mut primary
            && matches!(inner.as_ref(), Type::Function { .. })
        {
            *location = Some(type_location(inner));
        }

        let mut types = flatten_type_sequence(primary, true);
        while let Some(question) = self.consume_char('?') {
            types.push(Type::Optional {
                syntax_id: self.fresh_syntax_id(),
                location: Some(question.location),
            });
        }
        let end = types
            .last()
            .map_or(start, |luau_type| type_location(luau_type).end);
        Type::Union {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(start, end)),
            types,
        }
    }

    /// Parses a primary type.
    pub(crate) fn parse_type_primary(&mut self) -> Type {
        if self.current.kind == TokenKind::Attribute {
            let attributes = self.parse_attributes();
            if self.current.kind == TokenKind::Char('(') {
                return self.parse_type_group_or_function_with_attributes(attributes);
            }
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: format!(
                    "Expected '(' when parsing function parameters, got {}",
                    self.current.display()
                ),
                location: self.current.location,
            });
            let start = self.current.location.begin;
            let inner = self.parse_type_expression();
            let close = self.expect_char_to_close(')', "identifier", start);
            let end = close.map_or(self.current.location.end, |token| token.location.end);
            return Type::Group {
                syntax_id: self.fresh_syntax_id(),
                location: Some(Location::new(start, end)),
                inner: Box::new(inner),
            };
        }
        if self.current.kind == TokenKind::Char('<') {
            let start = self.current.location.begin;
            let (generics, generic_packs) = self.parse_generic_parameters();
            return self.parse_function_type_after_generics(start, generics, generic_packs);
        }
        if self.current.kind == TokenKind::Char('(') {
            return self.parse_type_group_or_function_with_attributes(Vec::new());
        }
        if self.current.kind == TokenKind::Char('{') {
            return self.parse_type_table();
        }
        if self.current.kind == TokenKind::Name && token_name(&self.current) == "typeof" {
            return self.parse_typeof();
        }
        if matches!(
            self.current.kind,
            TokenKind::QuotedString | TokenKind::RawString
        ) {
            let token = self.advance();
            if let Some(value) = self.string_value_from_token(&token) {
                return Type::SingletonString {
                    syntax_id: self.fresh_syntax_id(),
                    location: Some(token.location),
                    value,
                };
            }
            let message_index = self.errors.len();
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "string literal contains malformed escape sequence".to_owned(),
                location: malformed_string_escape_location(&token).unwrap_or(token.location),
            });
            return self.type_error_at_message(token.location, message_index);
        }
        if matches!(
            self.current.kind,
            TokenKind::InterpStringBegin
                | TokenKind::InterpStringMid
                | TokenKind::InterpStringEnd
                | TokenKind::InterpStringSimple
        ) {
            let token = self.current.clone();
            let message_index = self.errors.len();
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "interpolated string literals cannot be used as types".to_owned(),
                location: token.location,
            });
            self.skip_interpolated_string();
            return self.type_error_at_message(token.location, message_index);
        }
        if self.current.kind == TokenKind::BrokenString {
            let token = self.current.clone();
            let message_index = self.errors.len();
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "Malformed string; did you forget to finish it?".to_owned(),
                location: token.location,
            });
            self.advance();
            return self.type_error_at_message(token.location, message_index);
        }
        if matches!(
            self.current.kind,
            TokenKind::ReservedTrue | TokenKind::ReservedFalse
        ) {
            let token = self.advance();
            return Type::SingletonBool {
                syntax_id: self.fresh_syntax_id(),
                location: Some(token.location),
                value: token.kind == TokenKind::ReservedTrue,
            };
        }

        self.parse_type_reference()
    }

    /// Skips over an interpolated string after a non-expression recovery error.
    pub(crate) fn skip_interpolated_string(&mut self) {
        let mut saw_interpolation = false;
        loop {
            match self.current.kind {
                TokenKind::InterpStringSimple => {
                    self.advance();
                    break;
                }
                TokenKind::InterpStringBegin | TokenKind::InterpStringMid => {
                    saw_interpolation = true;
                    self.advance();
                }
                TokenKind::InterpStringEnd => {
                    self.advance();
                    break;
                }
                TokenKind::Eof | TokenKind::BrokenString => break,
                _ if saw_interpolation => {
                    self.advance();
                }
                _ => break,
            }
        }
    }

    /// Parses `typeof(expr)`.
    pub(crate) fn parse_typeof(&mut self) -> Type {
        let start = self.current.location.begin;
        self.advance();
        self.expect_char('(');
        let expr = self.parse_expression();
        let close = self.expect_char(')');
        let end = close.map_or_else(|| expr_location(&expr).end, |token| token.location.end);
        Type::Typeof {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(start, end)),
            expr,
        }
    }

    /// Parses a Luau table type.
    #[allow(clippy::cognitive_complexity)]
    pub(crate) fn parse_type_table(&mut self) -> Type {
        let start = self.current.location.begin;
        self.advance();
        let mut props = Vec::new();
        let mut indexer = None;
        let mut end_override = None;
        let mut last_field_end = start;
        let mut reported_missing_field_at_eof = false;

        while !matches!(self.current.kind, TokenKind::Char('}') | TokenKind::Eof) {
            self.skip_comments();
            if matches!(self.current.kind, TokenKind::Char('}') | TokenKind::Eof) {
                break;
            }

            let mut field_read_only = false;
            let mut field_write_only = false;
            if self.current.kind == TokenKind::Name
                && matches!(self.current.name.as_deref(), Some("read" | "write"))
                && matches!(
                    self.peek_significant_kind(),
                    TokenKind::Name | TokenKind::Char('[')
                )
            {
                field_read_only = self.current.name.as_deref() == Some("read");
                field_write_only = self.current.name.as_deref() == Some("write");
                self.advance();
            }

            match self.current.kind {
                TokenKind::Char('[') => {
                    self.parse_bracket_table_entry(
                        &mut props,
                        &mut indexer,
                        field_read_only,
                        field_write_only,
                    );
                    if let Some(prop) = props.last() {
                        last_field_end = last_field_end.max(type_deep_end(&prop.prop_type));
                    }
                    if let Some(indexer) = &indexer {
                        last_field_end =
                            last_field_end.max(indexer.location.unwrap_or_default().end);
                    }
                }
                TokenKind::Name if self.peek_significant_kind() == TokenKind::Char(':') => {
                    props.push(self.parse_table_prop());
                    if let Some(prop) = props.last_mut() {
                        prop.read_only = field_read_only;
                        prop.write_only = field_write_only;
                    }
                    if let Some(prop) = props.last() {
                        last_field_end = last_field_end.max(type_deep_end(&prop.prop_type));
                    }
                }
                TokenKind::Name if !props.is_empty() || indexer.is_some() => {
                    props.push(self.parse_malformed_table_prop_name());
                    if let Some(prop) = props.last_mut() {
                        prop.read_only = field_read_only;
                        prop.write_only = field_write_only;
                    }
                    if let Some(prop) = props.last() {
                        last_field_end = last_field_end.max(prop.location.unwrap_or_default().end);
                    }
                }
                _ if !props.is_empty() || indexer.is_some() => {
                    let token = self.current.clone();
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: format!(
                            "Expected identifier when parsing table field, got {}",
                            token.display()
                        ),
                        location: token.location,
                    });
                    end_override = Some(token.location.end);
                    if token.kind != TokenKind::Eof {
                        self.advance();
                    }
                    break;
                }
                _ => {
                    let result_type = self.parse_type_expression();
                    let location = type_location(&result_type);
                    last_field_end = last_field_end.max(location.end);
                    indexer = Some(TableIndexer {
                        location: Some(location),
                        index_type: Box::new(self.number_type_at(location)),
                        result_type: Box::new(result_type),
                        read_only: field_read_only,
                    });
                    break;
                }
            }

            if !matches!(
                self.current.kind,
                TokenKind::Char(',') | TokenKind::Char(';') | TokenKind::Char('}') | TokenKind::Eof
            ) {
                break;
            }

            if let Some(separator) = self.consume_char(',') {
                last_field_end = separator.location.end;
                if self.current.kind == TokenKind::Eof {
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: format!(
                            "Expected identifier when parsing table field, got {}",
                            self.current.display()
                        ),
                        location: self.current.location,
                    });
                    reported_missing_field_at_eof = true;
                    break;
                }
            } else if let Some(separator) = self.consume_char(';') {
                last_field_end = separator.location.end;
                if self.current.kind == TokenKind::Eof {
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: format!(
                            "Expected identifier when parsing table field, got {}",
                            self.current.display()
                        ),
                        location: self.current.location,
                    });
                    reported_missing_field_at_eof = true;
                    break;
                }
            }
        }

        let end = if let Some(close) = self.consume_char('}') {
            if end_override.is_some() {
                self.type_recovery_end = Some(close.location.end);
            }
            end_override.unwrap_or(close.location.end)
        } else {
            let error_kind = self.current.kind;
            let error_location = self.current.location;
            if !reported_missing_field_at_eof {
                self.errors.push(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: format!(
                        "Expected '}}' (to close '{{' at {}), got {}",
                        opening_position_description_for(start, &self.current),
                        self.current.display()
                    ),
                    location: error_location,
                });
            }
            if self.has_char_on_line('}', error_location.begin.line)
                && let Some(recovered) =
                    self.recover_to_char_on_line('}', error_location.begin.line)
            {
                self.type_recovery_end = Some(recovered.location.end);
            }
            if error_kind == TokenKind::Char(')') || error_location.begin.line != start.line {
                last_field_end
            } else if error_location.begin == error_location.end {
                self.previous_non_whitespace_byte_location(error_location.begin)
                    .map_or(last_field_end, |location| location.end)
            } else {
                error_location.end
            }
        };
        Type::Table {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(start, end)),
            props,
            indexer,
        }
    }

    /// Parses a table type property.
    pub(crate) fn parse_table_prop(&mut self) -> TableProp {
        let token = self.advance();
        self.expect_char(':');
        TableProp {
            name: Name::new(token_name(&token)),
            location: Some(token.location),
            prop_type: self.parse_type_expression(),
            read_only: false,
            write_only: false,
        }
    }

    /// Parses a table type field that was shaped like a name but missed `:`.
    pub(crate) fn parse_malformed_table_prop_name(&mut self) -> TableProp {
        let token = self.advance();
        let message_index = self.errors.len();
        let error_location = self.current.location;
        self.errors.push(Error {
            kind: ErrorKind::ExpectedToken,
            message: format!(
                "Expected ':' when parsing table field, got {}",
                self.current.display()
            ),
            location: error_location,
        });
        TableProp {
            name: Name::new(token_name(&token)),
            location: Some(token.location),
            prop_type: self.type_error_at_message(
                Location::new(error_location.begin, error_location.begin),
                message_index,
            ),
            read_only: false,
            write_only: false,
        }
    }

    /// Parses a bracketed table type entry.
    pub(crate) fn parse_bracket_table_entry(
        &mut self,
        props: &mut Vec<TableProp>,
        indexer: &mut Option<TableIndexer>,
        read_only: bool,
        write_only: bool,
    ) {
        let open = self.advance();
        let index_type = self.parse_type_expression();
        self.expect_char_to_close(']', "'['", open.location.begin);
        self.expect_char(':');
        let result_type = self.parse_type_expression();
        let end = type_location(&result_type).end;

        if matches!(index_type, Type::Error { .. }) {
            return;
        }

        if let Type::SingletonString {
            value,
            location: Some(_),
            ..
        } = &index_type
            && value.contains('\0')
        {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "String literal contains malformed escape sequence or \\0".to_owned(),
                location: open.location,
            });
            return;
        }

        if let Type::SingletonString { value, .. } = index_type {
            props.push(TableProp {
                name: Name::new(value),
                location: Some(open.location),
                prop_type: result_type,
                read_only,
                write_only,
            });
        } else {
            let next_indexer = TableIndexer {
                location: Some(Location::new(open.location.begin, end)),
                index_type: Box::new(index_type),
                result_type: Box::new(result_type),
                read_only,
            };
            if indexer.is_some() {
                self.errors.push(Error {
                    kind: ErrorKind::MalformedSyntax,
                    message: "cannot have more than one table indexer".to_owned(),
                    location: next_indexer.location.unwrap_or_default(),
                });
            } else {
                *indexer = Some(next_indexer);
            }
        }
    }

    /// Parses a parenthesized type group or function type.
    pub(crate) fn parse_type_group_or_function_with_attributes(
        &mut self,
        attributes: Vec<Attribute>,
    ) -> Type {
        let open = self.current.clone();
        let start = open.location.begin;
        if self.type_parens_start_function_type() {
            self.advance();
            let (args, arg_names) = self.parse_function_type_arg_list_until_close();
            let mut close = self.expect_function_type_close(start);
            if close.is_none()
                && self.current.kind == TokenKind::Char(',')
                && self.has_char_on_line(')', self.current.location.begin.line)
            {
                close = self.recover_to_char_on_line(')', self.current.location.begin.line);
            }
            if close.is_none()
                && self.current.kind == TokenKind::Char(';')
                && self.has_char_on_line(')', self.current.location.begin.line)
            {
                close = self.recover_to_char_on_line(')', self.current.location.begin.line);
            }
            let missing_close_at_broken_string =
                close.is_none() && self.current.kind == TokenKind::BrokenString;
            if missing_close_at_broken_string {
                self.advance();
            }
            if close.is_none() && self.current.kind == TokenKind::Eof {
                let (return_location, message_index) = if missing_close_at_broken_string {
                    let return_location =
                        Location::new(self.current.location.begin, self.current.location.begin);
                    let message_index = self.errors.len();
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: expected_function_type_arrow_message(&self.current),
                        location: return_location,
                    });
                    (return_location, message_index)
                } else {
                    (
                        Location::new(self.current.location.begin, self.current.location.begin),
                        self.errors.len().saturating_sub(1),
                    )
                };
                return self.function_type_with_error_return(
                    start,
                    attributes,
                    Vec::new(),
                    Vec::new(),
                    args,
                    arg_names,
                    return_location,
                    message_index,
                );
            }
            let after_args_end = close.map_or_else(
                || type_list_end(&args).unwrap_or(start),
                |token| token.location.end,
            );
            return self.parse_function_type_after_args(
                start,
                attributes,
                Vec::new(),
                Vec::new(),
                args,
                arg_names,
                after_args_end,
            );
        }

        self.advance();
        let type_list = self.parse_type_list_until_close();
        let close = self.expect_char_to_close(')', "'('", start);
        let group_end = close.as_ref().map_or_else(
            || {
                if self.current.kind == TokenKind::Eof {
                    type_list_end(&type_list).unwrap_or(start)
                } else {
                    self.current.location.end
                }
            },
            |token| token.location.end,
        );

        if self.current.kind == TokenKind::SkinnyArrow {
            return self.parse_function_type_after_args(
                start,
                attributes,
                Vec::new(),
                Vec::new(),
                type_list,
                Vec::new(),
                group_end,
            );
        }

        if self.current.kind == TokenKind::Char(':') {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message:
                    "Return types in function type annotations are written after '->' instead of ':'"
                        .to_owned(),
                location: self.current.location,
            });
            self.advance();
            let return_types = self.parse_return_type_pack();
            let end = type_pack_location(&return_types).end;
            return Type::Function {
                syntax_id: self.fresh_syntax_id(),
                location: Some(Location::new(start, end)),
                attributes,
                generics: Vec::new(),
                generic_packs: Vec::new(),
                arg_types: type_list,
                arg_names: Vec::new(),
                return_types,
            };
        }

        if attributes.is_empty() && type_list.types.is_empty() && type_list.tail_type.is_none() {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected '->' after '()' when parsing function type; did you mean 'nil'?"
                    .to_owned(),
                location: Location::new(start, group_end),
            });
            return Type::Reference {
                syntax_id: self.fresh_syntax_id(),
                location: Some(open.location),
                prefix: None,
                prefix_location: None,
                name: Name::new("nil"),
                name_location: Some(open.location),
                parameters: Vec::new(),
            };
        }

        if type_list.types.len() != 1 || type_list.tail_type.is_some() {
            if self.current.kind == TokenKind::SkinnyArrow
                || self.peek_significant_kind() == TokenKind::SkinnyArrow
            {
                return self.parse_function_type_after_args(
                    start,
                    attributes,
                    Vec::new(),
                    Vec::new(),
                    type_list,
                    Vec::new(),
                    group_end,
                );
            }

            let after_args_end = close.as_ref().map_or_else(
                || type_list_end(&type_list).unwrap_or(start),
                |token| token.location.end,
            );
            if close.is_some() {
                self.errors.push(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: expected_function_type_arrow_message(&self.current),
                    location: self.current.location,
                });
            }
            let return_location = if self.current.kind == TokenKind::Eof {
                let return_location = Location::new(after_args_end, self.current.location.end);
                let message_index =
                    self.push_expected_token(expected_type_message(&self.current), return_location);
                self.type_statement_end_override = Some(after_args_end);
                (return_location, message_index)
            } else {
                let return_location =
                    Location::new(self.current.location.begin, self.current.location.begin);
                (return_location, self.errors.len().saturating_sub(1))
            };
            return Type::Function {
                syntax_id: self.fresh_syntax_id(),
                location: Some(Location::new(start, return_location.0.end)),
                attributes,
                generics: Vec::new(),
                generic_packs: Vec::new(),
                arg_types: type_list,
                arg_names: Vec::new(),
                return_types: TypePack::Explicit {
                    location: Some(return_location.0),
                    type_list: TypeList::new(vec![
                        self.type_error_at_message(return_location.0, return_location.1),
                    ]),
                },
            };
        }

        let inner = type_list.types.into_iter().next().expect("len checked");
        Type::Group {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(start, group_end)),
            inner: Box::new(inner),
        }
    }

    /// Parses generic type and type-pack parameters like `<A, B...>`.
    pub(crate) fn parse_generic_parameters(&mut self) -> (Vec<GenericType>, Vec<GenericTypePack>) {
        let open = self.current.clone();
        self.expect_char('<');
        let mut generics = Vec::new();
        let mut generic_packs = Vec::new();
        let mut seen_pack = false;
        let mut seen_default_type = false;
        let mut seen_default_pack = false;
        if self.current.kind != TokenKind::Char('>') {
            loop {
                let token = self.current.clone();
                if token.kind == TokenKind::Name {
                    self.advance();
                    let name = Name::new(token_name(&token));
                    if self.current.kind == TokenKind::Dot3 || seen_pack {
                        seen_pack = true;
                        if self.current.kind == TokenKind::Dot3 {
                            self.advance();
                        } else {
                            self.errors.push(Error {
                                kind: ErrorKind::MalformedSyntax,
                                message: "generic types come before generic type packs".to_owned(),
                                location: self.current.location,
                            });
                        }
                        let default_type = if let Some(equals) = self.consume_char('=') {
                            seen_default_pack = true;
                            self.parse_generic_pack_default_after_equals(equals.location)
                        } else {
                            if seen_default_pack {
                                self.errors.push(Error {
                                    kind: ErrorKind::ExpectedToken,
                                    message: "expected default type pack after type pack name"
                                        .to_owned(),
                                    location: self.current.location,
                                });
                            }
                            None
                        };
                        generic_packs.push(GenericTypePack {
                            name,
                            location: Some(token.location),
                            default_type,
                        });
                    } else {
                        let default_type = if self.consume_char('=').is_some() {
                            seen_default_type = true;
                            Some(Box::new(self.parse_type_expression()))
                        } else {
                            if seen_default_type {
                                self.errors.push(Error {
                                    kind: ErrorKind::ExpectedToken,
                                    message: "expected default type after type name".to_owned(),
                                    location: self.current.location,
                                });
                            }
                            None
                        };
                        generics.push(GenericType {
                            name,
                            location: Some(token.location),
                            default_type,
                        });
                    }
                } else {
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: if token.kind == TokenKind::Char('>') {
                            expected_after_comma_message("type", &token)
                        } else {
                            expected_identifier_message(&token, None)
                        },
                        location: token.location,
                    });
                    if token.kind == TokenKind::Eof {
                        generics.push(GenericType {
                            name: Name::new("%error-id%"),
                            location: None,
                            default_type: None,
                        });
                    }
                    break;
                }

                if self.consume_char(',').is_none() {
                    break;
                }
            }
        }
        self.expect_char_to_close('>', "'<'", open.location.begin);
        (generics, generic_packs)
    }

    /// Parses a generic type-pack default after `=`.
    pub(crate) fn parse_generic_pack_default_after_equals(
        &mut self,
        equals_location: Location,
    ) -> Option<Box<TypePack>> {
        if self.current.kind == TokenKind::Char('>') {
            let space_before_current = Position::new(
                self.current.location.begin.line,
                self.current.location.begin.column.saturating_sub(1),
            );
            let expected_type_location =
                Location::new(space_before_current, self.current.location.end);
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: expected_type_message(&self.current),
                location: expected_type_location,
            });
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected type pack after '=', got type".to_owned(),
                location: Location::new(equals_location.end, self.current.location.begin),
            });
            return None;
        }

        let default = self.parse_generic_pack_default();
        if let TypePack::Explicit { type_list, .. } = &default
            && type_list.tail_type.is_none()
            && type_list.types.len() == 1
            && matches!(type_list.types.first(), Some(Type::Function { .. }))
        {
            let default_location = type_pack_location(&default);
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected type pack after '=', got type".to_owned(),
                location: Location::new(default_location.begin, self.current.location.begin),
            });
            return None;
        }

        Some(Box::new(default))
    }

    /// Parses a default value for a generic type pack.
    pub(crate) fn parse_generic_pack_default(&mut self) -> TypePack {
        if self.current.kind == TokenKind::Char('(') && !self.type_parens_start_function_type() {
            let start = self.current.location.begin;
            self.advance();
            let type_list = self.parse_type_list_until_close();
            self.expect_char_to_close(')', "'('", start);
            let end = Position::new(start.line, start.column.saturating_add(1));
            return TypePack::Explicit {
                location: Some(Location::new(start, end)),
                type_list,
            };
        }

        self.parse_return_type_pack()
    }

    /// Parses a function type after generic parameters.
    pub(crate) fn parse_function_type_after_generics(
        &mut self,
        start: Position,
        generics: Vec<GenericType>,
        generic_packs: Vec<GenericTypePack>,
    ) -> Type {
        let (args, arg_names, mut close) = if self.current.kind == TokenKind::Char('(') {
            let open = self.advance();
            let (args, arg_names) = self.parse_function_type_arg_list_until_close();
            let close = self.expect_function_type_close(open.location.begin);
            (args, arg_names, close)
        } else {
            let token = self.current.clone();
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: format!(
                    "Expected '(' when parsing function parameters, got {}",
                    token.display()
                ),
                location: token.location,
            });

            if token.kind == TokenKind::SkinnyArrow {
                let message_index = self.errors.len();
                let (diagnostic_location, error_location) = unexpected_type_locations(&token);
                self.errors.push(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: format!("Expected type, got {}", token.display()),
                    location: diagnostic_location,
                });
                let error_type = self.type_error_at_message(error_location, message_index);
                self.errors.push(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: format!(
                        "Expected ')' (to close '->' at {}), got {}",
                        opening_position_description(token.location.begin),
                        token.display()
                    ),
                    location: token.location,
                });
                (TypeList::new(vec![error_type]), Vec::new(), None)
            } else {
                let (args, arg_names) = self.parse_function_type_arg_list_until_close();
                let close = self.expect_char_to_close(')', "identifier", token.location.begin);
                (args, arg_names, close)
            }
        };
        if close.is_none()
            && self.current.kind == TokenKind::Char(',')
            && self.has_char_on_line(')', self.current.location.begin.line)
        {
            close = self.recover_to_char_on_line(')', self.current.location.begin.line);
        }
        if close.is_none()
            && self.current.kind == TokenKind::Char(';')
            && self.has_char_on_line(')', self.current.location.begin.line)
        {
            close = self.recover_to_char_on_line(')', self.current.location.begin.line);
        }
        let missing_close_at_broken_string =
            close.is_none() && self.current.kind == TokenKind::BrokenString;
        if missing_close_at_broken_string {
            self.advance();
        }
        if close.is_none() && self.current.kind == TokenKind::Eof {
            let (return_location, message_index) = if missing_close_at_broken_string {
                let return_location =
                    Location::new(self.current.location.begin, self.current.location.begin);
                let message_index = self.push_expected_token(
                    expected_function_type_arrow_message(&self.current),
                    return_location,
                );
                (return_location, message_index)
            } else {
                let after_args_end = type_list_end(&args).unwrap_or(start);
                let eof_location =
                    Location::new(self.current.location.begin, self.current.location.begin);
                if after_args_end == self.current.location.begin
                    && let Some(message_index) = self.error_index_at(eof_location)
                {
                    (eof_location, message_index)
                } else {
                    let return_location = Location::new(after_args_end, self.current.location.end);
                    let message_index = self
                        .push_expected_token(expected_type_message(&self.current), return_location);
                    (return_location, message_index)
                }
            };
            return self.function_type_with_error_return(
                start,
                Vec::new(),
                generics,
                generic_packs,
                args,
                arg_names,
                return_location,
                message_index,
            );
        }
        let after_args_end = close.map_or_else(
            || type_list_end(&args).unwrap_or(start),
            |token| token.location.end,
        );
        self.parse_function_type_after_args(
            start,
            Vec::new(),
            generics,
            generic_packs,
            args,
            arg_names,
            after_args_end,
        )
    }

    /// Parses a function type after its argument list.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn parse_function_type_after_args(
        &mut self,
        start: Position,
        attributes: Vec<Attribute>,
        generics: Vec<GenericType>,
        generic_packs: Vec<GenericTypePack>,
        args: TypeList,
        arg_names: Vec<Option<ArgumentName>>,
        _after_args_end: Position,
    ) -> Type {
        let arrow_message_index = self.errors.len();
        let arrow = if self.current.kind == TokenKind::SkinnyArrow {
            let token = self.advance();
            Some(token)
        } else {
            self.push_expected_token(
                format!(
                    "Expected '->' when parsing function type, got {}",
                    self.current.display()
                ),
                self.current.location,
            );

            if self.peek_significant_kind() == TokenKind::SkinnyArrow {
                self.advance();
                let token = self.advance();
                Some(token)
            } else {
                None
            }
        };
        if arrow.is_none() && matches!(self.current.kind, TokenKind::Eof | TokenKind::Char('>')) {
            let return_location =
                Location::new(self.current.location.begin, self.current.location.begin);
            return self.function_type_with_error_return(
                start,
                attributes,
                generics,
                generic_packs,
                args,
                arg_names,
                return_location,
                arrow_message_index,
            );
        }
        let return_types = self.parse_return_type_pack();
        self.type_recovery_end.take();
        let return_location = type_pack_location(&return_types);
        let end = return_location.end;
        Type::Function {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(start, end)),
            attributes,
            generics,
            generic_packs,
            arg_types: args,
            arg_names,
            return_types,
        }
    }

    /// Parses a function return type pack.
    pub(crate) fn parse_return_type_pack(&mut self) -> TypePack {
        if self.current.kind == TokenKind::Dot3 {
            let start = self.current.location.begin;
            self.advance();
            let variadic_type = self.parse_type_expression();
            let end = type_location(&variadic_type).end;
            return TypePack::Variadic {
                location: Some(Location::new(start, end)),
                variadic_type: Box::new(variadic_type),
            };
        }

        if self.current.kind == TokenKind::Name && self.peek_significant_kind() == TokenKind::Dot3 {
            let token = self.advance();
            let dots = self.expect_token(TokenKind::Dot3, "'...'");
            let end = dots.map_or(token.location.end, |dots| dots.location.end);
            return TypePack::Generic {
                location: Some(Location::new(token.location.begin, end)),
                name: Name::new(token_name(&token)),
            };
        }

        if self.current.kind == TokenKind::Char('(') && !self.type_parens_start_function_type() {
            let start = self.current.location.begin;
            self.advance();
            let types = self.parse_type_list_until_close();
            let mut close = self.expect_char_to_close(')', "'('", start);
            let syntax_end = type_list_end(&types).unwrap_or(start);
            if close.is_none() {
                if self.current.kind == TokenKind::Char(';')
                    && self.has_char_on_line(')', self.current.location.begin.line)
                {
                    close = self.recover_to_char_on_line(')', self.current.location.begin.line);
                }
                if self.current.kind == TokenKind::SkinnyArrow {
                    self.advance();
                    let return_types = self.parse_return_type_pack();
                    let end = type_pack_location(&return_types).end;
                    return TypePack::Explicit {
                        location: Some(Location::new(start, end)),
                        type_list: TypeList::new(vec![Type::Function {
                            syntax_id: self.fresh_syntax_id(),
                            location: Some(Location::new(start, end)),
                            attributes: Vec::new(),
                            generics: Vec::new(),
                            generic_packs: Vec::new(),
                            arg_types: types,
                            arg_names: Vec::new(),
                            return_types,
                        }]),
                    };
                }
                if self.current.kind == TokenKind::ReservedReturn {
                    let return_end = self.current.location.end;
                    self.advance();
                    let recovery_end =
                        if matches!(self.current.kind, TokenKind::ReservedEnd | TokenKind::Eof) {
                            return_end
                        } else {
                            let expression = self.parse_expression();
                            expr_end(&expression)
                        };
                    self.type_recovery_end = Some(recovery_end);
                    return TypePack::Explicit {
                        location: Some(Location::new(start, return_end)),
                        type_list: types,
                    };
                }
            }
            let variadic_comma_end = if close.is_none()
                && self.current.kind == TokenKind::Char(',')
                && self.has_char_on_line(')', self.current.location.begin.line)
            {
                let comma_end = self.current.location.end;
                close = self.recover_to_char_on_line(')', self.current.location.begin.line);
                if let Some(close) = &close {
                    self.type_recovery_end = Some(close.location.end);
                }
                Some(comma_end)
            } else {
                None
            };
            let mut end = if let Some(comma_end) = variadic_comma_end {
                comma_end
            } else if close.is_none() && self.current.kind == TokenKind::Eof {
                self.type_statement_end_override = Some(syntax_end);
                self.current.location.end
            } else {
                close.map_or(self.current.location.end, |token| token.location.end)
            };
            let mut type_list = if types.types.len() == 1 && types.tail_type.is_none() {
                let inner = types.types.into_iter().next().expect("len checked");
                TypeList::new(vec![Type::Group {
                    syntax_id: self.fresh_syntax_id(),
                    location: Some(Location::new(start, end)),
                    inner: Box::new(inner),
                }])
            } else {
                types
            };
            if type_list.types.len() == 1
                && type_list.tail_type.is_none()
                && self.current.kind == TokenKind::Char('?')
            {
                let inner = type_list.types.pop().expect("len checked");
                let optional = self.parse_optional_type_suffix(inner);
                end = type_location(&optional).end;
                type_list.types.push(optional);
            }
            if type_list.types.len() == 1
                && (self.current.kind == TokenKind::Char('|')
                    || self.current.kind == TokenKind::Char('&'))
            {
                let separator = self.current.kind;
                let union = separator == TokenKind::Char('|');
                if let Some(first) = type_list.types.pop() {
                    let mut sequence = self.parse_type_sequence(first, separator, union);
                    let sequence_end = type_location(&sequence).end;
                    match &mut sequence {
                        Type::Union { location, .. } | Type::Intersection { location, .. } => {
                            *location = Some(Location::new(start, sequence_end));
                        }
                        _ => {}
                    }
                    type_list.types.push(sequence);
                }
            }
            TypePack::Explicit {
                location: Some(Location::new(start, end)),
                type_list,
            }
        } else {
            let return_type = self.parse_type_expression();
            let location = type_location(&return_type);
            TypePack::Explicit {
                location: Some(location),
                type_list: TypeList::new(vec![return_type]),
            }
        }
    }

    /// Returns whether current parenthesized type syntax is followed by `->`.
    pub(crate) fn type_parens_start_function_type(&self) -> bool {
        if self.current.kind != TokenKind::Char('(') {
            return false;
        }

        let mut lexer = self.lexer.clone();
        let mut depth = 1_u32;
        let mut brace_depth = 0_u32;
        let mut bracket_depth = 0_u32;
        let mut previous_depth_one_kind: Option<TokenKind> = None;
        while depth > 0 {
            let token = lexer.next_token();
            match token.kind {
                TokenKind::Eof => return false,
                TokenKind::Char('(') => depth += 1,
                TokenKind::Char(')') => depth -= 1,
                TokenKind::Char('{') if depth == 1 => {
                    brace_depth += 1;
                    previous_depth_one_kind = None;
                }
                TokenKind::Char('}') if depth == 1 && brace_depth > 0 => {
                    brace_depth -= 1;
                    previous_depth_one_kind = None;
                }
                TokenKind::Char('[') if depth == 1 => {
                    bracket_depth += 1;
                    previous_depth_one_kind = None;
                }
                TokenKind::Char(']') if depth == 1 && bracket_depth > 0 => {
                    bracket_depth -= 1;
                    previous_depth_one_kind = None;
                }
                TokenKind::Char(':')
                    if depth == 1
                        && brace_depth == 0
                        && bracket_depth == 0
                        && previous_depth_one_kind == Some(TokenKind::Name) =>
                {
                    return true;
                }
                TokenKind::Comment | TokenKind::BlockComment | TokenKind::BrokenComment => {}
                _ => {
                    if depth == 1 && brace_depth == 0 && bracket_depth == 0 {
                        previous_depth_one_kind = Some(token.kind);
                    }
                }
            }
        }

        loop {
            let token = lexer.next_token();
            if !matches!(
                token.kind,
                TokenKind::Comment | TokenKind::BlockComment | TokenKind::BrokenComment
            ) {
                return token.kind == TokenKind::SkinnyArrow;
            }
        }
    }

    /// Returns whether current parenthesized type syntax is a type-pack parameter.
    pub(crate) fn type_parens_start_type_pack_parameter(&self) -> bool {
        if self.current.kind != TokenKind::Char('(') || self.type_parens_start_function_type() {
            return false;
        }

        let mut lexer = self.lexer.clone();
        let mut depth = 1_u32;
        // Generic type arguments use `<`/`>`; a comma inside them (e.g.
        // `Map<K, V>`) separates type arguments, not type-pack members, so it
        // must not be treated as a pack separator at paren depth one.
        let mut angle_depth = 0_u32;
        let mut saw_depth_one_token = false;
        while depth > 0 {
            let token = lexer.next_token();
            match token.kind {
                TokenKind::Eof => return false,
                TokenKind::Char(')') if depth == 1 => return !saw_depth_one_token,
                TokenKind::Char('(') => {
                    if depth == 1 {
                        saw_depth_one_token = true;
                    }
                    depth += 1;
                }
                TokenKind::Char(')') => depth -= 1,
                TokenKind::Char('<') => {
                    if depth == 1 {
                        saw_depth_one_token = true;
                    }
                    angle_depth += 1;
                }
                TokenKind::Char('>') => {
                    if depth == 1 {
                        saw_depth_one_token = true;
                    }
                    angle_depth = angle_depth.saturating_sub(1);
                }
                TokenKind::Char(',') | TokenKind::Dot3 if depth == 1 && angle_depth == 0 => {
                    return true;
                }
                _ => {
                    if depth == 1 {
                        saw_depth_one_token = true;
                    }
                }
            }
        }

        false
    }

    /// Parses a comma-separated type list until `)`.
    pub(crate) fn parse_type_list_until_close(&mut self) -> TypeList {
        let mut type_list = TypeList::new(Vec::new());
        if self.current.kind == TokenKind::Char(')') {
            return type_list;
        }

        loop {
            if self.current.kind == TokenKind::Dot3
                || (self.current.kind == TokenKind::Name
                    && self.peek_significant_kind() == TokenKind::Dot3)
            {
                type_list.tail_type = Some(Box::new(self.parse_return_type_pack()));
                break;
            }

            type_list.types.push(self.parse_type_expression());
            if self.consume_char(',').is_none() || self.current.kind == TokenKind::Char(')') {
                break;
            }
        }
        type_list
    }

    /// Parses a function type argument list until `)`.
    pub(crate) fn parse_function_type_arg_list_until_close(
        &mut self,
    ) -> (TypeList, Vec<Option<ArgumentName>>) {
        let mut type_list = TypeList::new(Vec::new());
        let mut arg_names = Vec::new();
        let mut saw_name = false;
        if self.current.kind == TokenKind::Char(')') {
            return (type_list, arg_names);
        }

        loop {
            if self.current.kind == TokenKind::Dot3
                || (self.current.kind == TokenKind::Name
                    && self.peek_significant_kind() == TokenKind::Dot3)
            {
                type_list.tail_type = Some(Box::new(self.parse_return_type_pack()));
                break;
            }

            let arg_name = if self.current.kind == TokenKind::Name
                && self.peek_significant_kind() == TokenKind::Char(':')
            {
                let token = self.advance();
                self.expect_char(':');
                saw_name = true;
                Some(ArgumentName {
                    name: Name::new(token_name(&token)),
                    location: Some(token.location),
                })
            } else {
                None
            };

            type_list.types.push(self.parse_type_expression());
            arg_names.push(arg_name);
            if self.consume_char(',').is_none() {
                break;
            }
            if self.current.kind == TokenKind::Char(')') {
                self.errors.push(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: expected_after_comma_message("type", &self.current),
                    location: self.current.location,
                });
                break;
            }
        }

        if !saw_name {
            arg_names.clear();
        }
        (type_list, arg_names)
    }

    /// Parses a named type reference.
    pub(crate) fn parse_type_reference(&mut self) -> Type {
        let first = self.current.clone();
        if first.kind == TokenKind::ReservedFunction {
            let message_index = self.errors.len();
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "Using 'function' as a type annotation is not supported, consider replacing with a function type annotation e.g. '(...any) -> ...any'".to_owned(),
                location: first.location,
            });
            self.advance();
            return self.type_error_at_message(first.location, message_index);
        }
        if !is_type_name_token(&first) {
            let message_index = self.errors.len();
            let (mut diagnostic_location, mut error_location) = unexpected_type_locations(&first);
            if first.kind == TokenKind::Eof
                && let Some(location) =
                    self.previous_horizontal_whitespace_location(first.location.begin)
            {
                diagnostic_location = location;
                error_location = location;
                self.type_statement_end_override = Some(location.begin);
            }
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: expected_type_message(&first),
                location: diagnostic_location,
            });
            return self.type_error_at_message(error_location, message_index);
        }
        let start = first.location.begin;
        self.advance();

        let (prefix, prefix_location, name_token) = if self.consume_char('.').is_some() {
            let name_token = self.current.clone();
            if is_type_name_token(&name_token) {
                self.advance();
                (
                    Some(Name::new(token_name(&first))),
                    Some(first.location),
                    name_token,
                )
            } else if is_reserved_keyword_token(name_token.kind) {
                self.errors.push(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: expected_identifier_message(&name_token, Some("field name")),
                    location: name_token.location,
                });
                self.advance();
                (
                    Some(Name::new(token_name(&first))),
                    Some(first.location),
                    name_token,
                )
            } else {
                self.errors.push(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: expected_identifier_message(&name_token, Some("field name")),
                    location: name_token.location,
                });
                let name_location =
                    Location::new(name_token.location.begin, name_token.location.begin);
                return Type::Reference {
                    syntax_id: self.fresh_syntax_id(),
                    location: Some(Location::new(start, name_location.end)),
                    prefix: Some(Name::new(token_name(&first))),
                    prefix_location: Some(first.location),
                    name: Name::new("%error-id%"),
                    name_location: Some(name_location),
                    parameters: Vec::new(),
                };
            }
        } else {
            (None, None, first)
        };

        let mut end = name_token.location.end;
        let mut parameters = Vec::new();
        if self.consume_char('<').is_some() {
            if self.current.kind != TokenKind::Char('>') {
                parameters.push(self.parse_type_parameter());
                while self.consume_char(',').is_some() {
                    parameters.push(self.parse_type_parameter());
                }
            }
            if let Some(close) = self.expect_char('>') {
                end = close.location.end;
            }
        }

        Type::Reference {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(start, end)),
            prefix,
            prefix_location,
            name: Name::new(token_name(&name_token)),
            name_location: Some(name_token.location),
            parameters,
        }
    }

    /// Parses a type-reference parameter.
    pub(crate) fn parse_type_parameter(&mut self) -> TypeParameter {
        if self.current.kind == TokenKind::Dot3
            || (self.current.kind == TokenKind::Name
                && self.peek_significant_kind() == TokenKind::Dot3)
            || (self.current.kind == TokenKind::Char('(')
                && self.type_parens_start_type_pack_parameter())
        {
            return TypeParameter::Pack(self.parse_generic_pack_default());
        }

        TypeParameter::Type(Box::new(self.parse_type_expression()))
    }
}
