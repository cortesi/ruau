//! Checked AST and module query data.

use std::collections::BTreeMap;

use ruau_ast::{
    Location, Position,
    syntax::{Expr, Local, LocalId, Stat, SyntaxId},
    visit::{NodePath, Visitor, WalkControl, find_node_at_position, walk_stat},
};

use crate::{
    builtins::string_primitive_documentation_symbol,
    checker::CheckedModule,
    diagnostics::DiagnosticLocation,
    overload::resolve_call_for_constraint,
    scopes::Symbol,
    types::{Arena, PrimitiveType, SingletonType, TypeId, TypeKind, TypePackId},
};

/// Source binding returned by checked-module lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Binding {
    /// Source-visible binding name.
    pub name: String,
    /// Inferred or builtin binding type, when available.
    pub ty: Option<TypeId>,
    /// Source range of the declaration. Builtin/global bindings use
    /// upstream's zero-width root sentinel.
    pub declaration_location: Option<Location>,
    /// Documentation symbol associated with this binding, when known.
    pub documentation_symbol: Option<String>,
}

/// Actual and expected type data retained for source queries.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Queries {
    actual_by_syntax: BTreeMap<SyntaxId, TypeId>,
    expected_by_syntax: BTreeMap<SyntaxId, TypeId>,
    actual_by_location: BTreeMap<DiagnosticLocation, TypeId>,
    expected_by_location: BTreeMap<DiagnosticLocation, TypeId>,
    call_arguments_by_syntax: BTreeMap<SyntaxId, TypePackId>,
    documentation_call_arguments_by_syntax: BTreeMap<SyntaxId, TypePackId>,
}

impl Queries {
    /// Records an actual type for an expression syntax id and optional range.
    pub(crate) fn record_actual(
        &mut self,
        syntax_id: SyntaxId,
        location: Option<DiagnosticLocation>,
        ty: TypeId,
    ) {
        self.actual_by_syntax.insert(syntax_id, ty);
        if let Some(location) = location {
            self.actual_by_location.insert(location, ty);
        }
    }

    /// Records an expected type for an expression syntax id and optional range.
    pub(crate) fn record_expected(
        &mut self,
        syntax_id: SyntaxId,
        location: Option<DiagnosticLocation>,
        ty: TypeId,
    ) {
        self.expected_by_syntax.insert(syntax_id, ty);
        if let Some(location) = location {
            self.expected_by_location.insert(location, ty);
        }
    }

    /// Records an expected type for a source range that does not have an
    /// expression syntax id, such as a local annotation.
    pub(crate) fn record_expected_location(&mut self, location: DiagnosticLocation, ty: TypeId) {
        self.expected_by_location.insert(location, ty);
    }

    /// Records the concrete argument pack used for a call expression.
    pub(crate) fn record_call_arguments(
        &mut self,
        syntax_id: SyntaxId,
        arguments: TypePackId,
        documentation_arguments: TypePackId,
    ) {
        self.call_arguments_by_syntax.insert(syntax_id, arguments);
        self.documentation_call_arguments_by_syntax
            .insert(syntax_id, documentation_arguments);
    }

    /// Looks up an actual type by syntax id.
    #[must_use]
    pub fn actual_by_syntax(&self, syntax_id: SyntaxId) -> Option<TypeId> {
        self.actual_by_syntax.get(&syntax_id).copied()
    }

    /// Looks up an expected type by syntax id.
    #[must_use]
    pub fn expected_by_syntax(&self, syntax_id: SyntaxId) -> Option<TypeId> {
        self.expected_by_syntax.get(&syntax_id).copied()
    }

    /// Looks up an actual type by source range.
    #[must_use]
    pub fn actual_by_location(&self, location: DiagnosticLocation) -> Option<TypeId> {
        self.actual_by_location.get(&location).copied()
    }

    /// Looks up an expected type by source range.
    #[must_use]
    pub fn expected_by_location(&self, location: DiagnosticLocation) -> Option<TypeId> {
        self.expected_by_location.get(&location).copied()
    }

    /// Looks up a call's argument pack by syntax id.
    #[must_use]
    pub fn call_arguments_by_syntax(&self, syntax_id: SyntaxId) -> Option<TypePackId> {
        self.call_arguments_by_syntax.get(&syntax_id).copied()
    }

    /// Looks up the argument pack used for documentation overload selection.
    #[must_use]
    pub fn documentation_call_arguments_by_syntax(
        &self,
        syntax_id: SyntaxId,
    ) -> Option<TypePackId> {
        self.documentation_call_arguments_by_syntax
            .get(&syntax_id)
            .copied()
    }

