//! Data-flow graph scaffolding for constraint generation.
//!
//! This remains in `ruau-typecheck` rather than `ruau-analysis` because data
//! flow definitions carry inferred [`crate::types::TypeId`] handles and are
//! updated by the type-generation pipeline.

use std::collections::{BTreeMap, BTreeSet};

use ruau_ast::{
    json::JsonTableItemKind,
    syntax::{Expr, Local, LocalId, Stat, SyntaxId, Type, TypePack},
};

use crate::{
    scopes::{ScopeId, ScopeTree, Symbol},
    types::{Arena, TypeId, TypeKind, TypeLevel, TypeVariable},
};

/// Map from refinement lvalues to refined types.
pub type RefinementMap = BTreeMap<RefinementKey, TypeId>;

/// Stable handle for a data-flow definition.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DefId(u32);

impl DefId {
    /// Returns the zero-based graph index.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    fn from_index(index: usize) -> Self {
        let index = u32::try_from(index).expect("data-flow graph exceeded u32 handle space");
        Self(index)
    }
}

/// Stable handle for an interned refinement key.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RefinementKeyId(u32);

impl RefinementKeyId {
    /// Returns the zero-based key-arena index.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    fn from_index(index: usize) -> Self {
        let index = u32::try_from(index).expect("refinement-key arena exceeded u32 handle space");
        Self(index)
    }
}

/// Key used for flow-sensitive refinement.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RefinementKey {
    /// A local or global symbol.
    Symbol(Symbol),
    /// A property path rooted in another definition.
    Property {
        /// Base definition.
        base: DefId,
        /// Property name.
        name: String,
    },
}

/// Merges refinement maps, unioning overlapping branch types.
#[cfg(any())]
pub fn merge_refinement_maps(arena: &mut Arena, left: &mut RefinementMap, right: &RefinementMap) {
    for (key, right_ty) in right {
        if let Some(left_ty) = left.get(key).copied() {
            let merged = union_types(arena, [left_ty, *right_ty]);
            left.insert(key.clone(), merged);
        } else {
            left.insert(key.clone(), *right_ty);
        }
    }
}

#[cfg(any())]
fn union_types<const N: usize>(arena: &mut Arena, types: [TypeId; N]) -> TypeId {
    let mut options = BTreeSet::new();
    for ty in types {
        match arena.get(ty) {
            TypeKind::Union(types) => {
                options.extend(types.iter().copied());
            }
            _ => {
                options.insert(ty);
            }
        }
    }
    if options.len() == 1 {
        *options.iter().next().expect("one option")
    } else {
        arena.alloc(TypeKind::Union(options.into_iter().collect()))
    }
}

/// Data-flow definition category.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DefKind {
    /// Local binding definition.
    Local {
        /// Local identity.
        local: LocalId,
        /// Source-visible local name.
        name: String,
    },
    /// Expression definition.
    Expression {
        /// Parser syntax id.
        syntax_id: SyntaxId,
    },
    /// Versioned single-assignment cell over a base binding.
    ///
    /// Produced by the upstream-shaped DFG walk when a binding is
    /// observed in one branch or loop iteration; reads through the
    /// cell see only the writes that reached that point.
    Cell {
        /// Underlying base binding (typically `Local` or `Global`).
        base: DefId,
        /// Version index among writes to `base` along this control-
        /// flow edge.
        version: u32,
    },
    /// Phi join of multiple incoming versions at a control-flow merge.
    ///
    /// Each predecessor contributes one `Cell` (or another `Phi`);
    /// the resulting type is the join of the predecessor types.
    Phi {
        /// Underlying base binding the phi joins versions of.
        base: DefId,
        /// Predecessor definitions whose values flow into this phi.
        predecessors: Vec<DefId>,
    },
}

/// DFG scope kind.
///
/// The upstream-shaped walk classifies each scope so phi-insertion
/// and capture resolution can use the scope's flavour rather than
/// pattern-matching on the originating statement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DfgScope {
    /// Straight-line scope inside a block.
    Linear,
    /// Loop body whose writes may converge through a back-edge.
    Loop,
    /// Function body — its captures resolve to the enclosing scope's
    /// versions at the function-definition site.
    Function,
}

/// One data-flow definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Def {
    /// Definition category.
    pub kind: DefKind,
    /// Placeholder type owned by the checker arena.
    pub ty: TypeId,
    /// Lexical scope where this definition was observed.
    pub scope: ScopeId,
    /// Optional interned refinement key.
    pub key: Option<RefinementKeyId>,
}

