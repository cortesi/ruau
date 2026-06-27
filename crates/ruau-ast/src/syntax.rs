//! Rust-native Luau AST structures.
//!
//! These structures are the parser-facing model consumed by the analyzer
//! and typechecker. The JSON compatibility layer is the comparison
//! surface against upstream Luau's AST dumps.

use std::{collections::BTreeMap, iter};

use crate::{
    Location,
    json::{
        JsonBinaryOp, JsonCompoundAssignOp, JsonKind, JsonNode, JsonNumber, JsonTableItemKind,
        JsonUnaryOp, JsonValue, KnownJsonKind,
    },
};

/// A Luau identifier.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Name(String);

impl Name {
    /// Creates a name.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the name text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Parser-assigned identity for a local binding.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LocalId(u32);

impl LocalId {
    /// Creates a local id from its parser allocation index.
    #[must_use]
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// Returns the parser allocation index.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// Parser-assigned identity for an expression or type node.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SyntaxId(u32);

impl SyntaxId {
    /// Creates a syntax id from its parser allocation index.
    #[must_use]
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// Returns the parser allocation index.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// A local binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Local {
    /// Parser-assigned binding identity.
    pub id: LocalId,
    /// Binding name.
    pub name: Name,
    /// Binding source location.
    pub location: Option<Location>,
    /// Optional type annotation.
    pub luau_type: Option<Box<Type>>,
    /// Whether the binding is const.
    pub is_const: bool,
    /// Whether upstream's current JSON mode emits the `isConst` field.
    pub emit_is_const: bool,
    /// Function nesting depth used by parser recovery and type-function rules.
    pub function_depth: usize,
}

impl Local {
    /// Returns a reference snapshot for this local.
    #[must_use]
    pub fn as_ref(&self) -> LocalRef {
        LocalRef {
            id: self.id,
            name: self.name.clone(),
            location: self.location,
            luau_type: self.luau_type.clone(),
            is_const: self.is_const,
            emit_is_const: self.emit_is_const,
            function_depth: self.function_depth,
        }
    }

    /// Converts this local binding into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        local_json_node(
            self.name,
            self.location,
            self.luau_type,
            self.is_const,
            self.emit_is_const,
        )
    }
}

/// A reference to a local binding, including the JSON-visible local snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalRef {
    /// Referenced binding identity.
    pub id: LocalId,
    /// Referenced binding name.
    pub name: Name,
    /// Referenced binding source location.
    pub location: Option<Location>,
    /// Optional referenced binding annotation snapshot.
    pub luau_type: Option<Box<Type>>,
    /// Whether the referenced binding is const.
    pub is_const: bool,
    /// Whether upstream's current JSON mode emits the `isConst` field.
    pub emit_is_const: bool,
    /// Function nesting depth used by parser recovery and type-function rules.
    pub function_depth: usize,
}

impl LocalRef {
    /// Converts this local reference snapshot into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        local_json_node(
            self.name,
            self.location,
            self.luau_type,
            self.is_const,
            self.emit_is_const,
        )
    }
}

/// Converts local fields into upstream AST JSON.
fn local_json_node(
    name: Name,
    location: Option<Location>,
    luau_type: Option<Box<Type>>,
    is_const: bool,
    emit_is_const: bool,
) -> JsonNode {
    let luau_type = luau_type.map_or(JsonValue::Null, |luau_type| {
        JsonValue::Node(Box::new(luau_type.into_json()))
    });
    let mut fields = BTreeMap::from([
        ("luauType".to_owned(), luau_type),
        ("name".to_owned(), JsonValue::String(name.0)),
    ]);
    if emit_is_const {
        fields.insert("isConst".to_owned(), JsonValue::Bool(is_const));
    }
    JsonNode {
        kind: JsonKind::Known(KnownJsonKind::AstLocal),
        location,
        fields,
    }
}

/// A function attribute.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attribute {
    /// Attribute name.
    pub name: Name,
    /// Attribute source location.
    pub location: Option<Location>,
}

impl Attribute {
    /// Converts this attribute into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        json_node(
            KnownJsonKind::AstAttr,
            self.location,
            [("name", JsonValue::String(self.name.0))],
        )
    }
}

/// A generic type parameter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenericType {
    /// Parameter name.
    pub name: Name,
    /// Parameter source location.
    pub location: Option<Location>,
    /// Optional default type.
    pub luau_type: Option<Box<Type>>,
}

impl GenericType {
    /// Converts this generic type parameter into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        let mut fields = BTreeMap::from([("name".to_owned(), JsonValue::String(self.name.0))]);
        if let Some(luau_type) = self.luau_type {
            fields.insert(
                "luauType".to_owned(),
                JsonValue::Node(Box::new(luau_type.into_json())),
            );
        }
        JsonNode {
            kind: JsonKind::Known(KnownJsonKind::AstGenericType),
            // Upstream AST JSON does not encode locations on generic parameter nodes.
            location: None,
            fields,
        }
    }
}

/// A generic type-pack parameter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenericTypePack {
    /// Parameter name.
    pub name: Name,
    /// Parameter source location.
    pub location: Option<Location>,
    /// Optional default type pack.
    pub luau_type: Option<Box<TypePack>>,
}

impl GenericTypePack {
    /// Converts this generic type-pack parameter into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        let mut fields = BTreeMap::from([("name".to_owned(), JsonValue::String(self.name.0))]);
        if let Some(luau_type) = self.luau_type {
            fields.insert(
                "luauType".to_owned(),
                JsonValue::Node(Box::new(luau_type.into_json())),
            );
        }
        JsonNode {
            kind: JsonKind::Known(KnownJsonKind::AstGenericTypePack),
            // Upstream AST JSON does not encode locations on generic parameter nodes.
            location: None,
            fields,
        }
    }
}

/// A Rust-native type list with an optional tail type pack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeList {
    /// Type entries.
    pub types: Vec<Type>,
    /// Optional tail pack.
    pub tail_type: Option<Box<TypePack>>,
}

impl TypeList {
    /// Builds a type list without a tail pack.
    #[must_use]
    pub fn new(types: Vec<Type>) -> Self {
        Self {
            types,
            tail_type: None,
        }
    }

    /// Converts this type list into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        let mut fields = BTreeMap::from([("types".to_owned(), type_array(self.types))]);
        if let Some(tail_type) = self.tail_type {
            fields.insert(
                "tailType".to_owned(),
                JsonValue::Node(Box::new(tail_type.into_json())),
            );
        }
        JsonNode {
            kind: JsonKind::Known(KnownJsonKind::AstTypeList),
            location: None,
            fields,
        }
    }
}

/// A Rust-native function type argument name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArgumentName {
    /// Argument name.
    pub name: Name,
    /// Argument source location.
    pub location: Option<Location>,
}

impl ArgumentName {
    /// Converts this argument name into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        json_node(
            KnownJsonKind::AstArgumentName,
            self.location,
            [("name", JsonValue::String(self.name.0))],
        )
    }
}

