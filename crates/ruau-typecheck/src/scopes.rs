//! Scope, symbol, import, and export tracking scaffolding.
//!
//! This remains in `ruau-typecheck` rather than `ruau-analysis` because scope
//! bindings carry source-level type metadata and arena-backed
//! [`crate::types::TypeId`] handles once lowering begins.

use std::collections::BTreeMap;

use ruau_ast::{
    Location,
    syntax::{
        DeclaredClassProp, Expr, GenericType, GenericTypePack, Local, LocalId, Stat, SyntaxId,
        TableIndexer, Type, TypeList, TypePack, TypeParameter,
    },
};

use crate::types::{TableAliasIdentity, TypeId};

/// Stable identity for a checked symbol.
///
/// Luau treats globals as name-based identities and locals as binding-based
/// identities. Ruau mirrors that distinction with parser-assigned [`LocalId`]
/// handles instead of C++ AST node pointers.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Default)]
pub enum Symbol {
    /// No symbol.
    #[default]
    Empty,
    /// A global name, compared by text.
    Global(String),
    /// A local binding, compared by parser identity.
    Local(LocalId),
}

/// Value binding category.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ValueBindingKind {
    /// Ordinary local binding.
    Local,
    /// Function parameter or synthetic self parameter.
    FunctionParameter,
    /// Local function binding.
    Function,
    /// Global binding.
    Global,
    /// Declared global function.
    DeclaredFunction,
    /// Builtin global.
    Builtin,
}

/// Type-level binding category.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TypeBindingKind {
    /// Generic type parameter.
    GenericParameter,
    /// Generic type-pack parameter.
    GenericPackParameter,
    /// Non-exported type alias.
    TypeAlias,
    /// Exported type alias.
    ExportedTypeAlias,
    /// User-defined class/type declaration.
    Class,
    /// Declared external class/type.
    DeclaredClass,
    /// User-defined type function.
    TypeFunction,
    /// Builtin primitive or standard-library type.
    BuiltinType,
}

/// Stable handle for a lexical scope stored in a [`ScopeTree`].
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ScopeId(u32);

impl ScopeId {
    /// Returns the zero-based scope-tree index.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// Returns the raw scope-tree handle value.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }

    fn from_index(index: usize) -> Self {
        let index = u32::try_from(index).expect("scope tree exceeded u32 handle space");
        Self(index)
    }
}

/// A value binding visible through lexical lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValueBinding {
    /// Source-visible name.
    pub name: String,
    /// Symbol identity for this binding.
    pub symbol: Symbol,
    /// Binding category.
    pub kind: ValueBindingKind,
    /// Optional inferred or annotated type.
    pub ty: Option<TypeId>,
    /// Documentation symbol for query consumers.
    pub documentation_symbol: Option<String>,
}

/// A type-level binding visible through type-reference lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeBinding {
    /// Source name of the type binding.
    pub name: String,
    /// Optional nominal display name used when materializing alias results.
    pub display_name: Option<String>,
    /// Source alias definition identity for nominal table alias results.
    pub alias_identity: Option<TableAliasIdentity>,
    /// Binding category.
    pub kind: TypeBindingKind,
    /// Optional type target once aliases/classes are elaborated.
    pub ty: Option<TypeId>,
    /// Alias body for non-elaborated type aliases.
    pub alias: Option<Type>,
    /// Function body for user-defined type functions.
    pub type_function: Option<Expr>,
    /// Optional superclass for declared class or extern type bindings.
    pub class_super_name: Option<String>,
    /// Declared class/extern properties retained for later annotation lowering.
    pub class_props: Vec<DeclaredClassProp>,
    /// Declared class/extern indexer retained for later annotation lowering.
    pub class_indexer: Option<TableIndexer>,
    /// True when the alias body depends on generic parameters or packs.
    pub alias_has_generics: bool,
    /// Ordered generic type parameter names for source aliases.
    pub generic_names: Vec<String>,
    /// Ordered generic type parameter locations for source aliases.
    pub generic_locations: Vec<Option<Location>>,
    /// Ordered generic type parameter defaults for source aliases.
    pub generic_defaults: Vec<Option<Type>>,
    /// Ordered generic type-pack parameter names for source aliases.
    pub generic_pack_names: Vec<String>,
    /// Ordered generic type-pack parameter locations for source aliases.
    pub generic_pack_locations: Vec<Option<Location>>,
    /// Ordered generic type-pack parameter defaults for source aliases.
    pub generic_pack_defaults: Vec<Option<TypePack>>,
    /// True for exported aliases.
    pub exported: bool,
}

impl TypeBinding {
    /// Returns a type binding with name and kind populated and all other fields
    /// at their empty defaults. Callers override the specific fields they need.
    pub(crate) fn empty(name: impl Into<String>, kind: TypeBindingKind) -> Self {
        let name = name.into();
        Self {
            name,
            display_name: None,
            alias_identity: None,
            kind,
            ty: None,
            alias: None,
            type_function: None,
            class_super_name: None,
            class_props: Vec::new(),
            class_indexer: None,
            alias_has_generics: false,
            generic_names: Vec::new(),
            generic_locations: Vec::new(),
            generic_defaults: Vec::new(),
            generic_pack_names: Vec::new(),
            generic_pack_locations: Vec::new(),
            generic_pack_defaults: Vec::new(),
            exported: false,
        }
    }
}