    /// Returns true when no query data has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actual_by_syntax.is_empty()
            && self.expected_by_syntax.is_empty()
            && self.actual_by_location.is_empty()
            && self.expected_by_location.is_empty()
            && self.call_arguments_by_syntax.is_empty()
            && self.documentation_call_arguments_by_syntax.is_empty()
    }

    /// Number of syntax-id-keyed expected type entries.
    #[must_use]
    #[cfg(any())]
    pub fn expected_syntax_len(&self) -> usize {
        self.expected_by_syntax.len()
    }

    /// Number of syntax-id-keyed actual type entries.
    #[must_use]
    pub fn actual_syntax_len(&self) -> usize {
        self.actual_by_syntax.len()
    }
}

/// Finds the source binding referenced or declared at `position`.
#[must_use]
pub fn find_binding_at_position(module: &CheckedModule, position: Position) -> Option<Binding> {
    match find_expr_or_local_at_position(module.root(), position)? {
        BindingTarget::Global { name, .. } => {
            let binding = module
                .scopes()
                .lookup_global(module.scopes().root(), &name)?;
            Some(Binding {
                name: binding.name.clone(),
                ty: binding.ty,
                declaration_location: Some(Location::default()),
                documentation_symbol: binding.documentation_symbol.clone(),
            })
        }
        BindingTarget::Local {
            local,
            declaration_location,
            ..
        } => {
            let binding = module.scopes().lookup_local_id(local)?;
            let ty = binding.ty.or_else(|| {
                module
                    .dfg()
                    .local(local)
                    .map(|def| module.dfg().get(def).ty)
            });
            Some(Binding {
                name: binding.name.clone(),
                ty,
                declaration_location,
                documentation_symbol: binding.documentation_symbol.clone(),
            })
        }
    }
}

/// Resolves the inferred type bound to a source `name`, the way upstream's
/// `requireType("name")` does: the latest-declared local with that name, then a
/// module global. Returns the binding's annotated or inferred type handle into
/// the checker arena, or `None` when no such binding carries a type.
#[must_use]
pub fn type_of_symbol(module: &CheckedModule, name: &str) -> Option<TypeId> {
    if let Some(binding) = module.scopes().lookup_local_by_name(name) {
        if let Symbol::Local(local) = binding.symbol {
            // A mutable unannotated local's declared type widens a bare literal;
            // the by-name query reports that, not the precise singleton the
            // value-flow keeps. Local function queries likewise use a
            // query-only generalized view, leaving the solver's monomorphic
            // value-flow type untouched.
            if let Some(query_ty) = module.query_local_type(local) {
                return Some(query_ty);
            }
        }
        if let Some(ty) = binding.ty {
            return Some(ty);
        }
        if let Symbol::Local(local) = binding.symbol
            && let Some(def) = module.dfg().local(local)
        {
            return Some(module.dfg().get(def).ty);
        }
    }
    // User-defined global functions (`function f() ... end`) and declare
    // globals live in the checker's global-def map, which also carries
    // query-only surface fixes such as generalized function parameters.
    if let Some(ty) = module.global_def(name) {
        return Some(ty);
    }
    if let Some(ty) = module
        .scopes()
        .lookup_global(module.scopes().root(), name)
        .and_then(|binding| binding.ty)
    {
        return Some(ty);
    }
    None
}

/// Resolves the inferred type of the expression covering `position`, the way
/// upstream's `requireTypeAtPosition` does. Resolution is by AST containment
/// (then the expression's recorded actual type), not arithmetic point
/// translation: `position` must already be in the assembled source's
/// coordinates, so callers translate original-snippet positions through the
/// run's source transform first.
#[must_use]
pub fn type_at_position(module: &CheckedModule, position: Position) -> Option<TypeId> {
    let node = find_node_at_position(module.root(), position)?;
    let expr = node.as_expr()?;
    module.queries().actual_by_syntax(expr.syntax_id())
}

/// Finds a documentation symbol at `position`, using checked expression types
/// and binding metadata.
#[must_use]
pub fn find_documentation_symbol_at_position(
    module: &CheckedModule,
    arena: &Arena,
    position: Position,
) -> Option<String> {
    if let Some(symbol) = documentation_symbol_for_callback_parameter(module, arena, position) {
        return Some(symbol);
    }

    let parent_call = find_call_with_function_at_position(module.root(), position);

    if let Some(Expr::IndexName {
        expr,
        index,
        index_location,
        ..
    }) = find_node_at_position(module.root(), position).and_then(|node| node.as_expr())
        && index_location.is_none_or(|location| location.contains(position))
    {
        let base_ty = module.queries().actual_by_syntax(expr.syntax_id())?;
        if let Some(symbol) =
            documentation_symbol_for_property(module, arena, base_ty, index.as_str(), parent_call)
        {
            return Some(symbol);
        }
    }

    let binding = find_binding_at_position(module, position)?;
    let binding_ty = binding.ty.or_else(|| {
        find_node_at_position(module.root(), position)
            .and_then(|node| node.as_expr())
            .and_then(|expr| module.queries().actual_by_syntax(expr.syntax_id()))
    });
    overloaded_documentation_symbol(
        module,
        arena,
        binding_ty,
        parent_call,
        binding.documentation_symbol,
    )
}