/// A table type property.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableProp {
    /// Property name.
    pub name: Name,
    /// Property source location.
    pub location: Option<Location>,
    /// Property type.
    pub prop_type: Type,
    /// Whether callers may only read this property.
    pub read_only: bool,
    /// Whether callers may only write this property.
    pub write_only: bool,
}

impl TableProp {
    /// Converts this table property into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        json_node(
            KnownJsonKind::AstTableProp,
            self.location,
            [
                ("name", JsonValue::String(self.name.0)),
                (
                    "propType",
                    JsonValue::Node(Box::new(self.prop_type.into_json())),
                ),
            ],
        )
    }
}

/// A table type indexer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableIndexer {
    /// Indexer source location.
    pub location: Option<Location>,
    /// Index type.
    pub index_type: Box<Type>,
    /// Result type.
    pub result_type: Box<Type>,
    /// Whether callers may only read through this indexer.
    pub read_only: bool,
}

impl TableIndexer {
    /// Converts this table indexer into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        JsonValue::Object(BTreeMap::from([
            ("location".to_owned(), location_value(self.location)),
            (
                "indexType".to_owned(),
                JsonValue::Node(Box::new(self.index_type.into_json())),
            ),
            (
                "resultType".to_owned(),
                JsonValue::Node(Box::new(self.result_type.into_json())),
            ),
        ]))
    }
}

/// A declared class property.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclaredClassProp {
    /// Property name.
    pub name: Name,
    /// Property name source location.
    pub name_location: Option<Location>,
    /// Property type.
    pub luau_type: Type,
    /// Whether this property came from declaration method syntax.
    pub is_method: bool,
    /// Whether callers may only read this property.
    pub read_only: bool,
    /// Whether callers may only write this property.
    pub write_only: bool,
    /// Full property source location.
    pub location: Option<Location>,
}

impl DeclaredClassProp {
    /// Converts this declared class property into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        json_node(
            KnownJsonKind::AstDeclaredClassProp,
            self.location,
            [
                ("name", JsonValue::String(self.name.0)),
                ("nameLocation", location_value(self.name_location)),
                (
                    "luauType",
                    JsonValue::Node(Box::new(self.luau_type.into_json())),
                ),
            ],
        )
    }
}

/// A type-reference parameter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TypeParameter {
    /// A type argument.
    Type(Box<Type>),
    /// A type-pack argument.
    Pack(TypePack),
}

impl TypeParameter {
    /// Converts this parameter into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        match self {
            Self::Type(luau_type) => (*luau_type).into_json(),
            Self::Pack(type_pack) => type_pack.into_json(),
        }
    }
}

/// A Rust-native expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Expr {
    /// `nil`.
    Nil {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
    },
    /// Boolean literal.
    Bool {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Literal value.
        value: bool,
    },
    /// Numeric literal.
    Number {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Literal value.
        value: JsonNumber,
    },
    /// Integer literal. Upstream parses this node but its AST JSON encoder
    /// does not emit it, leaving null/missing expression slots.
    Integer {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Literal value.
        value: i64,
    },
    /// String literal.
    String {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Literal value.
        value: String,
    },
    /// Global name expression.
    Global {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Global name.
        name: Name,
    },
    /// Local binding expression.
    Local {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Referenced local.
        local: LocalRef,
    },
    /// Varargs expression.
    Varargs {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
    },
    /// Function call expression.
    Call {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Callee expression.
        func: Box<Self>,
        /// Explicit type or type-pack arguments.
        type_arguments: Vec<TypeParameter>,
        /// Argument expressions.
        args: Vec<Self>,
        /// Whether this is a method call.
        is_self: bool,
        /// Argument-list source location.
        arg_location: Option<Location>,
    },
    /// Binary expression.
    Binary {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Binary operation.
        op: JsonBinaryOp,
        /// Left-hand expression.
        left: Box<Self>,
        /// Right-hand expression.
        right: Box<Self>,
    },
    /// Unary expression.
    Unary {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Unary operation.
        op: JsonUnaryOp,
        /// Operand.
        expr: Box<Self>,
    },
    /// Conditional expression.
    IfElse {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Condition expression.
        condition: Box<Self>,
        /// Whether the `then` token was present.
        has_then: bool,
        /// Expression used when the condition is truthy.
        true_expr: Box<Self>,
        /// Whether the `else` token was present.
        has_else: bool,
        /// Expression used when the condition is falsey.
        false_expr: Box<Self>,
    },
    /// Type assertion expression.
    TypeAssertion {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Asserted expression.
        expr: Box<Self>,
        /// Type annotation.
        annotation: Box<Type>,
    },
    /// Name indexing expression, such as `expr.name`.
    IndexName {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Indexed expression.
        expr: Box<Self>,
        /// Field name.
        index: Name,
        /// Field-name source location.
        index_location: Option<Location>,
        /// Index operator spelling.
        op: &'static str,
    },
    /// Expression indexing expression, such as `expr[key]`.
    IndexExpr {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Indexed expression.
        expr: Box<Self>,
        /// Index expression.
        index: Box<Self>,
    },
    /// Parenthesized expression group.
    Group {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Grouped expression.
        expr: Box<Self>,
    },
    /// Table constructor expression.
    Table {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Table items.
        items: Vec<TableItem>,
    },
    /// Interpolated string expression.
    InterpString {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// String sections between expressions.
        strings: Vec<String>,
        /// Embedded expressions.
        expressions: Vec<Self>,
    },
    /// Function expression.
    Function {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Function attributes.
        attributes: Vec<Attribute>,
        /// Generic type parameters.
        generics: Vec<GenericType>,
        /// Generic type-pack parameters.
        generic_packs: Vec<GenericTypePack>,
        /// Function arguments.
        args: Vec<Local>,
        /// Synthetic method receiver local for `function expr:name()`.
        self_arg: Option<Local>,
        /// Whether the function accepts varargs.
        vararg: bool,
        /// Vararg source location.
        vararg_location: Option<Location>,
        /// Optional vararg annotation.
        vararg_annotation: Option<Box<TypePack>>,
        /// Optional return annotation.
        return_annotation: Option<Box<TypePack>>,
        /// Function body.
        body: Box<Stat>,
        /// Function nesting depth reported by upstream AST JSON.
        function_depth: usize,
        /// Upstream debug name.
        debug_name: String,
    },
    /// Explicit type instantiation expression, such as `f<<T>>`.
    Instantiate {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Instantiated expression.
        expr: Box<Self>,
        /// Type or type-pack arguments.
        type_arguments: Vec<TypeParameter>,
    },
    /// Recoverable expression parse error.
    Error {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Expressions attached to the recovery node.
        expressions: Vec<Self>,
        /// Optional parser diagnostic index.
        message_index: Option<usize>,
    },
}