/// One lexical scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scope {
    /// Stable id of this scope.
    pub id: ScopeId,
    /// Parent scope, if any.
    pub parent: Option<ScopeId>,
    /// Child scopes in creation order.
    pub children: Vec<ScopeId>,
    /// Local value bindings keyed by parser local identity.
    pub locals: BTreeMap<LocalId, ValueBinding>,
    /// Global value bindings keyed by name. Normally only populated on root.
    pub globals: BTreeMap<String, ValueBinding>,
    /// Type aliases, generic parameters, and class/type declarations by name.
    pub type_bindings: BTreeMap<String, TypeBinding>,
    /// Expression syntax nodes bound to a resolved symbol.
    pub expression_symbols: BTreeMap<SyntaxId, Symbol>,
    /// Type syntax nodes bound to a resolved type binding name.
    pub type_syntax_bindings: BTreeMap<SyntaxId, String>,
}

impl Scope {
    fn new(id: ScopeId, parent: Option<ScopeId>) -> Self {
        Self {
            id,
            parent,
            children: Vec::new(),
            locals: BTreeMap::new(),
            globals: BTreeMap::new(),
            type_bindings: BTreeMap::new(),
            expression_symbols: BTreeMap::new(),
            type_syntax_bindings: BTreeMap::new(),
        }
    }
}

/// Lexical scope forest for one checked module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopeTree {
    scopes: Vec<Scope>,
    root: ScopeId,
    alias_module: Option<String>,
}

impl ScopeTree {
    /// Creates a scope tree with one root scope.
    #[must_use]
    pub fn new() -> Self {
        Self::new_with_alias_module(None)
    }

    /// Creates a scope tree whose source aliases belong to `alias_module`.
    pub(crate) fn new_with_alias_module(alias_module: Option<String>) -> Self {
        let root = ScopeId::from_index(0);
        Self {
            scopes: vec![Scope::new(root, None)],
            root,
            alias_module,
        }
    }

    /// Returns the root scope.
    #[must_use]
    pub const fn root(&self) -> ScopeId {
        self.root
    }

    /// Returns a scope by id.
    #[must_use]
    pub fn get(&self, id: ScopeId) -> &Scope {
        &self.scopes[id.index()]
    }

    /// Walks the parent chain from `scope` and returns the first ancestor that
    /// owns `local_id` in its locals map, or `None` if no ancestor defines it.
    #[must_use]
    pub fn local_definition_scope(&self, mut scope: ScopeId, local_id: LocalId) -> Option<ScopeId> {
        loop {
            let current = self.get(scope);
            if current.locals.contains_key(&local_id) {
                return Some(scope);
            }
            scope = current.parent?;
        }
    }

    /// Returns true when `scope` is `ancestor`, or is reachable from `ancestor`
    /// via repeated parent-of relationships.
    #[must_use]
    pub fn is_descendant_or_same(&self, mut scope: ScopeId, ancestor: ScopeId) -> bool {
        loop {
            if scope == ancestor {
                return true;
            }
            let Some(parent) = self.get(scope).parent else {
                return false;
            };
            scope = parent;
        }
    }

    /// Creates a child scope.
    pub fn add_child(&mut self, parent: ScopeId) -> ScopeId {
        let id = ScopeId::from_index(self.scopes.len());
        self.scopes.push(Scope::new(id, Some(parent)));
        self.scopes[parent.index()].children.push(id);
        id
    }

    /// Defines a local value with an explicit binding kind.
    pub fn define_local_with_kind(
        &mut self,
        scope: ScopeId,
        local: LocalId,
        name: impl Into<String>,
        kind: ValueBindingKind,
        ty: Option<TypeId>,
    ) -> Symbol {
        let symbol = Symbol::local(local);
        self.scopes[scope.index()].locals.insert(
            local,
            ValueBinding {
                name: name.into(),
                symbol: symbol.clone(),
                kind,
                ty,
                documentation_symbol: None,
            },
        );
        symbol
    }

    /// Defines a global value in a scope.
    pub fn define_global(
        &mut self,
        scope: ScopeId,
        name: impl Into<String>,
        ty: Option<TypeId>,
    ) -> Symbol {
        self.define_global_with_kind(scope, name, ValueBindingKind::Global, ty)
    }

    /// Defines a global value with an explicit binding kind.
    pub fn define_global_with_kind(
        &mut self,
        scope: ScopeId,
        name: impl Into<String>,
        kind: ValueBindingKind,
        ty: Option<TypeId>,
    ) -> Symbol {
        self.define_global_with_documentation(scope, name, kind, ty, None)
    }

    /// Defines a global value with optional documentation metadata.
    pub fn define_global_with_documentation(
        &mut self,
        scope: ScopeId,
        name: impl Into<String>,
        kind: ValueBindingKind,
        ty: Option<TypeId>,
        documentation_symbol: Option<String>,
    ) -> Symbol {
        let name = name.into();
        let symbol = Symbol::global(name.clone());
        self.scopes[scope.index()].globals.insert(
            name.clone(),
            ValueBinding {
                name,
                symbol: symbol.clone(),
                kind,
                ty,
                documentation_symbol,
            },
        );
        symbol
    }

    /// Defines a type-level binding with an explicit binding kind.
    pub fn define_type_with_kind(
        &mut self,
        scope: ScopeId,
        name: impl Into<String>,
        kind: TypeBindingKind,
        ty: Option<TypeId>,
        exported: bool,
    ) {
        let name = name.into();
        self.scopes[scope.index()].type_bindings.insert(
            name.clone(),
            TypeBinding {
                ty,
                exported,
                ..TypeBinding::empty(name, kind)
            },
        );
    }

