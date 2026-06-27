//! Parser decl parsing.

use std::collections::BTreeSet;

use super::{
    Parser,
    common::{
        attribute_starts_arguments, class_has_json_visible_members, class_method_name_error,
        expected_identifier_message, expr_end, expr_location,
        last_class_member_has_missing_function_end, opening_position_description, token_name,
        type_location, type_pack_location,
    },
};
use crate::{
    Location, Position,
    lexer::TokenKind,
    parse::{Error, ErrorKind},
    syntax::{
        ArgumentName, Attribute, DeclaredClassProp, Expr, Name, Stat, TableIndexer, Type, TypeList,
        TypePack,
    },
};

/// Validation facts from a declared class method parameter list.
#[derive(Default)]
pub struct DeclareMethodParamStatus {
    /// Whether the first parameter was an unannotated `self`.
    has_unannotated_self: bool,
    /// Whether a non-self parameter lacked an annotation.
    has_unannotated_non_self: bool,
    /// Whether a vararg parameter lacked an annotation.
    has_unannotated_vararg: bool,
}

impl<'source> Parser<'source> {
    /// Parses `export type ...`.
    pub(crate) fn parse_export_type_alias(&mut self) -> Stat {
        let start = self.current.location.begin;
        self.advance();
        self.parse_type_alias_with_start(true, start)
    }

    /// Parses `export class ...`.
    pub(crate) fn parse_export_class(&mut self) -> Stat {
        self.advance();
        self.parse_class(true)
    }