impl Expr {
    /// Returns this expression's parser-assigned syntax identity.
    #[must_use]
    pub const fn syntax_id(&self) -> SyntaxId {
        match self {
            Self::Nil { syntax_id, .. }
            | Self::Bool { syntax_id, .. }
            | Self::Number { syntax_id, .. }
            | Self::Integer { syntax_id, .. }
            | Self::String { syntax_id, .. }
            | Self::Global { syntax_id, .. }
            | Self::Local { syntax_id, .. }
            | Self::Varargs { syntax_id, .. }
            | Self::Call { syntax_id, .. }
            | Self::Binary { syntax_id, .. }
            | Self::Unary { syntax_id, .. }
            | Self::IfElse { syntax_id, .. }
            | Self::TypeAssertion { syntax_id, .. }
            | Self::IndexName { syntax_id, .. }
            | Self::IndexExpr { syntax_id, .. }
            | Self::Group { syntax_id, .. }
            | Self::Table { syntax_id, .. }
            | Self::InterpString { syntax_id, .. }
            | Self::Function { syntax_id, .. }
            | Self::Instantiate { syntax_id, .. }
            | Self::Error { syntax_id, .. } => *syntax_id,
        }
    }

    /// Returns this expression's source range, when the parser recorded one.
    #[must_use]
    pub const fn location(&self) -> Option<Location> {
        match self {
            Self::Nil { location, .. }
            | Self::Bool { location, .. }
            | Self::Number { location, .. }
            | Self::Integer { location, .. }
            | Self::String { location, .. }
            | Self::Global { location, .. }
            | Self::Local { location, .. }
            | Self::Varargs { location, .. }
            | Self::Call { location, .. }
            | Self::Binary { location, .. }
            | Self::Unary { location, .. }
            | Self::IfElse { location, .. }
            | Self::TypeAssertion { location, .. }
            | Self::IndexName { location, .. }
            | Self::IndexExpr { location, .. }
            | Self::Group { location, .. }
            | Self::Table { location, .. }
            | Self::InterpString { location, .. }
            | Self::Function { location, .. }
            | Self::Instantiate { location, .. }
            | Self::Error { location, .. } => *location,
        }
    }

    /// Converts this expression into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        match self {
            Self::Nil { location, .. } => {
                json_node(KnownJsonKind::AstExprConstantNil, location, [])
            }
            Self::Bool {
                location, value, ..
            } => json_node(
                KnownJsonKind::AstExprConstantBool,
                location,
                [("value", JsonValue::Bool(value))],
            ),
            Self::Number {
                location, value, ..
            } => json_node(
                KnownJsonKind::AstExprConstantNumber,
                location,
                [("value", JsonValue::Number(value))],
            ),
            Self::Integer { location, .. } => json_node(
                KnownJsonKind::AstExprConstantNumber,
                location,
                [("value", JsonValue::Number(json_number(0.0)))],
            ),
            Self::String {
                location, value, ..
            } => json_node(
                KnownJsonKind::AstExprConstantString,
                location,
                [("value", JsonValue::String(value))],
            ),
            Self::Global { location, name, .. } => json_node(
                KnownJsonKind::AstExprGlobal,
                location,
                [("global", JsonValue::String(name.0))],
            ),
            Self::Local {
                location, local, ..
            } => json_node(
                KnownJsonKind::AstExprLocal,
                location,
                [("local", JsonValue::Node(Box::new(local.into_json())))],
            ),
            Self::Varargs { location, .. } => {
                json_node(KnownJsonKind::AstExprVarargs, location, [])
            }
            Self::Call {
                location,
                func,
                type_arguments: _,
                args,
                is_self,
                arg_location,
                ..
            } => {
                let mut fields = BTreeMap::from([
                    ("args".to_owned(), expr_array(args)),
                    ("self".to_owned(), JsonValue::Bool(is_self)),
                    ("argLocation".to_owned(), location_value(arg_location)),
                ]);

                let func = *func;
                let (func, adjacent_types) = match (is_self, func) {
                    (
                        false,
                        Self::Instantiate {
                            expr,
                            type_arguments,
                            ..
                        },
                    ) => (*expr, type_arguments),
                    (_, func) => (func, Vec::new()),
                };
                fields.insert("func".to_owned(), expr_value(func));
                insert_adjacent_type_arguments(&mut fields, adjacent_types);

                JsonNode {
                    kind: JsonKind::Known(KnownJsonKind::AstExprCall),
                    location,
                    fields,
                }
            }
            Self::Binary {
                location,
                op,
                left,
                right,
                ..
            } => json_node(
                KnownJsonKind::AstExprBinary,
                location,
                [
                    ("op", JsonValue::String(format!("{op:?}"))),
                    ("left", expr_value(*left)),
                    ("right", expr_value(*right)),
                ],
            ),
            Self::Unary {
                location, op, expr, ..
            } => json_node(
                KnownJsonKind::AstExprUnary,
                location,
                [
                    ("op", JsonValue::String(format!("{op:?}"))),
                    ("expr", expr_value(*expr)),
                ],
            ),
            Self::IfElse {
                location,
                condition,
                has_then,
                true_expr,
                has_else,
                false_expr,
                ..
            } => json_node(
                KnownJsonKind::AstExprIfElse,
                location,
                [
                    ("condition", expr_value(*condition)),
                    ("hasThen", JsonValue::Bool(has_then)),
                    ("trueExpr", expr_value(*true_expr)),
                    ("hasElse", JsonValue::Bool(has_else)),
                    ("falseExpr", expr_value(*false_expr)),
                ],
            ),
            Self::TypeAssertion {
                location,
                expr,
                annotation,
                ..
            } => json_node(
                KnownJsonKind::AstExprTypeAssertion,
                location,
                [
                    ("expr", expr_value(*expr)),
                    (
                        "annotation",
                        JsonValue::Node(Box::new(annotation.into_json())),
                    ),
                ],
            ),
            Self::IndexName {
                location,
                expr,
                index,
                index_location,
                op,
                ..
            } => json_node(
                KnownJsonKind::AstExprIndexName,
                location,
                [
                    ("expr", expr_value(*expr)),
                    ("index", JsonValue::String(index.0)),
                    ("indexLocation", location_value(index_location)),
                    ("op", JsonValue::String(op.to_owned())),
                ],
            ),
            Self::IndexExpr {
                location,
                expr,
                index,
                ..
            } => json_node(
                KnownJsonKind::AstExprIndexExpr,
                location,
                [("expr", expr_value(*expr)), ("index", expr_value(*index))],
            ),
            Self::Group { location, expr, .. } => json_node(
                KnownJsonKind::AstExprGroup,
                location,
                [("expr", expr_value(*expr))],
            ),
            Self::Table {
                location, items, ..
            } => json_node(
                KnownJsonKind::AstExprTable,
                location,
                [(
                    "items",
                    JsonValue::Array(
                        items
                            .into_iter()
                            .map(|item| JsonValue::Node(Box::new(item.into_json())))
                            .collect(),
                    ),
                )],
            ),
            Self::InterpString {
                location,
                strings,
                expressions,
                ..
            } => json_node(
                KnownJsonKind::AstExprInterpString,
                location,
                [
                    (
                        "strings",
                        JsonValue::Array(strings.into_iter().map(JsonValue::String).collect()),
                    ),
                    ("expressions", expr_array(expressions)),
                ],
            ),
            Self::Function {
                location,
                attributes,
                generics,
                generic_packs,
                args,
                self_arg,
                vararg,
                vararg_location,
                vararg_annotation,
                return_annotation,
                body,
                function_depth,
                debug_name,
                ..
            } => {
                let mut fields = BTreeMap::from([
                    ("attributes".to_owned(), attribute_array(attributes)),
                    ("generics".to_owned(), generic_type_array(generics)),
                    (
                        "genericPacks".to_owned(),
                        generic_type_pack_array(generic_packs),
                    ),
                    ("args".to_owned(), local_array(args)),
                    ("vararg".to_owned(), JsonValue::Bool(vararg)),
                    ("varargLocation".to_owned(), location_value(vararg_location)),
                    (
                        "body".to_owned(),
                        JsonValue::Node(Box::new(body.into_json())),
                    ),
                    (
                        "functionDepth".to_owned(),
                        JsonValue::Number(json_number(function_depth as f64)),
                    ),
                    ("debugname".to_owned(), JsonValue::String(debug_name)),
                ]);
                if let Some(self_arg) = self_arg {
                    fields.insert(
                        "self".to_owned(),
                        JsonValue::Node(Box::new(self_arg.into_json())),
                    );
                }
                if let Some(return_annotation) = return_annotation {
                    fields.insert(
                        "returnAnnotation".to_owned(),
                        JsonValue::Node(Box::new(return_annotation.into_json())),
                    );
                }
                if let Some(vararg_annotation) = vararg_annotation {
                    fields.insert(
                        "varargAnnotation".to_owned(),
                        JsonValue::Node(Box::new(vararg_annotation.into_json())),
                    );
                }
                JsonNode {
                    kind: JsonKind::Known(KnownJsonKind::AstExprFunction),
                    location,
                    fields,
                }
            }
            Self::Instantiate { expr, .. } => expr.into_json(),
            Self::Error {
                location,
                expressions,
                message_index,
                ..
            } => {
                let mut fields =
                    BTreeMap::from([("expressions".to_owned(), expr_array(expressions))]);
                if let Some(message_index) = message_index {
                    fields.insert(
                        "messageIndex".to_owned(),
                        JsonValue::Number(json_number(message_index as f64)),
                    );
                }
                JsonNode {
                    kind: JsonKind::Known(KnownJsonKind::AstExprError),
                    location,
                    fields,
                }
            }
        }
    }
}