    /// Defines a source type alias and keeps its body available for checker
    /// stages that lower annotations after scope construction.
    pub fn define_type_alias(
        &mut self,
        scope: ScopeId,
        name: impl Into<String>,
        value: &Type,
        generics: &[GenericType],
        generic_packs: &[GenericTypePack],
        exported: bool,
    ) {
        let name = name.into();
        let kind = if exported {
            TypeBindingKind::ExportedTypeAlias
        } else {
            TypeBindingKind::TypeAlias
        };
        let alias_identity = self.alias_identity(scope, &name);
        self.scopes[scope.index()].type_bindings.insert(
            name.clone(),
            TypeBinding {
                alias_identity: Some(alias_identity),
                alias: Some(value.clone()),
                alias_has_generics: !generics.is_empty() || !generic_packs.is_empty(),
                generic_names: generics
                    .iter()
                    .map(|generic| generic.name.as_str().to_owned())
                    .collect(),
                generic_locations: generics.iter().map(|generic| generic.location).collect(),
                generic_defaults: generics
                    .iter()
                    .map(|generic| generic.default_type.as_deref().cloned())
                    .collect(),
                generic_pack_names: generic_packs
                    .iter()
                    .map(|generic| generic.name.as_str().to_owned())
                    .collect(),
                generic_pack_locations: generic_packs
                    .iter()
                    .map(|generic| generic.location)
                    .collect(),
                generic_pack_defaults: generic_packs
                    .iter()
                    .map(|generic| generic.default_type.as_deref().cloned())
                    .collect(),
                exported,
                ..TypeBinding::empty(name, kind)
            },
        );
    }

    /// Builds the stable identity for a type alias definition in this scope tree.
    #[must_use]
    pub(crate) fn alias_identity(&self, scope: ScopeId, name: &str) -> TableAliasIdentity {
        TableAliasIdentity {
            module: self.alias_module.clone(),
            scope: scope.raw(),
            name: name.to_owned(),
        }
    }

    /// Defines a type binding from an already-collected module export.
    pub fn define_type_binding(&mut self, scope: ScopeId, binding: TypeBinding) {
        self.scopes[scope.index()]
            .type_bindings
            .insert(binding.name.clone(), binding);
    }

    /// Defines a generic type parameter.
    pub fn define_generic_type(&mut self, scope: ScopeId, generic: &GenericType) {
        self.define_type_with_kind(
            scope,
            generic.name.as_str(),
            TypeBindingKind::GenericParameter,
            None,
            false,
        );
    }

    /// Defines a generic type-pack parameter.
    pub fn define_generic_type_pack(&mut self, scope: ScopeId, generic: &GenericTypePack) {
        self.define_type_with_kind(
            scope,
            generic.name.as_str(),
            TypeBindingKind::GenericPackParameter,
            None,
            false,
        );
    }

    /// Defines a class or extern type.
    pub fn define_class(
        &mut self,
        scope: ScopeId,
        name: impl Into<String>,
        declared: bool,
        exported: bool,
    ) {
        let kind = if declared {
            TypeBindingKind::DeclaredClass
        } else {
            TypeBindingKind::Class
        };
        self.define_type_with_kind(scope, name, kind, None, exported);
    }

    /// Defines a declared class or extern type and keeps its source surface
    /// available for checker stages that lower annotations later.
    pub fn define_declared_class(
        &mut self,
        scope: ScopeId,
        name: impl Into<String>,
        super_name: Option<&str>,
        props: &[DeclaredClassProp],
        indexer: Option<&TableIndexer>,
        exported: bool,
    ) {
        let name = name.into();
        self.scopes[scope.index()].type_bindings.insert(
            name.clone(),
            TypeBinding {
                class_super_name: super_name.map(ToOwned::to_owned),
                class_props: props.to_vec(),
                class_indexer: indexer.cloned(),
                exported,
                ..TypeBinding::empty(name, TypeBindingKind::DeclaredClass)
            },
        );
    }

    /// Defines a source user-defined type function and keeps its body
    /// available for local reduction.
    pub fn define_type_function(
        &mut self,
        scope: ScopeId,
        name: impl Into<String>,
        func: &Expr,
        exported: bool,
    ) {
        let name = name.into();
        self.scopes[scope.index()].type_bindings.insert(
            name.clone(),
            TypeBinding {
                type_function: Some(func.clone()),
                exported,
                ..TypeBinding::empty(name, TypeBindingKind::TypeFunction)
            },
        );
    }

    /// Associates an expression node with a symbol lookup result.
    pub fn bind_expression(&mut self, scope: ScopeId, syntax: SyntaxId, symbol: Symbol) {
        self.scopes[scope.index()]
            .expression_symbols
            .insert(syntax, symbol);
    }

    /// Associates a type syntax node with a type binding.
    pub fn bind_type_syntax(&mut self, scope: ScopeId, syntax: SyntaxId, name: impl Into<String>) {
        self.scopes[scope.index()]
            .type_syntax_bindings
            .insert(syntax, name.into());
    }

    /// Looks up a local binding by parser identity in any scope.
    #[must_use]
    pub fn lookup_local_id(&self, local: LocalId) -> Option<&ValueBinding> {
        self.scopes
            .iter()
            .find_map(|scope| scope.locals.get(&local))
    }

