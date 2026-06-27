//! Borrowed AST traversal helpers.

use std::fmt;

use crate::{
    Location, Position,
    syntax::{
        DeclaredClassProp, Expr, GenericType, GenericTypePack, Local, Stat, TableIndexer,
        TableItem, TableProp, Type, TypeList, TypePack, TypeParameter,
    },
};

/// A stable logical path to a node in an AST.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct NodePath {
    /// Path components from the traversal root.
    components: Vec<PathComponent>,
}

impl NodePath {
    /// Returns the root path.
    #[must_use]
    pub fn root() -> Self {
        Self::default()
    }

    /// Returns the path components.
    #[must_use]
    pub fn components(&self) -> &[PathComponent] {
        &self.components
    }

    /// Returns a child path under a named field.
    #[must_use]
    pub fn field(&self, name: &'static str) -> Self {
        let mut child = self.clone();
        child.components.push(PathComponent::Field(name));
        child
    }

    /// Returns an indexed child path under a repeated field.
    #[must_use]
    pub fn index(&self, index: usize) -> Self {
        let mut child = self.clone();
        child.components.push(PathComponent::Index(index));
        child
    }
}

impl fmt::Display for NodePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("$")?;
        for component in &self.components {
            match component {
                PathComponent::Field(field) => write!(formatter, ".{field}")?,
                PathComponent::Index(index) => write!(formatter, "[{index}]")?,
            }
        }
        Ok(())
    }
}

/// One component in an [`NodePath`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum PathComponent {
    /// Named struct or enum field.
    Field(&'static str),
    /// Index inside a repeated field.
    Index(usize),
}

/// Controls traversal after a node callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalkControl {
    /// Visit this node's children.
    Continue,
    /// Do not visit this node's children.
    SkipChildren,
}

/// Borrowed AST visitor.
pub trait Visitor<'ast> {
    /// Visits a statement.
    fn visit_stat(&mut self, _path: &NodePath, _stat: &'ast Stat) -> WalkControl {
        WalkControl::Continue
    }

    /// Visits a local declaration.
    fn visit_local(&mut self, _path: &NodePath, _local: &'ast Local) -> WalkControl {
        WalkControl::Continue
    }

    /// Visits an expression.
    fn visit_expr(&mut self, _path: &NodePath, _expr: &'ast Expr) -> WalkControl {
        WalkControl::Continue
    }

    /// Visits a type.
    fn visit_type(&mut self, _path: &NodePath, _luau_type: &'ast Type) -> WalkControl {
        WalkControl::Continue
    }

    /// Visits a type pack.
    fn visit_type_pack(&mut self, _path: &NodePath, _type_pack: &'ast TypePack) -> WalkControl {
        WalkControl::Continue
    }
}

/// Walks a statement tree.
pub fn walk_stat<'ast, V: Visitor<'ast> + ?Sized>(stat: &'ast Stat, visitor: &mut V) {
    walk_stat_at(stat, visitor, &NodePath::root());
}

/// Walks an expression tree.
pub fn walk_expr<'ast, V: Visitor<'ast> + ?Sized>(expr: &'ast Expr, visitor: &mut V) {
    walk_expr_at(expr, visitor, &NodePath::root());
}

/// Walks a type tree.
pub fn walk_type<'ast, V: Visitor<'ast> + ?Sized>(luau_type: &'ast Type, visitor: &mut V) {
    walk_type_at(luau_type, visitor, &NodePath::root());
}

/// Walks a type-pack tree.
pub fn walk_type_pack<'ast, V: Visitor<'ast> + ?Sized>(type_pack: &'ast TypePack, visitor: &mut V) {
    walk_type_pack_at(type_pack, visitor, &NodePath::root());
}