/// A table constructor item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableItem {
    /// Item kind.
    pub kind: JsonTableItemKind,
    /// Optional key expression.
    pub key: Option<Expr>,
    /// Value expression.
    pub value: Expr,
}

impl TableItem {
    /// Converts this table item into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        let mut fields = BTreeMap::from([(
            "kind".to_owned(),
            JsonValue::String(table_item_kind(self.kind).to_owned()),
        )]);
        if let Some(key) = self.key {
            fields.insert("key".to_owned(), expr_value(key));
        }
        fields.insert("value".to_owned(), expr_value(self.value));
        JsonNode {
            kind: JsonKind::Known(KnownJsonKind::AstExprTableItem),
            location: None,
            fields,
        }
    }
}

/// A Rust-native statement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Stat {
    /// Statement block.
    Block {
        /// Source location.
        location: Option<Location>,
        /// Whether the block has an explicit end.
        has_end: bool,
        /// Whether this is a lexical `do ... end` block.
        is_do: bool,
        /// Block body.
        body: Vec<Self>,
    },
    /// Return statement.
    Return {
        /// Source location.
        location: Option<Location>,
        /// Returned expressions.
        list: Vec<Expr>,
    },
    /// Expression statement.
    Expr {
        /// Source location.
        location: Option<Location>,
        /// Expression.
        expr: Box<Expr>,
    },
    /// Local declaration statement.
    Local {
        /// Source location.
        location: Option<Location>,
        /// Declared locals.
        vars: Vec<Local>,
        /// Initializer expressions.
        values: Vec<Expr>,
    },
    /// Assignment statement.
    Assign {
        /// Source location.
        location: Option<Location>,
        /// Assigned expressions.
        vars: Vec<Expr>,
        /// Value expressions.
        values: Vec<Expr>,
    },
    /// Compound assignment statement.
    CompoundAssign {
        /// Source location.
        location: Option<Location>,
        /// Compound operator.
        op: JsonCompoundAssignOp,
        /// Assigned expression.
        var: Box<Expr>,
        /// Value expression.
        value: Box<Expr>,
    },
    /// If statement.
    If {
        /// Source location.
        location: Option<Location>,
        /// Condition expression.
        condition: Box<Expr>,
        /// Then body.
        then_body: Box<Self>,
        /// Optional else body.
        else_body: Option<Box<Self>>,
        /// Whether upstream saw the `then` token.
        has_then: bool,
    },
    /// `break` statement.
    Break {
        /// Source location.
        location: Option<Location>,
    },
    /// `continue` statement.
    Continue {
        /// Source location.
        location: Option<Location>,
    },
    /// `while` loop.
    While {
        /// Source location.
        location: Option<Location>,
        /// Loop condition.
        condition: Box<Expr>,
        /// Loop body.
        body: Box<Self>,
        /// Whether the loop has an explicit `do`.
        has_do: bool,
    },
    /// `repeat` loop.
    Repeat {
        /// Source location.
        location: Option<Location>,
        /// Loop condition.
        condition: Box<Expr>,
        /// Loop body.
        body: Box<Self>,
    },
    /// Numeric `for` loop.
    For {
        /// Source location.
        location: Option<Location>,
        /// Loop variable.
        var: Local,
        /// Initial expression.
        from: Box<Expr>,
        /// Limit expression.
        to: Box<Expr>,
        /// Optional step expression.
        step: Option<Box<Expr>>,
        /// Loop body.
        body: Box<Self>,
        /// Whether the loop has an explicit `do`.
        has_do: bool,
    },
    /// Generic `for` loop.
    ForIn {
        /// Source location.
        location: Option<Location>,
        /// Loop variables.
        vars: Vec<Local>,
        /// Iterator expressions.
        values: Vec<Expr>,
        /// Loop body.
        body: Box<Self>,
        /// Whether the loop has an explicit `in`.
        has_in: bool,
        /// Whether the loop has an explicit `do`.
        has_do: bool,
    },
    /// Function declaration.
    Function {
        /// Source location.
        location: Option<Location>,
        /// Function name expression.
        name: Box<Expr>,
        /// Function expression.
        func: Box<Expr>,
    },
    /// Local function declaration.
    LocalFunction {
        /// Source location.
        location: Option<Location>,
        /// Function local.
        name: Local,
        /// Function expression.
        func: Box<Expr>,
    },
    /// Global declaration.
    DeclareGlobal {
        /// Source location.
        location: Option<Location>,
        /// Global name.
        name: Name,
        /// Name source location.
        name_location: Option<Location>,
        /// Declared type.
        luau_type: Box<Type>,
    },
    /// Function declaration.
    DeclareFunction {
        /// Source location.
        location: Option<Location>,
        /// Function attributes.
        attributes: Vec<Attribute>,
        /// Function name.
        name: Name,
        /// Name source location.
        name_location: Option<Location>,
        /// Generic type parameters.
        generics: Vec<GenericType>,
        /// Generic type-pack parameters.
        generic_packs: Vec<GenericTypePack>,
        /// Parameter types.
        params: TypeList,
        /// Parameter names.
        param_names: Vec<ArgumentName>,
        /// Whether the function accepts varargs.
        vararg: bool,
        /// Vararg source location.
        vararg_location: Option<Location>,
        /// Return types.
        ret_types: Box<TypePack>,
    },
    /// Declared class or external type.
    DeclareClass {
        /// Source location.
        location: Option<Location>,
        /// Class name.
        name: Name,
        /// Optional superclass name.
        super_name: Option<Name>,
        /// Declared properties and methods.
        props: Vec<DeclaredClassProp>,
        /// Optional class indexer.
        indexer: Option<TableIndexer>,
    },
    /// Type alias declaration.
    TypeAlias {
        /// Source location.
        location: Option<Location>,
        /// Alias name.
        name: Name,
        /// Generic type parameters.
        generics: Vec<GenericType>,
        /// Generic type-pack parameters.
        generic_packs: Vec<GenericTypePack>,
        /// Aliased type.
        value: Box<Type>,
        /// Whether this is an exported alias.
        exported: bool,
    },
    /// User-defined type function. Upstream AST JSON emits the function
    /// expression, not the unencoded statement wrapper.
    TypeFunction {
        /// Source location.
        location: Option<Location>,
        /// Function name.
        name: Name,
        /// Function name source location.
        name_location: Option<Location>,
        /// Function body expression.
        func: Box<Expr>,
        /// Whether this type function is exported.
        exported: bool,
    },
    /// User-defined class. Upstream AST JSON emits only JSON-visible members,
    /// not the unencoded class statement wrapper.
    Class {
        /// Source location.
        location: Option<Location>,
        /// Hidden class binding used by native lowering. Upstream AST JSON
        /// does not expose this local.
        class_local: Option<Local>,
        /// JSON-visible class members.
        members: Vec<Self>,
        /// Whether upstream leaves an unencoded class-statement placeholder.
        emit_placeholder: bool,
        /// Whether this class is exported.
        exported: bool,
    },
    /// User-defined class property. Upstream AST JSON emits only the property
    /// type for this unencoded wrapper.
    ClassProperty {
        /// Source location.
        location: Option<Location>,
        /// Property name.
        name: Name,
        /// Property name source location.
        name_location: Option<Location>,
        /// Property type.
        luau_type: Option<Box<Type>>,
        /// Whether the owning class is exported.
        exported: bool,
    },
    /// Recoverable statement parse error.
    Error {
        /// Source location.
        location: Option<Location>,
        /// Expressions attached to the recovery node.
        expressions: Vec<Expr>,
        /// Statements attached to the recovery node.
        statements: Vec<Self>,
    },
}