    /// Looks up the most-recently-declared local binding with `name`, searching
    /// every scope. This mirrors how upstream's `requireType("name")` resolves
    /// a binding by source name rather than by position; ties are broken toward
    /// the highest `LocalId`, i.e. the latest declaration shadowing earlier ones.
    #[must_use]
    #[cfg_attr(not(any()), allow(dead_code))]
    pub fn lookup_local_by_name(&self, name: &str) -> Option<&ValueBinding> {
        self.scopes
            .iter()
            .flat_map(|scope| scope.locals.iter())
            .filter(|(_, binding)| binding.name == name)
            .max_by_key(|(local, _)| **local)
            .map(|(_, binding)| binding)
    }

    /// Looks up a global binding by walking from `scope` toward the root.
    #[must_use]
    pub fn lookup_global(&self, mut scope: ScopeId, name: &str) -> Option<&ValueBinding> {
        loop {
            let current = self.get(scope);
            if let Some(binding) = current.globals.get(name) {
                return Some(binding);
            }
            scope = current.parent?;
        }
    }

    /// Looks up a type binding and returns the scope where it was defined.
    #[must_use]
    pub fn lookup_type_with_scope(
        &self,
        mut scope: ScopeId,
        name: &str,
    ) -> Option<(ScopeId, &TypeBinding)> {
        loop {
            let current = self.get(scope);
            if let Some(binding) = current.type_bindings.get(name) {
                return Some((scope, binding));
            }
            scope = current.parent?;
        }
    }

    /// Looks up the symbol associated with an expression syntax node.
    #[must_use]
    pub fn symbol_for_expression(&self, mut scope: ScopeId, syntax: SyntaxId) -> Option<&Symbol> {
        loop {
            let current = self.get(scope);
            if let Some(symbol) = current.expression_symbols.get(&syntax) {
                return Some(symbol);
            }
            scope = current.parent?;
        }
    }

    /// Populates declaration and reference bindings from a parsed module AST.
    pub fn populate_module_bindings(&mut self, module: &Stat) {
        self.populate_statement_bindings(self.root, module);
    }