/// Borrowed AST node returned by source-position queries.
#[derive(Clone, Copy, Debug)]
pub enum NodeRef<'a> {
    /// Statement node.
    Stat(&'a Stat),
    /// Expression node.
    Expr(&'a Expr),
    /// Type node.
    Type(&'a Type),
    /// Type-pack node.
    TypePack(&'a TypePack),
}

impl<'a> NodeRef<'a> {
    /// Returns the node as an expression, if it is one.
    #[must_use]
    pub const fn as_expr(self) -> Option<&'a Expr> {
        match self {
            Self::Expr(expr) => Some(expr),
            Self::Stat(_) | Self::Type(_) | Self::TypePack(_) => None,
        }
    }

    /// Returns the node's source location, if it has one.
    #[must_use]
    pub fn location(self) -> Option<Location> {
        match self {
            Self::Stat(stat) => stat.location(),
            Self::Expr(expr) => expr.location(),
            Self::Type(luau_type) => luau_type.location(),
            Self::TypePack(type_pack) => type_pack.location(),
        }
    }
}

/// Finds the innermost AST node at a source position.
///
/// Positions past the root end are clamped to the root end, matching upstream
/// Luau's autocomplete-facing AST query behavior.
#[must_use]
pub fn find_node_at_position(root: &Stat, mut position: Position) -> Option<NodeRef<'_>> {
    let document_end = if let Some(location) = root.location() {
        if position < location.begin {
            return Some(NodeRef::Stat(root));
        }

        if position > location.end {
            position = location.end;
        }

        location.end
    } else {
        position
    };

    let mut best = None;
    find_node_in_stat(root, position, document_end, &mut best);
    best
}

/// Finds the innermost expression at a source position.
#[must_use]
pub fn find_expr_at_position(root: &Stat, position: Position) -> Option<&Expr> {
    find_node_at_position(root, position).and_then(NodeRef::as_expr)
}

/// Walks a statement at `path`.
fn walk_stat_at<'ast, V: Visitor<'ast> + ?Sized>(
    stat: &'ast Stat,
    visitor: &mut V,
    path: &NodePath,
) {
    if visitor.visit_stat(path, stat) == WalkControl::SkipChildren {
        return;
    }

    match stat {
        Stat::Block { body, .. } => walk_stats(body, visitor, &path.field("body")),
        Stat::Return { list, .. } => walk_exprs(list, visitor, &path.field("list")),
        Stat::Expr { expr, .. } => walk_expr_at(expr, visitor, &path.field("expr")),
        Stat::Local { vars, values, .. } => {
            walk_locals(vars, visitor, &path.field("vars"));
            walk_exprs(values, visitor, &path.field("values"));
        }
        Stat::Assign { vars, values, .. } => {
            walk_exprs(vars, visitor, &path.field("vars"));
            walk_exprs(values, visitor, &path.field("values"));
        }
        Stat::CompoundAssign { var, value, .. } => {
            walk_expr_at(var, visitor, &path.field("var"));
            walk_expr_at(value, visitor, &path.field("value"));
        }
        Stat::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            walk_expr_at(condition, visitor, &path.field("condition"));
            walk_stat_at(then_body, visitor, &path.field("thenBody"));
            if let Some(else_body) = else_body {
                walk_stat_at(else_body, visitor, &path.field("elseBody"));
            }
        }
        Stat::While {
            condition, body, ..
        } => {
            walk_expr_at(condition, visitor, &path.field("condition"));
            walk_stat_at(body, visitor, &path.field("body"));
        }
        Stat::Repeat {
            condition, body, ..
        } => {
            walk_stat_at(body, visitor, &path.field("body"));
            walk_expr_at(condition, visitor, &path.field("condition"));
        }
        Stat::For {
            var,
            from,
            to,
            step,
            body,
            ..
        } => {
            walk_local(var, visitor, &path.field("var"));
            walk_expr_at(from, visitor, &path.field("from"));
            walk_expr_at(to, visitor, &path.field("to"));
            if let Some(step) = step {
                walk_expr_at(step, visitor, &path.field("step"));
            }
            walk_stat_at(body, visitor, &path.field("body"));
        }
        Stat::ForIn {
            vars, values, body, ..
        } => {
            walk_locals(vars, visitor, &path.field("vars"));
            walk_exprs(values, visitor, &path.field("values"));
            walk_stat_at(body, visitor, &path.field("body"));
        }
        Stat::Function { name, func, .. } => {
            walk_expr_at(name, visitor, &path.field("name"));
            walk_expr_at(func, visitor, &path.field("func"));
        }
        Stat::LocalFunction { name, func, .. } => {
            walk_local(name, visitor, &path.field("name"));
            walk_expr_at(func, visitor, &path.field("func"));
        }
        Stat::DeclareGlobal { luau_type, .. } => {
            walk_type_at(luau_type, visitor, &path.field("luauType"));
        }
        Stat::DeclareFunction {
            generics,
            generic_packs,
            params,
            ret_types,
            ..
        } => {
            walk_generic_types(generics, visitor, &path.field("generics"));
            walk_generic_type_packs(generic_packs, visitor, &path.field("genericPacks"));
            walk_type_list(params, visitor, &path.field("params"));
            walk_type_pack_at(ret_types, visitor, &path.field("retTypes"));
        }
        Stat::DeclareClass { props, indexer, .. } => {
            for (index, prop) in props.iter().enumerate() {
                walk_declared_class_prop(prop, visitor, &path.field("props").index(index));
            }
            if let Some(indexer) = indexer {
                walk_table_indexer(indexer, visitor, &path.field("indexer"));
            }
        }
        Stat::TypeAlias {
            generics,
            generic_packs,
            value,
            ..
        } => {
            walk_generic_types(generics, visitor, &path.field("generics"));
            walk_generic_type_packs(generic_packs, visitor, &path.field("genericPacks"));
            walk_type_at(value, visitor, &path.field("value"));
        }
        Stat::TypeFunction { func, .. } => walk_expr_at(func, visitor, &path.field("func")),
        Stat::Class {
            class_local,
            members,
            ..
        } => {
            if let Some(class_local) = class_local {
                walk_local(class_local, visitor, &path.field("classLocal"));
            }
            walk_stats(members, visitor, &path.field("members"));
        }
        Stat::ClassProperty {
            luau_type: Some(luau_type),
            ..
        } => {
            walk_type_at(luau_type, visitor, &path.field("luauType"));
        }
        Stat::ClassProperty {
            luau_type: None, ..
        } => {}
        Stat::Error {
            expressions,
            statements,
            ..
        } => {
            walk_exprs(expressions, visitor, &path.field("expressions"));
            walk_stats(statements, visitor, &path.field("statements"));
        }
        Stat::Break { .. } | Stat::Continue { .. } => {}
    }
}