fn documentation_symbol_for_property(
    module: &CheckedModule,
    arena: &Arena,
    ty: TypeId,
    property: &str,
    parent_call: Option<SyntaxId>,
) -> Option<String> {
    match arena.get(arena.follow(ty)) {
        TypeKind::Table(table) => {
            let property = table.properties.get(property)?;
            overloaded_documentation_symbol(
                module,
                arena,
                Some(property.ty),
                parent_call,
                property.documentation_symbol.clone(),
            )
        }
        TypeKind::Extern { properties, .. } => {
            let property = properties.get(property)?;
            overloaded_documentation_symbol(
                module,
                arena,
                Some(property.ty),
                parent_call,
                property.documentation_symbol.clone(),
            )
        }
        TypeKind::Primitive(PrimitiveType::String)
        | TypeKind::Singleton(SingletonType::String(_)) => {
            string_primitive_documentation_symbol(property)
        }
        _ => None,
    }
}

fn overloaded_documentation_symbol(
    module: &CheckedModule,
    arena: &Arena,
    ty: Option<TypeId>,
    parent_call: Option<SyntaxId>,
    documentation_symbol: Option<String>,
) -> Option<String> {
    let documentation_symbol = documentation_symbol?;
    let Some(ty) = ty else {
        return Some(documentation_symbol);
    };
    if !matches!(arena.get(arena.follow(ty)), TypeKind::Intersection(_)) {
        return Some(documentation_symbol);
    }
    let Some(syntax_id) = parent_call else {
        return Some(documentation_symbol);
    };
    let Some(selected) =
        resolve_documentation_overload(module, arena, ty, syntax_id, &documentation_symbol)
    else {
        return Some(documentation_symbol);
    };
    let overload =
        class_method_documentation_overload(&documentation_symbol, arena, selected.function)
            .unwrap_or_else(|| arena.summary(selected.function));
    Some(format!("{documentation_symbol}/overload/{overload}",))
}

fn resolve_documentation_overload(
    module: &CheckedModule,
    arena: &Arena,
    ty: TypeId,
    syntax_id: SyntaxId,
    documentation_symbol: &str,
) -> Option<crate::overload::OverloadResolution> {
    let queries = module.queries();
    let documentation_arguments = queries
        .documentation_call_arguments_by_syntax(syntax_id)
        .or_else(|| queries.call_arguments_by_syntax(syntax_id))?;
    if let Ok(selected) =
        resolve_call_for_constraint(arena, ty, documentation_arguments, false, false, true)
    {
        return Some(selected);
    }

    if !documentation_symbol.starts_with("@test/globaltype/") {
        return None;
    }
    let call_arguments = queries.call_arguments_by_syntax(syntax_id)?;
    resolve_call_for_constraint(arena, ty, call_arguments, false, false, true).ok()
}

fn class_method_documentation_overload(
    documentation_symbol: &str,
    arena: &Arena,
    function: TypeId,
) -> Option<String> {
    let rest = documentation_symbol.strip_prefix("@test/globaltype/")?;
    let (class_name, _) = rest.split_once('.')?;
    let summary = arena.summary(function);
    let parameters = summary.strip_prefix('(')?;
    let (parameters, returns) = parameters.split_once(')')?;
    let parameters = match arena.get(arena.follow(function)) {
        TypeKind::Function(function) if function.has_self => {
            parameters.split_once(", ").map_or("", |(_, rest)| rest)
        }
        _ => parameters,
    };
    let parameters = if parameters.is_empty() {
        class_name.to_owned()
    } else {
        format!("{class_name}, {parameters}")
    };
    Some(format!("({parameters}){returns}"))
}

fn documentation_symbol_for_callback_parameter(
    module: &CheckedModule,
    arena: &Arena,
    position: Position,
) -> Option<String> {
    let (parent_location, arg_index, param_index) =
        find_call_function_argument_at_position(module.root(), position)?;
    let parent_symbol =
        find_documentation_symbol_at_position(module, arena, parent_location.begin)?;
    Some(format!(
        "{parent_symbol}/param/{arg_index}/param/{param_index}"
    ))
}

