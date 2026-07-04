//! Parser stat parsing.

use super::{
    PARSER_RECURSION_LIMIT, Parser,
    common::{
        compound_assign_op, expected_call_arguments_message, expected_expression_message,
        expected_identifier_message, expr_end, expr_is_assignable, expr_is_const_local,
        expr_location, expr_may_return_multiple, expression_identifier_name, local_end,
        opening_position_description, starts_ambiguous_newline_call, stat_end,
        stat_error_from_expression, statement_is_last, token_name,
    },
};
use crate::{
    Location, Position,
    lexer::TokenKind,
    parse::{Error, ErrorKind},
    syntax::{Attribute, CompoundAssignOp, Expr, IndexOp, Local, Name, Stat},
};

const EXPORT_TOP_LEVEL_MESSAGE: &str = "'export' may only be applied to top-level statements";
const EXPORT_RETURN_CONFLICT_MESSAGE: &str =
    "Exporting values is not compatible with top-level return (export/return conflict)";

impl<'source> Parser<'source> {
    /// Parses a block until EOF.
    pub(crate) fn parse_block(&mut self) -> Stat {
        self.recursion_depth += 1;
        let result = if self.recursion_depth >= PARSER_RECURSION_LIMIT {
            self.recursion_limit_error_block()
        } else {
            self.parse_block_inner()
        };
        self.recursion_depth -= 1;
        result
    }

    pub(crate) fn parse_block_inner(&mut self) -> Stat {
        let mut body = Vec::new();
        let mut end = self.current.location.begin;

        while (self.current.kind != TokenKind::Eof || !self.pending_statements.is_empty())
            && !self.errors.limit_reached()
        {
            self.skip_comments();
            if self.current.kind == TokenKind::Eof && self.pending_statements.is_empty() {
                break;
            }
            if self.current.kind == TokenKind::ReservedEnd && self.pending_statements.is_empty() {
                end = self.current.location.begin;
                break;
            }

            if let Some(statement) = self.parse_statement() {
                body.push(statement);
                if statement_is_last(body.last().expect("statement was just pushed")) {
                    end = self.current.location.begin;
                    break;
                }
            }
        }

        if self.current.kind == TokenKind::Eof {
            end = self.current.location.begin;
        }
        let location = Location::new(Position::new(0, 0), end);

        Stat::Block {
            location: Some(location),
            has_end: true,
            is_do: false,
            body,
        }
    }

    /// Parses one statement.
    pub(crate) fn parse_statement(&mut self) -> Option<Stat> {
        if let Some(statement) = self.pending_statements.pop_front() {
            return Some(statement);
        }

        match self.current.kind {
            TokenKind::Char(';') => {
                let token = self.current.clone();
                self.errors.push(Error {
                    kind: ErrorKind::MalformedSyntax,
                    message: "Incomplete statement: expected assignment or a function call"
                        .to_owned(),
                    location: token.location,
                });
                self.advance();
                None
            }
            TokenKind::ReservedBreak => Some(self.parse_break()),
            TokenKind::ReservedDo => Some(self.parse_do()),
            TokenKind::ReservedFor => Some(self.parse_for()),
            TokenKind::ReservedFunction => Some(self.parse_function_statement()),
            TokenKind::ReservedIf => Some(self.parse_if()),
            TokenKind::Attribute | TokenKind::AttributeOpen => {
                Some(self.parse_attribute_statement())
            }
            TokenKind::Name
                if self.current.name.as_deref() == Some("continue")
                    && self.peek_starts_statement_terminator() =>
            {
                Some(self.parse_continue())
            }
            TokenKind::Name
                if self.options.allow_declaration_syntax
                    && self.current.name.as_deref() == Some("declare") =>
            {
                Some(self.parse_declaration())
            }
            TokenKind::Name
                if self.current.name.as_deref() == Some("type")
                    && matches!(
                        self.peek_significant_kind(),
                        TokenKind::Name
                            | TokenKind::ReservedFunction
                            | TokenKind::Number
                            | TokenKind::Eof
                            | TokenKind::Char('<')
                    ) =>
            {
                Some(self.parse_type_alias(false))
            }
            TokenKind::Name
                if self.syntax_flags.luau_export_value_syntax
                    && self.current.name.as_deref() == Some("export")
                    && self.peek_significant_kind() == TokenKind::ReservedFunction =>
            {
                Some(self.parse_export_function(self.current.location.begin, Vec::new()))
            }
            TokenKind::Name
                if self.syntax_flags.luau_export_value_syntax
                    && self.current.name.as_deref() == Some("export")
                    && self.peek_significant_kind() == TokenKind::ReservedLocal =>
            {
                Some(self.parse_export_local(self.current.location.begin))
            }
            TokenKind::Name
                if self.syntax_flags.luau_export_value_syntax
                    && self.current.name.as_deref() == Some("export")
                    && self.peek_significant_name().as_deref() == Some("const") =>
            {
                Some(self.parse_export_const_local(self.current.location.begin))
            }
            TokenKind::Name
                if self.current.name.as_deref() == Some("export")
                    && self.peek_significant_name().as_deref() == Some("type") =>
            {
                Some(self.parse_export_type_alias())
            }
            TokenKind::Name
                if self.syntax_flags.debug_luau_user_defined_classes
                    && self.current.name.as_deref() == Some("export")
                    && self.peek_significant_name().as_deref() == Some("class") =>
            {
                Some(self.parse_export_class())
            }
            TokenKind::Name
                if self.syntax_flags.debug_luau_user_defined_classes
                    && self.current.name.as_deref() == Some("class")
                    && self.peek_significant_kind() == TokenKind::Name =>
            {
                Some(self.parse_class(false))
            }
            TokenKind::Name
                if self.syntax_flags.luau_const2
                    && self.current.name.as_deref() == Some("const")
                    && self.peek_significant_kind() == TokenKind::ReservedFunction =>
            {
                Some(self.parse_const_function(self.current.location.begin, Vec::new()))
            }
            TokenKind::Name
                if self.syntax_flags.luau_const2
                    && self.current.name.as_deref() == Some("const")
                    && matches!(
                        self.peek_significant_kind(),
                        TokenKind::Name | TokenKind::Eof
                    ) =>
            {
                Some(self.parse_const_local())
            }
            TokenKind::ReservedLocal => Some(self.parse_local()),
            TokenKind::ReservedRepeat => Some(self.parse_repeat()),
            TokenKind::ReservedReturn => Some(self.parse_return()),
            TokenKind::ReservedWhile => Some(self.parse_while()),
            _ => self.parse_expr_statement(),
        }
    }