    /// Parses user-defined class syntax, emitting only JSON-visible member nodes.
    pub(crate) fn parse_class(&mut self, exported: bool) -> Stat {
        let start = self.current.location.begin;
        let saved_local_count = self.locals.len();
        self.advance();

        let name_token = self.current.clone();
        let class_name = if name_token.kind == TokenKind::Name {
            self.advance();
            let class_name = token_name(&name_token);
            let local = self.fresh_local(
                Name::new(class_name.clone()),
                Some(name_token.location),
                None,
                true,
                self.syntax_flags.luau_const2,
                self.function_depth,
            );
            self.locals.push(local.as_ref());
            if self.function_depth > 0 || self.block_depth > 0 {
                self.errors.push(Error {
                    kind: ErrorKind::MalformedSyntax,
                    message: format!(
                        "Cannot declare class '{class_name}' inside another statement or expression"
                    ),
                    location: name_token.location,
                });
            }
            Some((class_name, local))
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "Expected class name".to_owned(),
                location: name_token.location,
            });
            None
        };

        let mut members = Vec::new();
        let mut member_names = BTreeSet::new();
        while !matches!(self.current.kind, TokenKind::ReservedEnd | TokenKind::Eof) {
            self.skip_comments();
            if matches!(self.current.kind, TokenKind::ReservedEnd | TokenKind::Eof) {
                break;
            }

            let qualifier_location = if self.current.kind == TokenKind::Name
                && self.current.name.as_deref() == Some("public")
            {
                let token = self.current.clone();
                self.advance();
                Some(token.location)
            } else {
                None
            };

            if qualifier_location.is_some() && self.current.kind != TokenKind::ReservedFunction {
                if self.current.kind == TokenKind::Name {
                    let name = self.current.clone();
                    self.advance();
                    let luau_type = if self.consume_char(':').is_some() {
                        Some(self.parse_type_expression())
                    } else {
                        None
                    };
                    let member_name = token_name(&name);
                    if member_name.starts_with("__") {
                        self.errors.push(Error {
                            kind: ErrorKind::MalformedSyntax,
                            message: "Class properties cannot start with '__'".to_owned(),
                            location: name.location,
                        });
                    }
                    if self.record_class_member_name(&mut member_names, &member_name, name.location)
                    {
                        let end = luau_type
                            .as_ref()
                            .map_or(name.location.end, |luau_type| type_location(luau_type).end);
                        let member = Stat::ClassProperty {
                            location: Some(Location::new(name.location.begin, end)),
                            name: Name::new(member_name),
                            name_location: Some(name.location),
                            luau_type: luau_type.map(Box::new),
                            exported,
                        };
                        members.push(member);
                    }
                } else {
                    self.errors.push(Error {
                        kind: ErrorKind::MalformedSyntax,
                        message:
                            "Only class properties and functions can be declared within a class"
                                .to_owned(),
                        location: self.current.location,
                    });
                    self.advance();
                }
                continue;
            }

            match self.current.kind {
                TokenKind::ReservedFunction => {
                    let member = self.parse_class_function(exported);
                    if self.record_class_member(&mut member_names, &member) {
                        members.push(member);
                    }
                }
                TokenKind::Name => {
                    let name = self.current.clone();
                    self.advance();
                    if self.consume_char(':').is_some() {
                        let luau_type = self.parse_type_expression();
                        if token_name(&name).starts_with("__") {
                            self.errors.push(Error {
                                kind: ErrorKind::MalformedSyntax,
                                message: "Class properties cannot start with '__'".to_owned(),
                                location: name.location,
                            });
                        }
                        let member = Stat::ClassProperty {
                            location: Some(Location::new(
                                name.location.begin,
                                type_location(&luau_type).end,
                            )),
                            name: Name::new(token_name(&name)),
                            name_location: Some(name.location),
                            luau_type: Some(Box::new(luau_type)),
                            exported,
                        };
                        if self.record_class_member(&mut member_names, &member) {
                            members.push(member);
                        }
                    } else if name.name.as_deref() == Some("function") {
                        // Defensive fallback for contextual `function`, should not fire with
                        // the current lexer, but keeps class recovery moving if it does.
                        let member = self.parse_class_function(exported);
                        if self.record_class_member(&mut member_names, &member) {
                            members.push(member);
                        }
                    } else {
                        self.errors.push(Error {
                            kind: ErrorKind::MalformedSyntax,
                            message:
                                "Only class properties and functions can be declared within a class"
                                    .to_owned(),
                            location: name.location,
                        });
                    }
                }
                _ => {
                    self.errors.push(Error {
                        kind: ErrorKind::MalformedSyntax,
                        message:
                            "Only class properties and functions can be declared within a class"
                                .to_owned(),
                        location: self.current.location,
                    });
                    self.advance();
                }
            }
        }

        let end = if self.current.kind == TokenKind::ReservedEnd {
            let token = self.current.clone();
            self.advance();
            token.location.begin
        } else if self.current.kind == TokenKind::Eof
            && last_class_member_has_missing_function_end(&members)
        {
            self.current.location.begin
        } else {
            self.consume_class_end_or_report()
        };
        let emit_placeholder =
            self.function_depth == 0 && self.block_depth == 0 && members.is_empty();
        let class = Stat::Class {
            location: Some(Location::new(start, end)),
            class_local: class_name.as_ref().map(|(_, local)| local.clone()),
            members,
            emit_placeholder,
            exported,
        };

        if let Some((class_name, _)) = class_name {
            if self.function_depth > 0 || self.block_depth > 0 {
                if class_has_json_visible_members(&class) {
                    return class;
                }
                self.locals.truncate(saved_local_count);
                return Stat::Error {
                    location: Some(name_token.location),
                    expressions: Vec::new(),
                    statements: Vec::new(),
                };
            }

            if !self.class_names.insert(class_name.clone()) {
                self.locals.truncate(saved_local_count);
                self.errors.push(Error {
                    kind: ErrorKind::MalformedSyntax,
                    message: format!(
                        "A class named '{class_name}' has already been declared in this module"
                    ),
                    location: name_token.location,
                });
                return Stat::Error {
                    location: Some(name_token.location),
                    expressions: Vec::new(),
                    statements: Vec::new(),
                };
            }
        }

        class
    }

    /// Records a class member name and reports duplicates.
    pub(crate) fn record_class_member(
        &mut self,
        member_names: &mut BTreeSet<String>,
        member: &Stat,
    ) -> bool {
        match member {
            Stat::TypeFunction {
                name,
                name_location,
                ..
            }
            | Stat::ClassProperty {
                name,
                name_location,
                ..
            } => self.record_class_member_name(
                member_names,
                name.as_str(),
                name_location.unwrap_or_default(),
            ),
            _ => true,
        }
    }

    /// Records a class member name and reports duplicates.
    pub(crate) fn record_class_member_name(
        &mut self,
        member_names: &mut BTreeSet<String>,
        name: &str,
        location: Location,
    ) -> bool {
        if member_names.insert(name.to_owned()) {
            return true;
        }

        let location = if name == "%error-id%" {
            Location::new(location.begin, location.begin)
        } else {
            location
        };
        self.errors.push(Error {
            kind: ErrorKind::MalformedSyntax,
            message: format!("Duplicate class member '{name}'"),
            location,
        });
        false
    }

    /// Parses a user-defined class function member.
    pub(crate) fn parse_class_function(&mut self, exported: bool) -> Stat {
        let function_token = self.current.clone();
        self.advance();

        let name_token = self.current.clone();
        let name = if name_token.kind == TokenKind::Name {
            self.advance();
            Name::new(token_name(&name_token))
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: expected_identifier_message(&name_token, Some("method name")),
                location: name_token.location,
            });
            Name::new("%error-id%")
        };

        let func = self.parse_function_tail(
            function_token.location.begin,
            name.as_str().to_owned(),
            Vec::new(),
            None,
        );
        if let Some(message) = class_method_name_error(name.as_str()) {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message,
                location: name_token.location,
            });
        }
        if let Expr::Function { args, .. } = &func
            && let Some(first) = args.first()
            && first.name.as_str() == "self"
            && let Some(annotation) = &first.luau_type
        {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "The 'self' parameter cannot have a type annotation".to_owned(),
                location: type_location(annotation),
            });
        }
        Stat::TypeFunction {
            location: Some(Location::new(
                function_token.location.begin,
                expr_end(&func),
            )),
            name,
            name_location: Some(name_token.location),
            func: Box::new(func),
            exported,
        }
    }

    /// Parses a type alias declaration.
    pub(crate) fn parse_type_alias(&mut self, exported: bool) -> Stat {
        let start = self.current.location.begin;
        self.parse_type_alias_with_start(exported, start)
    }

    /// Parses a type alias declaration with an explicit statement start.
    pub(crate) fn parse_type_alias_with_start(&mut self, exported: bool, start: Position) -> Stat {
        self.advance();
        if self.current.kind == TokenKind::ReservedFunction {
            return self.parse_type_function(start, exported);
        }

        let name_token = self.current.clone();
        let name = if name_token.kind == TokenKind::Name {
            self.advance();
            Name::new(token_name(&name_token))
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: expected_identifier_message(&name_token, Some("type name")),
                location: name_token.location,
            });
            if name_token.kind != TokenKind::Eof {
                self.advance();
            }
            Name::new("%error-id%")
        };

        let error_base = self.errors.len();
        let (generics, generic_packs) = if self.current.kind == TokenKind::Char('<') {
            self.parse_generic_parameters()
        } else {
            (Vec::new(), Vec::new())
        };

        let value = if self.current.kind == TokenKind::Char('=') {
            self.advance();
            self.parse_type_expression()
        } else {
            let message_index = if error_base < self.errors.len() {
                error_base
            } else {
                let message_index = self.errors.len();
                self.errors.push(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: format!(
                        "Expected '=' when parsing type alias, got {}",
                        self.current.display
                    ),
                    location: self.current.location,
                });
                message_index
            };
            self.type_error_at_message(self.current.location, message_index)
        };
        let mut end = type_location(&value).end;
        if let Some(semicolon) = self.consume_char(';') {
            end = semicolon.location.end;
        }

        Stat::TypeAlias {
            location: Some(Location::new(start, end)),
            name,
            generics,
            generic_packs,
            value: Box::new(value),
            exported,
        }
    }

    /// Parses a user-defined type function.
    pub(crate) fn parse_type_function(&mut self, start: Position, exported: bool) -> Stat {
        let function_token = self.current.clone();
        self.advance();

        let name_token = self.current.clone();
        let name = if name_token.kind == TokenKind::Name {
            self.advance();
            Name::new(token_name(&name_token))
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected type function name".to_owned(),
                location: name_token.location,
            });
            Name::new("")
        };

        let old_type_function_depth = self.type_function_depth;
        self.type_function_depth = self.function_depth + 1;
        let func = self.parse_function_tail(
            function_token.location.begin,
            name.as_str().to_owned(),
            Vec::new(),
            None,
        );
        self.type_function_depth = old_type_function_depth;
        let location = Some(Location::new(start, expr_location(&func).end));
        Stat::TypeFunction {
            location,
            name,
            name_location: Some(name_token.location),
            func: Box::new(func),
            exported,
        }
    }

    /// Parses a declaration statement after `declare`.
    pub(crate) fn parse_declaration(&mut self) -> Stat {
        let start = self.current.location.begin;
        self.advance();

        if self.current.kind == TokenKind::ReservedFunction {
            return self.parse_declare_function(start, Vec::new());
        }

        if self.current.kind == TokenKind::Name && self.current.name.as_deref() == Some("class") {
            if self
                .syntax_flags
                .luau_allow_global_declaration_to_be_called_class
                && self.peek_significant_kind() == TokenKind::Char(':')
            {
                return self.parse_declare_global(start);
            }

            self.advance();
            return self.parse_declare_class(false);
        }

        if self.current.kind == TokenKind::Name && self.current.name.as_deref() == Some("extern") {
            self.advance();
            if self.current.kind == TokenKind::Name && self.current.name.as_deref() == Some("type")
            {
                self.advance();
                return self.parse_declare_class(true);
            }

            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected 'type' after declare extern".to_owned(),
                location: self.current.location,
            });
        }

        self.parse_declare_global(start)
    }

    /// Parses `declare name: Type`.
    pub(crate) fn parse_declare_global(&mut self, start: Position) -> Stat {
        let name_token = self.current.clone();
        let name = if name_token.kind == TokenKind::Name {
            self.advance();
            Name::new(token_name(&name_token))
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected global declaration name".to_owned(),
                location: name_token.location,
            });
            Name::new("")
        };

        let luau_type = if self.consume_char(':').is_some() {
            self.parse_type_expression()
        } else {
            let message_index = self.errors.len();
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: format!(
                    "Expected ':' when parsing global variable declaration, got {}",
                    self.current.display
                ),
                location: self.current.location,
            });
            self.type_error_at_message(self.current.location, message_index)
        };
        let end = type_location(&luau_type).end;

        Stat::DeclareGlobal {
            location: Some(Location::new(start, end)),
            name,
            name_location: Some(name_token.location),
            luau_type: Box::new(luau_type),
        }
    }

    /// Parses `declare function name(...): ...`.
    pub(crate) fn parse_declare_function(
        &mut self,
        start: Position,
        attributes: Vec<Attribute>,
    ) -> Stat {
        self.advance();

        let name_token = self.current.clone();
        let name = if name_token.kind == TokenKind::Name {
            self.advance();
            Name::new(token_name(&name_token))
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected global function name".to_owned(),
                location: name_token.location,
            });
            Name::new("")
        };

        let (generics, generic_packs) = if self.current.kind == TokenKind::Char('<') {
            self.parse_generic_parameters()
        } else {
            (Vec::new(), Vec::new())
        };

        self.expect_char('(');
        let (params, param_names, vararg, vararg_location, has_unannotated_param) =
            self.parse_declare_function_params();
        self.expect_char(')');

        if has_unannotated_param {
            let end = self.current.location.begin;
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "all declaration parameters must be annotated".to_owned(),
                location: Location::new(start, end),
            });
            return Stat::Error {
                location: Some(Location::new(start, end)),
                expressions: Vec::new(),
                statements: Vec::new(),
            };
        }

        let ret_types = if self.consume_char(':').is_some() {
            self.parse_return_type_pack()
        } else {
            TypePack::Explicit {
                location: Some(self.current.location),
                type_list: TypeList::new(Vec::new()),
            }
        };
        let end = self.current.location.end;

        Stat::DeclareFunction {
            location: Some(Location::new(start, end)),
            attributes,
            name,
            name_location: Some(name_token.location),
            generics,
            generic_packs,
            params,
            param_names,
            vararg,
            vararg_location,
            ret_types: Box::new(ret_types),
        }
    }

    /// Parses a statement preceded by one or more attributes.
    pub(crate) fn parse_attribute_statement(&mut self) -> Stat {
        let attributes = self.parse_attributes();
        let start = self.current.location.begin;
        let attribute_start = attributes
            .first()
            .and_then(|attribute| attribute.location)
            .map_or(start, |location| location.begin);
        if self.current.kind == TokenKind::ReservedFunction {
            return self.parse_function_statement_with_attributes(attribute_start, attributes);
        }
        if self.current.kind == TokenKind::ReservedLocal {
            self.advance();
            if self.current.kind == TokenKind::ReservedFunction {
                return self.parse_local_function_with_attributes(attribute_start, attributes);
            }
            self.push_error_dedup(Error {
                kind: ErrorKind::ExpectedToken,
                message: format!(
                    "Expected 'function' after local declaration with attribute, but got {} instead",
                    self.current.display
                ),
                location: self.current.location,
            });
            return Stat::Error {
                location: Some(self.current.location),
                expressions: Vec::new(),
                statements: Vec::new(),
            };
        }
        if self.syntax_flags.luau_const2
            && self.current.kind == TokenKind::Name
            && self.current.name.as_deref() == Some("const")
            && self.peek_significant_kind() == TokenKind::ReservedFunction
        {
            return self.parse_const_function(attribute_start, attributes);
        }
        if self.current.kind == TokenKind::Name && self.current.name.as_deref() == Some("declare") {
            self.advance();
            if self.current.kind == TokenKind::ReservedFunction {
                return self.parse_declare_function(start, attributes);
            }
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: format!(
                    "Expected a function type declaration after attribute, but got {} instead",
                    self.current.display
                ),
                location: self.current.location,
            });
            return Stat::Error {
                location: Some(self.current.location),
                expressions: Vec::new(),
                statements: Vec::new(),
            };
        }

        if self.current.kind == TokenKind::Eof
            && attributes
                .iter()
                .any(|attribute| attribute.name.as_str() == "%error-id%")
        {
            return Stat::Error {
                location: Some(self.current.location),
                expressions: Vec::new(),
                statements: Vec::new(),
            };
        }

        self.push_error_dedup(Error {
            kind: ErrorKind::ExpectedToken,
            message: format!(
                "Expected 'function', 'local function', 'declare function' or a function type declaration after attribute, but got {} instead",
                self.current.display
            ),
            location: self.current.location,
        });
        Stat::Error {
            location: Some(self.current.location),
            expressions: Vec::new(),
            statements: Vec::new(),
        }
    }

    /// Parses simple attribute tokens.
    pub(crate) fn parse_attributes(&mut self) -> Vec<Attribute> {
        let mut attributes = Vec::new();
        while matches!(
            self.current.kind,
            TokenKind::Attribute | TokenKind::AttributeOpen
        ) {
            if self.current.kind == TokenKind::Attribute {
                let token = self.current.clone();
                self.advance();
                let name = token.name.clone().unwrap_or_else(|| token_name(&token));
                if name.is_empty() {
                    self.errors.push(Error {
                        kind: ErrorKind::MalformedSyntax,
                        message: "attribute name is missing".to_owned(),
                        location: token.location,
                    });
                } else if !self.is_supported_attribute(&name) {
                    self.errors.push(Error {
                        kind: ErrorKind::MalformedSyntax,
                        message: format!("invalid attribute '@{name}'"),
                        location: token.location,
                    });
                }
                if attributes
                    .iter()
                    .any(|attribute: &Attribute| attribute.name.as_str() == name)
                {
                    self.errors.push(Error {
                        kind: ErrorKind::MalformedSyntax,
                        message: format!("cannot duplicate attribute '@{name}'"),
                        location: token.location,
                    });
                }
                attributes.push(Attribute {
                    name: Name::new(name),
                    location: Some(token.location),
                });
            } else {
                attributes.push(self.parse_parameterized_attribute());
            }
        }
        attributes
    }

    /// Returns whether an attribute is accepted by the current syntax flags.
    pub(crate) fn is_supported_attribute(&self, name: &str) -> bool {
        matches!(name, "checked" | "native" | "deprecated")
            || (self.syntax_flags.debug_luau_no_inline && name == "debugnoinline")
    }

    /// Parses a parameterized attribute as a name plus skipped argument payload.
    pub(crate) fn parse_parameterized_attribute(&mut self) -> Attribute {
        let open = self.current.clone();
        self.advance();

        if self.current.kind == TokenKind::Char(']') {
            let close = self.current.clone();
            let location = Location::new(open.location.begin, close.location.end);
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "attribute list cannot be empty".to_owned(),
                location,
            });
            self.advance();
            return Attribute {
                name: Name::new("%error-id%"),
                location: Some(location),
            };
        }

        let name_token = self.current.clone();
        let (name, start, end, missing_name) = if name_token.kind == TokenKind::Name {
            self.advance();
            (
                token_name(&name_token),
                name_token.location.begin,
                name_token.location.end,
                false,
            )
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: format!(
                    "Expected identifier when parsing attribute name, got {}",
                    name_token.display
                ),
                location: name_token.location,
            });
            (
                String::new(),
                name_token.location.begin,
                name_token.location.begin,
                true,
            )
        };

        let name = if missing_name {
            if self.current.kind != TokenKind::Eof {
                self.errors.push(Error {
                    kind: ErrorKind::MalformedSyntax,
                    message: "invalid attribute '@%error-id%'".to_owned(),
                    location: Location::new(name_token.location.begin, name_token.location.begin),
                });
            }
            "%error-id%".to_owned()
        } else {
            name
        };

        let mut end = end;
        if !missing_name && attribute_starts_arguments(&self.current) {
            self.validate_and_skip_attribute_arguments(&name, Location::new(start, end));
        }

        if self.current.kind == TokenKind::Char(']') {
            let close = self.current.clone();
            end = close.location.begin;
            self.advance();
        } else if self.current.kind != TokenKind::Eof {
            let opener = if self.current.location.begin.line > open.location.begin.line {
                format!("line {}", open.location.begin.line + 1)
            } else {
                opening_position_description(open.location.begin)
            };
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: format!(
                    "Expected ']' (to close '@[' at {}), got {}",
                    opener, self.current.display
                ),
                location: self.current.location,
            });
        }

        Attribute {
            name: Name::new(name),
            location: Some(Location::new(start, end)),
        }
    }

    /// Validates parameterized attribute arguments and skips to the closing `]`.
    pub(crate) fn validate_and_skip_attribute_arguments(
        &mut self,
        name: &str,
        name_location: Location,
    ) {
        match self.current.kind {
            TokenKind::Char('{') => self.validate_and_skip_attribute_table(name),
            TokenKind::Char('(') => self.validate_and_skip_attribute_call(name, name_location),
            TokenKind::QuotedString | TokenKind::RawString => {
                if name == "deprecated" {
                    self.errors.push(Error {
                        kind: ErrorKind::MalformedSyntax,
                        message: "unknown argument type for @deprecated".to_owned(),
                        location: self.current.location,
                    });
                }
                self.advance();
            }
            _ => {
                while !matches!(self.current.kind, TokenKind::Char(']') | TokenKind::Eof) {
                    self.advance();
                }
            }
        }

        while !matches!(self.current.kind, TokenKind::Char(']') | TokenKind::Eof) {
            self.advance();
        }
    }

    /// Validates a table argument for `@deprecated`.
    pub(crate) fn validate_and_skip_attribute_table(&mut self, name: &str) {
        let start = self.current.location.end;
        self.advance();
        let mut non_literal_table = false;
        let mut table_errors = Vec::new();
        let mut end = self.current.location.begin;

        while !matches!(
            self.current.kind,
            TokenKind::Char('}') | TokenKind::Char(']') | TokenKind::Eof
        ) {
            if self.current.kind == TokenKind::Char(',') {
                self.advance();
                continue;
            }

            let key = self.current.clone();
            if key.kind == TokenKind::Name && self.peek_significant_kind() == TokenKind::Char('=') {
                self.advance();
                self.expect_char('=');
                let value = self.current.clone();
                if name == "deprecated" {
                    let key_name = token_name(&key);
                    if key_name != "use" && key_name != "reason" {
                        table_errors.push(Error {
                            kind: ErrorKind::MalformedSyntax,
                            message: format!(
                                "Unknown argument '{key_name}' for @deprecated. Only string constants for 'use' and 'reason' are allowed"
                            ),
                            location: key.location,
                        });
                    } else if !matches!(value.kind, TokenKind::QuotedString | TokenKind::RawString)
                    {
                        table_errors.push(Error {
                            kind: ErrorKind::MalformedSyntax,
                            message: format!(
                                "only constant string allowed as value for '{key_name}'"
                            ),
                            location: value.location,
                        });
                    }
                }
                if !matches!(
                    value.kind,
                    TokenKind::QuotedString
                        | TokenKind::RawString
                        | TokenKind::Number
                        | TokenKind::ReservedTrue
                        | TokenKind::ReservedFalse
                        | TokenKind::ReservedNil
                ) {
                    non_literal_table = true;
                }
                if value.kind != TokenKind::Eof {
                    end = value.location.end;
                    self.advance();
                }
            } else {
                non_literal_table = true;
                end = self.current.location.end;
                self.advance();
            }
        }

        if self.current.kind == TokenKind::Char('}') {
            end = self.current.location.end;
            self.advance();
        }

        if non_literal_table {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "only literals can be passed as arguments for attributes".to_owned(),
                location: Location::new(start, end),
            });
        }
        self.errors.extend(table_errors);
    }

    /// Validates a parenthesized argument list for `@deprecated`.
    pub(crate) fn validate_and_skip_attribute_call(&mut self, name: &str, name_location: Location) {
        self.advance();
        let mut depth = 0_u32;
        let mut arg_count = 0_usize;
        let mut first_arg_location = None;
        let mut first_arg_is_table = false;

        while self.current.kind != TokenKind::Eof {
            match self.current.kind {
                TokenKind::Char(')') if depth == 0 => {
                    self.advance();
                    break;
                }
                TokenKind::Char(',') if depth == 0 => {
                    arg_count += 1;
                    self.advance();
                }
                TokenKind::Char('{') => {
                    if arg_count == 0 && first_arg_location.is_none() {
                        first_arg_location = Some(self.current.location);
                        first_arg_is_table = true;
                    }
                    depth += 1;
                    self.advance();
                }
                TokenKind::Char('(') | TokenKind::Char('[') => {
                    if arg_count == 0 && first_arg_location.is_none() {
                        first_arg_location = Some(self.current.location);
                    }
                    depth += 1;
                    self.advance();
                }
                TokenKind::Char('}') | TokenKind::Char(']') if depth > 0 => {
                    depth -= 1;
                    self.advance();
                }
                _ => {
                    if arg_count == 0 && first_arg_location.is_none() {
                        first_arg_location = Some(self.current.location);
                    }
                    self.advance();
                }
            }
        }

        if first_arg_location.is_some() {
            arg_count += 1;
        }
        if name == "deprecated" {
            if arg_count > 1 {
                self.errors.push(Error {
                    kind: ErrorKind::MalformedSyntax,
                    message: "@deprecated can be parametrized only by 1 argument".to_owned(),
                    location: name_location,
                });
            } else if !first_arg_is_table && let Some(location) = first_arg_location {
                self.errors.push(Error {
                    kind: ErrorKind::MalformedSyntax,
                    message: "unknown argument type for @deprecated".to_owned(),
                    location,
                });
            }
        }
    }

    /// Parses declaration function parameters.
    pub(crate) fn parse_declare_function_params(
        &mut self,
    ) -> (TypeList, Vec<ArgumentName>, bool, Option<Location>, bool) {
        let mut params = TypeList::new(Vec::new());
        let mut param_names = Vec::new();
        let mut vararg = false;
        let mut vararg_location = None;
        let mut has_unannotated_param = false;

        if self.current.kind == TokenKind::Char(')') {
            return (
                params,
                param_names,
                vararg,
                vararg_location,
                has_unannotated_param,
            );
        }

        loop {
            match self.current.kind {
                TokenKind::Name => {
                    let token = self.current.clone();
                    self.advance();
                    param_names.push(ArgumentName {
                        name: Name::new(token_name(&token)),
                        location: Some(token.location),
                    });
                    if self.consume_char(':').is_some() {
                        params.types.push(self.parse_type_expression());
                    } else {
                        has_unannotated_param = true;
                    }
                }
                TokenKind::Dot3 => {
                    vararg = true;
                    vararg_location = Some(self.current.location);
                    self.advance();
                    if self.consume_char(':').is_some() {
                        params.tail_type = Some(Box::new(self.parse_vararg_annotation()));
                    } else {
                        has_unannotated_param = true;
                    }
                    break;
                }
                _ => {
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: "expected declaration parameter".to_owned(),
                        location: self.current.location,
                    });
                    break;
                }
            }

            if self.consume_char(',').is_none() || self.current.kind == TokenKind::Char(')') {
                break;
            }
        }

        (
            params,
            param_names,
            vararg,
            vararg_location,
            has_unannotated_param,
        )
    }

    /// Parses `declare class Name ... end` or `declare extern type Name ... end`.
    pub(crate) fn parse_declare_class(&mut self, is_extern: bool) -> Stat {
        let name_token = self.current.clone();
        let start = name_token.location.begin;
        let name = if name_token.kind == TokenKind::Name {
            self.advance();
            Name::new(token_name(&name_token))
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected class name".to_owned(),
                location: name_token.location,
            });
            Name::new("")
        };

        let super_name = if self.current.kind == TokenKind::Name
            && self.current.name.as_deref() == Some("extends")
        {
            self.advance();
            let token = self.current.clone();
            if token.kind == TokenKind::Name {
                self.advance();
                Some(Name::new(token_name(&token)))
            } else {
                self.errors.push(Error {
                    kind: ErrorKind::ExpectedToken,
                    message: "expected superclass name".to_owned(),
                    location: token.location,
                });
                None
            }
        } else {
            None
        };

        let has_with =
            self.current.kind == TokenKind::Name && self.current.name.as_deref() == Some("with");
        if has_with {
            self.advance();
        } else if is_extern && !matches!(self.current.kind, TokenKind::Eof | TokenKind::ReservedEnd)
        {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: format!(
                    "Expected `with` keyword before listing properties of the external type, but got {} instead",
                    token_name(&self.current)
                ),
                location: self.current.location,
            });
        }

        let mut props = Vec::new();
        let mut indexer = None;
        let mut recovery_end = None;
        while !matches!(self.current.kind, TokenKind::Eof | TokenKind::ReservedEnd)
            || !self.pending_statements.is_empty()
        {
            self.skip_comments();
            if matches!(self.current.kind, TokenKind::Eof | TokenKind::ReservedEnd)
                && self.pending_statements.is_empty()
            {
                break;
            }

            match self.current.kind {
                TokenKind::ReservedFunction => props.push(self.parse_declare_class_method()),
                TokenKind::Char('[') => {
                    let parsed = self.parse_declare_class_indexer();
                    if indexer.is_some() {
                        self.errors.push(Error {
                            kind: ErrorKind::MalformedSyntax,
                            message: "cannot have more than one indexer on an extern type"
                                .to_owned(),
                            location: parsed.location.unwrap_or_default(),
                        });
                    } else {
                        indexer = Some(parsed);
                    }
                }
                TokenKind::Name => props.push(self.parse_declare_class_prop()),
                TokenKind::Char(',') => {
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: expected_identifier_message(&self.current, Some("property name")),
                        location: self.current.location,
                    });
                    recovery_end = Some(self.current.location.end);
                    self.advance();
                    break;
                }
                _ => {
                    self.errors.push(Error {
                        kind: ErrorKind::UnsupportedSyntax,
                        message: "unsupported class declaration member".to_owned(),
                        location: self.current.location,
                    });
                    self.advance();
                }
            }
        }

        let end = recovery_end.unwrap_or_else(|| self.consume_end_or_report());
        Stat::DeclareClass {
            location: Some(Location::new(start, end)),
            name,
            super_name,
            props,
            indexer,
        }
    }

    /// Parses a declared class property.
    pub(crate) fn parse_declare_class_prop(&mut self) -> DeclaredClassProp {
        let mut read_only = false;
        let mut write_only = false;
        if self.current.kind == TokenKind::Name
            && self.peek_significant_kind() != TokenKind::Char(':')
        {
            match self.current.name.as_deref() {
                Some("read") => {
                    read_only = true;
                    self.advance();
                }
                Some("write") => {
                    write_only = true;
                    self.advance();
                }
                Some(_) => {
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: format!(
                            "Expected blank or 'read' or 'write' attribute, got {}",
                            self.current.display
                        ),
                        location: self.current.location,
                    });
                    self.advance();
                }
                None => {}
            }
        }

        let name_token = self.current.clone();
        if name_token.kind == TokenKind::Name {
            self.advance();
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: expected_identifier_message(&name_token, Some("property name")),
                location: name_token.location,
            });
        }
        self.expect_char(':');
        let luau_type = self.parse_type_expression();
        let mut end = type_location(&luau_type).end;
        if let Some(recovery_end) = self.type_recovery_end.take()
            && recovery_end > end
        {
            end = recovery_end;
        }
        DeclaredClassProp {
            name: Name::new(token_name(&name_token)),
            name_location: Some(name_token.location),
            luau_type,
            is_method: false,
            read_only,
            write_only,
            location: Some(Location::new(name_token.location.begin, end)),
        }
    }

    /// Parses a declared class indexer.
    pub(crate) fn parse_declare_class_indexer(&mut self) -> TableIndexer {
        let open = self.current.clone();
        self.advance();
        let index_type = self.parse_type_expression();
        self.expect_char_to_close(']', "'['", open.location.begin);
        self.expect_char(':');
        let result_type = self.parse_type_expression();
        let end = type_location(&result_type).end;
        TableIndexer {
            location: Some(Location::new(open.location.begin, end)),
            index_type: Box::new(index_type),
            result_type: Box::new(result_type),
            read_only: false,
        }
    }

    /// Parses a declared class method property.
    pub(crate) fn parse_declare_class_method(&mut self) -> DeclaredClassProp {
        let function_token = self.current.clone();
        let function_start = function_token.location.begin;
        self.advance();

        let name_token = self.current.clone();
        let name = if name_token.kind == TokenKind::Name {
            self.advance();
            Name::new(token_name(&name_token))
        } else {
            self.errors.push(Error {
                kind: ErrorKind::ExpectedToken,
                message: "expected method name".to_owned(),
                location: name_token.location,
            });
            Name::new("")
        };

        let (generics, generic_packs) = if self.current.kind == TokenKind::Char('<') {
            self.parse_generic_parameters()
        } else {
            (Vec::new(), Vec::new())
        };

        self.expect_char('(');
        let (arg_types, arg_names, vararg_tail, param_status) = self.parse_declare_method_params();
        let close = self.expect_char(')');
        let close_end = close.map_or(self.current.location.begin, |token| token.location.end);
        let (return_types, function_end) = if self.consume_char(':').is_some() {
            let return_types = self.parse_return_type_pack();
            let end = type_pack_location(&return_types).end;
            (return_types, end)
        } else {
            (
                TypePack::Explicit {
                    location: Some(self.current.location),
                    type_list: TypeList::new(Vec::new()),
                },
                close_end,
            )
        };
        let method_location = Location::new(function_start, function_end);
        let mut args = arg_types;
        if !param_status.has_unannotated_self {
            let message_index = self.errors.len();
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "'self' must be present as the unannotated first parameter".to_owned(),
                location: method_location,
            });
            return DeclaredClassProp {
                name,
                name_location: Some(name_token.location),
                luau_type: self.type_error_at_message(method_location, message_index),
                is_method: true,
                read_only: false,
                write_only: false,
                location: Some(Location::default()),
            };
        } else if param_status.has_unannotated_non_self {
            let message_index = self.errors.len();
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "All declaration parameters aside from 'self' must be annotated"
                    .to_owned(),
                location: method_location,
            });
            args.types
                .push(self.type_error_at_message(method_location, message_index));
        } else if param_status.has_unannotated_vararg {
            self.errors.push(Error {
                kind: ErrorKind::MalformedSyntax,
                message: "All declaration parameters aside from 'self' must be annotated"
                    .to_owned(),
                location: function_token.location,
            });
        }

        args.tail_type = vararg_tail;
        let luau_type = Type::Function {
            syntax_id: self.fresh_syntax_id(),
            location: Some(method_location),
            attributes: Vec::new(),
            generics,
            generic_packs,
            arg_types: args,
            arg_names,
            return_types,
        };

        DeclaredClassProp {
            name,
            name_location: Some(name_token.location),
            luau_type,
            is_method: true,
            read_only: false,
            write_only: false,
            location: Some(method_location),
        }
    }

    /// Parses a declared class method parameter list, excluding `self`.
    pub(crate) fn parse_declare_method_params(
        &mut self,
    ) -> (
        TypeList,
        Vec<Option<ArgumentName>>,
        Option<Box<TypePack>>,
        DeclareMethodParamStatus,
    ) {
        let mut args = TypeList::new(Vec::new());
        let mut arg_names = Vec::new();
        let mut tail = None;
        let mut first = true;
        let mut status = DeclareMethodParamStatus::default();

        while self.current.kind != TokenKind::Eof && self.current.kind != TokenKind::Char(')') {
            match self.current.kind {
                TokenKind::Name => {
                    let token = self.current.clone();
                    self.advance();
                    let is_self = first && token.name.as_deref() == Some("self");
                    if let Some(annotation) = self.parse_optional_type_annotation() {
                        if !is_self {
                            args.types.push(*annotation);
                            arg_names.push(Some(ArgumentName {
                                name: Name::new(token_name(&token)),
                                location: Some(token.location),
                            }));
                        }
                    } else if is_self {
                        status.has_unannotated_self = true;
                    } else {
                        status.has_unannotated_non_self = true;
                        arg_names.push(Some(ArgumentName {
                            name: Name::new(token_name(&token)),
                            location: Some(token.location),
                        }));
                    }
                }
                TokenKind::Dot3 => {
                    self.advance();
                    if self.consume_char(':').is_some() {
                        tail = Some(Box::new(self.parse_vararg_annotation()));
                    } else {
                        status.has_unannotated_vararg = true;
                    }
                    break;
                }
                _ => {
                    self.errors.push(Error {
                        kind: ErrorKind::ExpectedToken,
                        message: "expected method parameter".to_owned(),
                        location: self.current.location,
                    });
                    break;
                }
            }

            first = false;
            if self.consume_char(',').is_none() {
                break;
            }
        }

        if args.types.is_empty() && !status.has_unannotated_non_self {
            arg_names.clear();
        }

        (args, arg_names, tail, status)
    }
}