fn find_call_with_function_at_position(root: &Stat, position: Position) -> Option<SyntaxId> {
    let mut finder = CallFunctionPositionFinder {
        position,
        result: None,
    };
    walk_stat(root, &mut finder);
    finder.result.map(|(syntax_id, _)| syntax_id)
}

struct CallFunctionPositionFinder {
    position: Position,
    result: Option<(SyntaxId, Location)>,
}

impl Visitor<'_> for CallFunctionPositionFinder {
    fn visit_expr(&mut self, _path: &NodePath, expr: &Expr) -> WalkControl {
        let Expr::Call {
            syntax_id, func, ..
        } = expr
        else {
            return WalkControl::Continue;
        };
        let Some(location) = func.location() else {
            return WalkControl::Continue;
        };
        if !location.contains(self.position) {
            return WalkControl::Continue;
        }
        let current = self.result.map(|(_, location)| location);
        if current.is_none_or(|current| current.encloses(location)) {
            self.result = Some((*syntax_id, location));
        }
        WalkControl::Continue
    }
}

fn find_call_function_argument_at_position(
    root: &Stat,
    position: Position,
) -> Option<(Location, usize, usize)> {
    let mut finder = CallFunctionArgumentPositionFinder {
        position,
        result: None,
    };
    walk_stat(root, &mut finder);
    finder
        .result
        .map(|(func_location, arg_index, param_index, _)| (func_location, arg_index, param_index))
}

struct CallFunctionArgumentPositionFinder {
    position: Position,
    result: Option<(Location, usize, usize, Location)>,
}

impl Visitor<'_> for CallFunctionArgumentPositionFinder {
    fn visit_expr(&mut self, _path: &NodePath, expr: &Expr) -> WalkControl {
        let Expr::Call { func, args, .. } = expr else {
            return WalkControl::Continue;
        };
        let Some(func_location) = func.location() else {
            return WalkControl::Continue;
        };
        for (index, arg) in args.iter().enumerate() {
            let Expr::Function {
                location,
                args: params,
                ..
            } = arg
            else {
                continue;
            };
            let Some(function_location) = *location else {
                continue;
            };
            if !function_location.contains(self.position) {
                continue;
            }
            let Some(param_index) = params.iter().position(|param| {
                param
                    .location
                    .is_some_and(|location| location.contains(self.position))
            }) else {
                continue;
            };
            let current = self.result.map(|(_, _, _, location)| location);
            if current.is_none_or(|current| current.encloses(function_location)) {
                self.result = Some((func_location, index, param_index, function_location));
            }
        }
        WalkControl::Continue
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BindingTarget {
    Global {
        name: String,
        location: Option<Location>,
    },
    Local {
        local: LocalId,
        location: Option<Location>,
        declaration_location: Option<Location>,
    },
}

impl BindingTarget {
    fn location(&self) -> Option<Location> {
        match self {
            Self::Global { location, .. } | Self::Local { location, .. } => *location,
        }
    }
}

fn find_expr_or_local_at_position(root: &Stat, position: Position) -> Option<BindingTarget> {
    let mut finder = ExprOrLocalFinder {
        position,
        result: None,
    };
    walk_stat(root, &mut finder);
    finder.result
}

struct ExprOrLocalFinder {
    position: Position,
    result: Option<BindingTarget>,
}

impl ExprOrLocalFinder {
    fn consider(&mut self, candidate: BindingTarget) {
        let Some(location) = candidate.location() else {
            return;
        };
        if !location.contains(self.position) {
            return;
        }

        let current = self.result.as_ref().and_then(BindingTarget::location);
        if current.is_none_or(|current| current.encloses(location)) {
            self.result = Some(candidate);
        }
    }
}

impl Visitor<'_> for ExprOrLocalFinder {
    fn visit_local(&mut self, _path: &NodePath, local: &Local) -> WalkControl {
        self.consider(BindingTarget::Local {
            local: local.id,
            location: local.location,
            declaration_location: local.location,
        });
        WalkControl::Continue
    }

    fn visit_expr(&mut self, _path: &NodePath, expr: &Expr) -> WalkControl {
        match expr {
            Expr::Global { name, location, .. } => self.consider(BindingTarget::Global {
                name: name.as_str().to_owned(),
                location: *location,
            }),
            Expr::Local {
                local, location, ..
            } => self.consider(BindingTarget::Local {
                local: local.id,
                location: *location,
                declaration_location: local.location,
            }),
            _ => {}
        }
        WalkControl::Continue
    }
}

#[cfg(any())]
mod tests;