#[allow(clippy::missing_docs_in_private_items, clippy::multiple_inherent_impl)]
impl Stat {
    /// Returns this statement's source range, when the parser recorded one.
    #[must_use]
    pub const fn location(&self) -> Option<Location> {
        match self {
            Self::Block { location, .. }
            | Self::Return { location, .. }
            | Self::Expr { location, .. }
            | Self::Local { location, .. }
            | Self::Assign { location, .. }
            | Self::CompoundAssign { location, .. }
            | Self::If { location, .. }
            | Self::Break { location, .. }
            | Self::Continue { location, .. }
            | Self::While { location, .. }
            | Self::Repeat { location, .. }
            | Self::For { location, .. }
            | Self::ForIn { location, .. }
            | Self::Function { location, .. }
            | Self::LocalFunction { location, .. }
            | Self::DeclareGlobal { location, .. }
            | Self::DeclareFunction { location, .. }
            | Self::DeclareClass { location, .. }
            | Self::TypeAlias { location, .. }
            | Self::TypeFunction { location, .. }
            | Self::Class { location, .. }
            | Self::ClassProperty { location, .. }
            | Self::Error { location, .. } => *location,
        }
    }

    /// Converts this statement into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        match self {
            Self::Block {
                location,
                has_end,
                is_do: _,
                body,
            } => json_node(
                KnownJsonKind::AstStatBlock,
                location,
                [
                    ("hasEnd", JsonValue::Bool(has_end)),
                    ("body", stat_array(body)),
                ],
            ),
            Self::Return { location, list } => json_node(
                KnownJsonKind::AstStatReturn,
                location,
                [("list", expr_array(list))],
            ),
            Self::Expr { location, expr } => json_node(
                KnownJsonKind::AstStatExpr,
                location,
                [("expr", expr_value(*expr))],
            ),
            Self::Local {
                location,
                vars,
                values,
            } => json_node(
                KnownJsonKind::AstStatLocal,
                location,
                [("vars", local_array(vars)), ("values", expr_array(values))],
            ),
            Self::Assign {
                location,
                vars,
                values,
            } => json_node(
                KnownJsonKind::AstStatAssign,
                location,
                [("vars", expr_array(vars)), ("values", expr_array(values))],
            ),
            Self::CompoundAssign {
                location,
                op,
                var,
                value,
            } => json_node(
                KnownJsonKind::AstStatCompoundAssign,
                location,
                [
                    ("op", JsonValue::String(format!("{op:?}"))),
                    ("var", expr_value(*var)),
                    ("value", expr_value(*value)),
                ],
            ),
            Self::If {
                location,
                condition,
                then_body,
                else_body,
                has_then,
            } => {
                let mut fields = BTreeMap::from([
                    ("condition".to_owned(), expr_value(*condition)),
                    (
                        "thenbody".to_owned(),
                        JsonValue::Node(Box::new(then_body.into_json())),
                    ),
                    ("hasThen".to_owned(), JsonValue::Bool(has_then)),
                ]);
                if let Some(else_body) = else_body {
                    fields.insert(
                        "elsebody".to_owned(),
                        JsonValue::Node(Box::new(else_body.into_json())),
                    );
                }
                JsonNode {
                    kind: JsonKind::Known(KnownJsonKind::AstStatIf),
                    location,
                    fields,
                }
            }
            Self::Break { location } => json_node(KnownJsonKind::AstStatBreak, location, []),
            Self::Continue { location } => json_node(KnownJsonKind::AstStatContinue, location, []),
            Self::While {
                location,
                condition,
                body,
                has_do,
            } => json_node(
                KnownJsonKind::AstStatWhile,
                location,
                [
                    ("condition", expr_value(*condition)),
                    ("body", JsonValue::Node(Box::new(body.into_json()))),
                    ("hasDo", JsonValue::Bool(has_do)),
                ],
            ),
            Self::Repeat {
                location,
                condition,
                body,
            } => json_node(
                KnownJsonKind::AstStatRepeat,
                location,
                [
                    ("body", JsonValue::Node(Box::new(body.into_json()))),
                    ("condition", expr_value(*condition)),
                ],
            ),
            Self::For {
                location,
                var,
                from,
                to,
                step,
                body,
                has_do,
            } => {
                let mut fields = BTreeMap::from([
                    ("var".to_owned(), JsonValue::Node(Box::new(var.into_json()))),
                    ("from".to_owned(), expr_value(*from)),
                    ("to".to_owned(), expr_value(*to)),
                    (
                        "body".to_owned(),
                        JsonValue::Node(Box::new(body.into_json())),
                    ),
                    ("hasDo".to_owned(), JsonValue::Bool(has_do)),
                ]);
                if let Some(step) = step {
                    fields.insert("step".to_owned(), expr_value(*step));
                }
                JsonNode {
                    kind: JsonKind::Known(KnownJsonKind::AstStatFor),
                    location,
                    fields,
                }
            }
            Self::ForIn {
                location,
                vars,
                values,
                body,
                has_in,
                has_do,
            } => json_node(
                KnownJsonKind::AstStatForIn,
                location,
                [
                    ("vars", local_array(vars)),
                    ("values", expr_array(values)),
                    ("body", JsonValue::Node(Box::new(body.into_json()))),
                    ("hasIn", JsonValue::Bool(has_in)),
                    ("hasDo", JsonValue::Bool(has_do)),
                ],
            ),
            Self::Function {
                location,
                name,
                func,
            } => json_node(
                KnownJsonKind::AstStatFunction,
                location,
                [
                    ("name", JsonValue::Node(Box::new(name.into_json()))),
                    ("func", JsonValue::Node(Box::new(func.into_json()))),
                ],
            ),
            Self::LocalFunction {
                location,
                name,
                func,
            } => json_node(
                KnownJsonKind::AstStatLocalFunction,
                location,
                [
                    ("name", JsonValue::Node(Box::new(name.into_json()))),
                    ("func", JsonValue::Node(Box::new(func.into_json()))),
                ],
            ),
            Self::DeclareGlobal {
                location,
                name,
                name_location,
                luau_type,
            } => json_node(
                KnownJsonKind::AstStatDeclareGlobal,
                location,
                [
                    ("name", JsonValue::String(name.0)),
                    ("nameLocation", location_value(name_location)),
                    ("type", JsonValue::Node(Box::new(luau_type.into_json()))),
                ],
            ),
            Self::DeclareFunction {
                location,
                attributes,
                name,
                name_location,
                generics,
                generic_packs,
                params,
                param_names,
                vararg,
                vararg_location,
                ret_types,
            } => json_node(
                KnownJsonKind::AstStatDeclareFunction,
                location,
                [
                    ("attributes", attribute_array(attributes)),
                    ("name", JsonValue::String(name.0)),
                    ("nameLocation", location_value(name_location)),
                    ("params", type_list_value(params)),
                    ("paramNames", argument_name_required_array(param_names)),
                    ("vararg", JsonValue::Bool(vararg)),
                    ("varargLocation", location_value(vararg_location)),
                    ("retTypes", JsonValue::Node(Box::new(ret_types.into_json()))),
                    ("generics", generic_type_array(generics)),
                    ("genericPacks", generic_type_pack_array(generic_packs)),
                ],
            ),
            Self::DeclareClass {
                location,
                name,
                super_name,
                props,
                indexer,
            } => {
                let mut fields = BTreeMap::from([
                    ("name".to_owned(), JsonValue::String(name.0)),
                    ("props".to_owned(), declared_class_prop_array(props)),
                    (
                        "indexer".to_owned(),
                        indexer.map_or(JsonValue::Null, TableIndexer::into_json),
                    ),
                ]);
                if let Some(super_name) = super_name {
                    fields.insert("superName".to_owned(), JsonValue::String(super_name.0));
                }
                JsonNode {
                    kind: JsonKind::Known(KnownJsonKind::AstStatDeclareClass),
                    location,
                    fields,
                }
            }
            Self::TypeAlias {
                location,
                name,
                generics,
                generic_packs,
                value,
                exported,
            } => json_node(
                KnownJsonKind::AstStatTypeAlias,
                location,
                [
                    ("name", JsonValue::String(name.0)),
                    ("generics", generic_type_array(generics)),
                    ("genericPacks", generic_type_pack_array(generic_packs)),
                    ("value", JsonValue::Node(Box::new(value.into_json()))),
                    ("exported", JsonValue::Bool(exported)),
                ],
            ),
            Self::TypeFunction { func, .. } => func.into_json(),
            Self::Class {
                location, members, ..
            } => json_node(
                KnownJsonKind::AstStatBlock,
                location,
                [
                    ("hasEnd", JsonValue::Bool(true)),
                    ("body", stat_array(members)),
                ],
            ),
            Self::ClassProperty {
                location,
                luau_type: None,
                ..
            } => json_node(
                KnownJsonKind::AstStatBlock,
                location,
                [
                    ("hasEnd", JsonValue::Bool(true)),
                    ("body", JsonValue::Array(Vec::new())),
                ],
            ),
            Self::ClassProperty {
                luau_type: Some(luau_type),
                ..
            } => luau_type.into_json(),
            Self::Error {
                location,
                expressions,
                statements,
            } => json_node(
                KnownJsonKind::AstStatError,
                location,
                [
                    ("expressions", expr_array(expressions)),
                    ("statements", stat_array(statements)),
                ],
            ),
        }
    }
}

