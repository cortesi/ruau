//! Parser expr parsing.

use super::{
    PARSER_RECURSION_LIMIT, Parser,
    common::{
        binary_op, call_close_end, expected_after_comma_message, expected_call_arguments_message,
        expected_expression_message, expected_index_name_message, expr_can_be_called, expr_end,
        expr_location, missing_call_args_location, number_literal_is_malformed,
        opening_position_description, parse_integer_literal, parse_number,
        starts_ambiguous_newline_call, starts_table_item, statement_is_last, token_name,
        type_location, type_pack_deep_end, type_pack_location, type_parameter_end, unary_op,
        uses_missing_method_call_arguments_message,
    },
};
use crate::{
    Location, Position,
    lexer::{Lexeme, TokenKind},
    parse::{Error, ErrorKind, comment_from_token},
    syntax::{
        Attribute, BinaryOp, Expr, IndexOp, Local, Name, Stat, TableItem, TableItemKind, TypePack,
        TypeParameter, UnaryOp,
    },
};

impl<'source> Parser<'source> {
    /// Parses an expression.
    pub(crate) fn parse_expression(&mut self) -> Expr {
        self.parse_binary_expression(0)
    }

    /// Parses an expression in contexts where a newline call is recovered.
    pub(crate) fn parse_expression_allowing_ambiguous_newline_call(&mut self) -> Expr {
        let saved = self.allow_ambiguous_newline_call;
        self.allow_ambiguous_newline_call = true;
        let expression = self.parse_expression();
        self.allow_ambiguous_newline_call = saved;
        expression
    }

    /// Parses a binary expression using precedence climbing.
    pub(crate) fn parse_binary_expression(&mut self, minimum_precedence: u8) -> Expr {
        self.recursion_depth += 1;
        let result = if self.recursion_depth >= PARSER_RECURSION_LIMIT {
            self.recursion_limit_error_expr()
        } else {
            self.parse_binary_expression_inner(minimum_precedence)
        };
        self.recursion_depth -= 1;
        result
    }

    pub(crate) fn parse_binary_expression_inner(&mut self, minimum_precedence: u8) -> Expr {
        let expression = if let Some((op, start)) = self.consume_unary_operator() {
            let expr = self.parse_binary_expression(7);
            Expr::Unary {
                syntax_id: self.fresh_syntax_id(),
                location: Some(Location::new(start, expr_end(&expr))),
                op,
                expr: Box::new(expr),
            }
        } else {
            self.parse_primary()
        };
        self.finish_expression_from_primary(expression, minimum_precedence)
    }

    /// Finishes parsing suffix and binary operators after a primary expression.
    pub(crate) fn finish_expression_from_primary(
        &mut self,
        mut expression: Expr,
        minimum_precedence: u8,
    ) -> Expr {
        expression = self.parse_suffixes(expression);
        while let Some((op, precedence, right_associative)) =
            self.consume_binary_operator(minimum_precedence)
        {
            let next_minimum = if right_associative {
                precedence
            } else {
                precedence + 1
            };
            let right = self.parse_binary_expression(next_minimum);
            let location = Location::new(expr_location(&expression).begin, expr_end(&right));
            expression = Expr::Binary {
                syntax_id: self.fresh_syntax_id(),
                location: Some(location),
                op,
                left: Box::new(expression),
                right: Box::new(right),
            };
        }

        expression
    }

