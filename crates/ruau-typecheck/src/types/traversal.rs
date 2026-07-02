//! Type-graph traversal helpers used by structural tests.

#[cfg(any())]
use std::collections::VecDeque;

use super::{Arena, TypeId};
#[cfg(any())]
use super::{TypeKind, TypePackId, TypePackTail};

impl Arena {
    /// Traces a type graph in the same broad order as upstream's iterative
    /// visitor: nodes are visited before their nested children at the next
    /// depth, repeated nodes can be skipped, and active-path cycles are
    /// reported without recursing forever.
    #[must_use]
    #[cfg(any())]
    pub(crate) fn trace_type(&self, id: TypeId, options: TypeTraversalOptions) -> TypeTraversal {
        TypeWalker::new(self, options).trace(id)
    }
}

/// Options for [`Arena::trace_type`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TypeTraversalOptions {
    /// Visit each type id at most once.
    pub visit_once: bool,
    /// Follow bound types directly to their target.
    pub skip_bound_types: bool,
    /// Visit table nodes and their properties.
    pub visit_table_types: bool,
    /// Maximum type-depth to traverse before reporting a limit.
    pub recursion_limit: Option<usize>,
}

impl Default for TypeTraversalOptions {
    fn default() -> Self {
        Self {
            visit_once: true,
            skip_bound_types: true,
            visit_table_types: true,
            recursion_limit: None,
        }
    }
}

/// Result of tracing a type graph.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TypeTraversal {
    /// Visited type ids, in traversal order.
    pub visited: Vec<TypeId>,
    /// Type ids encountered again on an active traversal path.
    pub cycles: Vec<TypeId>,
    /// Whether traversal stopped at least one branch because the configured
    /// depth limit was exceeded.
    pub limit_exceeded: bool,
}

impl TypeTraversal {
    /// Renders visited type ids with the arena summary renderer.
    #[must_use]
    pub fn visited_summaries(&self, arena: &Arena) -> Vec<String> {
        self.visited.iter().map(|id| arena.summary(*id)).collect()
    }
}

/// Work item for breadth-first type traversal.
#[derive(Clone, Debug)]
#[cfg(any())]
struct TypeVisitItem {
    /// Type to visit.
    id: TypeId,
    /// Current type depth.
    depth: usize,
    /// Type ids on the path to this item.
    ancestors: Vec<TypeId>,
}

/// Type graph traversal worker.
#[cfg(any())]
struct TypeWalker<'arena> {
    /// Type arena being traversed.
    arena: &'arena Arena,
    /// Traversal options.
    options: TypeTraversalOptions,
    /// Completed traversal.
    traversal: TypeTraversal,
    /// Once-only visited set.
    visited_once: Vec<TypeId>,
}

#[cfg(any())]
impl<'arena> TypeWalker<'arena> {
    /// Creates a walker.
    fn new(arena: &'arena Arena, options: TypeTraversalOptions) -> Self {
        Self {
            arena,
            options,
            traversal: TypeTraversal::default(),
            visited_once: Vec::new(),
        }
    }

    /// Runs traversal from `root`.
    fn trace(mut self, root: TypeId) -> TypeTraversal {
        let mut queue = VecDeque::from([TypeVisitItem {
            id: root,
            depth: 0,
            ancestors: Vec::new(),
        }]);

        while let Some(item) = queue.pop_front() {
            self.visit(item, &mut queue);
        }

        self.traversal
    }

    /// Visits one queued type.
    fn visit(&mut self, mut item: TypeVisitItem, queue: &mut VecDeque<TypeVisitItem>) {
        if let Some(limit) = self.options.recursion_limit
            && item.depth > limit
        {
            self.traversal.limit_exceeded = true;
            return;
        }

        if self.options.skip_bound_types {
            while let TypeKind::Bound(bound) = self.arena.get(item.id) {
                item.id = *bound;
            }
        }

        if item.ancestors.contains(&item.id) {
            self.traversal.cycles.push(item.id);
            return;
        }

        if self.options.visit_once && self.visited_once.contains(&item.id) {
            return;
        }

        if matches!(self.arena.get(item.id), TypeKind::Table(_)) && !self.options.visit_table_types
        {
            return;
        }

        if self.options.visit_once {
            self.visited_once.push(item.id);
        }
        self.traversal.visited.push(item.id);

        let mut ancestors = item.ancestors;
        ancestors.push(item.id);
        for child in self.children(item.id) {
            queue.push_back(TypeVisitItem {
                id: child,
                depth: item.depth + 1,
                ancestors: ancestors.clone(),
            });
        }
    }

    /// Returns direct child types in upstream visitor order.
    fn children(&self, id: TypeId) -> Vec<TypeId> {
        match self.arena.get(id) {
            TypeKind::Function(function) => {
                let mut children = Vec::new();
                self.push_pack_children(function.arguments, &mut children);
                self.push_pack_children(function.returns, &mut children);
                children
            }
            TypeKind::Table(table) => table
                .properties
                .values()
                .map(|property| property.ty)
                .chain(
                    table
                        .indexer
                        .iter()
                        .flat_map(|indexer| [indexer.key, indexer.value]),
                )
                .collect(),
            TypeKind::Metatable {
                table, metatable, ..
            } => vec![*table, *metatable],
            TypeKind::TypeFunctionInstance { arguments, .. } => arguments.clone(),
            TypeKind::Union(types) | TypeKind::Intersection(types) => types.clone(),
            TypeKind::Negation(ty) => vec![*ty],
            TypeKind::Bound(bound) if !self.options.skip_bound_types => vec![*bound],
            TypeKind::Free(variable) => [variable.lower_bound, variable.upper_bound]
                .into_iter()
                .flatten()
                .collect(),
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Bound(_)
            | TypeKind::Generic(_)
            | TypeKind::Blocked(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => Vec::new(),
        }
    }

    /// Pushes direct type children from a type pack.
    fn push_pack_children(&self, id: TypePackId, children: &mut Vec<TypeId>) {
        let normalized = self.arena.normalize_pack(id);
        children.extend(normalized.types);
        if let Some(TypePackTail::Variadic(ty)) = normalized.tail {
            children.push(ty);
        }
    }
}