/// A Rust-native type expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Type {
    /// Named type reference.
    Reference {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Optional dotted prefix.
        prefix: Option<Name>,
        /// Prefix source location.
        prefix_location: Option<Location>,
        /// Type name.
        name: Name,
        /// Type-name source location.
        name_location: Option<Location>,
        /// Type or type-pack parameters.
        parameters: Vec<TypeParameter>,
    },
    /// `typeof(expr)`.
    Typeof {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Referenced expression.
        expr: Expr,
    },
    /// Optional type.
    Optional {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
    },
    /// Parenthesized type group.
    Group {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Inner type.
        inner: Box<Self>,
    },
    /// Union type.
    Union {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Union options.
        types: Vec<Self>,
    },
    /// Intersection type.
    Intersection {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Intersection options.
        types: Vec<Self>,
    },
    /// Function type.
    Function {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Function type attributes.
        attributes: Vec<Attribute>,
        /// Generic type parameters.
        generics: Vec<GenericType>,
        /// Generic type-pack parameters.
        generic_packs: Vec<GenericTypePack>,
        /// Argument types.
        arg_types: TypeList,
        /// Optional argument names.
        arg_names: Vec<Option<ArgumentName>>,
        /// Return type pack.
        return_types: TypePack,
    },
    /// Table type.
    Table {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Table properties.
        props: Vec<TableProp>,
        /// Optional table indexer.
        indexer: Option<TableIndexer>,
    },
    /// Singleton string type.
    SingletonString {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// String value.
        value: String,
    },
    /// Singleton boolean type.
    SingletonBool {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Boolean value.
        value: bool,
    },
    /// Recoverable type parse error.
    Error {
        /// Parser-assigned syntax identity.
        syntax_id: SyntaxId,
        /// Source location.
        location: Option<Location>,
        /// Types attached to the recovery node.
        types: Vec<Self>,
        /// Optional parser diagnostic index.
        message_index: Option<usize>,
    },
}