/// DFG plus lookup arenas used by later constraint generation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DataFlowGraph {
    defs: Vec<Def>,
    refinement_keys: Vec<RefinementKey>,
    scope_kinds: BTreeMap<ScopeId, DfgScope>,
    locals: BTreeMap<LocalId, DefId>,
    expressions: BTreeMap<SyntaxId, DefId>,
    flow_expressions: BTreeMap<SyntaxId, DefId>,
    key_ids: BTreeMap<RefinementKey, RefinementKeyId>,
    key_defs: BTreeMap<RefinementKeyId, DefId>,
    current_key_defs: BTreeMap<RefinementKeyId, DefId>,
}

/// Test helper: persisted module-local DFG state with the scopes and arena
/// that produced it.
///
/// The in-file DFG tests inspect interned refinement-key arenas and
/// per-scope defs that `TestContext` does not surface, so this helper
/// produces scopes/types/graph triples directly.
#[cfg(any())]
#[derive(Clone, Debug)]
pub struct DataFlowModule {
    pub scopes: ScopeTree,
    pub types: Arena,
    pub graph: DataFlowGraph,
}

#[cfg(any())]
impl DataFlowModule {
    /// Populates scopes, allocates placeholder types, and builds the DFG for a
    /// parsed module root.
    #[must_use]
    pub fn build(module: &Stat) -> Self {
        let mut scopes = ScopeTree::new();
        scopes.populate_module_bindings(module);
        let mut types = Arena::new();
        let graph = DataFlowGraph::build(module, &scopes, &mut types);
        Self {
            scopes,
            types,
            graph,
        }
    }
}

impl DataFlowGraph {
    /// Builds a graph from a parsed module root.
    #[must_use]
    pub fn build(module: &Stat, scopes: &ScopeTree, arena: &mut Arena) -> Self {
        let mut builder = DataFlowGraphBuilder {
            graph: Self::default(),
            scopes,
            arena,
            next_child: BTreeMap::new(),
            current_defs: BTreeMap::new(),
            key_versions: BTreeMap::new(),
            function_scopes: Vec::new(),
            function_capture_phis: Vec::new(),
            capture_phis_by_key: BTreeMap::new(),
            uninitialized_locals: BTreeSet::new(),
        };
        builder
            .graph
            .scope_kinds
            .insert(scopes.root(), DfgScope::Linear);
        builder.visit_stat(scopes.root(), module);
        builder.graph
    }

    /// Returns a definition by id.
    #[must_use]
    pub fn get(&self, id: DefId) -> &Def {
        &self.defs[id.index()]
    }

    /// Number of definitions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// Iterates over definitions in stable allocation order.
    pub fn defs(&self) -> impl Iterator<Item = (DefId, &Def)> {
        self.defs
            .iter()
            .enumerate()
            .map(|(index, def)| (DefId::from_index(index), def))
    }

    /// Returns an interned refinement key by id.
    #[must_use]
    pub fn refinement_key(&self, id: RefinementKeyId) -> &RefinementKey {
        &self.refinement_keys[id.index()]
    }

    /// Looks up a refinement-key id.
    #[must_use]
    pub fn refinement_key_id(&self, key: &RefinementKey) -> Option<RefinementKeyId> {
        self.key_ids.get(key).copied()
    }

    /// Looks up a local definition.
    #[must_use]
    pub fn local(&self, local: LocalId) -> Option<DefId> {
        self.locals.get(&local).copied()
    }

    /// Looks up an expression definition.
    #[must_use]
    pub fn expression(&self, syntax_id: SyntaxId) -> Option<DefId> {
        self.expressions.get(&syntax_id).copied()
    }

    /// Looks up the flow-sensitive definition for an expression.
    ///
    /// Unlike [`Self::expression`], this returns the upstream DFG view where a
    /// local/property read can map to the current `Cell` or `Phi`. The checker
    /// still consumes `expression`'s stable expression-placeholder types.
    #[must_use]
    pub fn flow_expression(&self, syntax_id: SyntaxId) -> Option<DefId> {
        self.flow_expressions
            .get(&syntax_id)
            .or_else(|| self.expressions.get(&syntax_id))
            .copied()
    }

    /// Looks up a refinement-key definition.
    #[must_use]
    pub fn key(&self, key: &RefinementKey) -> Option<DefId> {
        let key = self.refinement_key_id(key)?;
        self.key_defs.get(&key).copied()
    }