    /// Populates declaration and reference bindings from one statement.
    pub fn populate_statement_bindings(&mut self, scope: ScopeId, stat: &Stat) {
        match stat {
            Stat::Block { body, is_do, .. } => {
                let scope = if *is_do { self.add_child(scope) } else { scope };
                for stat in body {
                    self.populate_statement_bindings(scope, stat);
                }
            }
            Stat::Return { list, .. } => {
                for expr in list {
                    self.populate_expr_bindings(scope, expr);
                }
            }
            Stat::Expr { expr, .. } => self.populate_expr_bindings(scope, expr),
            Stat::Local { vars, values, .. } => {
                for local in vars {
                    self.define_local_from_ast(scope, local, ValueBindingKind::Local);
                }
                for value in values {
                    self.populate_expr_bindings(scope, value);
                }
            }
            Stat::Assign { vars, values, .. } => {
                for expr in vars.iter().chain(values) {
                    self.populate_expr_bindings(scope, expr);
                }
            }
            Stat::CompoundAssign { var, value, .. } => {
                self.populate_expr_bindings(scope, var);
                self.populate_expr_bindings(scope, value);
            }
            Stat::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.populate_expr_bindings(scope, condition);
                let then_scope = self.add_child(scope);
                self.populate_statement_bindings(then_scope, then_body);
                if let Some(else_body) = else_body {
                    let else_scope = self.add_child(scope);
                    self.populate_statement_bindings(else_scope, else_body);
                }
            }
            Stat::Break { .. } | Stat::Continue { .. } => {}
            Stat::While {
                condition, body, ..
            } => {
                self.populate_expr_bindings(scope, condition);
                let body_scope = self.add_child(scope);
                self.populate_statement_bindings(body_scope, body);
            }
            Stat::Repeat {
                condition, body, ..
            } => {
                let body_scope = self.add_child(scope);
                self.populate_statement_bindings(body_scope, body);
                self.populate_expr_bindings(body_scope, condition);
            }
            Stat::For {
                var,
                from,
                to,
                step,
                body,
                ..
            } => {
                self.populate_expr_bindings(scope, from);
                self.populate_expr_bindings(scope, to);
                if let Some(step) = step {
                    self.populate_expr_bindings(scope, step);
                }
                let body_scope = self.add_child(scope);
                self.define_local_from_ast(body_scope, var, ValueBindingKind::Local);
                self.populate_statement_bindings(body_scope, body);
            }
            Stat::ForIn {
                vars, values, body, ..
            } => {
                for value in values {
                    self.populate_expr_bindings(scope, value);
                }
                let body_scope = self.add_child(scope);
                for local in vars {
                    self.define_local_from_ast(body_scope, local, ValueBindingKind::Local);
                }
                self.populate_statement_bindings(body_scope, body);
            }
            Stat::Function { name, func, .. } => {
                if let Expr::Global {
                    name: global_name, ..
                } = &**name
                {
                    self.define_global(scope, global_name.as_str(), None);
                }
                self.populate_expr_bindings(scope, name);
                self.populate_expr_bindings(scope, func);
            }
            Stat::LocalFunction { name, func, .. } => {
                self.define_local_from_ast(scope, name, ValueBindingKind::Function);
                self.populate_expr_bindings(scope, func);
            }
            Stat::DeclareGlobal {
                name,
                declared_type,
                ..
            } => {
                self.define_global_with_documentation(
                    scope,
                    name.as_str(),
                    ValueBindingKind::Global,
                    None,
                    Some(format!("@test/global/{}", name.as_str())),
                );
                self.populate_type_bindings(scope, declared_type);
            }
            Stat::DeclareFunction {
                name,
                generics,
                generic_packs,
                params,
                ret_types,
                ..
            } => {
                self.define_global_with_documentation(
                    scope,
                    name.as_str(),
                    ValueBindingKind::DeclaredFunction,
                    None,
                    Some(format!("@test/global/{}", name.as_str())),
                );
                let function_scope = self.add_child(scope);
                self.define_generic_lists(function_scope, generics, generic_packs);
                self.populate_type_list_bindings(function_scope, params);
                self.populate_type_pack_bindings(function_scope, ret_types);
            }
            Stat::DeclareClass {
                name,
                super_name,
                props,
                indexer,
                ..
            } => {
                self.define_declared_class(
                    scope,
                    name.as_str(),
                    super_name.as_ref().map(|name| name.as_str()),
                    props,
                    indexer.as_ref(),
                    true,
                );
                if let Some(super_name) = super_name {
                    self.bind_type_name(scope, SyntaxId::default(), super_name.as_str());
                }
                for prop in props {
                    self.populate_declared_class_prop_bindings(scope, prop);
                }
                if let Some(indexer) = indexer {
                    self.populate_type_bindings(scope, &indexer.index_type);
                    self.populate_type_bindings(scope, &indexer.result_type);
                }
            }
            Stat::TypeAlias {
                name,
                generics,
                generic_packs,
                value,
                exported,
                ..
            } => {
                self.define_type_alias(
                    scope,
                    name.as_str(),
                    value,
                    generics,
                    generic_packs,
                    *exported,
                );
                let alias_scope = self.add_child(scope);
                self.define_generic_lists(alias_scope, generics, generic_packs);
                self.populate_type_bindings(alias_scope, value);
            }
            Stat::TypeFunction {
                name,
                func,
                exported,
                ..
            } => {
                self.define_type_function(scope, name.as_str(), func, *exported);
                self.populate_expr_bindings(scope, func);
            }
            Stat::Class {
                members, exported, ..
            } => {
                let class_scope = self.add_child(scope);
                for member in members {
                    if let Stat::ClassProperty { name, .. } = member {
                        self.define_class(class_scope, name.as_str(), false, *exported);
                    }
                    self.populate_statement_bindings(class_scope, member);
                }
            }
            Stat::ClassProperty {
                declared_type: Some(declared_type),
                ..
            } => {
                self.populate_type_bindings(scope, declared_type);
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
                for expr in expressions {
                    self.populate_expr_bindings(scope, expr);
                }
                for stat in statements {
                    self.populate_statement_bindings(scope, stat);
                }
            }
        }
    }

    fn define_local_from_ast(
        &mut self,
        scope: ScopeId,
        local: &Local,
        kind: ValueBindingKind,
    ) -> Symbol {
        if let Some(annotation) = &local.annotation {
            self.populate_type_bindings(scope, annotation);
        }
        self.define_local_with_kind(scope, local.id, local.name.as_str(), kind, None)
    }

    fn define_generic_lists(
        &mut self,
        scope: ScopeId,
        generics: &[GenericType],
        generic_packs: &[GenericTypePack],
    ) {
        for generic in generics {
            self.define_generic_type(scope, generic);
            if let Some(default) = &generic.default_type {
                self.populate_type_bindings(scope, default);
            }
        }
        for generic in generic_packs {
            self.define_generic_type_pack(scope, generic);
            if let Some(default) = &generic.default_type {
                self.populate_type_pack_bindings(scope, default);
            }
        }
    }

    fn populate_declared_class_prop_bindings(&mut self, scope: ScopeId, prop: &DeclaredClassProp) {
        self.populate_type_bindings(scope, &prop.declared_type);
    }

    fn populate_expr_bindings(&mut self, scope: ScopeId, expr: &Expr) {
        match expr {
            Expr::Nil { .. }
            | Expr::Bool { .. }
            | Expr::Number { .. }
            | Expr::Integer { .. }
            | Expr::String { .. }
            | Expr::Varargs { .. } => {}
            Expr::Global {
                syntax_id, name, ..
            } => self.bind_expression(scope, *syntax_id, Symbol::global(name.as_str())),
            Expr::Local {
                syntax_id, local, ..
            } => self.bind_expression(scope, *syntax_id, Symbol::local(local.id)),
            Expr::Call {
                func,
                type_arguments,
                args,
                ..
            } => {
                self.populate_expr_bindings(scope, func);
                for parameter in type_arguments {
                    if let TypeParameter::Type(ty) = parameter {
                        self.populate_type_bindings(scope, ty);
                    }
                }
                for arg in args {
                    self.populate_expr_bindings(scope, arg);
                }
            }
            Expr::Binary { left, right, .. } => {
                self.populate_expr_bindings(scope, left);
                self.populate_expr_bindings(scope, right);
            }
            Expr::Unary { expr, .. } | Expr::Group { expr, .. } => {
                self.populate_expr_bindings(scope, expr);
            }
            Expr::IfElse {
                condition,
                true_expr,
                false_expr,
                ..
            } => {
                self.populate_expr_bindings(scope, condition);
                self.populate_expr_bindings(scope, true_expr);
                self.populate_expr_bindings(scope, false_expr);
            }
            Expr::TypeAssertion {
                expr, annotation, ..
            } => {
                self.populate_expr_bindings(scope, expr);
                self.populate_type_bindings(scope, annotation);
            }
            Expr::IndexName { expr, .. } => self.populate_expr_bindings(scope, expr),
            Expr::IndexExpr { expr, index, .. } => {
                self.populate_expr_bindings(scope, expr);
                self.populate_expr_bindings(scope, index);
            }
            Expr::Table { items, .. } => {
                for item in items {
                    if let Some(key) = &item.key {
                        self.populate_expr_bindings(scope, key);
                    }
                    self.populate_expr_bindings(scope, &item.value);
                }
            }
            Expr::InterpString { expressions, .. } | Expr::Error { expressions, .. } => {
                for expr in expressions {
                    self.populate_expr_bindings(scope, expr);
                }
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
                let function_scope = self.add_child(scope);
                self.define_generic_lists(function_scope, generics, generic_packs);
                if let Some(self_arg) = self_arg {
                    self.define_local_from_ast(
                        function_scope,
                        self_arg,
                        ValueBindingKind::FunctionParameter,
                    );
                }
                for arg in args {
                    self.define_local_from_ast(
                        function_scope,
                        arg,
                        ValueBindingKind::FunctionParameter,
                    );
                }
                if let Some(vararg_annotation) = vararg_annotation {
                    self.populate_type_pack_bindings(function_scope, vararg_annotation);
                }
                if let Some(return_annotation) = return_annotation {
                    self.populate_type_pack_bindings(function_scope, return_annotation);
                }
                let body_scope = self.add_child(function_scope);
                self.populate_statement_bindings(body_scope, body);
            }
            Expr::Instantiate {
                expr,
                type_arguments,
                ..
            } => {
                self.populate_expr_bindings(scope, expr);
                for parameter in type_arguments {
                    if let TypeParameter::Type(ty) = parameter {
                        self.populate_type_bindings(scope, ty);
                    }
                }
            }
        }
    }

    fn populate_type_bindings(&mut self, scope: ScopeId, ty: &Type) {
        match ty {
            Type::Reference {
                syntax_id,
                prefix,
                name,
                parameters,
                ..
            } => {
                let binding_name = prefix
                    .as_ref()
                    .map(|prefix| format!("{}.{}", prefix.as_str(), name.as_str()))
                    .unwrap_or_else(|| name.as_str().to_owned());
                self.bind_type_name(scope, *syntax_id, &binding_name);
                for parameter in parameters {
                    self.populate_type_parameter_bindings(scope, parameter);
                }
            }
            Type::Typeof { expr, .. } => self.populate_expr_bindings(scope, expr),
            Type::Optional { .. } | Type::SingletonString { .. } | Type::SingletonBool { .. } => {}
            Type::Group { inner, .. } => self.populate_type_bindings(scope, inner),
            Type::Union { types, .. } | Type::Intersection { types, .. } => {
                for ty in types {
                    self.populate_type_bindings(scope, ty);
                }
            }
            Type::Function {
                generics,
                generic_packs,
                arg_types,
                return_types,
                ..
            } => {
                let function_scope = self.add_child(scope);
                self.define_generic_lists(function_scope, generics, generic_packs);
                self.populate_type_list_bindings(function_scope, arg_types);
                self.populate_type_pack_bindings(function_scope, return_types);
            }
            Type::Table { props, indexer, .. } => {
                for prop in props {
                    self.populate_type_bindings(scope, &prop.prop_type);
                }
                if let Some(indexer) = indexer {
                    self.populate_type_bindings(scope, &indexer.index_type);
                    self.populate_type_bindings(scope, &indexer.result_type);
                }
            }
            Type::Error { types, .. } => {
                for ty in types {
                    self.populate_type_bindings(scope, ty);
                }
            }
        }
    }

    fn populate_type_list_bindings(&mut self, scope: ScopeId, list: &TypeList) {
        for ty in &list.types {
            self.populate_type_bindings(scope, ty);
        }
        if let Some(tail) = &list.tail_type {
            self.populate_type_pack_bindings(scope, tail);
        }
    }

    fn populate_type_pack_bindings(&mut self, scope: ScopeId, pack: &TypePack) {
        match pack {
            TypePack::Explicit { type_list, .. } => {
                self.populate_type_list_bindings(scope, type_list)
            }
            TypePack::Generic { name, .. } => {
                self.bind_type_name(scope, SyntaxId::default(), name.as_str());
            }
            TypePack::Variadic { variadic_type, .. } => {
                self.populate_type_bindings(scope, variadic_type);
            }
        }
    }

    fn populate_type_parameter_bindings(&mut self, scope: ScopeId, parameter: &TypeParameter) {
        match parameter {
            TypeParameter::Type(ty) => self.populate_type_bindings(scope, ty),
            TypeParameter::Pack(pack) => self.populate_type_pack_bindings(scope, pack),
        }
    }

    fn bind_type_name(&mut self, scope: ScopeId, syntax: SyntaxId, name: &str) {
        self.bind_type_syntax(scope, syntax, name);
    }
}