impl Type {
    /// Returns this type node's parser-assigned syntax identity.
    #[must_use]
    pub const fn syntax_id(&self) -> SyntaxId {
        match self {
            Self::Reference { syntax_id, .. }
            | Self::Typeof { syntax_id, .. }
            | Self::Optional { syntax_id, .. }
            | Self::Group { syntax_id, .. }
            | Self::Union { syntax_id, .. }
            | Self::Intersection { syntax_id, .. }
            | Self::Function { syntax_id, .. }
            | Self::Table { syntax_id, .. }
            | Self::SingletonString { syntax_id, .. }
            | Self::SingletonBool { syntax_id, .. }
            | Self::Error { syntax_id, .. } => *syntax_id,
        }
    }

    /// Returns this type node's source range, when the parser recorded one.
    #[must_use]
    pub const fn location(&self) -> Option<Location> {
        match self {
            Self::Reference { location, .. }
            | Self::Typeof { location, .. }
            | Self::Optional { location, .. }
            | Self::Group { location, .. }
            | Self::Union { location, .. }
            | Self::Intersection { location, .. }
            | Self::Function { location, .. }
            | Self::Table { location, .. }
            | Self::SingletonString { location, .. }
            | Self::SingletonBool { location, .. }
            | Self::Error { location, .. } => *location,
        }
    }

    /// Converts this type into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        match self {
            Self::Reference {
                location,
                prefix,
                prefix_location,
                name,
                name_location,
                parameters,
                ..
            } => {
                let mut fields = BTreeMap::from([
                    ("name".to_owned(), JsonValue::String(name.0)),
                    ("nameLocation".to_owned(), location_value(name_location)),
                    ("parameters".to_owned(), type_parameter_array(parameters)),
                ]);
                if let Some(prefix) = prefix {
                    fields.insert("prefix".to_owned(), JsonValue::String(prefix.0));
                }
                if prefix_location.is_some() {
                    fields.insert("prefixLocation".to_owned(), location_value(prefix_location));
                }
                JsonNode {
                    kind: JsonKind::Known(KnownJsonKind::AstTypeReference),
                    location,
                    fields,
                }
            }
            Self::Typeof { location, expr, .. } => json_node(
                KnownJsonKind::AstTypeTypeof,
                location,
                [("expr", expr_value(expr))],
            ),
            Self::Optional { location, .. } => {
                json_node(KnownJsonKind::AstTypeOptional, location, [])
            }
            Self::Group {
                location, inner, ..
            } => json_node(
                KnownJsonKind::AstTypeGroup,
                location,
                [("inner", JsonValue::Node(Box::new(inner.into_json())))],
            ),
            Self::Union {
                location, types, ..
            } => json_node(
                KnownJsonKind::AstTypeUnion,
                location,
                [("types", type_array(types))],
            ),
            Self::Intersection {
                location, types, ..
            } => json_node(
                KnownJsonKind::AstTypeIntersection,
                location,
                [("types", type_array(types))],
            ),
            Self::Function {
                location,
                attributes,
                generics,
                generic_packs,
                arg_types,
                arg_names,
                return_types,
                ..
            } => json_node(
                KnownJsonKind::AstTypeFunction,
                location,
                [
                    ("attributes", attribute_array(attributes)),
                    ("generics", generic_type_array(generics)),
                    ("genericPacks", generic_type_pack_array(generic_packs)),
                    ("argTypes", type_list_value(arg_types)),
                    ("argNames", argument_name_array(arg_names)),
                    (
                        "returnTypes",
                        JsonValue::Node(Box::new(return_types.into_json())),
                    ),
                ],
            ),
            Self::Table {
                location,
                props,
                indexer,
                ..
            } => json_node(
                KnownJsonKind::AstTypeTable,
                location,
                [
                    ("props", table_prop_array(props)),
                    (
                        "indexer",
                        indexer.map_or(JsonValue::Null, TableIndexer::into_json),
                    ),
                ],
            ),
            Self::SingletonString {
                location, value, ..
            } => json_node(
                KnownJsonKind::AstTypeSingletonString,
                location,
                [("value", JsonValue::String(value))],
            ),
            Self::SingletonBool {
                location, value, ..
            } => json_node(
                KnownJsonKind::AstTypeSingletonBool,
                location,
                [("value", JsonValue::Bool(value))],
            ),
            Self::Error {
                location,
                types,
                message_index,
                ..
            } => {
                let mut fields = BTreeMap::from([("types".to_owned(), type_array(types))]);
                if let Some(message_index) = message_index {
                    fields.insert(
                        "messageIndex".to_owned(),
                        JsonValue::Number(json_number(message_index as f64)),
                    );
                }
                JsonNode {
                    kind: JsonKind::Known(KnownJsonKind::AstTypeError),
                    location,
                    fields,
                }
            }
        }
    }
}

/// A Rust-native type pack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TypePack {
    /// Explicit type-pack list.
    Explicit {
        /// Source location.
        location: Option<Location>,
        /// Explicit type-list entries.
        type_list: TypeList,
    },
    /// Generic type pack.
    Generic {
        /// Source location.
        location: Option<Location>,
        /// Generic pack name.
        name: Name,
    },
    /// Variadic type pack.
    Variadic {
        /// Source location.
        location: Option<Location>,
        /// Variadic element type.
        variadic_type: Box<Type>,
    },
}

impl TypePack {
    /// Returns this type pack's source range, when the parser recorded one.
    #[must_use]
    pub const fn location(&self) -> Option<Location> {
        match self {
            Self::Explicit { location, .. }
            | Self::Generic { location, .. }
            | Self::Variadic { location, .. } => *location,
        }
    }

    /// Converts this type pack into AST JSON.
    #[must_use]
    pub fn into_json(self) -> JsonNode {
        match self {
            Self::Explicit {
                location,
                type_list,
            } => json_node(
                KnownJsonKind::AstTypePackExplicit,
                location,
                [("typeList", type_list_value(type_list))],
            ),
            Self::Generic { location, name } => json_node(
                KnownJsonKind::AstTypePackGeneric,
                location,
                [("genericName", JsonValue::String(name.0))],
            ),
            Self::Variadic {
                location,
                variadic_type,
            } => json_node(
                KnownJsonKind::AstTypePackVariadic,
                location,
                [(
                    "variadicType",
                    JsonValue::Node(Box::new(variadic_type.into_json())),
                )],
            ),
        }
    }
}

/// Builds a JSON node from field pairs.
fn json_number(value: f64) -> JsonNumber {
    JsonNumber::finite(value).expect("finite number")
}

/// Converts an optional location into an upstream location field.
fn location_value(location: Option<Location>) -> JsonValue {
    JsonValue::String(location.unwrap_or_default().to_upstream_string())
}

/// Converts expressions into an AST JSON array.
fn expr_array(exprs: Vec<Expr>) -> JsonValue {
    let last_index = exprs.len().saturating_sub(1);
    JsonValue::Array(
        exprs
            .into_iter()
            .enumerate()
            .flat_map(|(index, expr)| expr_array_values(index == last_index, expr))
            .collect(),
    )
}