/// Walks an expression at `path`.
fn walk_expr_at<'ast, V: Visitor<'ast> + ?Sized>(
    expr: &'ast Expr,
    visitor: &mut V,
    path: &NodePath,
) {
    if visitor.visit_expr(path, expr) == WalkControl::SkipChildren {
        return;
    }

    match expr {
        Expr::Call {
            func,
            type_arguments,
            args,
            ..
        } => {
            walk_expr_at(func, visitor, &path.field("func"));
            walk_type_parameters(type_arguments, visitor, &path.field("typeArguments"));
            walk_exprs(args, visitor, &path.field("args"));
        }
        Expr::Binary { left, right, .. } => {
            walk_expr_at(left, visitor, &path.field("left"));
            walk_expr_at(right, visitor, &path.field("right"));
        }
        Expr::Unary { expr, .. } | Expr::Group { expr, .. } => {
            walk_expr_at(expr, visitor, &path.field("expr"));
        }
        Expr::IfElse {
            condition,
            true_expr,
            false_expr,
            ..
        } => {
            walk_expr_at(condition, visitor, &path.field("condition"));
            walk_expr_at(true_expr, visitor, &path.field("trueExpr"));
            walk_expr_at(false_expr, visitor, &path.field("falseExpr"));
        }
        Expr::TypeAssertion {
            expr, annotation, ..
        } => {
            walk_expr_at(expr, visitor, &path.field("expr"));
            walk_type_at(annotation, visitor, &path.field("annotation"));
        }
        Expr::IndexName { expr, .. } => walk_expr_at(expr, visitor, &path.field("expr")),
        Expr::IndexExpr { expr, index, .. } => {
            walk_expr_at(expr, visitor, &path.field("expr"));
            walk_expr_at(index, visitor, &path.field("index"));
        }
        Expr::Table { items, .. } => {
            for (index, item) in items.iter().enumerate() {
                walk_table_item(item, visitor, &path.field("items").index(index));
            }
        }
        Expr::InterpString { expressions, .. } => {
            walk_exprs(expressions, visitor, &path.field("expressions"));
        }
        Expr::Function {
            generics,
            generic_packs,
            args,
            self_arg,
            vararg_annotation,
            return_annotation,
            body,
            ..
        } => {
            walk_generic_types(generics, visitor, &path.field("generics"));
            walk_generic_type_packs(generic_packs, visitor, &path.field("genericPacks"));
            walk_locals(args, visitor, &path.field("args"));
            if let Some(self_arg) = self_arg {
                walk_local(self_arg, visitor, &path.field("self"));
            }
            if let Some(vararg_annotation) = vararg_annotation {
                walk_type_pack_at(vararg_annotation, visitor, &path.field("varargAnnotation"));
            }
            if let Some(return_annotation) = return_annotation {
                walk_type_pack_at(return_annotation, visitor, &path.field("returnAnnotation"));
            }
            walk_stat_at(body, visitor, &path.field("body"));
        }
        Expr::Instantiate {
            expr,
            type_arguments,
            ..
        } => {
            walk_expr_at(expr, visitor, &path.field("expr"));
            walk_type_parameters(type_arguments, visitor, &path.field("typeArguments"));
        }
        Expr::Error { expressions, .. } => {
            walk_exprs(expressions, visitor, &path.field("expressions"));
        }
        Expr::Nil { .. }
        | Expr::Bool { .. }
        | Expr::Number { .. }
        | Expr::Integer { .. }
        | Expr::String { .. }
        | Expr::Global { .. }
        | Expr::Local { .. }
        | Expr::Varargs { .. } => {}
    }
}

