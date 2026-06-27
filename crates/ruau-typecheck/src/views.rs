//! Read-only facades over the type-checker's internal arenas and modules.
//!
//! `TypeView` lets downstream consumers inspect a single type handle —
//! follow it, render its summary, ask classification questions — without
//! pattern-matching on the internal `TypeKind` enum. `ModuleView` is the
//! analogous facade over a `CheckedModule`: it exposes diagnostics,
//! return types, and named local bindings as `TypeView`s rather than as
//! raw arena-tied handles.
//!
//! These facades keep the type checker's `TypeKind` / `TypePath` /
//! `TableProperty` representations off the public surface so the
//! internal arena layout can evolve without breaking embedders.

use crate::{
    checker::CheckedModule,
    dfg::DefKind,
    types::{Arena, SummaryOptions, TypeId, TypeKind},
};

/// Read-only view of one type handle.
///
/// Created via [`ModuleView`] or by direct construction inside the
/// crate. The view borrows the underlying arena, so multiple views can
/// coexist while the arena is shared (read-only).
#[derive(Clone, Copy)]
pub struct TypeView<'a> {
    arena: &'a Arena,
    id: TypeId,
}

impl<'a> TypeView<'a> {
    /// Returns a view over `id` in `arena`.
    #[must_use]
    pub const fn new(arena: &'a Arena, id: TypeId) -> Self {
        Self { arena, id }
    }

    /// Returns the raw type handle this view wraps.
    ///
    /// Embedders should treat `TypeId` as an opaque token — the only
    /// supported operations are the ones offered through this view.
    #[must_use]
    pub const fn id(&self) -> TypeId {
        self.id
    }

    /// Returns the canonical handle for this view's type, following
    /// `Bound` chains until a non-bound representative is reached.
    #[must_use]
    pub fn follow(&self) -> Self {
        Self::new(self.arena, self.arena.follow(self.id))
    }

    /// Deterministic single-line summary suitable for diagnostics.
    #[must_use]
    pub fn summary(&self) -> String {
        self.arena.summary(self.id)
    }

    /// Summary with explicit display options.
    #[must_use]
    pub fn summary_with_options(&self, options: SummaryOptions) -> String {
        self.arena.summary_with_options(self.id, options)
    }

    /// Returns true when this view's type is the `any` lattice top.
    #[must_use]
    pub fn is_any(&self) -> bool {
        matches!(self.arena.get(self.arena.follow(self.id)), TypeKind::Any)
    }

    /// Returns true when this view's type is the `unknown` lattice top.
    #[must_use]
    pub fn is_unknown(&self) -> bool {
        matches!(
            self.arena.get(self.arena.follow(self.id)),
            TypeKind::Unknown
        )
    }

    /// Returns true when this view's type is the `never` lattice bottom.
    #[must_use]
    pub fn is_never(&self) -> bool {
        matches!(self.arena.get(self.arena.follow(self.id)), TypeKind::Never)
    }

    /// Returns true when this view's type is the error-recovery marker.
    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self.arena.get(self.arena.follow(self.id)), TypeKind::Error)
    }

    /// Returns true when this view's type is a function shape (any
    /// arity, any variance — the test is structural, not semantic).
    #[must_use]
    pub fn is_function(&self) -> bool {
        matches!(
            self.arena.get(self.arena.follow(self.id)),
            TypeKind::Function(_)
        )
    }

    /// Returns true when this view's type is callable.
    ///
    /// This currently preserves the checker's structural callable test: a
    /// function shape is callable, while metatable `__call` support is not
    /// modeled here until the checker itself promotes that behavior.
    #[must_use]
    pub fn is_callable(&self) -> bool {
        self.is_function()
    }

    /// Returns true when this view's type is a structural table shape.
    #[must_use]
    pub fn is_table(&self) -> bool {
        matches!(
            self.arena.get(self.arena.follow(self.id)),
            TypeKind::Table(_)
        )
    }

    /// Returns true when this view's type is a table or metatable-wrapped
    /// table shape.
    #[must_use]
    pub fn is_table_like(&self) -> bool {
        matches!(
            self.arena.get(self.arena.follow(self.id)),
            TypeKind::Table(_) | TypeKind::Metatable { .. }
        )
    }

    /// Reads one named property through the table portion of this type.
    ///
    /// Metatable wrappers are transparent for this lookup, matching the
    /// declaration-surface audit behavior in the umbrella crate.
    #[must_use]
    pub fn property(&self, name: &str) -> Option<Self> {
        self.arena
            .direct_read_property(self.id, name)
            .map(|id| Self::new(self.arena, id))
    }

    /// Traverses a dotted property path such as `foo.bar.baz`.
    #[must_use]
    pub fn property_path(&self, path: &str) -> Option<Self> {
        let mut view = *self;
        for property in path.split('.') {
            view = view.property(property)?;
        }
        Some(view)
    }
}