    /// Looks up the latest versioned definition for a refinement key.
    #[must_use]
    pub fn current_key(&self, key: &RefinementKey) -> Option<DefId> {
        let key = self.refinement_key_id(key)?;
        self.current_key_defs
            .get(&key)
            .or_else(|| self.key_defs.get(&key))
            .copied()
    }

    fn push(&mut self, def: Def) -> DefId {
        let id = DefId::from_index(self.defs.len());
        if let DefKind::Local { local, .. } = def.kind {
            self.locals.insert(local, id);
        }
        if let DefKind::Expression { syntax_id } = def.kind {
            self.expressions.insert(syntax_id, id);
        }
        if let Some(key) = def.key {
            self.key_defs.entry(key).or_insert(id);
        }
        self.defs.push(def);
        id
    }

    fn intern_key(&mut self, key: RefinementKey) -> RefinementKeyId {
        if let Some(id) = self.key_ids.get(&key) {
            return *id;
        }
        let id = RefinementKeyId::from_index(self.refinement_keys.len());
        self.refinement_keys.push(key.clone());
        self.key_ids.insert(key, id);
        id
    }
}

struct DataFlowGraphBuilder<'a> {
    graph: DataFlowGraph,
    scopes: &'a ScopeTree,
    arena: &'a mut Arena,
    next_child: BTreeMap<ScopeId, usize>,
    current_defs: BTreeMap<RefinementKey, DefId>,
    key_versions: BTreeMap<RefinementKey, u32>,
    function_scopes: Vec<ScopeId>,
    function_capture_phis: Vec<BTreeMap<RefinementKey, DefId>>,
    capture_phis_by_key: BTreeMap<RefinementKey, Vec<DefId>>,
    uninitialized_locals: BTreeSet<LocalId>,
}