/// Walks a type at `path`.
fn walk_type_at<'ast, V: Visitor<'ast> + ?Sized>(
    luau_type: &'ast Type,
    visitor: &mut V,
    path: &NodePath,
) {
    if visitor.visit_type(path, luau_type) == WalkControl::SkipChildren {
        return;
    }

    match luau_type {
        Type::Reference { parameters, .. } => {
            walk_type_parameters(parameters, visitor, &path.field("parameters"));
        }
        Type::Typeof { expr, .. } => walk_expr_at(expr, visitor, &path.field("expr")),
        Type::Group { inner, .. } => walk_type_at(inner, visitor, &path.field("inner")),
        Type::Union { types, .. } | Type::Intersection { types, .. } => {
            walk_types(types, visitor, &path.field("types"));
        }
        Type::Function {
            generics,
            generic_packs,
            arg_types,
            return_types,
            ..
        } => {
            walk_generic_types(generics, visitor, &path.field("generics"));
            walk_generic_type_packs(generic_packs, visitor, &path.field("genericPacks"));
            walk_type_list(arg_types, visitor, &path.field("argTypes"));
            walk_type_pack_at(return_types, visitor, &path.field("returnTypes"));
        }
        Type::Table { props, indexer, .. } => {
            for (index, prop) in props.iter().enumerate() {
                walk_table_prop(prop, visitor, &path.field("props").index(index));
            }
            if let Some(indexer) = indexer {
                walk_table_indexer(indexer, visitor, &path.field("indexer"));
            }
        }
        Type::Error { types, .. } => walk_types(types, visitor, &path.field("types")),
        Type::Optional { .. } | Type::SingletonString { .. } | Type::SingletonBool { .. } => {}
    }
}

/// Walks a type pack at `path`.
fn walk_type_pack_at<'ast, V: Visitor<'ast> + ?Sized>(
    type_pack: &'ast TypePack,
    visitor: &mut V,
    path: &NodePath,
) {
    if visitor.visit_type_pack(path, type_pack) == WalkControl::SkipChildren {
        return;
    }

    match type_pack {
        TypePack::Explicit { type_list, .. } => {
            walk_type_list(type_list, visitor, &path.field("typeList"));
        }
        TypePack::Variadic { variadic_type, .. } => {
            walk_type_at(variadic_type, visitor, &path.field("variadicType"));
        }
        TypePack::Generic { .. } => {}
    }
}

/// Walks type children for a declared class property.
fn walk_declared_class_prop<'ast, V: Visitor<'ast> + ?Sized>(
    prop: &'ast DeclaredClassProp,
    visitor: &mut V,
    path: &NodePath,
) {
    walk_type_at(&prop.luau_type, visitor, &path.field("luauType"));
}

