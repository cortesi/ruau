//! Module require graph: nodes, bidirectional require edges, and the
//! topology queries (dependent traversal, cycle detection, cycle paths) that
//! run over them.
//!
//! [`RequireGraph`] owns the `ModuleName -> SourceNode` map and is the single
//! place that maintains the forward (`requires`) / reverse (`dependents`) edge
//! invariant. [`crate::graph_checker::Frontend`] drives parsing and caching on top of
//! it.

use std::collections::{BTreeMap, BTreeSet};

use ruau_source::ModuleName;

/// Source graph node tracked by the require graph.
///
/// Fields are private: the forward/reverse edge invariant and the dirty flag
/// are maintained by the graph, not mutated from outside.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceNode {
    /// Unique required modules. Per-call locations live in the require trace.
    requires: BTreeSet<ModuleName>,
    /// Modules that require this module.
    dependents: BTreeSet<ModuleName>,
    /// Whether the parsed source for this node is stale.
    dirty: bool,
}

impl SourceNode {
    /// Creates a clean node from its forward and reverse edge sets.
    #[must_use]
    pub(crate) fn new(requires: BTreeSet<ModuleName>, dependents: BTreeSet<ModuleName>) -> Self {
        Self {
            requires,
            dependents,
            dirty: false,
        }
    }

    /// Returns the modules this node requires.
    #[must_use]
    pub const fn requires(&self) -> &BTreeSet<ModuleName> {
        &self.requires
    }

    /// Returns the modules that require this node.
    #[must_use]
    pub const fn dependents(&self) -> &BTreeSet<ModuleName> {
        &self.dependents
    }

    /// Returns whether this node's parsed source is stale.
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }
}

/// Static module require graph keyed by module name.
#[derive(Debug, Default)]
pub struct RequireGraph {
    nodes: BTreeMap<ModuleName, SourceNode>,
}

impl RequireGraph {
    /// Returns one graph node.
    pub fn node(&self, name: &ModuleName) -> Option<&SourceNode> {
        self.nodes.get(name)
    }

    /// Returns whether a node exists and its parsed source is current.
    pub fn is_clean(&self, name: &ModuleName) -> bool {
        self.nodes.get(name).is_some_and(|node| !node.dirty)
    }

    /// Inserts or replaces a node.
    pub fn insert(&mut self, name: ModuleName, node: SourceNode) {
        self.nodes.insert(name, node);
    }

    /// Records that `dependent` requires `dependency`, when `dependency` exists.
    pub fn link_dependent(&mut self, dependency: &ModuleName, dependent: &ModuleName) {
        if let Some(node) = self.nodes.get_mut(dependency) {
            node.dependents.insert(dependent.clone());
        }
    }

    /// Iterates graph nodes by module name.
    pub fn iter(&self) -> impl Iterator<Item = (&ModuleName, &SourceNode)> {
        self.nodes.iter()
    }

    /// Removes the reverse edges left by a node's current required modules.
    pub fn unlink_forward_edges(&mut self, name: &ModuleName) {
        let Some(dependencies) = self.nodes.get(name).map(|node| node.requires().clone()) else {
            return;
        };
        for dependency in dependencies {
            if let Some(node) = self.nodes.get_mut(&dependency) {
                node.dependents.remove(name);
            }
        }
    }

    /// Removes a node and the reverse edges its requires contributed.
    pub fn remove(&mut self, name: &ModuleName) {
        self.unlink_forward_edges(name);
        self.nodes.remove(name);
    }

    /// Drops all nodes.
    pub fn clear(&mut self) {
        self.nodes.clear();
    }

    /// Traverses known dependents, including the starting node.
    ///
    /// `descend` decides whether to descend into a node's dependents; returning
    /// `false` prunes that subtree.
    #[cfg(any())]
    pub fn traverse_dependents(
        &self,
        start: &ModuleName,
        mut descend: impl FnMut(&ModuleName) -> bool,
    ) {
        walk_dependent_subtree(&self.nodes, start, |name, node| {
            node.is_some() && descend(name)
        });
    }

    /// Marks a node and its transitive dependents dirty, returning the names of
    /// nodes newly marked by this call in traversal order.
    pub fn mark_dirty_subtree(&mut self, start: &ModuleName) -> Vec<ModuleName> {
        let mut marked = Vec::new();
        walk_dependent_subtree(&self.nodes, start, |name, node| {
            let Some(node) = node else {
                return false;
            };
            if node.is_dirty() {
                return false;
            }
            marked.push(name.clone());
            true
        });
        for name in &marked {
            if let Some(node) = self.nodes.get_mut(name) {
                node.dirty = true;
            }
        }
        marked
    }

