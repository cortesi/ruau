//! Borrowed AST traversal helpers.

use crate::{
    Location, Position,
    syntax::{
        DeclaredClassProp, Expr, GenericType, GenericTypePack, Local, Stat, TableIndexer,
        TableItem, TableProp, Type, TypeList, TypePack, TypeParameter,
    },
};

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
    fn visit_stat(&mut self, _stat: &'ast Stat) -> WalkControl {
        WalkControl::Continue
    }

    /// Visits a local declaration.
    fn visit_local(&mut self, _local: &'ast Local) -> WalkControl {
        WalkControl::Continue
    }

    /// Visits an expression.
    fn visit_expr(&mut self, _expr: &'ast Expr) -> WalkControl {
        WalkControl::Continue
    }

    /// Visits a type.
    fn visit_type(&mut self, _luau_type: &'ast Type) -> WalkControl {
        WalkControl::Continue
    }

    /// Visits a type pack.
    fn visit_type_pack(&mut self, _type_pack: &'ast TypePack) -> WalkControl {
        WalkControl::Continue
    }
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

/// Walks a statement tree.
pub fn walk_stat<'ast, V: Visitor<'ast> + ?Sized>(stat: &'ast Stat, visitor: &mut V) {
    if visitor.visit_stat(stat) == WalkControl::SkipChildren {
        return;
    }

    match stat {
        Stat::Block { body, .. } => walk_stats(body, visitor),
        Stat::Return { list, .. } => walk_exprs(list, visitor),
        Stat::Expr { expr, .. } => walk_expr(expr, visitor),
        Stat::Local { vars, values, .. } => {
            walk_locals(vars, visitor);
            walk_exprs(values, visitor);
        }
        Stat::Assign { vars, values, .. } => {
            walk_exprs(vars, visitor);
            walk_exprs(values, visitor);
        }
        Stat::CompoundAssign { var, value, .. } => {
            walk_expr(var, visitor);
            walk_expr(value, visitor);
        }
        Stat::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            walk_expr(condition, visitor);
            walk_stat(then_body, visitor);
            if let Some(else_body) = else_body {
                walk_stat(else_body, visitor);
            }
        }
        Stat::While {
            condition, body, ..
        } => {
            walk_expr(condition, visitor);
            walk_stat(body, visitor);
        }
        Stat::Repeat {
            condition, body, ..
        } => {
            walk_stat(body, visitor);
            walk_expr(condition, visitor);
        }
        Stat::For {
            var,
            from,
            to,
            step,
            body,
            ..
        } => {
            walk_local(var, visitor);
            walk_expr(from, visitor);
            walk_expr(to, visitor);
            if let Some(step) = step {
                walk_expr(step, visitor);
            }
            walk_stat(body, visitor);
        }
        Stat::ForIn {
            vars, values, body, ..
        } => {
            walk_locals(vars, visitor);
            walk_exprs(values, visitor);
            walk_stat(body, visitor);
        }
        Stat::Function { name, func, .. } => {
            walk_expr(name, visitor);
            walk_expr(func, visitor);
        }
        Stat::LocalFunction { name, func, .. } => {
            walk_local(name, visitor);
            walk_expr(func, visitor);
        }
        Stat::DeclareGlobal { declared_type, .. } => {
            walk_type(declared_type, visitor);
        }
        Stat::DeclareFunction {
            generics,
            generic_packs,
            params,
            ret_types,
            ..
        } => {
            walk_generic_types(generics, visitor);
            walk_generic_type_packs(generic_packs, visitor);
            walk_type_list(params, visitor);
            walk_type_pack(ret_types, visitor);
        }
        Stat::DeclareClass { props, indexer, .. } => {
            for prop in props {
                walk_declared_class_prop(prop, visitor);
            }
            if let Some(indexer) = indexer {
                walk_table_indexer(indexer, visitor);
            }
        }
        Stat::TypeAlias {
            generics,
            generic_packs,
            value,
            ..
        } => {
            walk_generic_types(generics, visitor);
            walk_generic_type_packs(generic_packs, visitor);
            walk_type(value, visitor);
        }
        Stat::TypeFunction { func, .. } => walk_expr(func, visitor),
        Stat::Class {
            class_local,
            super_class,
            members,
            ..
        } => {
            if let Some(class_local) = class_local {
                walk_local(class_local, visitor);
            }
            if let Some(super_class) = super_class {
                walk_expr(super_class, visitor);
            }
            walk_stats(members, visitor);
        }
        Stat::ClassProperty {
            declared_type: Some(declared_type),
            ..
        } => {
            walk_type(declared_type, visitor);
        }
        Stat::ClassProperty {
            declared_type: None,
            ..
        } => {}
        Stat::Error {
            expressions,
            statements,
            ..
        } => {
            walk_exprs(expressions, visitor);
            walk_stats(statements, visitor);
        }
        Stat::Break { .. } | Stat::Continue { .. } => {}
    }
}