/// Walks expression children for a table item.
fn walk_table_item<'ast, V: Visitor<'ast> + ?Sized>(
    item: &'ast TableItem,
    visitor: &mut V,
    path: &NodePath,
) {
    if let Some(key) = &item.key {
        walk_expr_at(key, visitor, &path.field("key"));
    }
    walk_expr_at(&item.value, visitor, &path.field("value"));
}

/// Walks type children for a table property.
fn walk_table_prop<'ast, V: Visitor<'ast> + ?Sized>(
    prop: &'ast TableProp,
    visitor: &mut V,
    path: &NodePath,
) {
    walk_type_at(&prop.prop_type, visitor, &path.field("propType"));
}

/// Walks type children for a table indexer.
fn walk_table_indexer<'ast, V: Visitor<'ast> + ?Sized>(
    indexer: &'ast TableIndexer,
    visitor: &mut V,
    path: &NodePath,
) {
    walk_type_at(&indexer.index_type, visitor, &path.field("indexType"));
    walk_type_at(&indexer.result_type, visitor, &path.field("resultType"));
}

/// Walks type annotation children for a local.
fn walk_local<'ast, V: Visitor<'ast> + ?Sized>(
    local: &'ast Local,
    visitor: &mut V,
    path: &NodePath,
) {
    if visitor.visit_local(path, local) == WalkControl::SkipChildren {
        return;
    }

    if let Some(luau_type) = &local.luau_type {
        walk_type_at(luau_type, visitor, &path.field("luauType"));
    }
}

/// Walks default type children for generic type parameters.
fn walk_generic_types<'ast, V: Visitor<'ast> + ?Sized>(
    generics: &'ast [GenericType],
    visitor: &mut V,
    path: &NodePath,
) {
    for (index, generic) in generics.iter().enumerate() {
        if let Some(luau_type) = &generic.luau_type {
            walk_type_at(luau_type, visitor, &path.index(index).field("luauType"));
        }
    }
}

/// Walks default type-pack children for generic type-pack parameters.
fn walk_generic_type_packs<'ast, V: Visitor<'ast> + ?Sized>(
    generic_packs: &'ast [GenericTypePack],
    visitor: &mut V,
    path: &NodePath,
) {
    for (index, generic) in generic_packs.iter().enumerate() {
        if let Some(luau_type) = &generic.luau_type {
            walk_type_pack_at(luau_type, visitor, &path.index(index).field("luauType"));
        }
    }
}

/// Walks child types and tail pack for a type list.
fn walk_type_list<'ast, V: Visitor<'ast> + ?Sized>(
    type_list: &'ast TypeList,
    visitor: &mut V,
    path: &NodePath,
) {
    walk_types(&type_list.types, visitor, &path.field("types"));
    if let Some(tail_type) = &type_list.tail_type {
        walk_type_pack_at(tail_type, visitor, &path.field("tailType"));
    }
}

/// Walks type-reference parameters.
fn walk_type_parameters<'ast, V: Visitor<'ast> + ?Sized>(
    parameters: &'ast [TypeParameter],
    visitor: &mut V,
    path: &NodePath,
) {
    for (index, parameter) in parameters.iter().enumerate() {
        let path = path.index(index);
        match parameter {
            TypeParameter::Type(luau_type) => walk_type_at(luau_type, visitor, &path),
            TypeParameter::Pack(type_pack) => walk_type_pack_at(type_pack, visitor, &path),
        }
    }
}

/// Walks a repeated statement field.
fn walk_stats<'ast, V: Visitor<'ast> + ?Sized>(
    stats: &'ast [Stat],
    visitor: &mut V,
    path: &NodePath,
) {
    for (index, stat) in stats.iter().enumerate() {
        walk_stat_at(stat, visitor, &path.index(index));
    }
}

/// Walks a repeated expression field.
fn walk_exprs<'ast, V: Visitor<'ast> + ?Sized>(
    exprs: &'ast [Expr],
    visitor: &mut V,
    path: &NodePath,
) {
    for (index, expr) in exprs.iter().enumerate() {
        walk_expr_at(expr, visitor, &path.index(index));
    }
}