    /// Parses a `break` statement.
    pub(crate) fn parse_break(&mut self) -> Stat {
        let token = self.advance();
        let end = self
            .consume_char(';')
            .map_or(token.location.end, |semicolon| semicolon.location.end);
        let statement = Stat::Break {
            location: Some(Location::new(token.location.begin, end)),
        };
        if !self.loop_function_depths.contains(&self.function_depth) {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "break statement must be inside a loop".to_owned(),
                location: Location::new(token.location.begin, end),
            });
            return Stat::Error {
                location: Some(Location::new(token.location.begin, end)),
                expressions: Vec::new(),
                statements: vec![statement],
            };
        }
        statement
    }

    /// Parses a `continue` statement.
    pub(crate) fn parse_continue(&mut self) -> Stat {
        let token = self.advance();
        let end = self
            .consume_char(';')
            .map_or(token.location.end, |semicolon| semicolon.location.end);
        let statement = Stat::Continue {
            location: Some(Location::new(token.location.begin, end)),
        };
        if !self.loop_function_depths.contains(&self.function_depth) {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "continue statement must be inside a loop".to_owned(),
                location: Location::new(token.location.begin, end),
            });
            return Stat::Error {
                location: Some(Location::new(token.location.begin, end)),
                expressions: Vec::new(),
                statements: vec![statement],
            };
        }
        statement
    }

    /// Parses a `do ... end` block.
    pub(crate) fn parse_do(&mut self) -> Stat {
        let start = self.current.location.begin;
        let local_count = self.locals.len();
        self.advance();

        let mut body = Vec::new();
        self.block_depth += 1;
        while !matches!(self.current.kind, TokenKind::Eof | TokenKind::ReservedEnd)
            || !self.pending_statements.is_empty()
        {
            self.skip_comments();
            if matches!(self.current.kind, TokenKind::Eof | TokenKind::ReservedEnd)
                && self.pending_statements.is_empty()
            {
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
        let mut end = if has_end {
            let token = self.advance();
            token.location.end
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: format!(
                    "Expected 'end' (to close 'do' at {}), got {}",
                    opening_position_description(start),
                    self.current.display()
                ),
                location: self.current.location,
            });
            self.current.location.begin
        };
        if let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }
        self.locals.truncate(local_count);

        Stat::Block {
            location: Some(Location::new(start, end)),
            has_end,
            is_do: true,
            body,
        }
    }

    /// Parses statements until one of the terminator tokens is reached.
    pub(crate) fn parse_block_until(&mut self, start: Position, terminators: &[TokenKind]) -> Stat {
        self.parse_block_until_inner(start, terminators, true)
    }

    /// Parses statements until a terminator without closing the local scope.
    pub(crate) fn parse_block_until_keep_locals(
        &mut self,
        start: Position,
        terminators: &[TokenKind],
    ) -> Stat {
        self.parse_block_until_inner(start, terminators, false)
    }

    /// Parses statements until one of the terminator tokens is reached.
    pub(crate) fn parse_block_until_inner(
        &mut self,
        start: Position,
        terminators: &[TokenKind],
        truncate_locals: bool,
    ) -> Stat {
        let local_count = self.locals.len();
        self.recursion_depth += 1;
        if self.recursion_depth >= PARSER_RECURSION_LIMIT {
            let block = self.recursion_limit_error_block();
            self.recursion_depth -= 1;
            if truncate_locals {
                self.locals.truncate(local_count);
            }
            return block;
        }

        let mut body = Vec::new();

        self.block_depth += 1;
        while ((self.current.kind != TokenKind::Eof && !terminators.contains(&self.current.kind))
            || !self.pending_statements.is_empty())
            && !self.errors.limit_reached()
        {
            self.skip_comments();
            if (self.current.kind == TokenKind::Eof || terminators.contains(&self.current.kind))
                && self.pending_statements.is_empty()
            {
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

        let end = self.current.location.begin;
        let has_end = terminators.contains(&self.current.kind);
        self.recursion_depth -= 1;
        if truncate_locals {
            self.locals.truncate(local_count);
        }
        Stat::Block {
            location: Some(Location::new(start, end)),
            has_end,
            is_do: false,
            body,
        }
    }

    /// Parses a local declaration.
    pub(crate) fn parse_local(&mut self) -> Stat {
        let local_token = self.current.clone();
        let start = local_token.location.begin;
        self.advance();

        if self.current.kind == TokenKind::ReservedFunction {
            return self.parse_local_function(start);
        }

        self.parse_local_tail(start, local_token.location.end, false)
    }

    /// Parses the variable/value tail after a consumed `local` keyword.
    fn parse_local_tail(
        &mut self,
        start: Position,
        malformed_end: Position,
        exported: bool,
    ) -> Stat {
        let mut vars = Vec::new();
        loop {
            match self.current.kind {
                TokenKind::Attribute => {
                    let token = self.current.clone();
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: expected_identifier_message(&token, Some("variable name")),
                        location: token.location,
                    });
                    self.advance();
                    let local = self.fresh_local(
                        Name::new("%error-id%"),
                        Some(token.location),
                        None,
                        false,
                        self.function_depth,
                    );
                    vars.push(local);
                    if self.current.kind == TokenKind::Char('=') {
                        self.queue_attribute_assignment_recovery();
                    }
                    self.locals.extend(vars.iter().map(Local::to_local_ref));
                    return Stat::Local {
                        location: Some(Location::new(start, malformed_end)),
                        vars,
                        values: Vec::new(),
                        exported,
                    };
                }
                TokenKind::BrokenUnicode => {
                    let token = self.current.clone();
                    let message_index = self.errors.len();
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: expected_identifier_message(&token, Some("variable name")),
                        location: token.location,
                    });
                    self.advance();
                    let local = self.fresh_local(
                        Name::new("%error-id%"),
                        Some(token.location),
                        None,
                        false,
                        self.function_depth,
                    );
                    vars.push(local);
                    let syntax_id = self.fresh_syntax_id();
                    self.pending_statements.push_back(Stat::Error {
                        location: Some(token.location),
                        expressions: vec![Expr::Error {
                            syntax_id,
                            location: Some(token.location),
                            expressions: Vec::new(),
                            message_index: Some(message_index),
                        }],
                        statements: Vec::new(),
                    });
                    self.locals.extend(vars.iter().map(Local::to_local_ref));
                    return Stat::Local {
                        location: Some(Location::new(start, malformed_end)),
                        vars,
                        values: Vec::new(),
                        exported,
                    };
                }
                TokenKind::Name => {
                    let token = self.advance();
                    let annotation = self.parse_optional_type_annotation();
                    let local = self.fresh_local(
                        Name::new(token_name(&token)),
                        Some(token.location),
                        annotation,
                        false,
                        self.function_depth,
                    );
                    vars.push(local);
                }
                _ => {
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: expected_identifier_message(&self.current, Some("variable name")),
                        location: self.current.location,
                    });
                    break;
                }
            }

            if self.consume_char(',').is_none() {
                break;
            }
        }

        let mut values = Vec::new();
        if self.consume_char('=').is_some() {
            values.push(self.parse_expression());
            while self.consume_char(',').is_some() {
                values.push(self.parse_expression());
            }
            if let Some(value) = values.last()
                && starts_ambiguous_newline_call(value, &self.current)
            {
                self.report_ambiguous_newline_call(self.current.location);
            }
        }

        let type_statement_end = self.type_statement_end_override.take();
        let type_recovery_end = self.type_recovery_end.take();
        let mut end = values
            .last()
            .map_or_else(|| vars.last().map_or(start, local_end), expr_end);
        if values.is_empty() {
            if let Some(type_statement_end) = type_statement_end {
                end = type_statement_end;
            } else if let Some(recovery_end) = type_recovery_end
                && recovery_end > end
            {
                end = recovery_end;
            }
        }
        if let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }
        self.locals.extend(vars.iter().map(Local::to_local_ref));
        Stat::Local {
            location: Some(Location::new(start, end)),
            vars,
            values,
            exported,
        }
    }

    /// Parses an `export local` declaration.
    pub(crate) fn parse_export_local(&mut self, start: Position) -> Stat {
        let export_location = self.current.location;
        self.validate_export_value_declaration(export_location);
        self.advance();
        let Some(local_token) = self.expect_token(TokenKind::ReservedLocal, "'local'") else {
            return Stat::Error {
                location: Some(Location::new(start, self.current.location.end)),
                expressions: Vec::new(),
                statements: Vec::new(),
            };
        };

        if self.current.kind == TokenKind::ReservedFunction {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message:
                    "'export' must be followed by an identifier or 'function'; try removing 'local'"
                        .to_owned(),
                location: Location::new(start, local_token.location.end),
            });
            return self.parse_local_function(local_token.location.begin);
        }

        let stat = self.parse_local_tail(start, local_token.location.end, true);
        if let Stat::Local { vars, .. } = &stat {
            self.record_exported_value_bindings(vars);
        }
        stat
    }

    /// Parses an `export const` declaration.
    pub(crate) fn parse_export_const_local(&mut self, start: Position) -> Stat {
        let export_location = self.current.location;
        self.validate_export_value_declaration(export_location);
        self.advance();

        let const_token = self.current.clone();
        if const_token.kind != TokenKind::Name || const_token.name.as_deref() != Some("const") {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "Expected 'const'".to_owned(),
                location: const_token.location,
            });
            return Stat::Error {
                location: Some(Location::new(start, const_token.location.end)),
                expressions: Vec::new(),
                statements: Vec::new(),
            };
        }
        self.advance();

        if self.current.kind == TokenKind::ReservedFunction {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "'export' must be followed by an identifier or 'function'".to_owned(),
                location: Location::new(start, const_token.location.end),
            });
            return self.parse_local_function(const_token.location.begin);
        }

        let stat = self.parse_const_local_tail(start, true);
        if let Stat::Local { vars, .. } = &stat {
            self.record_exported_value_bindings(vars);
        }
        stat
    }

    /// Queues the statements produced after a malformed local attribute binding.
    pub(crate) fn queue_attribute_assignment_recovery(&mut self) {
        let equals = self.current.clone();
        let message_index = self.errors.len();
        self.errors.push(Error {
            kind: ErrorKind::ExpectedToken,
            message: format!(
                "Expected 'function', 'local function', 'declare function' or a function type declaration after attribute, but got {} instead",
                equals.display()
            ),
            location: equals.location,
        });

        self.pending_statements.push_back(Stat::Error {
            location: Some(equals.location),
            expressions: Vec::new(),
            statements: Vec::new(),
        });

        self.advance();
        let value = self.parse_expression();
        let var = Expr::Error {
            syntax_id: self.fresh_syntax_id(),
            location: Some(equals.location),
            expressions: vec![Expr::Error {
                syntax_id: self.fresh_syntax_id(),
                location: Some(equals.location),
                expressions: Vec::new(),
                message_index: Some(message_index),
            }],
            message_index: Some(message_index),
        };
        self.pending_statements.push_back(Stat::Assign {
            location: Some(Location::new(equals.location.begin, expr_end(&value))),
            vars: vec![var],
            values: vec![value],
        });
    }

    /// Parses a top-level `const` declaration when `LuauConst2` is enabled.
    pub(crate) fn parse_const_local(&mut self) -> Stat {
        let start = self.current.location.begin;
        self.advance();
        self.parse_const_local_tail(start, false)
    }

    /// Parses the variable/value tail after a consumed `const` keyword.
    fn parse_const_local_tail(&mut self, start: Position, exported: bool) -> Stat {
        let mut vars = Vec::new();
        loop {
            match self.current.kind {
                TokenKind::Name => {
                    let token = self.advance();
                    let annotation = self.parse_optional_type_annotation();
                    let local = self.fresh_local(
                        Name::new(token_name(&token)),
                        Some(token.location),
                        annotation,
                        true,
                        self.function_depth,
                    );
                    vars.push(local);
                }
                _ => {
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: "expected const name".to_owned(),
                        location: self.current.location,
                    });
                    break;
                }
            }

            if self.consume_char(',').is_none() {
                break;
            }
        }

        let mut values = Vec::new();
        if self.consume_char('=').is_some() {
            values.push(self.parse_expression());
            while self.consume_char(',').is_some() {
                values.push(self.parse_expression());
            }
        }

        let mut end = values
            .last()
            .map_or_else(|| vars.last().map_or(start, local_end), expr_end);
        if values.is_empty()
            && let Some(recovery_end) = self.type_recovery_end.take()
            && recovery_end > end
        {
            end = recovery_end;
        }
        if let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }
        if values.len() < vars.len() && !values.last().is_some_and(expr_may_return_multiple) {
            let location = Location::new(start, end);
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "missing initializer in const declaration".to_owned(),
                location,
            });
            self.locals.extend(vars.iter().map(Local::to_local_ref));
            return Stat::Error {
                location: Some(location),
                expressions: Vec::new(),
                statements: Vec::new(),
            };
        }
        self.locals.extend(vars.iter().map(Local::to_local_ref));
        Stat::Local {
            location: Some(Location::new(start, end)),
            vars,
            values,
            exported,
        }
    }

    /// Parses a `const function` declaration.
    pub(crate) fn parse_const_function(
        &mut self,
        start: Position,
        attributes: Vec<Attribute>,
    ) -> Stat {
        self.advance();
        self.expect_token(TokenKind::ReservedFunction, "'function'");

        let name_token = self.current.clone();
        let local = if name_token.kind == TokenKind::Name {
            self.advance();
            self.fresh_local(
                Name::new(token_name(&name_token)),
                Some(name_token.location),
                None,
                true,
                self.function_depth,
            )
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected const function name".to_owned(),
                location: name_token.location,
            });
            self.fresh_local(
                Name::new(""),
                Some(name_token.location),
                None,
                true,
                self.function_depth,
            )
        };
        self.locals.push(local.to_local_ref());
        let func_start = self.function_start_from_attributes(start, &attributes);
        let func =
            self.parse_function_tail(func_start, local.name.as_str().to_owned(), attributes, None);
        let func_location = expr_location(&func);
        let mut end = func_location.end;
        if let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }
        Stat::LocalFunction {
            location: Some(Location::new(start, end)),
            name: local,
            func: Box::new(func),
            exported: false,
        }
    }

    /// Parses an `export function` declaration.
    pub(crate) fn parse_export_function(
        &mut self,
        start: Position,
        attributes: Vec<Attribute>,
    ) -> Stat {
        let export_location = self.current.location;
        self.validate_export_value_declaration(export_location);
        self.advance();
        self.expect_token(TokenKind::ReservedFunction, "'function'");

        let name_token = self.current.clone();
        let local = if name_token.kind == TokenKind::Name {
            self.advance();
            self.fresh_local(
                Name::new(token_name(&name_token)),
                Some(name_token.location),
                None,
                true,
                self.function_depth,
            )
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected exported function name".to_owned(),
                location: name_token.location,
            });
            self.fresh_local(
                Name::new(""),
                Some(name_token.location),
                None,
                true,
                self.function_depth,
            )
        };
        // Upstream lowers `export function` to the local-function AST shape.
        self.record_exported_value_binding(&local);
        self.finish_local_function_statement(start, local, attributes, true)
    }

    /// Parses a global function declaration.
    pub(crate) fn parse_function_statement(&mut self) -> Stat {
        self.parse_function_statement_with_attributes(self.current.location.begin, Vec::new())
    }

    /// Parses a global function declaration with already-consumed attributes.
    pub(crate) fn parse_function_statement_with_attributes(
        &mut self,
        start: Position,
        attributes: Vec<Attribute>,
    ) -> Stat {
        let function_token = self.advance();

        let self_location = attributes
            .first()
            .and_then(|attribute| attribute.location)
            .unwrap_or(function_token.location);
        let (mut name, debug_name, self_arg) = self.parse_function_name(self_location);
        name = self.validate_assignment_target(name);
        let func_start = self.function_start_from_attributes(start, &attributes);
        let func = self.parse_function_tail(func_start, debug_name, attributes, self_arg);
        let func_location = expr_location(&func);
        let mut end = func_location.end;
        if let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }
        Stat::Function {
            location: Some(Location::new(start, end)),
            name: Box::new(name),
            func: Box::new(func),
        }
    }

    /// Parses a function declaration name after `function`.
    pub(crate) fn parse_function_name(
        &mut self,
        self_location: Location,
    ) -> (Expr, String, Option<Local>) {
        let name_token = self.current.clone();
        let (mut name, mut debug_name) = if name_token.kind == TokenKind::Name {
            self.advance();
            (self.name_expression(&name_token), token_name(&name_token))
        } else {
            let message_index = self.errors.len();
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: expected_identifier_message(&name_token, Some("function name")),
                location: name_token.location,
            });
            (
                Expr::Error {
                    syntax_id: self.fresh_syntax_id(),
                    location: Some(name_token.location),
                    expressions: Vec::new(),
                    message_index: Some(message_index),
                },
                String::new(),
            )
        };
        let mut self_arg = None;

        while matches!(
            self.current.kind,
            TokenKind::Char('.') | TokenKind::Char(':')
        ) {
            let op_token = self.current.clone();
            let op = if op_token.kind == TokenKind::Char(':') {
                IndexOp::Colon
            } else {
                IndexOp::Dot
            };
            self.advance();

            let index_token = self.current.clone();
            let index_name = if index_token.kind == TokenKind::Name {
                self.advance();
                token_name(&index_token)
            } else {
                self.errors.push(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: expected_identifier_message(&index_token, Some("function name")),
                    location: index_token.location,
                });
                String::new()
            };
            debug_name = index_name.clone();
            let start = expr_location(&name).begin;
            name = Expr::IndexName {
                syntax_id: self.fresh_syntax_id(),
                location: Some(Location::new(start, index_token.location.end)),
                expr: Box::new(name),
                index: Name::new(index_name),
                index_location: Some(index_token.location),
                op,
            };

            if op == IndexOp::Colon {
                self_arg = Some(self.fresh_local(
                    Name::new("self"),
                    Some(self_location),
                    None,
                    false,
                    self.function_depth + 1,
                ));
                break;
            }
        }

        (name, debug_name, self_arg)
    }

    /// Reports Luau's newline-call ambiguity diagnostic.
    pub(crate) fn report_ambiguous_newline_call(&mut self, location: Location) {
        self.errors.push(Error {
            kind: ErrorKind::MalformedSyntax,
            message: "Ambiguous syntax: this looks like an argument list for a function call, but could also be a start of new statement; use ';' to separate statements".to_owned(),
            location,
        });
    }

    /// Parses an `if` statement.
    pub(crate) fn parse_if(&mut self) -> Stat {
        let start = self.current.location.begin;
        self.advance();
        self.parse_if_after_keyword(start, true)
    }

    /// Parses a `while` loop.
    pub(crate) fn parse_while(&mut self) -> Stat {
        let start = self.current.location.begin;
        self.advance();
        let condition = self.parse_expression();
        let do_token = self.expect_token(TokenKind::ReservedDo, "'do'");
        let body_start = do_token
            .as_ref()
            .map_or(self.current.location.begin, |token| token.location.end);
        self.loop_function_depths.push(self.function_depth);
        let body = self.parse_block_until(body_start, &[TokenKind::ReservedEnd]);
        self.loop_function_depths.pop();
        let mut end = if self.current.kind == TokenKind::ReservedEnd {
            let token = self.advance();
            token.location.end
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: format!(
                    "Expected 'end' (to close 'do' at {}), got {}",
                    do_token.as_ref().map_or_else(
                        || opening_position_description(start),
                        |token| { opening_position_description(token.location.begin) }
                    ),
                    self.current.display()
                ),
                location: self.current.location,
            });
            self.current.location.end
        };
        if let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }

        Stat::While {
            location: Some(Location::new(start, end)),
            condition: Box::new(condition),
            body: Box::new(body),
            has_do: do_token.is_some(),
        }
    }

    /// Parses a `repeat ... until` loop.
    pub(crate) fn parse_repeat(&mut self) -> Stat {
        let start_token = self.current.clone();
        let saved_local_count = self.locals.len();
        self.advance();
        self.loop_function_depths.push(self.function_depth);
        let body = self
            .parse_block_until_keep_locals(start_token.location.end, &[TokenKind::ReservedUntil]);
        self.loop_function_depths.pop();
        let until_message_index = self.errors.len();
        let until = if self.current.kind == TokenKind::ReservedUntil {
            let token = self.advance();
            Some(token)
        } else {
            self.push_expected_token(
                format!(
                    "Expected 'until' (to close 'repeat' at {}), got {}{}",
                    opening_position_description(start_token.location.begin),
                    self.current.display(),
                    self.nesting_hint("repeat")
                ),
                self.current.location,
            );
            None
        };
        let condition = if until.is_none() && self.current.kind == TokenKind::Eof {
            self.error_expr_at(self.current.location, until_message_index)
        } else {
            self.parse_expression()
        };

        let mut end = expr_end(&condition);
        if let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }
        self.locals.truncate(saved_local_count);
        Stat::Repeat {
            location: Some(Location::new(start_token.location.begin, end)),
            condition: Box::new(condition),
            body: Box::new(body),
        }
    }

    /// Parses a numeric or generic `for` loop.
    pub(crate) fn parse_for(&mut self) -> Stat {
        let start = self.current.location.begin;
        let saved_local_count = self.locals.len();
        self.advance();

        let first = self.parse_loop_local();
        if self.current.kind == TokenKind::Char('=') {
            self.parse_numeric_for(start, saved_local_count, first)
        } else {
            self.parse_generic_for(start, saved_local_count, first)
        }
    }

    /// Parses a numeric `for` loop after the loop variable.
    pub(crate) fn parse_numeric_for(
        &mut self,
        start: Position,
        saved_local_count: usize,
        var: Local,
    ) -> Stat {
        self.expect_char('=');
        let from = self.parse_expression();
        self.expect_char(',');
        let to = self.parse_expression();
        let step = if self.consume_char(',').is_some() {
            Some(Box::new(self.parse_expression()))
        } else {
            None
        };
        self.locals.push(var.to_local_ref());
        let do_token = self.expect_token(TokenKind::ReservedDo, "'do'");
        let body_start = do_token
            .as_ref()
            .map_or(self.current.location.begin, |token| token.location.end);
        self.loop_function_depths.push(self.function_depth);
        let body = self.parse_block_until(body_start, &[TokenKind::ReservedEnd]);
        self.loop_function_depths.pop();
        let mut end = self.consume_end_or_report();
        if let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }
        self.locals.truncate(saved_local_count);

        Stat::For {
            location: Some(Location::new(start, end)),
            var,
            from: Box::new(from),
            to: Box::new(to),
            step,
            body: Box::new(body),
            has_do: do_token.is_some(),
        }
    }

    /// Parses a generic `for` loop after the first loop variable.
    pub(crate) fn parse_generic_for(
        &mut self,
        start: Position,
        saved_local_count: usize,
        first: Local,
    ) -> Stat {
        let mut vars = vec![first];
        while self.consume_char(',').is_some() {
            vars.push(self.parse_loop_local());
        }

        let in_token = self.expect_token(TokenKind::ReservedIn, "'in'");
        let mut values = vec![self.parse_expression()];
        while self.consume_char(',').is_some() {
            values.push(self.parse_expression());
        }

        self.locals.extend(vars.iter().map(Local::to_local_ref));
        let do_token = self.expect_token(TokenKind::ReservedDo, "'do'");
        let body_start = do_token
            .as_ref()
            .map_or(self.current.location.begin, |token| token.location.end);
        self.loop_function_depths.push(self.function_depth);
        let body = self.parse_block_until(body_start, &[TokenKind::ReservedEnd]);
        self.loop_function_depths.pop();
        let mut end = self.consume_end_or_report();
        if let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }
        self.locals.truncate(saved_local_count);

        Stat::ForIn {
            location: Some(Location::new(start, end)),
            vars,
            values,
            body: Box::new(body),
            has_in: in_token.is_some(),
            has_do: do_token.is_some(),
        }
    }

    /// Parses one loop local.
    pub(crate) fn parse_loop_local(&mut self) -> Local {
        let token = self.current.clone();
        if token.kind == TokenKind::Name {
            self.advance();
            let annotation = self.parse_optional_type_annotation();
            self.fresh_local(
                Name::new(token_name(&token)),
                Some(token.location),
                annotation,
                false,
                self.function_depth,
            )
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected loop variable".to_owned(),
                location: token.location,
            });
            self.fresh_local(
                Name::new(""),
                Some(token.location),
                None,
                false,
                self.function_depth,
            )
        }
    }

    /// Parses an `if` or `elseif` body after consuming the introducer keyword.
    pub(crate) fn parse_if_after_keyword(
        &mut self,
        start: Position,
        consume_semicolon: bool,
    ) -> Stat {
        let condition = self.parse_expression();
        let then = self.expect_token(TokenKind::ReservedThen, "'then'");
        let then_start = then
            .as_ref()
            .map_or(self.current.location.begin, |token| token.location.end);
        let mut then_body = self.parse_block_until(
            then_start,
            &[
                TokenKind::ReservedElseif,
                TokenKind::ReservedElse,
                TokenKind::ReservedEnd,
            ],
        );

        let (else_body, else_is_elseif) = match self.current.kind {
            TokenKind::ReservedElseif => {
                let elseif_start = self.current.location.begin;
                self.advance();
                (
                    Some(Box::new(self.parse_if_after_keyword(elseif_start, false))),
                    true,
                )
            }
            TokenKind::ReservedElse => {
                let else_start = self.current.location.end;
                self.advance();
                (
                    Some(Box::new(
                        self.parse_block_until(else_start, &[TokenKind::ReservedEnd]),
                    )),
                    false,
                )
            }
            _ => (None, false),
        };

        let mut end = if else_is_elseif {
            else_body.as_ref().map_or_else(
                || self.current.location.begin,
                |else_body| stat_end(else_body),
            )
        } else if self.current.kind == TokenKind::ReservedEnd {
            let token = self.advance();
            token.location.end
        } else if self.peek_significant_kind() == TokenKind::ReservedEnd {
            let token = self.current.clone();
            if let Stat::Block { has_end, .. } = &mut then_body {
                *has_end = true;
            }
            let opener = then.as_ref().map_or(start, |token| token.location.begin);
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: format!(
                    "Expected 'end' (to close 'then' at {}), got {}",
                    opening_position_description(opener),
                    token.display()
                ),
                location: token.location,
            });
            self.advance();
            self.advance();
            token.location.end
        } else if let Some(else_body) = &else_body {
            stat_end(else_body)
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected 'end'".to_owned(),
                location: self.current.location,
            });
            self.current.location.begin
        };
        if consume_semicolon && let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }

        Stat::If {
            location: Some(Location::new(start, end)),
            condition: Box::new(condition),
            then_body: Box::new(then_body),
            else_body,
            has_then: then.is_some(),
        }
    }

    /// Parses a local function declaration after `local`.
    pub(crate) fn parse_local_function(&mut self, start: Position) -> Stat {
        self.parse_local_function_with_attributes(start, Vec::new())
    }

    /// Parses a local function declaration with already-consumed attributes.
    pub(crate) fn parse_local_function_with_attributes(
        &mut self,
        start: Position,
        attributes: Vec<Attribute>,
    ) -> Stat {
        self.advance();
        let name_token = self.current.clone();
        let local = if name_token.kind == TokenKind::Name {
            self.advance();
            self.fresh_local(
                Name::new(token_name(&name_token)),
                Some(name_token.location),
                None,
                false,
                self.function_depth,
            )
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected local function name".to_owned(),
                location: name_token.location,
            });
            self.fresh_local(
                Name::new(""),
                Some(name_token.location),
                None,
                false,
                self.function_depth,
            )
        };
        self.finish_local_function_statement(start, local, attributes, false)
    }

    /// Finishes a local-function-shaped statement after the local is created.
    fn finish_local_function_statement(
        &mut self,
        start: Position,
        local: Local,
        attributes: Vec<Attribute>,
        exported: bool,
    ) -> Stat {
        self.locals.push(local.to_local_ref());
        let func_start = self.function_start_from_attributes(start, &attributes);
        let func =
            self.parse_function_tail(func_start, local.name.as_str().to_owned(), attributes, None);
        let func_location = expr_location(&func);
        let mut end = func_location.end;
        if let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }
        Stat::LocalFunction {
            location: Some(Location::new(start, end)),
            name: local,
            func: Box::new(func),
            exported,
        }
    }

    /// Returns the function-expression start corresponding to parsed attributes.
    fn function_start_from_attributes(
        &self,
        fallback: Position,
        attributes: &[Attribute],
    ) -> Position {
        attributes
            .first()
            .and_then(|attribute| attribute.location)
            .map_or(fallback, |location| location.begin)
    }

    /// Parses a return statement.
    pub(crate) fn parse_return(&mut self) -> Stat {
        let start = self.current.location.begin;
        self.advance();

        let mut list = Vec::new();
        if !matches!(
            self.current.kind,
            TokenKind::Eof
                | TokenKind::ReservedEnd
                | TokenKind::ReservedElse
                | TokenKind::ReservedElseif
                | TokenKind::ReservedUntil
                | TokenKind::Char(';')
        ) {
            list.push(self.parse_expression());
            while self.consume_char(',').is_some() {
                list.push(self.parse_expression());
            }
        }

        let mut end = list.last().map_or(
            Position::new(start.line, start.column + "return".len() as u32),
            expr_end,
        );
        if let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }
        let location = Location::new(start, end);
        if self.syntax_flags.luau_export_value_syntax && self.function_depth == 0 {
            if !self.declared_export_bindings.is_empty() {
                self.errors.push(Error {
                    kind: ErrorKind::MalformedSyntax,
                    message: EXPORT_RETURN_CONFLICT_MESSAGE.to_owned(),
                    location,
                });
            }
            self.has_module_return = true;
        }
        Stat::Return {
            location: Some(location),
            list,
        }
    }

    /// Applies parser-side checks shared by exported value declarations.
    pub(crate) fn validate_export_value_declaration(&mut self, location: Location) {
        if self.function_depth != 0 || self.block_depth != 0 {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: EXPORT_TOP_LEVEL_MESSAGE.to_owned(),
                location,
            });
        }
        if self.has_module_return {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: EXPORT_RETURN_CONFLICT_MESSAGE.to_owned(),
                location,
            });
        }
    }

    /// Records exported value locals and reports duplicate module exports.
    pub(crate) fn record_exported_value_bindings(&mut self, locals: &[Local]) {
        for local in locals {
            self.record_exported_value_binding(local);
        }
    }

    /// Records one exported value binding and reports duplicate module exports.
    pub(crate) fn record_exported_value_binding(&mut self, local: &Local) {
        let Some(location) = local.location else {
            return;
        };
        let name = local.name.as_str();
        if self.declared_export_bindings.contains_key(name) {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: format!("Duplicate exported identifier '{name}'"),
                location,
            });
        } else {
            self.declared_export_bindings
                .insert(name.to_owned(), location);
        }
    }

    /// Records one exported class binding and reports duplicate module exports.
    pub(crate) fn record_exported_class_binding(&mut self, local: &Local) {
        let Some(location) = local.location else {
            return;
        };
        let name = local.name.as_str();
        if self.declared_export_bindings.contains_key(name) {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: format!("Duplicate exported class '{name}'"),
                location,
            });
        } else {
            self.declared_export_bindings
                .insert(name.to_owned(), location);
        }
    }

    /// Parses an expression statement.
    pub(crate) fn parse_expr_statement(&mut self) -> Option<Stat> {
        if self.current.kind == TokenKind::Char('=') {
            let equals = self.current.clone();
            let message_index = self.error_index_at(equals.location).unwrap_or_else(|| {
                let message_index = self.errors.len();
                self.errors.push(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: expected_expression_message(&equals),
                    location: equals.location,
                });
                message_index
            });
            self.advance();
            let value = self.parse_expression();
            return Some(Stat::Assign {
                location: Some(Location::new(equals.location.begin, expr_end(&value))),
                vars: vec![self.wrapped_error_expr_at(equals.location, message_index)],
                values: vec![value],
            });
        }

        if let Some(op) = compound_assign_op(self.current.kind) {
            let op_token = self.current.clone();
            let message_index = self.errors.len();
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: expected_expression_message(&op_token),
                location: op_token.location,
            });
            let var = self.wrapped_error_expr_at(op_token.location, message_index);
            return Some(self.parse_compound_assignment(var, op));
        }

        if !matches!(
            self.current.kind,
            TokenKind::Name | TokenKind::Char('(') | TokenKind::BrokenComment
        ) {
            let token = self.current.clone();
            let message_index = self.error_index_at(token.location).unwrap_or_else(|| {
                let message_index = self.errors.len();
                self.errors.push(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: expected_expression_message(&token),
                    location: token.location,
                });
                message_index
            });
            self.advance();
            let expression = Expr::Error {
                syntax_id: self.fresh_syntax_id(),
                location: Some(token.location),
                expressions: Vec::new(),
                message_index: Some(message_index),
            };
            return Some(stat_error_from_expression(expression, token.location));
        }

        let expression = self.parse_expression_allowing_ambiguous_newline_call();
        let location = expr_location(&expression);
        if let Some(op) = compound_assign_op(self.current.kind) {
            if matches!(expression, Expr::Call { .. }) {
                return Some(Stat::Expr {
                    location: Some(location),
                    expr: Box::new(expression),
                });
            }
            return Some(self.parse_compound_assignment(expression, op));
        }
        if matches!(
            self.current.kind,
            TokenKind::Char('=') | TokenKind::Char(',')
        ) {
            return Some(self.parse_assignment(expression));
        }
        match expression {
            Expr::Call { .. } => {
                let mut end = location.end;
                if let Some(semicolon) = self.consume_char(';') {
                    end = semicolon.location.end;
                }
                Some(Stat::Expr {
                    location: Some(Location::new(location.begin, end)),
                    expr: Box::new(expression),
                })
            }
            _ if expression_identifier_name(&expression).as_deref() == Some("continue") => {
                let mut end = location.end;
                if let Some(semicolon) = self.consume_char(';') {
                    end = semicolon.location.end;
                }
                Some(Stat::Continue {
                    location: Some(Location::new(location.begin, end)),
                })
            }
            Expr::IndexName {
                op: IndexOp::Colon, ..
            } => {
                let error_end = if self.current.location.begin.line > location.end.line {
                    location.end
                } else {
                    self.current.location.begin
                };
                let error_location = Location::new(location.begin, error_end);
                self.push_error_dedup(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: expected_call_arguments_message(&self.current),
                    location: error_location,
                });
                let message_index = self.errors.len() - 1;
                Some(Stat::Error {
                    location: Some(error_location),
                    expressions: vec![Expr::Error {
                        syntax_id: self.fresh_syntax_id(),
                        location: Some(error_location),
                        expressions: vec![expression],
                        message_index: Some(message_index),
                    }],
                    statements: Vec::new(),
                })
            }
            Expr::Error { .. } => Some(stat_error_from_expression(expression, location)),
            _ => {
                self.push_error_dedup(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: "Incomplete statement: expected assignment or a function call"
                        .to_owned(),
                    location,
                });
                Some(stat_error_from_expression(expression, location))
            }
        }
    }

    /// Parses an assignment statement after the first lvalue expression.
    pub(crate) fn parse_assignment(&mut self, first_var: Expr) -> Stat {
        let start = expr_location(&first_var).begin;
        let mut vars = vec![self.validate_assignment_target(first_var)];
        while self.consume_char(',').is_some() {
            let var = self.parse_expression_allowing_ambiguous_newline_call();
            vars.push(self.validate_assignment_target(var));
        }
        if let Some(op) = compound_assign_op(self.current.kind) {
            let op_token = self.current.clone();
            let message_index = self.errors.len();
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: format!(
                    "Expected '=' when parsing assignment, got {}",
                    op_token.display()
                ),
                location: op_token.location,
            });
            let value_error = self.error_expr_at(op_token.location, message_index);
            let compound_var = self.wrapped_error_expr_at(op_token.location, message_index);
            self.advance();
            let compound_value = self.parse_expression();
            let compound_end = expr_end(&compound_value);
            self.pending_statements.push_back(Stat::CompoundAssign {
                location: Some(Location::new(op_token.location.begin, compound_end)),
                op,
                var: Box::new(compound_var),
                value: Box::new(compound_value),
            });
            return Stat::Assign {
                location: Some(Location::new(start, op_token.location.end)),
                vars,
                values: vec![value_error],
            };
        }
        self.expect_char('=');

        let mut values = vec![self.parse_expression()];
        while self.consume_char(',').is_some() {
            values.push(self.parse_expression());
        }

        let mut end = values.last().map_or(start, expr_end);
        if let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }
        Stat::Assign {
            location: Some(Location::new(start, end)),
            vars,
            values,
        }
    }

    /// Parses a compound assignment statement after the lvalue expression.
    pub(crate) fn parse_compound_assignment(&mut self, var: Expr, op: CompoundAssignOp) -> Stat {
        let start = expr_location(&var).begin;
        let var = self.validate_assignment_target(var);
        self.advance();
        let value = self.parse_expression();

        let mut end = expr_end(&value);
        if let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }
        Stat::CompoundAssign {
            location: Some(Location::new(start, end)),
            op,
            var: Box::new(var),
            value: Box::new(value),
        }
    }

    /// Wraps invalid assignment targets in upstream-style error expressions.
    pub(crate) fn validate_assignment_target(&mut self, expression: Expr) -> Expr {
        if expr_is_assignable(&expression) && !expr_is_const_local(&expression) {
            return expression;
        }

        let location = expr_location(&expression);
        self.errors.push(Error {
            kind: ErrorKind::MalformedSyntax,
            message: "Assigned expression must be a variable or a field".to_owned(),
            location,
        });
        let message_index = self.errors.len() - 1;
        Expr::Error {
            syntax_id: self.fresh_syntax_id(),
            location: Some(location),
            expressions: vec![expression],
            message_index: Some(message_index),
        }
    }
}