/// Converts an expression into one or more upstream AST JSON values for arrays.
fn expr_array_values(is_last: bool, expr: Expr) -> Vec<JsonValue> {
    match expr {
        Expr::Integer { .. } if is_last => Vec::new(),
        Expr::Integer { .. } => vec![JsonValue::Null],
        Expr::Instantiate {
            expr,
            type_arguments,
            ..
        } => iter::once(expr_value(*expr))
            .chain(
                type_arguments
                    .into_iter()
                    .map(|argument| JsonValue::Node(Box::new(argument.into_json()))),
            )
            .collect(),
        expr => vec![expr_value(expr)],
    }
}

/// Converts an expression into an upstream AST JSON value.
fn expr_value(expr: Expr) -> JsonValue {
    match expr {
        Expr::Integer { .. } => JsonValue::Null,
        expr => JsonValue::Node(Box::new(expr.into_json())),
    }
}

/// Adds upstream's normalized adjacent fields for unencoded instantiation nodes.
fn insert_adjacent_type_arguments(
    fields: &mut BTreeMap<String, JsonValue>,
    type_arguments: Vec<TypeParameter>,
) {
    for (index, argument) in type_arguments.into_iter().enumerate() {
        fields.insert(
            format!("__ruau_adjacent_{}", index + 1),
            JsonValue::Node(Box::new(argument.into_json())),
        );
    }
}

/// Converts statements into an AST JSON array.
fn stat_array(stats: Vec<Stat>) -> JsonValue {
    let mut nodes = Vec::new();
    for stat in stats {
        nodes.extend(stat_array_values(stat));
    }
    while matches!(nodes.last(), Some(JsonValue::Null)) {
        nodes.pop();
    }
    JsonValue::Array(nodes)
}

/// Converts a statement into one or more upstream AST JSON body nodes.
fn stat_array_values(stat: Stat) -> Vec<JsonValue> {
    match stat {
        Stat::Class {
            members,
            emit_placeholder,
            ..
        } => {
            let mut values = Vec::new();
            if emit_placeholder {
                values.push(JsonValue::Null);
            }
            values.extend(members.into_iter().flat_map(stat_array_values));
            values
        }
        Stat::ClassProperty {
            luau_type: None, ..
        } => Vec::new(),
        Stat::ClassProperty {
            luau_type: Some(luau_type),
            ..
        } => vec![JsonValue::Node(Box::new(luau_type.into_json()))],
        stat => vec![JsonValue::Node(Box::new(stat.into_json()))],
    }
}

/// Converts locals into an AST JSON array.
fn local_array(locals: Vec<Local>) -> JsonValue {
    JsonValue::Array(
        locals
            .into_iter()
            .map(|local| JsonValue::Node(Box::new(local.into_json())))
            .collect(),
    )
}

/// Converts attributes into an AST JSON array.
fn attribute_array(attributes: Vec<Attribute>) -> JsonValue {
    JsonValue::Array(
        attributes
            .into_iter()
            .map(|attribute| JsonValue::Node(Box::new(attribute.into_json())))
            .collect(),
    )
}

/// Converts generic type parameters into an AST JSON array.
fn generic_type_array(generics: Vec<GenericType>) -> JsonValue {
    JsonValue::Array(
        generics
            .into_iter()
            .map(|generic| JsonValue::Node(Box::new(generic.into_json())))
            .collect(),
    )
}

/// Converts generic type-pack parameters into an AST JSON array.
fn generic_type_pack_array(generics: Vec<GenericTypePack>) -> JsonValue {
    JsonValue::Array(
        generics
            .into_iter()
            .map(|generic| JsonValue::Node(Box::new(generic.into_json())))
            .collect(),
    )
}

/// Converts argument names into an AST JSON array.
fn argument_name_array(names: Vec<Option<ArgumentName>>) -> JsonValue {
    JsonValue::Array(
        names
            .into_iter()
            .map(|name| {
                name.map_or(JsonValue::Null, |name| {
                    JsonValue::Node(Box::new(name.into_json()))
                })
            })
            .collect(),
    )
}

/// Converts required argument names into an AST JSON array.
fn argument_name_required_array(names: Vec<ArgumentName>) -> JsonValue {
    JsonValue::Array(
        names
            .into_iter()
            .map(|name| JsonValue::Node(Box::new(name.into_json())))
            .collect(),
    )
}

/// Converts table properties into an AST JSON array.
fn table_prop_array(props: Vec<TableProp>) -> JsonValue {
    JsonValue::Array(
        props
            .into_iter()
            .map(|prop| JsonValue::Node(Box::new(prop.into_json())))
            .collect(),
    )
}

/// Converts declared class properties into an AST JSON array.
fn declared_class_prop_array(props: Vec<DeclaredClassProp>) -> JsonValue {
    JsonValue::Array(
        props
            .into_iter()
            .map(|prop| JsonValue::Node(Box::new(prop.into_json())))
            .collect(),
    )
}

/// Converts types into an AST JSON array.
fn type_array(types: Vec<Type>) -> JsonValue {
    JsonValue::Array(
        types
            .into_iter()
            .map(|ty| JsonValue::Node(Box::new(ty.into_json())))
            .collect(),
    )
}

/// Converts type-reference parameters into an AST JSON array.
fn type_parameter_array(parameters: Vec<TypeParameter>) -> JsonValue {
    JsonValue::Array(
        parameters
            .into_iter()
            .map(|parameter| JsonValue::Node(Box::new(parameter.into_json())))
            .collect(),
    )
}

/// Converts a type list into an `AstTypeList` node value.
fn type_list_value(type_list: TypeList) -> JsonValue {
    JsonValue::Node(Box::new(type_list.into_json()))
}

/// Returns the upstream JSON spelling for a table item kind.
fn table_item_kind(kind: JsonTableItemKind) -> &'static str {
    match kind {
        JsonTableItemKind::Item => "item",
        JsonTableItemKind::Record => "record",
        JsonTableItemKind::General => "general",
    }
}

/// Builds a JSON node from field pairs.
fn json_node<const N: usize>(
    kind: KnownJsonKind,
    location: Option<Location>,
    fields: [(&str, JsonValue); N],
) -> JsonNode {
    JsonNode {
        kind: JsonKind::Known(kind),
        location,
        fields: fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<BTreeMap<_, _>>(),
    }
}

#[cfg(any())]
mod tests {
    use super::{Expr, Name, Stat, SyntaxId};
    use crate::{
        Location, Position,
        json::{JsonKind, KnownJsonKind},
    };

    #[test]
    fn converts_native_block_to_json() {
        let location = Some(Location::new(Position::new(0, 0), Position::new(0, 11)));
        let node = Stat::Return {
            location,
            list: vec![Expr::Global {
                syntax_id: SyntaxId::default(),
                location,
                name: Name::new("foo"),
            }],
        }
        .into_json();

        assert_eq!(node.kind, JsonKind::Known(KnownJsonKind::AstStatReturn));
    }
}