/// Walks a repeated type field.
fn walk_types<'ast, V: Visitor<'ast> + ?Sized>(
    types: &'ast [Type],
    visitor: &mut V,
    path: &NodePath,
) {
    for (index, luau_type) in types.iter().enumerate() {
        walk_type_at(luau_type, visitor, &path.index(index));
    }
}

/// Walks a repeated local field.
fn walk_locals<'ast, V: Visitor<'ast> + ?Sized>(
    locals: &'ast [Local],
    visitor: &mut V,
    path: &NodePath,
) {
    for (index, local) in locals.iter().enumerate() {
        walk_local(local, visitor, &path.index(index));
    }
}

fn find_node_in_stat<'a>(
    stat: &'a Stat,
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    if let Some(location) = stat.location() {
        consider_node(NodeRef::Stat(stat), location, position, document_end, best);
    }

    match stat {
        Stat::Block { body, .. } => {
            for child in body {
                find_node_in_stat(child, position, document_end, best);
            }
        }
        Stat::Return { list, .. } => find_node_in_exprs(list, position, document_end, best),
        Stat::Expr { expr, .. } => find_node_in_expr(expr, position, document_end, best),
        Stat::Local { vars, values, .. } => {
            find_node_in_locals(vars, position, document_end, best);
            find_node_in_exprs(values, position, document_end, best);
        }
        Stat::Assign { vars, values, .. } => {
            find_node_in_exprs(vars, position, document_end, best);
            find_node_in_exprs(values, position, document_end, best);
        }
        Stat::CompoundAssign { var, value, .. } => {
            find_node_in_expr(var, position, document_end, best);
            find_node_in_expr(value, position, document_end, best);
        }
        Stat::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            find_node_in_expr(condition, position, document_end, best);
            find_node_in_stat(then_body, position, document_end, best);
            if let Some(else_body) = else_body {
                find_node_in_stat(else_body, position, document_end, best);
            }
        }
        Stat::While {
            condition, body, ..
        } => {
            find_node_in_expr(condition, position, document_end, best);
            find_node_in_stat(body, position, document_end, best);
        }
        Stat::Repeat {
            condition, body, ..
        } => {
            find_node_in_stat(body, position, document_end, best);
            find_node_in_expr(condition, position, document_end, best);
        }
        Stat::For {
            var,
            from,
            to,
            step,
            body,
            ..
        } => {
            find_node_in_local(var, position, document_end, best);
            find_node_in_expr(from, position, document_end, best);
            find_node_in_expr(to, position, document_end, best);
            if let Some(step) = step {
                find_node_in_expr(step, position, document_end, best);
            }
            find_node_in_stat(body, position, document_end, best);
        }
        Stat::ForIn {
            vars, values, body, ..
        } => {
            find_node_in_locals(vars, position, document_end, best);
            find_node_in_exprs(values, position, document_end, best);
            find_node_in_stat(body, position, document_end, best);
        }
        Stat::Function { name, func, .. } => {
            find_node_in_expr(name, position, document_end, best);
            find_node_in_expr(func, position, document_end, best);
        }
        Stat::LocalFunction { name, func, .. } => {
            find_node_in_local(name, position, document_end, best);
            find_node_in_expr(func, position, document_end, best);
        }
        Stat::DeclareGlobal { luau_type, .. } => {
            find_node_in_type(luau_type, position, document_end, best);
        }
        Stat::DeclareFunction {
            generics,
            generic_packs,
            params,
            ret_types,
            ..
        } => {
            find_node_in_generic_types(generics, position, document_end, best);
            find_node_in_generic_type_packs(generic_packs, position, document_end, best);
            find_node_in_type_list(params, position, document_end, best);
            find_node_in_type_pack(ret_types, position, document_end, best);
        }
        Stat::DeclareClass { props, indexer, .. } => {
            for prop in props {
                find_node_in_type(&prop.luau_type, position, document_end, best);
            }
            if let Some(indexer) = indexer {
                find_node_in_table_indexer(indexer, position, document_end, best);
            }
        }
        Stat::TypeAlias {
            generics,
            generic_packs,
            value,
            ..
        } => {
            find_node_in_generic_types(generics, position, document_end, best);
            find_node_in_generic_type_packs(generic_packs, position, document_end, best);
            find_node_in_type(value, position, document_end, best);
        }
        Stat::TypeFunction { func, .. } => {
            find_node_in_expr(func, position, document_end, best);
        }
        Stat::Class { members, .. } => {
            for member in members {
                find_node_in_stat(member, position, document_end, best);
            }
        }
        Stat::ClassProperty {
            luau_type: Some(luau_type),
            ..
        } => {
            find_node_in_type(luau_type, position, document_end, best);
        }
        Stat::ClassProperty {
            luau_type: None, ..
        } => {}
        Stat::Error {
            expressions,
            statements,
            ..
        } => {
            find_node_in_exprs(expressions, position, document_end, best);
            for statement in statements {
                find_node_in_stat(statement, position, document_end, best);
            }
        }
        Stat::Break { .. } | Stat::Continue { .. } => {}
    }
}