    /// Consumes a unary operator, including Luau's recovered confusable `!`.
    pub(crate) fn consume_unary_operator(&mut self) -> Option<(UnaryOp, Position)> {
        if let Some(op) = unary_op(self.current.kind) {
            let start = self.current.location.begin;
            self.advance();
            return Some((op, start));
        }

        if self.current.kind == TokenKind::Char('!') {
            let token = self.current.clone();
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "unexpected '!'; did you mean 'not'?".to_owned(),
                location: token.location,
            });
            self.advance();
            return Some((UnaryOp::Not, token.location.begin));
        }

        None
    }

    /// Consumes a binary operator, including contiguous Luau confusable tokens.
    pub(crate) fn consume_binary_operator(
        &mut self,
        minimum_precedence: u8,
    ) -> Option<(BinaryOp, u8, bool)> {
        if let Some((op, precedence, location, message)) = self.current_binary_confusable() {
            if precedence < minimum_precedence {
                return None;
            }
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message,
                location,
            });
            self.advance();
            self.advance();
            return Some((op, precedence, false));
        }

        let (op, precedence, right_associative) = binary_op(self.current.kind)?;
        if precedence < minimum_precedence {
            return None;
        }
        self.advance();
        Some((op, precedence, right_associative))
    }

    /// Returns the recovered binary operator for a contiguous confusable pair.
    pub(crate) fn current_binary_confusable(&self) -> Option<(BinaryOp, u8, Location, String)> {
        let current = self.current.clone();
        if !matches!(
            current.kind,
            TokenKind::Char('!') | TokenKind::Char('&') | TokenKind::Char('|')
        ) {
            return None;
        }

        let next = self.peek_raw();
        if current.location.end != next.location.begin {
            return None;
        }

        let (op, precedence, message) = match (current.kind, next.kind) {
            (TokenKind::Char('!'), TokenKind::Char('=')) => (
                BinaryOp::CompareNe,
                3,
                "Unexpected '!='; did you mean '~='?",
            ),
            (TokenKind::Char('&'), TokenKind::Char('&')) => {
                (BinaryOp::And, 2, "Unexpected '&&'; did you mean 'and'?")
            }
            (TokenKind::Char('|'), TokenKind::Char('|')) => {
                (BinaryOp::Or, 1, "Unexpected '||'; did you mean 'or'?")
            }
            _ => return None,
        };
        Some((
            op,
            precedence,
            Location::new(current.location.begin, next.location.end),
            message.to_owned(),
        ))
    }

    /// Parses postfix expression suffixes.
    pub(crate) fn parse_suffixes(&mut self, mut expression: Expr) -> Expr {
        loop {
            let crosses_line = self.current.location.begin.line > expr_end(&expression).line;
            match self.current.kind {
                TokenKind::Char('(') if !crosses_line && expr_can_be_called(&expression) => {
                    expression = self.parse_call(expression);
                }
                TokenKind::Char('(')
                    if crosses_line
                        && self.allow_ambiguous_newline_call
                        && expr_can_be_called(&expression) =>
                {
                    self.report_ambiguous_newline_call(self.current.location);
                    expression = self.parse_call(expression);
                }
                TokenKind::Char('{') if !crosses_line && expr_can_be_called(&expression) => {
                    expression = self.parse_table_call(expression);
                }
                TokenKind::QuotedString | TokenKind::RawString | TokenKind::InterpStringSimple => {
                    if crosses_line || !expr_can_be_called(&expression) {
                        break;
                    }
                    expression = self.parse_string_call(expression);
                }
                TokenKind::DoubleColon => expression = self.parse_type_assertion(expression),
                TokenKind::Char('.') => {
                    expression = self.parse_index_name(expression, IndexOp::Dot);
                }
                TokenKind::Char(':') => {
                    let method = self.parse_index_name(expression, IndexOp::Colon);
                    let (type_arguments, type_arguments_end) =
                        if self.starts_explicit_type_instantiation() {
                            self.parse_type_instantiation()
                        } else {
                            (Vec::new(), expr_end(&method))
                        };
                    expression = if self.current.kind == TokenKind::Char('(') {
                        if self.current.location.begin.line > expr_end(&method).line {
                            self.report_ambiguous_newline_call(self.current.location);
                        }
                        self.parse_self_call(method, type_arguments)
                    } else if type_arguments.is_empty()
                        && starts_ambiguous_newline_call(&method, &self.current)
                    {
                        self.report_ambiguous_newline_call(self.current.location);
                        self.parse_self_call(method, type_arguments)
                    } else if type_arguments.is_empty()
                        && !starts_ambiguous_newline_call(&method, &self.current)
                    {
                        let location = missing_call_args_location(&method, &self.current);
                        let message_index = self.errors.len();
                        let message =
                            if uses_missing_method_call_arguments_message(&method, &self.current) {
                                "Expected function call arguments after '('".to_owned()
                            } else {
                                expected_call_arguments_message(&self.current)
                            };
                        self.errors.push(Error {
                            kind: ErrorKind::ExpectedToken,
                            message,
                            location,
                        });
                        Expr::Error {
                            syntax_id: self.fresh_syntax_id(),
                            location: Some(location),
                            expressions: vec![method],
                            message_index: Some(message_index),
                        }
                    } else if type_arguments.is_empty() {
                        method
                    } else {
                        let location =
                            Location::new(expr_location(&method).begin, type_arguments_end);
                        let message_index = self.errors.len();
                        self.errors.push(Error {
                            kind: ErrorKind::ExpectedToken,
                            message: expected_call_arguments_message(&self.current),
                            location,
                        });
                        Expr::Error {
                            syntax_id: self.fresh_syntax_id(),
                            location: Some(location),
                            expressions: vec![method],
                            message_index: Some(message_index),
                        }
                    };
                }
                TokenKind::Char('[') => expression = self.parse_index_expr(expression),
                TokenKind::Char('<') if self.starts_explicit_type_instantiation() => {
                    expression = self.parse_explicit_type_instantiation(expression);
                }
                _ => break,
            }
        }
        expression
    }

    /// Parses a primary expression.
    pub(crate) fn parse_primary(&mut self) -> Expr {
        if self.current.kind == TokenKind::InterpStringBegin {
            return self.parse_interp_string();
        }

        if self.current.kind == TokenKind::Attribute {
            let attributes = self.parse_attributes();
            let start = attributes
                .first()
                .and_then(|attribute| attribute.location)
                .map_or(self.current.location.begin, |location| location.begin);
            let attribute_location = attributes
                .first()
                .and_then(|attribute| attribute.location)
                .unwrap_or(self.current.location);
            if self.current.kind == TokenKind::ReservedFunction {
                self.advance();
                return self.parse_function_tail(start, String::new(), attributes, None);
            }

            let message_index = self.errors.len();
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: format!(
                    "Expected 'function' declaration after attribute, but got {} instead",
                    self.current.display()
                ),
                location: attribute_location,
            });
            return Expr::Error {
                syntax_id: self.fresh_syntax_id(),
                location: Some(attribute_location),
                expressions: Vec::new(),
                message_index: Some(message_index),
            };
        }

        if self.current.kind == TokenKind::ReservedReturn {
            let token = self.current.clone();
            let message_index = self.errors.len();
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: expected_expression_message(&token),
                location: token.location,
            });
            return Expr::Error {
                syntax_id: self.fresh_syntax_id(),
                location: Some(token.location),
                expressions: Vec::new(),
                message_index: Some(message_index),
            };
        }

        let token = self.advance();
        match token.kind {
            TokenKind::Name => self.name_expression(&token),
            TokenKind::ReservedNil => Expr::Nil {
                syntax_id: self.fresh_syntax_id(),
                location: Some(token.location),
            },
            TokenKind::ReservedTrue | TokenKind::ReservedFalse => Expr::Bool {
                syntax_id: self.fresh_syntax_id(),
                location: Some(token.location),
                value: token.kind == TokenKind::ReservedTrue,
            },
            TokenKind::QuotedString | TokenKind::RawString | TokenKind::InterpStringSimple => {
                if let Some(value) = self.string_value_from_token(&token) {
                    Expr::String {
                        syntax_id: self.fresh_syntax_id(),
                        location: Some(token.location),
                        value,
                    }
                } else {
                    let message_index = self.errors.len();
                    self.errors.push(Error {
                        kind: ErrorKind::MalformedSyntax,
                        message: "string literal contains malformed escape sequence".to_owned(),
                        location: token.location,
                    });
                    Expr::Error {
                        syntax_id: self.fresh_syntax_id(),
                        location: Some(token.location),
                        expressions: Vec::new(),
                        message_index: Some(message_index),
                    }
                }
            }
            TokenKind::BrokenString => {
                let message_index = self.errors.len();
                self.errors.push(Error {
                    kind: ErrorKind::MalformedSyntax,
                    message: "Malformed string; did you forget to finish it?".to_owned(),
                    location: token.location,
                });
                Expr::Error {
                    syntax_id: self.fresh_syntax_id(),
                    location: Some(token.location),
                    expressions: Vec::new(),
                    message_index: Some(message_index),
                }
            }
            TokenKind::BrokenInterpDoubleBrace => {
                let message_index = self.errors.len();
                self.errors.push(Error {
                    kind: ErrorKind::MalformedSyntax,
                    message:
                        "Double braces are not permitted within interpolated strings; did you mean '\\{'?"
                            .to_owned(),
                    location: token.location,
                });
                Expr::Error {
                    syntax_id: self.fresh_syntax_id(),
                    location: Some(token.location),
                    expressions: Vec::new(),
                    message_index: Some(message_index),
                }
            }
            TokenKind::BrokenComment => {
                if self.options.capture_comments {
                    self.comments.push(comment_from_token(token.clone()));
                }
                self.errors.push(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: expected_expression_message(&token),
                    location: token.location,
                });
                Expr::Error {
                    syntax_id: self.fresh_syntax_id(),
                    location: Some(token.location),
                    expressions: Vec::new(),
                    message_index: Some(0),
                }
            }
            TokenKind::ReservedFunction => {
                self.parse_function_tail(token.location.begin, String::new(), Vec::new(), None)
            }
            TokenKind::ReservedIf => self.parse_if_else_expression(token.location.begin),
            TokenKind::Dot3 => {
                if self.function_varargs.last().copied().unwrap_or(true) {
                    Expr::Varargs {
                        syntax_id: self.fresh_syntax_id(),
                        location: Some(token.location),
                    }
                } else {
                    let message_index = self.errors.len();
                    self.errors.push(Error {
                        kind: ErrorKind::MalformedSyntax,
                        message: "Cannot use '...' outside of a vararg function".to_owned(),
                        location: token.location,
                    });
                    Expr::Error {
                        syntax_id: self.fresh_syntax_id(),
                        location: Some(token.location),
                        expressions: Vec::new(),
                        message_index: Some(message_index),
                    }
                }
            }
            TokenKind::Number
                if self.syntax_flags.luau_integer_type
                    && token
                        .text
                        .as_deref()
                        .is_some_and(|text| text.ends_with(['i', 'I'])) =>
            {
                Expr::Integer {
                    syntax_id: self.fresh_syntax_id(),
                    location: Some(token.location),
                    value: token
                        .text
                        .as_deref()
                        .and_then(parse_integer_literal)
                        .unwrap_or(0),
                }
            }
            TokenKind::Number => {
                let text = token.text.as_deref().unwrap_or_default();
                if number_literal_is_malformed(text) {
                    let message_index = self.errors.len();
                    self.errors.push(Error {
                        kind: ErrorKind::MalformedSyntax,
                        message: "Malformed number".to_owned(),
                        location: token.location,
                    });
                    Expr::Error {
                        syntax_id: self.fresh_syntax_id(),
                        location: Some(token.location),
                        expressions: Vec::new(),
                        message_index: Some(message_index),
                    }
                } else {
                    Expr::Number {
                        syntax_id: self.fresh_syntax_id(),
                        location: Some(token.location),
                        value: parse_number(text),
                    }
                }
            }
            TokenKind::Char('(') => {
                let expression = self.parse_expression();
                let end = if self.current.kind == TokenKind::ReservedReturn {
                    token.location.end
                } else {
                    let close = self.expect_expression_group_close(token.location.begin);
                    close.map_or_else(|| expr_end(&expression), |token| token.location.end)
                };
                Expr::Group {
                    syntax_id: self.fresh_syntax_id(),
                    location: Some(Location::new(token.location.begin, end)),
                    expr: Box::new(expression),
                }
            }
            TokenKind::Char('{') => self.parse_table(token.location.begin),
            _ => {
                let message_index = self.error_index_at(token.location).unwrap_or_else(|| {
                    let message_index = self.errors.len();
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: expected_expression_message(&token),
                        location: token.location,
                    });
                    message_index
                });
                Expr::Error {
                    syntax_id: self.fresh_syntax_id(),
                    location: Some(token.location),
                    expressions: Vec::new(),
                    message_index: Some(message_index),
                }
            }
        }
    }

    /// Parses an interpolated string expression.
    pub(crate) fn parse_interp_string(&mut self) -> Expr {
        let start = self.current.location.begin;
        let mut end = self.current.location.end;
        let mut strings = Vec::new();
        let mut expressions = Vec::new();

        loop {
            let token = self.current.clone();
            match token.kind {
                TokenKind::InterpStringBegin
                | TokenKind::InterpStringMid
                | TokenKind::InterpStringEnd
                | TokenKind::InterpStringSimple => {}
                _ => break,
            }

            end = token.location.end;
            let is_final = matches!(
                token.kind,
                TokenKind::InterpStringEnd | TokenKind::InterpStringSimple
            );
            let Some(string_value) = self.string_value_from_token(&token) else {
                let location = Location::new(start, token.location.end);
                let message_index = self.errors.len();
                self.errors.push(Error {
                    kind: ErrorKind::MalformedSyntax,
                    message: "interpolated string literal contains malformed escape sequence"
                        .to_owned(),
                    location,
                });
                self.advance();
                return Expr::Error {
                    syntax_id: self.fresh_syntax_id(),
                    location: Some(location),
                    expressions: Vec::new(),
                    message_index: Some(message_index),
                };
            };
            strings.push(string_value);
            self.advance();

            if is_final {
                break;
            }

            match self.current.kind {
                TokenKind::InterpStringMid | TokenKind::InterpStringEnd => {
                    let location = token.location;
                    let message_index = self.errors.len();
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: "Malformed interpolated string, expected expression inside '{}'"
                            .to_owned(),
                        location,
                    });
                    expressions.push(Expr::Error {
                        syntax_id: self.fresh_syntax_id(),
                        location: Some(location),
                        expressions: Vec::new(),
                        message_index: Some(message_index),
                    });
                    self.advance();
                    end = location.end;
                    break;
                }
                TokenKind::BrokenString => {
                    let location = token.location;
                    let message_index = self.errors.len();
                    self.errors.push(Error {
                        kind: ErrorKind::MalformedSyntax,
                        message: "Malformed interpolated string; did you forget to add a '`'?"
                            .to_owned(),
                        location,
                    });
                    self.advance();
                    return Expr::Error {
                        syntax_id: self.fresh_syntax_id(),
                        location: Some(location),
                        expressions: Vec::new(),
                        message_index: Some(message_index),
                    };
                }
                _ => expressions.push(self.parse_expression()),
            }

            match self.current.kind {
                TokenKind::InterpStringBegin
                | TokenKind::InterpStringMid
                | TokenKind::InterpStringEnd => {}
                TokenKind::BrokenInterpDoubleBrace => {
                    let location = token.location;
                    let message_index = self.errors.len();
                    self.errors.push(Error {
                        kind: ErrorKind::MalformedSyntax,
                        message:
                            "Double braces are not permitted within interpolated strings; did you mean '\\{'?"
                                .to_owned(),
                        location,
                    });
                    self.advance();
                    return Expr::Error {
                        syntax_id: self.fresh_syntax_id(),
                        location: Some(location),
                        expressions: Vec::new(),
                        message_index: Some(message_index),
                    };
                }
                TokenKind::BrokenString | TokenKind::Eof => {
                    let location = expressions
                        .last()
                        .and_then(|expression| {
                            self.missing_interpolation_curly_location(
                                expr_end(expression),
                                self.current.location,
                                self.current.kind,
                            )
                        })
                        .unwrap_or(self.current.location);
                    end = location.end;
                    let message = self
                        .missing_interpolation_delimiter_message(location)
                        .to_owned();
                    self.errors.push(Error {
                        kind: ErrorKind::MalformedSyntax,
                        message,
                        location,
                    });
                    if self.current.kind == TokenKind::BrokenString {
                        self.advance();
                    }
                    break;
                }
                _ => {
                    let location = token.location;
                    let message_index = self.errors.len();
                    self.errors.push(Error {
                        kind: ErrorKind::MalformedSyntax,
                        message: format!(
                            "Malformed interpolated string, got {}",
                            self.current.display()
                        ),
                        location,
                    });
                    return Expr::Error {
                        syntax_id: self.fresh_syntax_id(),
                        location: Some(location),
                        expressions: Vec::new(),
                        message_index: Some(message_index),
                    };
                }
            }
        }

        Expr::InterpString {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(start, end)),
            strings,
            expressions,
        }
    }

    /// Parses an `if ... then ... else ...` expression after consuming `if`.
    pub(crate) fn parse_if_else_expression(&mut self, start: Position) -> Expr {
        let condition = self.parse_expression();
        let then = self.expect_token(TokenKind::ReservedThen, "'then'");
        let true_expr = self.parse_expression();
        let (has_else, false_expr) = if self.current.kind == TokenKind::ReservedElseif {
            let elseif_start = self.current.location.begin;
            self.advance();
            (true, self.parse_if_else_expression(elseif_start))
        } else {
            let else_token = self.expect_token(TokenKind::ReservedElse, "'else'");
            (else_token.is_some(), self.parse_expression())
        };
        let end = expr_end(&false_expr);

        Expr::IfElse {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(start, end)),
            condition: Box::new(condition),
            has_then: then.is_some(),
            true_expr: Box::new(true_expr),
            has_else,
            false_expr: Box::new(false_expr),
        }
    }

    /// Builds a name expression, resolving locals visible to this parser slice.
    pub(crate) fn name_expression(&mut self, token: &Lexeme) -> Expr {
        let name = token_name(token);
        if let Some(local) = self
            .locals
            .iter()
            .rev()
            .find(|local| local.name.as_str() == name)
            .cloned()
        {
            if local.function_depth < self.type_function_depth {
                let message_index = self.errors.len();
                self.errors.push(Error {
                    kind: ErrorKind::MalformedSyntax,
                    message: format!("type function cannot reference outer local '{name}'"),
                    location: self.current.location,
                });
                return Expr::Error {
                    syntax_id: self.fresh_syntax_id(),
                    location: Some(self.current.location),
                    expressions: Vec::new(),
                    message_index: Some(message_index),
                };
            }
            Expr::Local {
                syntax_id: self.fresh_syntax_id(),
                location: Some(token.location),
                local,
            }
        } else {
            Expr::Global {
                syntax_id: self.fresh_syntax_id(),
                location: Some(token.location),
                name: Name::new(name),
            }
        }
    }

    /// Parses a function call suffix.
    pub(crate) fn parse_call(&mut self, func: Expr) -> Expr {
        let func_location = expr_location(&func);
        let open = self.current.location;
        self.advance();

        let mut args = Vec::new();
        if self.current.kind != TokenKind::Char(')') {
            args.push(self.parse_expression());
            while self.consume_char(',').is_some() {
                if self.current.kind == TokenKind::Char(')') {
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: expected_after_comma_message("expression", &self.current),
                        location: self.current.location,
                    });
                    break;
                }
                args.push(self.parse_expression());
            }
        }

        let close = self.expect_char_to_close(')', "'('", open.begin);
        let end = close.map_or_else(
            || {
                if self.current.kind == TokenKind::Eof {
                    self.current.location.begin
                } else {
                    self.current.location.end
                }
            },
            |token| call_close_end(&args, &token),
        );
        Expr::Call {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(func_location.begin, end)),
            func: Box::new(func),
            type_arguments: Vec::new(),
            args,
            is_self: false,
            arg_location: Some(Location::new(open.end, end)),
        }
    }

    /// Parses a method-call suffix after the method index has been parsed.
    pub(crate) fn parse_self_call(
        &mut self,
        func: Expr,
        type_arguments: Vec<TypeParameter>,
    ) -> Expr {
        let func_location = expr_location(&func);
        let open = self.current.location;
        self.advance();

        let mut args = Vec::new();
        if self.current.kind != TokenKind::Char(')') {
            args.push(self.parse_expression());
            while self.consume_char(',').is_some() {
                if self.current.kind == TokenKind::Char(')') {
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: expected_after_comma_message("expression", &self.current),
                        location: self.current.location,
                    });
                    break;
                }
                args.push(self.parse_expression());
            }
        }

        let close = self.expect_char_to_close(')', "'('", open.begin);
        let end = close.map_or_else(
            || {
                if self.current.kind == TokenKind::Eof {
                    self.current.location.begin
                } else {
                    args.last().map_or(open.end, expr_end)
                }
            },
            |token| call_close_end(&args, &token),
        );
        Expr::Call {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(func_location.begin, end)),
            func: Box::new(func),
            type_arguments,
            args,
            is_self: true,
            arg_location: Some(Location::new(open.end, end)),
        }
    }

    /// Parses a table-call suffix.
    pub(crate) fn parse_table_call(&mut self, func: Expr) -> Expr {
        let func_location = expr_location(&func);
        let start = self.current.location.begin;
        self.advance();
        let table = self.parse_table(start);
        let table_location = expr_location(&table);
        Expr::Call {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(func_location.begin, table_location.end)),
            func: Box::new(func),
            type_arguments: Vec::new(),
            args: vec![table],
            is_self: false,
            arg_location: Some(Location::new(
                Position::new(table_location.begin.line, table_location.begin.column + 1),
                table_location.end,
            )),
        }
    }

    /// Parses a string-call suffix, such as `call "text"`.
    pub(crate) fn parse_string_call(&mut self, func: Expr) -> Expr {
        let func_location = expr_location(&func);
        let arg = self.parse_primary();
        let arg_location = expr_location(&arg);
        Expr::Call {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(func_location.begin, arg_location.end)),
            func: Box::new(func),
            type_arguments: Vec::new(),
            args: vec![arg],
            is_self: false,
            arg_location: Some(arg_location),
        }
    }

    /// Parses an explicit type-instantiation suffix, such as `<<T, U>>`.
    pub(crate) fn parse_explicit_type_instantiation(&mut self, expr: Expr) -> Expr {
        let start = expr_location(&expr).begin;
        let (type_arguments, end) = self.parse_type_instantiation();

        Expr::Instantiate {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(start, end)),
            expr: Box::new(expr),
            type_arguments,
        }
    }

    /// Parses the type argument list inside an explicit instantiation.
    pub(crate) fn parse_type_instantiation(&mut self) -> (Vec<TypeParameter>, Position) {
        self.expect_char('<');
        self.expect_char('<');

        let mut arguments = Vec::new();
        if self.current.kind != TokenKind::Char('>') {
            arguments.push(self.parse_type_parameter());
            while self.consume_char(',').is_some() {
                arguments.push(self.parse_type_parameter());
            }
        }

        self.expect_char('>');
        let end = self.expect_char('>').map_or_else(
            || {
                arguments
                    .last()
                    .map_or(self.current.location.begin, type_parameter_end)
            },
            |token| token.location.end,
        );
        (arguments, end)
    }

    /// Returns whether the current token begins an explicit type instantiation.
    pub(crate) fn starts_explicit_type_instantiation(&self) -> bool {
        self.current.kind == TokenKind::Char('<')
            && self.peek_significant_kind() == TokenKind::Char('<')
    }

    /// Parses a type assertion suffix.
    pub(crate) fn parse_type_assertion(&mut self, expr: Expr) -> Expr {
        let start = expr_location(&expr).begin;
        self.advance();
        let annotation = self.parse_type_expression();
        let end = type_location(&annotation).end;
        Expr::TypeAssertion {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(start, end)),
            expr: Box::new(expr),
            annotation: Box::new(annotation),
        }
    }

    /// Parses a name-index suffix.
    pub(crate) fn parse_index_name(&mut self, expr: Expr, op: IndexOp) -> Expr {
        let expr_location = expr_location(&expr);
        let op_token = self.advance();

        let token = self.current.clone();
        let separated_from_operator = token.location.begin.line > op_token.location.begin.line;
        let mut consumed_index = false;
        let index = if token.kind == TokenKind::Name {
            self.advance();
            consumed_index = true;
            Name::new(token_name(&token))
        } else if token.kind == TokenKind::Eof
            || (op == IndexOp::Colon && token.kind == TokenKind::Char('-'))
            || separated_from_operator
        {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: expected_index_name_message(&token, op),
                location: token.location,
            });
            Name::new("%error-id%")
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: expected_index_name_message(&token, op),
                location: token.location,
            });
            self.advance();
            consumed_index = true;
            Name::new(token_name(&token))
        };
        let index_location = if consumed_index {
            token.location
        } else {
            Location::new(token.location.begin, token.location.begin)
        };
        Expr::IndexName {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(expr_location.begin, index_location.end)),
            expr: Box::new(expr),
            index,
            index_location: Some(index_location),
            op,
        }
    }

    /// Parses an expression-index suffix.
    pub(crate) fn parse_index_expr(&mut self, expr: Expr) -> Expr {
        let expr_location = expr_location(&expr);
        let open = self.current.location.begin;
        self.advance();

        let index = self.parse_expression();
        let close = self.expect_char_to_close(']', "'['", open);
        let end = close.map_or_else(|| expr_end(&index), |token| token.location.end);
        Expr::IndexExpr {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(expr_location.begin, end)),
            expr: Box::new(expr),
            index: Box::new(index),
        }
    }

    /// Parses a table constructor after consuming the opening `{`.
    pub(crate) fn parse_table(&mut self, start: Position) -> Expr {
        let mut items = Vec::new();

        while self.current.kind != TokenKind::Eof && self.current.kind != TokenKind::Char('}') {
            self.skip_comments();
            if self.current.kind == TokenKind::Char('}') {
                break;
            }

            items.push(self.parse_table_item());
            match self.current.kind {
                TokenKind::Char(',') | TokenKind::Char(';') => {
                    self.advance();
                }
                TokenKind::Char('}') | TokenKind::Eof => break,
                _ if starts_table_item(self.current.kind) => {
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: "expected ',' after table constructor element".to_owned(),
                        location: self.current.location,
                    });
                }
                _ => break,
            }
        }

        let close = self.expect_char_to_close('}', "'{'", start);
        let end = close.map_or_else(
            || items.last().map_or(start, |item| expr_end(&item.value)),
            |token| token.location.end,
        );
        Expr::Table {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(start, end)),
            items,
        }
    }

    /// Parses one table constructor item.
    pub(crate) fn parse_table_item(&mut self) -> TableItem {
        match self.current.kind {
            TokenKind::Name => {
                let key = self.advance();
                if self.consume_char('=').is_some() {
                    let mut value = self.parse_expression();
                    if let Expr::Function { debug_name, .. } = &mut value {
                        *debug_name = token_name(&key);
                    }
                    TableItem {
                        kind: TableItemKind::Record,
                        key: Some(Expr::String {
                            syntax_id: self.fresh_syntax_id(),
                            location: Some(key.location),
                            value: token_name(&key),
                        }),
                        value,
                    }
                } else {
                    let key_expr = self.name_expression(&key);
                    let value = self.finish_expression_from_primary(key_expr, 0);
                    TableItem {
                        kind: TableItemKind::Item,
                        key: None,
                        value,
                    }
                }
            }
            TokenKind::Char('[') => {
                self.advance();
                let key = self.parse_expression();
                self.expect_char(']');
                self.expect_char('=');
                TableItem {
                    kind: TableItemKind::General,
                    key: Some(key),
                    value: self.parse_expression(),
                }
            }
            _ => TableItem {
                kind: TableItemKind::Item,
                key: None,
                value: self.parse_expression(),
            },
        }
    }

    /// Parses a function parameter list and body after the `function` keyword.
    pub(crate) fn parse_function_tail(
        &mut self,
        start: Position,
        debug_name: String,
        attributes: Vec<Attribute>,
        self_arg: Option<Local>,
    ) -> Expr {
        let (generics, generic_packs) = if self.current.kind == TokenKind::Char('<') {
            self.parse_generic_parameters()
        } else {
            (Vec::new(), Vec::new())
        };
        let open = self.expect_function_open_or_skip_extra();

        let saved_local_count = self.locals.len();
        if let Some(self_arg) = &self_arg {
            self.locals.push(self_arg.to_local_ref());
        }
        let mut args = Vec::new();
        let mut vararg = false;
        let mut vararg_location = None;
        let mut vararg_annotation = None;
        let mut param_end = self.current.location.begin;

        if self.current.kind != TokenKind::Char(')') {
            loop {
                match self.current.kind {
                    TokenKind::Name => {
                        let token = self.advance();
                        let annotation = self.parse_optional_type_annotation();
                        let local = self.fresh_local(
                            Name::new(token_name(&token)),
                            Some(token.location),
                            annotation,
                            false,
                            self.function_depth + 1,
                        );
                        param_end = token.location.end;
                        self.locals.push(local.to_local_ref());
                        args.push(local);
                    }
                    TokenKind::Dot3 => {
                        vararg = true;
                        vararg_location = Some(self.current.location);
                        param_end = self.current.location.end;
                        self.advance();
                        if self.consume_char(':').is_some() {
                            vararg_annotation = Some(Box::new(self.parse_vararg_annotation()));
                            if let Some(annotation) = &vararg_annotation {
                                param_end = type_pack_location(annotation).end;
                            }
                        }
                    }
                    _ => {
                        self.errors.push(Error {
                            kind: ErrorKind::ExpectedToken,
                            message: "expected function parameter".to_owned(),
                            location: self.current.location,
                        });
                        break;
                    }
                }

                if vararg || self.consume_char(',').is_none() {
                    break;
                }
            }
        }

        let open_position = open.as_ref().map_or(start, |token| token.location.begin);
        let mut close = self.expect_char_to_close(')', "'('", open_position);
        if close.is_none() && self.has_char_on_line(')', self.current.location.begin.line) {
            close = self.recover_to_char_on_line(')', self.current.location.begin.line);
        }
        let mut body_start = close.map_or(param_end, |token| token.location.end);
        let return_annotation = if self.consume_char(':').is_some() {
            let annotation = self.parse_return_type_pack();
            body_start = type_pack_deep_end(&annotation);
            if let Some(recovery_end) = self.type_recovery_end.take() {
                body_start = recovery_end;
            }
            if self.current.kind == TokenKind::Char(',') {
                self.errors.push(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: "Expected a statement, got ','; did you forget to wrap the list of return types in parentheses?"
                        .to_owned(),
                    location: self.current.location,
                });
                body_start = self.current.location.end;
                self.advance();
            }
            Some(Box::new(annotation))
        } else if self.current.kind == TokenKind::SkinnyArrow {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "Function return type annotations are written after ':' instead of '->'"
                    .to_owned(),
                location: self.current.location,
            });
            self.advance();
            let annotation = self.parse_return_type_pack();
            body_start = type_pack_deep_end(&annotation);
            if let Some(recovery_end) = self.type_recovery_end.take() {
                body_start = recovery_end;
            }
            Some(Box::new(annotation))
        } else {
            None
        };
        let function_depth = self.function_depth + 1;
        self.function_depth = function_depth;
        self.function_varargs.push(vararg);
        let (body, end) = self.parse_body_until_end(body_start, start);
        self.function_varargs.pop();
        self.function_depth -= 1;
        self.locals.truncate(saved_local_count);

        Expr::Function {
            syntax_id: self.fresh_syntax_id(),
            location: Some(Location::new(start, end)),
            attributes,
            generics,
            generic_packs,
            args,
            self_arg,
            vararg,
            vararg_location,
            vararg_annotation,
            return_annotation,
            body: Box::new(body),
            function_depth,
            debug_name,
        }
    }

    /// Parses the annotation after a function vararg parameter.
    pub(crate) fn parse_vararg_annotation(&mut self) -> TypePack {
        if self.current.kind == TokenKind::Name && self.peek_significant_kind() == TokenKind::Dot3 {
            let token = self.advance();
            let dots = self.expect_token(TokenKind::Dot3, "'...'");
            let end = dots.map_or(token.location.end, |dots| dots.location.end);
            return TypePack::Generic {
                location: Some(Location::new(token.location.begin, end)),
                name: Name::new(token_name(&token)),
            };
        }

        let variadic_type = self.parse_type_expression();
        let location = type_location(&variadic_type);
        TypePack::Variadic {
            location: Some(location),
            variadic_type: Box::new(variadic_type),
        }
    }

    /// Parses statements until an `end` token and consumes that token.
    pub(crate) fn parse_body_until_end(
        &mut self,
        start: Position,
        function_start: Position,
    ) -> (Stat, Position) {
        self.recursion_depth += 1;
        if self.recursion_depth >= PARSER_RECURSION_LIMIT {
            let body = self.recursion_limit_error_block();
            let end = self.current.location.begin;
            self.recursion_depth -= 1;
            return (body, end);
        }

        let mut body = Vec::new();
        self.block_depth += 1;
        while !matches!(self.current.kind, TokenKind::Eof | TokenKind::ReservedEnd) {
            self.skip_comments();
            if matches!(self.current.kind, TokenKind::Eof | TokenKind::ReservedEnd) {
                break;
            }

            if let Some(statement) = self.parse_statement() {
                body.push(statement);
                if statement_is_last(body.last().expect("statement was just pushed")) {
                    break;
                }
            }
        }
        self.block_depth -= 1;

        let has_end = self.current.kind == TokenKind::ReservedEnd;
        let (block_end, function_end) = if has_end {
            let token = self.advance();
            (token.location.begin, token.location.end)
        } else {
            let hint = self.nesting_hint("else");
            self.push_expected_token(
                format!(
                    "Expected 'end' (to close 'function' at {}), got {}{}",
                    opening_position_description(function_start),
                    self.current.display(),
                    hint
                ),
                self.current.location,
            );
            (self.current.location.begin, self.current.location.begin)
        };
        self.recursion_depth -= 1;

        (
            Stat::Block {
                location: Some(Location::new(start, block_end)),
                has_end,
                is_do: false,
                body,
            },
            function_end,
        )
    }
}