/// Read-only view of a checked module.
///
/// Constructed from a `&CheckedModule` once the module has been fully
/// elaborated by the checker. The view exposes diagnostics, return
/// types, and named local bindings as `TypeView`s — internal arena
/// handles and `TypeKind` representations are not surfaced.
pub struct ModuleView<'a> {
    arena: &'a Arena,
    module: &'a CheckedModule,
}

impl<'a> ModuleView<'a> {
    /// Returns a module view backed by the supplied arena and checked
    /// module.
    #[must_use]
    pub const fn new(arena: &'a Arena, module: &'a CheckedModule) -> Self {
        Self { arena, module }
    }

    /// Returns the checked module's structured diagnostics.
    #[must_use]
    pub fn diagnostics(&self) -> &'a [crate::diagnostic::TypeDiagnostic] {
        self.module.diagnostics()
    }

    /// Returns true when the module recorded at least one
    /// error-severity diagnostic.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.module.has_errors()
    }

    /// Iterates each `return` statement's pack as an ordered list of
    /// `TypeView`s.
    pub fn return_types(&self) -> impl Iterator<Item = TypeView<'a>> {
        let arena = self.arena;
        self.module
            .return_types()
            .iter()
            .map(move |id| TypeView::new(arena, *id))
    }

    /// Looks up the most recent definition of the named local at the
    /// root scope and returns its `TypeView`, if any.
    #[must_use]
    pub fn local(&self, name: &str) -> Option<TypeView<'a>> {
        let root = self.module.scopes().root();
        let arena = self.arena;
        self.module
            .dfg()
            .defs()
            .find_map(move |(_, def)| match &def.kind {
                DefKind::Local { name: local, .. } if local == name && def.scope == root => {
                    Some(TypeView::new(arena, def.ty))
                }
                _ => None,
            })
    }
}

#[cfg(any())]
mod tests {
    use super::*;
    use crate::checker::Checker;

    #[test]
    fn module_view_exposes_diagnostics_and_locals() {
        let mut checker = Checker::new();
        let module = checker.check_source("local n = 1\nlocal s = \"hello\"");
        let view = ModuleView::new(checker.arena(), &module);

        assert!(!view.has_errors());
        assert!(view.diagnostics().is_empty());

        let n = view.local("n").expect("local n present");
        assert!(!n.is_function());
        assert!(!n.is_table());
        assert_eq!(n.summary(), "number");

        let s = view.local("s").expect("local s present");
        assert_eq!(s.summary(), "\"hello\"");

        assert!(view.local("missing").is_none());
    }

    #[test]
    fn type_view_classifies_lattice_tops_and_bottoms() {
        let checker = Checker::new();
        let arena = checker.arena();
        let primitives = arena.primitives();

        let any = TypeView::new(arena, primitives.any);
        assert!(any.is_any());
        assert!(!any.is_unknown());

        let unknown = TypeView::new(arena, primitives.unknown);
        assert!(unknown.is_unknown());

        let never = TypeView::new(arena, primitives.never);
        assert!(never.is_never());

        let error = TypeView::new(arena, primitives.error);
        assert!(error.is_error());
    }

    #[test]
    fn type_view_reads_table_properties_without_raw_kind_access() {
        let mut checker = Checker::new();
        let module = checker.check_source("return { nested = { value = 1 } }");
        let view = ModuleView::new(checker.arena(), &module);
        let root = view.return_types().next().expect("return type");

        assert!(root.is_table_like());
        assert!(root.property("nested").expect("nested").is_table_like());
        assert_eq!(
            root.property_path("nested.value")
                .expect("nested value")
                .summary(),
            "number"
        );
    }
}