fn find_node_in_expr<'a>(
    expr: &'a Expr,
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    if let Some(location) = expr.location() {
        consider_node(NodeRef::Expr(expr), location, position, document_end, best);
    }

    match expr {
        Expr::Call {
            func,
            type_arguments,
            args,
            ..
        } => {
            find_node_in_expr(func, position, document_end, best);
            find_node_in_type_parameters(type_arguments, position, document_end, best);
            find_node_in_exprs(args, position, document_end, best);
        }
        Expr::Binary { left, right, .. } => {
            find_node_in_expr(left, position, document_end, best);
            find_node_in_expr(right, position, document_end, best);
        }
        Expr::Unary { expr, .. } | Expr::Group { expr, .. } => {
            find_node_in_expr(expr, position, document_end, best);
        }
        Expr::IfElse {
            condition,
            true_expr,
            false_expr,
            ..
        } => {
            find_node_in_expr(condition, position, document_end, best);
            find_node_in_expr(true_expr, position, document_end, best);
            find_node_in_expr(false_expr, position, document_end, best);
        }
        Expr::TypeAssertion {
            expr, annotation, ..
        } => {
            find_node_in_expr(expr, position, document_end, best);
            find_node_in_type(annotation, position, document_end, best);
        }
        Expr::IndexName { expr, .. } => find_node_in_expr(expr, position, document_end, best),
        Expr::IndexExpr { expr, index, .. } => {
            find_node_in_expr(expr, position, document_end, best);
            find_node_in_expr(index, position, document_end, best);
        }
        Expr::Table { items, .. } => {
            for item in items {
                if let Some(key) = &item.key {
                    find_node_in_expr(key, position, document_end, best);
                }
                find_node_in_expr(&item.value, position, document_end, best);
            }
        }
        Expr::InterpString { expressions, .. } => {
            find_node_in_exprs(expressions, position, document_end, best);
        }
        Expr::Function {
            generics,
            generic_packs,
            args,
            self_arg,
            vararg_annotation,
            return_annotation,
            body,
            ..
        } => {
            find_node_in_generic_types(generics, position, document_end, best);
            find_node_in_generic_type_packs(generic_packs, position, document_end, best);
            find_node_in_locals(args, position, document_end, best);
            if let Some(self_arg) = self_arg {
                find_node_in_local(self_arg, position, document_end, best);
            }
            if let Some(vararg_annotation) = vararg_annotation {
                find_node_in_type_pack(vararg_annotation, position, document_end, best);
            }
            if let Some(return_annotation) = return_annotation {
                find_node_in_type_pack(return_annotation, position, document_end, best);
            }
            find_node_in_stat(body, position, document_end, best);
        }
        Expr::Instantiate {
            expr,
            type_arguments,
            ..
        } => {
            find_node_in_expr(expr, position, document_end, best);
            find_node_in_type_parameters(type_arguments, position, document_end, best);
        }
        Expr::Error { expressions, .. } => {
            find_node_in_exprs(expressions, position, document_end, best);
        }
        Expr::Nil { .. }
        | Expr::Bool { .. }
        | Expr::Number { .. }
        | Expr::Integer { .. }
        | Expr::String { .. }
        | Expr::Global { .. }
        | Expr::Local { .. }
        | Expr::Varargs { .. } => {}
    }
}