impl Default for ScopeTree {
    fn default() -> Self {
        Self::new()
    }
}

impl Symbol {
    /// Creates a global symbol.
    #[must_use]
    pub fn global(name: impl Into<String>) -> Self {
        Self::Global(name.into())
    }

    /// Creates a local symbol.
    #[must_use]
    pub const fn local(id: LocalId) -> Self {
        Self::Local(id)
    }
}

#[cfg(any())]
mod tests {
    use std::collections::HashMap;

    use ruau_ast::syntax::Name;

    use super::*;

    #[test]
    fn globals_compare_and_hash_by_name() {
        ruau_upstream::upstream_case!(
            "Symbol.test.cpp::SymbolTests::equality_and_hashing_of_globals"
        );
        let one = Symbol::global(String::from("name"));
        let two = Symbol::global("name".to_owned());

        assert_eq!(one, one);
        assert_eq!(one, two);

        let mut symbols = HashMap::new();
        symbols.insert(one, 5);
        symbols.insert(two, 1);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols.get(&Symbol::global("name")), Some(&1));
    }

    #[test]
    fn locals_compare_and_hash_by_binding_identity() {
        ruau_upstream::upstream_case!(
            "Symbol.test.cpp::SymbolTests::equality_and_hashing_of_locals"
        );
        let one = Symbol::local(LocalId::new(0));
        let two = Symbol::local(LocalId::new(1));

        assert_eq!(one, one);
        assert_ne!(one, two);

        let mut symbols = HashMap::new();
        symbols.insert(one, 5);
        symbols.insert(two, 1);
        assert_eq!(symbols.len(), 2);
    }

    #[test]
    fn empty_symbols_only_equal_empty_symbols() {
        ruau_upstream::upstream_case!("Symbol.test.cpp::SymbolTests::equality_of_empty_symbols");
        let global = Symbol::global("name");
        let local = Symbol::local(LocalId::new(0));
        let empty1 = Symbol::Empty;
        let empty2 = Symbol::default();

        assert_ne!(empty1, global);
        assert_ne!(empty1, local);
        assert_eq!(empty1, empty2);
        assert!(matches!(empty1, Symbol::Empty));
    }

    #[test]
    fn scope_tree_tracks_lexical_bindings_and_syntax_owners() {
        let mut tree = ScopeTree::new();
        let root = tree.root();
        let child = tree.add_child(root);
        let grandchild = tree.add_child(child);

        let root_local = LocalId::new(0);
        let child_local = LocalId::new(1);
        let root_symbol =
            tree.define_local_with_kind(root, root_local, "root", ValueBindingKind::Local, None);
        let child_symbol =
            tree.define_local_with_kind(child, child_local, "child", ValueBindingKind::Local, None);
        let global_symbol = tree.define_global(root, "math", None);
        tree.define_type_with_kind(
            root,
            "Vector",
            TypeBindingKind::ExportedTypeAlias,
            None,
            true,
        );
        tree.bind_expression(child, SyntaxId::new(7), child_symbol.clone());
        tree.bind_type_syntax(grandchild, SyntaxId::new(8), "Vector");

        assert_eq!(tree.get(root).children, vec![child]);
        assert_eq!(tree.get(child).parent, Some(root));
        assert_eq!(tree.get(child).children, vec![grandchild]);
        assert_eq!(tree.get(root).locals[&root_local].symbol, root_symbol);
        assert_eq!(tree.get(child).locals[&child_local].symbol, child_symbol);
        assert_eq!(
            tree.lookup_global(grandchild, "math").unwrap().symbol,
            global_symbol
        );
        assert!(
            tree.lookup_type_with_scope(grandchild, "Vector")
                .unwrap()
                .1
                .exported
        );
        assert_eq!(
            tree.get(child)
                .expression_symbols
                .get(&SyntaxId::new(7))
                .unwrap(),
            &child_symbol
        );
        assert_eq!(
            tree.get(grandchild)
                .type_syntax_bindings
                .get(&SyntaxId::new(8))
                .map(String::as_str),
            Some("Vector")
        );
    }

    #[test]
    fn populates_declaration_binding_kinds_from_ast() {
        let alias_ref = Type::Reference {
            syntax_id: SyntaxId::new(100),
            location: None,
            prefix: None,
            prefix_location: None,
            name: Name::new("Alias"),
            name_location: None,
            parameters: Vec::new(),
        };
        let local_x = local(0, "x", Some(alias_ref.clone()));
        let local_fn = local(1, "lf", None);
        let param = local(2, "p", None);
        let module = Stat::Block {
            location: None,
            has_end: false,
            is_do: false,
            body: vec![
                Stat::TypeAlias {
                    location: None,
                    name: Name::new("Alias"),
                    generics: vec![generic_type("T")],
                    generic_packs: vec![generic_pack("U")],
                    value: Box::new(Type::SingletonBool {
                        syntax_id: SyntaxId::new(101),
                        location: None,
                        value: true,
                    }),
                    exported: true,
                },
                Stat::DeclareClass {
                    location: None,
                    name: Name::new("Widget"),
                    super_name: None,
                    props: Vec::new(),
                    indexer: None,
                },
                Stat::DeclareGlobal {
                    location: None,
                    name: Name::new("globalValue"),
                    name_location: None,
                    declared_type: Box::new(alias_ref.clone()),
                },
                Stat::DeclareFunction {
                    location: None,
                    attributes: Vec::new(),
                    name: Name::new("declaredFn"),
                    name_location: None,
                    generics: vec![generic_type("F")],
                    generic_packs: Vec::new(),
                    params: TypeList::new(vec![alias_ref.clone()]),
                    param_names: Vec::new(),
                    vararg: false,
                    vararg_location: None,
                    ret_types: Box::new(TypePack::Explicit {
                        location: None,
                        type_list: TypeList::new(vec![alias_ref]),
                    }),
                },
                Stat::Local {
                    location: None,
                    vars: vec![local_x.clone()],
                    values: vec![Expr::Global {
                        syntax_id: SyntaxId::new(200),
                        location: None,
                        name: Name::new("globalValue"),
                    }],
                    exported: false,
                },
                Stat::LocalFunction {
                    location: None,
                    name: local_fn.clone(),
                    func: Box::new(Expr::Function {
                        syntax_id: SyntaxId::new(201),
                        location: None,
                        attributes: Vec::new(),
                        generics: vec![generic_type("P")],
                        generic_packs: vec![generic_pack("R")],
                        args: vec![param.clone()],
                        self_arg: None,
                        vararg: false,
                        vararg_location: None,
                        vararg_annotation: None,
                        return_annotation: None,
                        body: Box::new(Stat::Return {
                            location: None,
                            list: vec![Expr::Local {
                                syntax_id: SyntaxId::new(202),
                                location: None,
                                local: param.to_local_ref(),
                            }],
                        }),
                        function_depth: 0,
                        debug_name: "lf".to_owned(),
                    }),
                    exported: false,
                },
            ],
        };
        let mut tree = ScopeTree::new();

        tree.populate_module_bindings(&module);

        let root = tree.root();
        assert_eq!(
            tree.get(root).type_bindings["Alias"].kind,
            TypeBindingKind::ExportedTypeAlias
        );
        assert_eq!(
            tree.get(root).type_bindings["Widget"].kind,
            TypeBindingKind::DeclaredClass
        );
        assert_eq!(
            tree.get(root).globals["globalValue"].kind,
            ValueBindingKind::Global
        );
        assert_eq!(
            tree.get(root).globals["declaredFn"].kind,
            ValueBindingKind::DeclaredFunction
        );
        assert_eq!(
            tree.get(root).locals[&local_x.id].kind,
            ValueBindingKind::Local
        );
        assert_eq!(
            tree.get(root).locals[&local_fn.id].kind,
            ValueBindingKind::Function
        );
        assert_eq!(
            tree.get(root).expression_symbols[&SyntaxId::new(200)],
            Symbol::global("globalValue")
        );
        assert_eq!(
            tree.get(root).type_syntax_bindings[&SyntaxId::new(100)],
            "Alias"
        );

        let function_scope = tree
            .scopes
            .iter()
            .find(|scope| scope.locals.contains_key(&param.id))
            .expect("function scope should contain parameter");
        assert_eq!(
            function_scope.locals[&param.id].kind,
            ValueBindingKind::FunctionParameter
        );
        assert_eq!(
            function_scope.type_bindings["P"].kind,
            TypeBindingKind::GenericParameter
        );
        assert_eq!(
            function_scope.type_bindings["R"].kind,
            TypeBindingKind::GenericPackParameter
        );
        let body_scope = tree
            .get(function_scope.id)
            .children
            .first()
            .copied()
            .expect("function body scope should be a child of the signature scope");
        assert_eq!(
            tree.get(body_scope).expression_symbols[&SyntaxId::new(202)],
            Symbol::local(param.id)
        );
    }

    #[test]
    fn checked_symbol_lookup_resolves_expressions_declarations_and_type_references() {
        let mut tree = ScopeTree::new();
        let root = tree.root();
        let child = tree.add_child(root);
        let local_id = LocalId::new(12);

        let local_symbol =
            tree.define_local_with_kind(root, local_id, "value", ValueBindingKind::Local, None);
        let global_symbol = tree.define_global(root, "print", None);
        tree.define_type_with_kind(
            root,
            "Alias",
            TypeBindingKind::ExportedTypeAlias,
            None,
            true,
        );
        tree.bind_expression(child, SyntaxId::new(30), local_symbol.clone());
        tree.bind_expression(child, SyntaxId::new(31), global_symbol.clone());
        tree.bind_type_syntax(child, SyntaxId::new(32), "Alias");

        assert_eq!(tree.get(root).locals[&local_id].symbol, local_symbol);
        assert_eq!(
            tree.lookup_global(child, "print").unwrap().symbol,
            global_symbol
        );
        assert_eq!(
            tree.symbol_for_expression(child, SyntaxId::new(30)),
            Some(&local_symbol)
        );
        assert_eq!(
            tree.lookup_type_with_scope(child, "Alias").unwrap().1.kind,
            TypeBindingKind::ExportedTypeAlias
        );
    }

    fn local(id: u32, name: &str, annotation: Option<Type>) -> Local {
        Local {
            id: LocalId::new(id),
            name: Name::new(name),
            location: None,
            annotation: annotation.map(Box::new),
            is_const: false,
            function_depth: 0,
        }
    }

    fn generic_type(name: &str) -> GenericType {
        GenericType {
            name: Name::new(name),
            location: None,
            default_type: None,
        }
    }

    fn generic_pack(name: &str) -> GenericTypePack {
        GenericTypePack {
            name: Name::new(name),
            location: None,
            default_type: None,
        }
    }
}