impl DataFlowGraphBuilder<'_> {
    fn enter_child(&mut self, parent: ScopeId) -> ScopeId {
        let next = self.next_child.entry(parent).or_default();
        let scope = self
            .scopes
            .get(parent)
            .children
            .get(*next)
            .copied()
            .unwrap_or(parent);
        *next += 1;
        scope
    }

    fn enter_child_with_kind(&mut self, parent: ScopeId, kind: DfgScope) -> ScopeId {
        let scope = self.enter_child(parent);
        self.graph.scope_kinds.insert(scope, kind);
        scope
    }

    fn set_current(&mut self, key: RefinementKey, def: DefId) {
        let key_id = self.graph.intern_key(key.clone());
        self.graph.current_key_defs.insert(key_id, def);
        self.current_defs.insert(key, def);
    }

    fn current_for_key(&self, key: &RefinementKey) -> Option<DefId> {
        self.current_defs
            .get(key)
            .copied()
            .or_else(|| self.graph.key(key))
    }

    fn base_for_key(&self, key: &RefinementKey) -> DefId {
        self.graph
            .key(key)
            .or_else(|| self.current_defs.get(key).copied())
            .unwrap_or_default()
    }

    fn write_key(&mut self, scope: ScopeId, key: &RefinementKey) -> DefId {
        let base = self.base_for_key(key);
        let version = {
            let version = self.key_versions.entry(key.clone()).or_default();
            *version += 1;
            *version
        };
        let key_id = self.graph.intern_key(key.clone());
        let ty = self.placeholder_type(format!("cell{}", self.graph.len()));
        let id = self.graph.push(Def {
            kind: DefKind::Cell { base, version },
            ty,
            scope,
            key: Some(key_id),
        });
        self.set_current(key.clone(), id);
        self.add_write_to_capture_phis(key, id);
        id
    }

    fn add_write_to_capture_phis(&mut self, key: &RefinementKey, def: DefId) {
        let Some(phis) = self.capture_phis_by_key.get(key).cloned() else {
            return;
        };

        for phi in phis {
            let DefKind::Phi { predecessors, .. } = &mut self.graph.defs[phi.index()].kind else {
                continue;
            };
            if !predecessors.contains(&def) {
                predecessors.push(def);
            }
        }
    }

    fn active_function_scope(&self) -> Option<ScopeId> {
        self.function_scopes.last().copied()
    }

    fn is_captured_key(&self, scope: ScopeId, key: &RefinementKey) -> bool {
        let Some(function_scope) = self.active_function_scope() else {
            return false;
        };
        let RefinementKey::Symbol(Symbol::Local(local)) = key else {
            return false;
        };
        let Some(definition_scope) = self.scopes.local_definition_scope(scope, *local) else {
            return false;
        };
        !self
            .scopes
            .is_descendant_or_same(definition_scope, function_scope)
    }

    fn capture_phi_for_key(&mut self, scope: ScopeId, key: RefinementKey) -> DefId {
        if let Some(def) = self
            .function_capture_phis
            .last()
            .and_then(|phis| phis.get(&key))
            .copied()
        {
            return def;
        }

        let mut predecessors = Vec::new();
        if let Some(current) = self.current_for_key(&key)
            && !self.is_uninitialized_local_base(&key, current)
        {
            predecessors.push(current);
        }
        let key_id = self.graph.intern_key(key.clone());
        let ty = self.placeholder_type(format!("capture{}", self.graph.len()));
        let id = self.graph.push(Def {
            kind: DefKind::Phi {
                base: self.base_for_key(&key),
                predecessors,
            },
            ty,
            scope,
            key: Some(key_id),
        });
        self.set_current(key.clone(), id);
        if let Some(phis) = self.function_capture_phis.last_mut() {
            phis.insert(key.clone(), id);
        }
        self.capture_phis_by_key.entry(key).or_default().push(id);
        id
    }

    fn is_uninitialized_local_base(&self, key: &RefinementKey, def: DefId) -> bool {
        let RefinementKey::Symbol(Symbol::Local(local)) = key else {
            return false;
        };
        self.uninitialized_locals.contains(local) && self.graph.local(*local) == Some(def)
    }

    fn join_branch_defs(
        &mut self,
        scope: ScopeId,
        base_defs: &BTreeMap<RefinementKey, DefId>,
        branch_defs: &[&BTreeMap<RefinementKey, DefId>],
    ) {
        let mut keys = BTreeMap::new();
        for key in base_defs.keys() {
            keys.insert(key.clone(), ());
        }
        for defs in branch_defs {
            for key in defs.keys() {
                if matches!(key, RefinementKey::Property { .. }) {
                    keys.insert(key.clone(), ());
                }
            }
        }

        for key in keys.into_keys() {
            let mut predecessors = Vec::new();
            for defs in branch_defs {
                if let Some(def) = defs
                    .get(&key)
                    .copied()
                    .or_else(|| base_defs.get(&key).copied())
                    && !predecessors.contains(&def)
                {
                    predecessors.push(def);
                }
            }

            if predecessors.len() < 2 {
                if let Some(def) = predecessors.first().copied()
                    && (base_defs.contains_key(&key)
                        || matches!(key, RefinementKey::Property { .. }))
                {
                    self.set_current(key, def);
                }
                continue;
            }

            let key_id = self.graph.intern_key(key.clone());
            let ty = self.placeholder_type(format!("phi{}", self.graph.len()));
            let id = self.graph.push(Def {
                kind: DefKind::Phi {
                    base: self.base_for_key(&key),
                    predecessors,
                },
                ty,
                scope,
                key: Some(key_id),
            });
            let version = self.key_versions.entry(key.clone()).or_default();
            *version += 1;
            self.set_current(key, id);
        }
    }

    fn commit_repeat_defs(
        &mut self,
        base_defs: &BTreeMap<RefinementKey, DefId>,
        body_defs: &BTreeMap<RefinementKey, DefId>,
    ) {
        let mut keys = BTreeMap::new();
        for key in base_defs.keys() {
            keys.insert(key.clone(), ());
        }
        for key in body_defs.keys() {
            if matches!(key, RefinementKey::Property { .. }) {
                keys.insert(key.clone(), ());
            }
        }

        for key in keys.into_keys() {
            if let Some(def) = body_defs
                .get(&key)
                .copied()
                .or_else(|| base_defs.get(&key).copied())
            {
                self.set_current(key, def);
            }
        }
    }

    fn target_key(&mut self, scope: ScopeId, expr: &Expr) -> Option<RefinementKey> {
        match expr {
            Expr::Local { syntax_id, .. } | Expr::Global { syntax_id, .. } => self
                .scopes
                .symbol_for_expression(scope, *syntax_id)
                .cloned()
                .map(RefinementKey::Symbol),
            Expr::IndexName { expr, index, .. } => {
                let base = self.visit_expr(scope, expr);
                let base = self.canonical_refinement_base(base);
                Some(RefinementKey::Property {
                    base,
                    name: index.as_str().to_owned(),
                })
            }
            _ => None,
        }
    }

    fn visit_lvalue_write(&mut self, scope: ScopeId, expr: &Expr) -> DefId {
        let target = self.target_key(scope, expr);
        if let Some(key) = target.as_ref()
            && self.is_captured_key(scope, key)
        {
            let def = self.capture_phi_for_key(scope, key.clone());
            self.visit_expr(scope, expr);
            self.graph.flow_expressions.insert(expr.syntax_id(), def);
            return def;
        }
        let expr_def = self.visit_expr(scope, expr);
        if let Some(key) = target {
            let def = self.write_key(scope, &key);
            self.graph.flow_expressions.insert(expr.syntax_id(), def);
            def
        } else {
            expr_def
        }
    }

    fn seed_table_literal_properties(&mut self, base: DefId, expr: &Expr) {
        let Expr::Table { items, .. } = expr else {
            return;
        };

        let base = self.canonical_refinement_base(base);
        for item in items {
            if item.kind != JsonTableItemKind::Record {
                continue;
            }
            let Some(Expr::String { value: name, .. }) = item.key.as_ref() else {
                continue;
            };
            let Some(def) = self.graph.flow_expression(item.value.syntax_id()) else {
                continue;
            };
            let key = RefinementKey::Property {
                base,
                name: name.clone(),
            };
            self.set_current(key, def);
        }
    }

    fn visit_stat(&mut self, scope: ScopeId, stat: &Stat) {
        match stat {
            Stat::Block { body, is_do, .. } => {
                let scope = if *is_do {
                    self.enter_child_with_kind(scope, DfgScope::Linear)
                } else {
                    scope
                };
                for stat in body {
                    self.visit_stat(scope, stat);
                }
            }
            Stat::Return { list, .. } => {
                for expr in list {
                    self.visit_expr(scope, expr);
                }
            }
            Stat::Expr { expr, .. } => {
                self.visit_expr(scope, expr);
            }
            Stat::Local { vars, values, .. } => {
                let local_defs = vars
                    .iter()
                    .enumerate()
                    .map(|(index, local)| {
                        let def = self.define_local(scope, local);
                        if index >= values.len() {
                            self.uninitialized_locals.insert(local.id);
                        }
                        if let Some(annotation) = &local.luau_type {
                            self.visit_type(scope, annotation);
                        }
                        def
                    })
                    .collect::<Vec<_>>();
                for (index, value) in values.iter().enumerate() {
                    self.visit_expr(scope, value);
                    if let Some(base) = local_defs.get(index).copied() {
                        self.seed_table_literal_properties(base, value);
                    }
                }
            }
            Stat::Assign { vars, values, .. } => {
                for value in values {
                    self.visit_expr(scope, value);
                }
                for (index, var) in vars.iter().enumerate() {
                    let def = self.visit_lvalue_write(scope, var);
                    if let Some(value) = values.get(index) {
                        self.seed_table_literal_properties(def, value);
                    }
                }
            }
            Stat::CompoundAssign { var, value, .. } => {
                self.visit_expr(scope, var);
                self.visit_expr(scope, value);
                self.visit_lvalue_write(scope, var);
            }
            Stat::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.visit_expr(scope, condition);
                let base_defs = self.current_defs.clone();
                let base_versions = self.key_versions.clone();
                let base_graph_current = self.graph.current_key_defs.clone();

                let then_scope = self.enter_child_with_kind(scope, DfgScope::Linear);
                self.visit_stat(then_scope, then_body);
                let then_defs = self.current_defs.clone();
                let then_versions = self.key_versions.clone();

                self.current_defs = base_defs.clone();
                self.key_versions = base_versions.clone();
                self.graph.current_key_defs = base_graph_current.clone();
                let (else_defs, else_versions) = if let Some(else_body) = else_body {
                    let else_scope = self.enter_child_with_kind(scope, DfgScope::Linear);
                    self.visit_stat(else_scope, else_body);
                    (self.current_defs.clone(), self.key_versions.clone())
                } else {
                    (base_defs.clone(), base_versions.clone())
                };

                self.current_defs = base_defs.clone();
                self.key_versions = base_versions;
                self.graph.current_key_defs = base_graph_current;
                for (key, version) in then_versions.into_iter().chain(else_versions) {
                    let entry = self.key_versions.entry(key).or_default();
                    *entry = (*entry).max(version);
                }
                self.join_branch_defs(scope, &base_defs, &[&then_defs, &else_defs]);
            }
            Stat::Break { .. } | Stat::Continue { .. } => {}
            Stat::While {
                condition, body, ..
            } => {
                self.visit_expr(scope, condition);
                let base_defs = self.current_defs.clone();
                let base_versions = self.key_versions.clone();
                let base_graph_current = self.graph.current_key_defs.clone();
                let body_scope = self.enter_child_with_kind(scope, DfgScope::Loop);
                self.visit_stat(body_scope, body);
                let body_defs = self.current_defs.clone();
                self.current_defs = base_defs.clone();
                self.key_versions = base_versions;
                self.graph.current_key_defs = base_graph_current;
                self.join_branch_defs(scope, &base_defs, &[&base_defs, &body_defs]);
            }
            Stat::Repeat {
                condition, body, ..
            } => {
                let base_defs = self.current_defs.clone();
                let base_versions = self.key_versions.clone();
                let base_graph_current = self.graph.current_key_defs.clone();
                let body_scope = self.enter_child_with_kind(scope, DfgScope::Loop);
                self.visit_stat(body_scope, body);
                self.visit_expr(body_scope, condition);
                let body_defs = self.current_defs.clone();
                self.current_defs = base_defs.clone();
                self.key_versions = base_versions;
                self.graph.current_key_defs = base_graph_current;
                self.commit_repeat_defs(&base_defs, &body_defs);
            }
            Stat::For {
                var,
                from,
                to,
                step,
                body,
                ..
            } => {
                self.visit_expr(scope, from);
                self.visit_expr(scope, to);
                if let Some(step) = step {
                    self.visit_expr(scope, step);
                }
                let base_defs = self.current_defs.clone();
                let base_versions = self.key_versions.clone();
                let base_graph_current = self.graph.current_key_defs.clone();
                let body_scope = self.enter_child_with_kind(scope, DfgScope::Loop);
                self.define_local(body_scope, var);
                self.visit_stat(body_scope, body);
                let body_defs = self.current_defs.clone();
                self.current_defs = base_defs.clone();
                self.key_versions = base_versions;
                self.graph.current_key_defs = base_graph_current;
                self.join_branch_defs(scope, &base_defs, &[&base_defs, &body_defs]);
            }
            Stat::ForIn {
                vars, values, body, ..
            } => {
                for value in values {
                    self.visit_expr(scope, value);
                }
                let base_defs = self.current_defs.clone();
                let base_versions = self.key_versions.clone();
                let base_graph_current = self.graph.current_key_defs.clone();
                let body_scope = self.enter_child_with_kind(scope, DfgScope::Loop);
                for local in vars {
                    self.define_local(body_scope, local);
                }
                self.visit_stat(body_scope, body);
                let body_defs = self.current_defs.clone();
                self.current_defs = base_defs.clone();
                self.key_versions = base_versions;
                self.graph.current_key_defs = base_graph_current;
                self.join_branch_defs(scope, &base_defs, &[&base_defs, &body_defs]);
            }
            Stat::Function { name, func, .. } => {
                let def = self.visit_lvalue_write(scope, name);
                self.visit_expr(scope, func);
                self.seed_table_literal_properties(def, func);
            }
            Stat::LocalFunction { name, func, .. } => {
                let def = self.define_local(scope, name);
                self.write_key(scope, &RefinementKey::Symbol(Symbol::local(name.id)));
                self.visit_expr(scope, func);
                self.seed_table_literal_properties(def, func);
            }
            Stat::DeclareGlobal { luau_type, .. } => {
                // The annotation may embed a `typeof(expr)`, whose inner
                // expression needs a data-flow def just like a type alias body.
                self.visit_type(scope, luau_type);
            }
            Stat::DeclareClass { .. } | Stat::TypeFunction { .. } | Stat::ClassProperty { .. } => {}
            Stat::DeclareFunction { .. } => {
                self.enter_child_with_kind(scope, DfgScope::Function);
            }
            Stat::TypeAlias { value, .. } => {
                let alias_scope = self.enter_child_with_kind(scope, DfgScope::Linear);
                self.visit_type(alias_scope, value);
            }
            Stat::Class { members, .. } => {
                let class_scope = self.enter_child_with_kind(scope, DfgScope::Linear);
                for member in members {
                    self.visit_stat(class_scope, member);
                }
            }
            Stat::Error {
                expressions,
                statements,
                ..
            } => {
                for expr in expressions {
                    self.visit_expr(scope, expr);
                }
                for stat in statements {
                    self.visit_stat(scope, stat);
                }
            }
        }
    }

    fn visit_expr(&mut self, scope: ScopeId, expr: &Expr) -> DefId {
        let refinement_key = self
            .scopes
            .symbol_for_expression(scope, expr.syntax_id())
            .cloned()
            .map(RefinementKey::Symbol);
        let key = refinement_key.clone().map(|key| self.graph.intern_key(key));
        let ty = self.placeholder_type(format!("expr{}", expr.syntax_id().index()));
        let id = self.graph.push(Def {
            kind: DefKind::Expression {
                syntax_id: expr.syntax_id(),
            },
            ty,
            scope,
            key,
        });
        if let Some(current) = refinement_key.as_ref().and_then(|key| {
            if self.is_captured_key(scope, key) {
                Some(self.capture_phi_for_key(scope, key.clone()))
            } else {
                self.current_for_key(key)
            }
        }) {
            self.graph
                .flow_expressions
                .insert(expr.syntax_id(), current);
        }

        match expr {
            Expr::Nil { .. }
            | Expr::Bool { .. }
            | Expr::Number { .. }
            | Expr::Integer { .. }
            | Expr::String { .. }
            | Expr::Global { .. }
            | Expr::Local { .. }
            | Expr::Varargs { .. } => {}
            Expr::Call {
                func,
                type_arguments,
                args,
                ..
            } => {
                self.visit_expr(scope, func);
                for parameter in type_arguments {
                    match parameter {
                        ruau_ast::syntax::TypeParameter::Type(ty) => self.visit_type(scope, ty),
                        ruau_ast::syntax::TypeParameter::Pack(pack) => {
                            self.visit_expression_type_pack_argument(scope, pack);
                        }
                    }
                }
                for arg in args {
                    self.visit_expr(scope, arg);
                }
            }
            Expr::Binary { left, right, .. } => {
                self.visit_expr(scope, left);
                self.visit_expr(scope, right);
            }
            Expr::Unary { expr, .. } | Expr::Group { expr, .. } => {
                self.visit_expr(scope, expr);
            }
            Expr::IfElse {
                condition,
                true_expr,
                false_expr,
                ..
            } => {
                self.visit_expr(scope, condition);
                self.visit_expr(scope, true_expr);
                self.visit_expr(scope, false_expr);
            }
            Expr::TypeAssertion { expr, .. } => {
                self.visit_expr(scope, expr);
            }
            Expr::IndexName { expr, index, .. } => {
                let base = self.visit_expr(scope, expr);
                let base = self.canonical_refinement_base(base);
                let refinement_key = RefinementKey::Property {
                    base,
                    name: index.as_str().to_owned(),
                };
                let key = self.graph.intern_key(refinement_key.clone());
                self.graph.defs[id.index()].key = Some(key);
                self.graph.key_defs.insert(key, id);
                if let Some(current) = self.current_for_key(&refinement_key) {
                    self.graph
                        .flow_expressions
                        .insert(expr.syntax_id(), current);
                }
            }
            Expr::IndexExpr { expr, index, .. } => {
                self.visit_expr(scope, expr);
                self.visit_expr(scope, index);
            }
            Expr::Table { items, .. } => {
                for item in items {
                    if let Some(key) = &item.key {
                        self.visit_expr(scope, key);
                    }
                    self.visit_expr(scope, &item.value);
                }
            }
            Expr::InterpString { expressions, .. } | Expr::Error { expressions, .. } => {
                for expr in expressions {
                    self.visit_expr(scope, expr);
                }
            }
            Expr::Function {
                args,
                self_arg,
                vararg_annotation,
                return_annotation,
                body,
                ..
            } => {
                let captured_defs = self.current_defs.clone();
                let captured_versions = self.key_versions.clone();
                let captured_graph_current = self.graph.current_key_defs.clone();
                let function_scope = self.enter_child_with_kind(scope, DfgScope::Function);
                self.current_defs = captured_defs.clone();
                self.key_versions = captured_versions.clone();
                self.function_scopes.push(function_scope);
                self.function_capture_phis.push(BTreeMap::new());
                if let Some(self_arg) = self_arg {
                    self.define_local(function_scope, self_arg);
                    if let Some(annotation) = &self_arg.luau_type {
                        self.visit_type(function_scope, annotation);
                    }
                }
                for arg in args {
                    self.define_local(function_scope, arg);
                    if let Some(annotation) = &arg.luau_type {
                        self.visit_type(function_scope, annotation);
                    }
                }
                if let Some(vararg_annotation) = vararg_annotation {
                    self.visit_type_pack(function_scope, vararg_annotation);
                }
                if let Some(return_annotation) = return_annotation {
                    self.visit_type_pack(function_scope, return_annotation);
                }
                let body_scope = self.enter_child_with_kind(function_scope, DfgScope::Function);
                self.visit_stat(body_scope, body);
                self.function_capture_phis.pop();
                self.function_scopes.pop();
                self.current_defs = captured_defs;
                self.key_versions = captured_versions;
                self.graph.current_key_defs = captured_graph_current;
            }
            Expr::Instantiate {
                expr,
                type_arguments,
                ..
            } => {
                self.visit_expr(scope, expr);
                for parameter in type_arguments {
                    match parameter {
                        ruau_ast::syntax::TypeParameter::Type(ty) => self.visit_type(scope, ty),
                        ruau_ast::syntax::TypeParameter::Pack(pack) => {
                            self.visit_expression_type_pack_argument(scope, pack);
                        }
                    }
                }
            }
        }

        id
    }

    /// Walks a type annotation so that any nested `typeof(expr)` expressions
    /// get DFG defs. Other type shapes only recurse into the parts that may
    /// contain expressions or nested types.
    fn visit_type(&mut self, scope: ScopeId, ty: &Type) {
        match ty {
            Type::Reference { parameters, .. } => {
                for parameter in parameters {
                    match parameter {
                        ruau_ast::syntax::TypeParameter::Type(inner) => {
                            self.visit_type(scope, inner)
                        }
                        ruau_ast::syntax::TypeParameter::Pack(pack) => {
                            self.visit_type_pack(scope, pack)
                        }
                    }
                }
            }
            Type::Typeof { expr, .. } => {
                self.visit_expr(scope, expr);
            }
            Type::Group { inner, .. } => self.visit_type(scope, inner),
            Type::Union { types, .. } | Type::Intersection { types, .. } => {
                for inner in types {
                    self.visit_type(scope, inner);
                }
            }
            Type::Function {
                arg_types,
                return_types,
                ..
            } => {
                for inner in &arg_types.types {
                    self.visit_type(scope, inner);
                }
                if let Some(tail) = &arg_types.tail_type {
                    self.visit_type_pack(scope, tail);
                }
                self.visit_type_pack(scope, return_types);
            }
            Type::Table { props, indexer, .. } => {
                for prop in props {
                    self.visit_type(scope, &prop.prop_type);
                }
                if let Some(indexer) = indexer {
                    self.visit_type(scope, &indexer.index_type);
                    self.visit_type(scope, &indexer.result_type);
                }
            }
            Type::Error { types, .. } => {
                for inner in types {
                    self.visit_type(scope, inner);
                }
            }
            Type::Optional { .. } | Type::SingletonString { .. } | Type::SingletonBool { .. } => {}
        }
    }

    fn visit_type_pack(&mut self, scope: ScopeId, pack: &TypePack) {
        match pack {
            TypePack::Explicit { type_list, .. } => {
                for inner in &type_list.types {
                    self.visit_type(scope, inner);
                }
                if let Some(tail) = &type_list.tail_type {
                    self.visit_type_pack(scope, tail);
                }
            }
            TypePack::Variadic { variadic_type, .. } => self.visit_type(scope, variadic_type),
            TypePack::Generic { .. } => {}
        }
    }

    fn visit_expression_type_pack_argument(&mut self, scope: ScopeId, pack: &TypePack) {
        match pack {
            TypePack::Explicit { type_list, .. } => {
                for inner in &type_list.types {
                    self.visit_type(scope, inner);
                }
            }
            TypePack::Variadic { variadic_type, .. } => self.visit_type(scope, variadic_type),
            TypePack::Generic { .. } => {}
        }
    }

    fn canonical_refinement_base(&self, base: DefId) -> DefId {
        let Some(key) = self.graph.defs[base.index()].key else {
            return base;
        };
        let refinement_key = self.graph.refinement_key(key);
        self.current_for_key(refinement_key)
            .or_else(|| self.graph.key_defs.get(&key).copied())
            .unwrap_or(base)
    }

    fn define_local(&mut self, scope: ScopeId, local: &Local) -> DefId {
        let symbol = Symbol::local(local.id);
        let refinement_key = RefinementKey::Symbol(symbol);
        let key = self.graph.intern_key(refinement_key.clone());
        let ty = self.placeholder_type(format!("local{}", local.id.index()));
        let id = self.graph.push(Def {
            kind: DefKind::Local {
                local: local.id,
                name: local.name.as_str().to_owned(),
            },
            ty,
            scope,
            key: Some(key),
        });
        self.set_current(refinement_key, id);
        id
    }

    fn placeholder_type(&mut self, name: String) -> TypeId {
        self.arena.alloc(TypeKind::Free(TypeVariable {
            level: TypeLevel(0),
            name: Some(name),
            lower_bound: None,
            upper_bound: None,
        }))
    }
}

#[cfg(any())]
mod tests;