fn find_node_in_type<'a>(
    luau_type: &'a Type,
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    if let Some(location) = luau_type.location() {
        consider_node(
            NodeRef::Type(luau_type),
            location,
            position,
            document_end,
            best,
        );
    }

    match luau_type {
        Type::Reference { parameters, .. } => {
            find_node_in_type_parameters(parameters, position, document_end, best);
        }
        Type::Typeof { expr, .. } => find_node_in_expr(expr, position, document_end, best),
        Type::Group { inner, .. } => find_node_in_type(inner, position, document_end, best),
        Type::Union { types, .. } | Type::Intersection { types, .. } => {
            find_node_in_types(types, position, document_end, best);
        }
        Type::Function {
            generics,
            generic_packs,
            arg_types,
            return_types,
            ..
        } => {
            find_node_in_generic_types(generics, position, document_end, best);
            find_node_in_generic_type_packs(generic_packs, position, document_end, best);
            find_node_in_type_list(arg_types, position, document_end, best);
            find_node_in_type_pack(return_types, position, document_end, best);
        }
        Type::Table { props, indexer, .. } => {
            for prop in props {
                find_node_in_type(&prop.prop_type, position, document_end, best);
            }
            if let Some(indexer) = indexer {
                find_node_in_table_indexer(indexer, position, document_end, best);
            }
        }
        Type::Error { types, .. } => find_node_in_types(types, position, document_end, best),
        Type::Optional { .. } | Type::SingletonString { .. } | Type::SingletonBool { .. } => {}
    }
}

fn find_node_in_type_pack<'a>(
    type_pack: &'a TypePack,
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    if let Some(location) = type_pack.location() {
        consider_node(
            NodeRef::TypePack(type_pack),
            location,
            position,
            document_end,
            best,
        );
    }

    match type_pack {
        TypePack::Explicit { type_list, .. } => {
            find_node_in_type_list(type_list, position, document_end, best);
        }
        TypePack::Variadic { variadic_type, .. } => {
            find_node_in_type(variadic_type, position, document_end, best);
        }
        TypePack::Generic { .. } => {}
    }
}

fn find_node_in_type_list<'a>(
    type_list: &'a TypeList,
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    find_node_in_types(&type_list.types, position, document_end, best);
    if let Some(tail_type) = &type_list.tail_type {
        find_node_in_type_pack(tail_type, position, document_end, best);
    }
}

fn find_node_in_type_parameters<'a>(
    parameters: &'a [TypeParameter],
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    for parameter in parameters {
        match parameter {
            TypeParameter::Type(luau_type) => {
                find_node_in_type(luau_type, position, document_end, best);
            }
            TypeParameter::Pack(type_pack) => {
                find_node_in_type_pack(type_pack, position, document_end, best);
            }
        }
    }
}

fn find_node_in_local<'a>(
    local: &'a Local,
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    if let Some(luau_type) = &local.luau_type {
        find_node_in_type(luau_type, position, document_end, best);
    }
}

fn find_node_in_generic_types<'a>(
    generics: &'a [GenericType],
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    for generic in generics {
        if let Some(luau_type) = &generic.luau_type {
            find_node_in_type(luau_type, position, document_end, best);
        }
    }
}

fn find_node_in_generic_type_packs<'a>(
    generic_packs: &'a [GenericTypePack],
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    for generic in generic_packs {
        if let Some(luau_type) = &generic.luau_type {
            find_node_in_type_pack(luau_type, position, document_end, best);
        }
    }
}

fn find_node_in_table_indexer<'a>(
    indexer: &'a TableIndexer,
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    find_node_in_type(&indexer.index_type, position, document_end, best);
    find_node_in_type(&indexer.result_type, position, document_end, best);
}

fn find_node_in_exprs<'a>(
    exprs: &'a [Expr],
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    for expr in exprs {
        find_node_in_expr(expr, position, document_end, best);
    }
}

fn find_node_in_types<'a>(
    types: &'a [Type],
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    for luau_type in types {
        find_node_in_type(luau_type, position, document_end, best);
    }
}

fn find_node_in_locals<'a>(
    locals: &'a [Local],
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    for local in locals {
        find_node_in_local(local, position, document_end, best);
    }
}

fn consider_node<'a>(
    node: NodeRef<'a>,
    location: Location,
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    if location.contains(position) || location.end == document_end && position >= document_end {
        *best = Some(node);
    }
}

#[cfg(any())]
mod tests;