    /// Returns the set of modules participating in a require cycle.
    pub fn cyclic_modules(&self) -> BTreeSet<ModuleName> {
        let mut tarjan = Tarjan {
            graph: self,
            index: 0,
            stack: Vec::new(),
            indices: BTreeMap::new(),
            lowlinks: BTreeMap::new(),
            on_stack: BTreeSet::new(),
            cyclic: BTreeSet::new(),
        };
        for name in self.nodes.keys() {
            if !tarjan.indices.contains_key(name) {
                tarjan.visit(name);
            }
        }
        tarjan.cyclic
    }

    /// Returns one require path from `from` back to `target`, if one exists.
    pub fn cycle_path(&self, from: &ModuleName, target: &ModuleName) -> Option<Vec<ModuleName>> {
        let mut path = Vec::new();
        let mut seen = BTreeSet::new();
        self.find_cycle_path(from, target, &mut seen, &mut path)
            .then_some(path)
    }

    /// Finds one dependency path from `current` to `target`.
    fn find_cycle_path(
        &self,
        current: &ModuleName,
        target: &ModuleName,
        seen: &mut BTreeSet<ModuleName>,
        path: &mut Vec<ModuleName>,
    ) -> bool {
        if !seen.insert(current.clone()) {
            return false;
        }

        path.push(current.clone());
        if current == target {
            return true;
        }

        if let Some(node) = self.node(current) {
            for dependency in node.requires() {
                if self.find_cycle_path(dependency, target, seen, path) {
                    return true;
                }
            }
        }

        path.pop();
        false
    }
}

/// Tarjan's strongly connected components scan over the require graph.
struct Tarjan<'a> {
    graph: &'a RequireGraph,
    index: usize,
    stack: Vec<ModuleName>,
    indices: BTreeMap<ModuleName, usize>,
    lowlinks: BTreeMap<ModuleName, usize>,
    on_stack: BTreeSet<ModuleName>,
    cyclic: BTreeSet<ModuleName>,
}

impl Tarjan<'_> {
    fn visit(&mut self, node: &ModuleName) {
        let node_index = self.index;
        self.index += 1;
        self.indices.insert(node.clone(), node_index);
        self.lowlinks.insert(node.clone(), node_index);
        self.stack.push(node.clone());
        self.on_stack.insert(node.clone());

        let mut node_lowlink = node_index;
        for dependency in self
            .graph
            .node(node)
            .into_iter()
            .flat_map(|node| node.requires())
        {
            let candidate = if let Some(&dep_index) = self.indices.get(dependency) {
                self.on_stack.contains(dependency).then_some(dep_index)
            } else {
                self.visit(dependency);
                Some(self.lowlinks[dependency])
            };
            if let Some(candidate) = candidate {
                node_lowlink = node_lowlink.min(candidate);
            }
        }
        self.lowlinks.insert(node.clone(), node_lowlink);

        if node_lowlink != node_index {
            return;
        }

        let mut scc = Vec::new();
        while let Some(member) = self.stack.pop() {
            self.on_stack.remove(&member);
            scc.push(member);
            if scc.last() == Some(node) {
                break;
            }
        }

        let is_cyclic = scc.len() > 1
            || scc.first().is_some_and(|name| {
                self.graph
                    .node(name)
                    .is_some_and(|node| node.requires().contains(name))
            });
        if is_cyclic {
            self.cyclic.extend(scc);
        }
    }
}

/// Walks `start` and transitive dependents depth-first.
///
/// `visit` receives each reached name and its node when one exists. Returning
/// `false` skips descending into that node's dependents.
fn walk_dependent_subtree(
    nodes: &BTreeMap<ModuleName, SourceNode>,
    start: &ModuleName,
    mut visit: impl FnMut(&ModuleName, Option<&SourceNode>) -> bool,
) {
    if nodes.get(start).is_none() {
        return;
    }

    let mut stack = vec![start.clone()];
    let mut seen = BTreeSet::new();

    while let Some(next) = stack.pop() {
        if !seen.insert(next.clone()) {
            continue;
        }

        let node = nodes.get(&next);
        if !visit(&next, node) {
            continue;
        }
        if let Some(node) = node {
            stack.extend(node.dependents().iter().cloned());
        }
    }
}