/// Walks an expression tree.
pub fn walk_expr<'ast, V: Visitor<'ast> + ?Sized>(expr: &'ast Expr, visitor: &mut V) {
    if visitor.visit_expr(expr) == WalkControl::SkipChildren {
        return;
    }

    match expr {
        Expr::Call {
            func,
            type_arguments,
            args,
            ..
        } => {
            walk_expr(func, visitor);
            walk_type_parameters(type_arguments, visitor);
            walk_exprs(args, visitor);
        }
        Expr::Binary { left, right, .. } => {
            walk_expr(left, visitor);
            walk_expr(right, visitor);
        }
        Expr::Unary { expr, .. } | Expr::Group { expr, .. } => {
            walk_expr(expr, visitor);
        }
        Expr::IfElse {
            condition,
            true_expr,
            false_expr,
            ..
        } => {
            walk_expr(condition, visitor);
            walk_expr(true_expr, visitor);
            walk_expr(false_expr, visitor);
        }
        Expr::TypeAssertion {
            expr, annotation, ..
        } => {
            walk_expr(expr, visitor);
            walk_type(annotation, visitor);
        }
        Expr::IndexName { expr, .. } => walk_expr(expr, visitor),
        Expr::IndexExpr { expr, index, .. } => {
            walk_expr(expr, visitor);
            walk_expr(index, visitor);
        }
        Expr::Table { items, .. } => {
            for item in items {
                walk_table_item(item, visitor);
            }
        }
        Expr::InterpString { expressions, .. } => {
            walk_exprs(expressions, visitor);
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
            walk_generic_types(generics, visitor);
            walk_generic_type_packs(generic_packs, visitor);
            walk_locals(args, visitor);
            if let Some(self_arg) = self_arg {
                walk_local(self_arg, visitor);
            }
            if let Some(vararg_annotation) = vararg_annotation {
                walk_type_pack(vararg_annotation, visitor);
            }
            if let Some(return_annotation) = return_annotation {
                walk_type_pack(return_annotation, visitor);
            }
            walk_stat(body, visitor);
        }
        Expr::Instantiate {
            expr,
            type_arguments,
            ..
        } => {
            walk_expr(expr, visitor);
            walk_type_parameters(type_arguments, visitor);
        }
        Expr::Error { expressions, .. } => {
            walk_exprs(expressions, visitor);
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

/// Walks a type tree.
pub fn walk_type<'ast, V: Visitor<'ast> + ?Sized>(luau_type: &'ast Type, visitor: &mut V) {
    if visitor.visit_type(luau_type) == WalkControl::SkipChildren {
        return;
    }

    match luau_type {
        Type::Reference { parameters, .. } => {
            walk_type_parameters(parameters, visitor);
        }
        Type::Typeof { expr, .. } => walk_expr(expr, visitor),
        Type::Group { inner, .. } => walk_type(inner, visitor),
        Type::Union { types, .. } | Type::Intersection { types, .. } => {
            walk_types(types, visitor);
        }
        Type::Function {
            generics,
            generic_packs,
            arg_types,
            return_types,
            ..
        } => {
            walk_generic_types(generics, visitor);
            walk_generic_type_packs(generic_packs, visitor);
            walk_type_list(arg_types, visitor);
            walk_type_pack(return_types, visitor);
        }
        Type::Table { props, indexer, .. } => {
            for prop in props {
                walk_table_prop(prop, visitor);
            }
            if let Some(indexer) = indexer {
                walk_table_indexer(indexer, visitor);
            }
        }
        Type::Error { types, .. } => walk_types(types, visitor),
        Type::Optional { .. } | Type::SingletonString { .. } | Type::SingletonBool { .. } => {}
    }
}

/// Walks a type-pack tree.
pub fn walk_type_pack<'ast, V: Visitor<'ast> + ?Sized>(type_pack: &'ast TypePack, visitor: &mut V) {
    if visitor.visit_type_pack(type_pack) == WalkControl::SkipChildren {
        return;
    }

    match type_pack {
        TypePack::Explicit { type_list, .. } => {
            walk_type_list(type_list, visitor);
        }
        TypePack::Variadic { variadic_type, .. } => {
            walk_type(variadic_type, visitor);
        }
        TypePack::Generic { .. } => {}
    }
}

/// Walks type children for a declared class property.
fn walk_declared_class_prop<'ast, V: Visitor<'ast> + ?Sized>(
    prop: &'ast DeclaredClassProp,
    visitor: &mut V,
) {
    walk_type(&prop.declared_type, visitor);
}

/// Walks expression children for a table item.
fn walk_table_item<'ast, V: Visitor<'ast> + ?Sized>(item: &'ast TableItem, visitor: &mut V) {
    if let Some(key) = &item.key {
        walk_expr(key, visitor);
    }
    walk_expr(&item.value, visitor);
}

/// Walks type children for a table property.
fn walk_table_prop<'ast, V: Visitor<'ast> + ?Sized>(prop: &'ast TableProp, visitor: &mut V) {
    walk_type(&prop.prop_type, visitor);
}

/// Walks type children for a table indexer.
fn walk_table_indexer<'ast, V: Visitor<'ast> + ?Sized>(
    indexer: &'ast TableIndexer,
    visitor: &mut V,
) {
    walk_type(&indexer.index_type, visitor);
    walk_type(&indexer.result_type, visitor);
}

/// Walks type annotation children for a local.
fn walk_local<'ast, V: Visitor<'ast> + ?Sized>(local: &'ast Local, visitor: &mut V) {
    if visitor.visit_local(local) == WalkControl::SkipChildren {
        return;
    }

    if let Some(annotation) = &local.annotation {
        walk_type(annotation, visitor);
    }
}

/// Walks default type children for generic type parameters.
fn walk_generic_types<'ast, V: Visitor<'ast> + ?Sized>(
    generics: &'ast [GenericType],
    visitor: &mut V,
) {
    for generic in generics {
        if let Some(default_type) = &generic.default_type {
            walk_type(default_type, visitor);
        }
    }
}

/// Walks default type-pack children for generic type-pack parameters.
fn walk_generic_type_packs<'ast, V: Visitor<'ast> + ?Sized>(
    generic_packs: &'ast [GenericTypePack],
    visitor: &mut V,
) {
    for generic in generic_packs {
        if let Some(default_type) = &generic.default_type {
            walk_type_pack(default_type, visitor);
        }
    }
}

/// Walks child types and tail pack for a type list.
fn walk_type_list<'ast, V: Visitor<'ast> + ?Sized>(type_list: &'ast TypeList, visitor: &mut V) {
    walk_types(&type_list.types, visitor);
    if let Some(tail_type) = &type_list.tail_type {
        walk_type_pack(tail_type, visitor);
    }
}

/// Walks type-reference parameters.
fn walk_type_parameters<'ast, V: Visitor<'ast> + ?Sized>(
    parameters: &'ast [TypeParameter],
    visitor: &mut V,
) {
    for parameter in parameters {
        match parameter {
            TypeParameter::Type(luau_type) => walk_type(luau_type, visitor),
            TypeParameter::Pack(type_pack) => walk_type_pack(type_pack, visitor),
        }
    }
}

/// Walks a repeated statement field.
fn walk_stats<'ast, V: Visitor<'ast> + ?Sized>(stats: &'ast [Stat], visitor: &mut V) {
    for stat in stats {
        walk_stat(stat, visitor);
    }
}

/// Walks a repeated expression field.
fn walk_exprs<'ast, V: Visitor<'ast> + ?Sized>(exprs: &'ast [Expr], visitor: &mut V) {
    for expr in exprs {
        walk_expr(expr, visitor);
    }
}

/// Walks a repeated type field.
fn walk_types<'ast, V: Visitor<'ast> + ?Sized>(types: &'ast [Type], visitor: &mut V) {
    for luau_type in types {
        walk_type(luau_type, visitor);
    }
}

/// Walks a repeated local field.
fn walk_locals<'ast, V: Visitor<'ast> + ?Sized>(locals: &'ast [Local], visitor: &mut V) {
    for local in locals {
        walk_local(local, visitor);
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
        Stat::DeclareGlobal { declared_type, .. } => {
            find_node_in_type(declared_type, position, document_end, best);
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
                find_node_in_type(&prop.declared_type, position, document_end, best);
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
        Stat::Class {
            super_class,
            members,
            ..
        } => {
            if let Some(super_class) = super_class {
                find_node_in_expr(super_class, position, document_end, best);
            }
            for member in members {
                find_node_in_stat(member, position, document_end, best);
            }
        }
        Stat::ClassProperty {
            declared_type: Some(declared_type),
            ..
        } => {
            find_node_in_type(declared_type, position, document_end, best);
        }
        Stat::ClassProperty {
            declared_type: None,
            ..
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
    if let Some(annotation) = &local.annotation {
        find_node_in_type(annotation, position, document_end, best);
    }
}

fn find_node_in_generic_types<'a>(
    generics: &'a [GenericType],
    position: Position,
    document_end: Position,
    best: &mut Option<NodeRef<'a>>,
) {
    for generic in generics {
        if let Some(default_type) = &generic.default_type {
            find_node_in_type(default_type, position, document_end, best);
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
        if let Some(default_type) = &generic.default_type {
            find_node_in_type_pack(default_type, position, document_end, best);
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
