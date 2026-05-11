//! Resolved module graph snapshots.

use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    result::Result as StdResult,
};

use super::{
    LocalResolveFuture, ModuleId, ModuleResolveError, ModuleResolver, ModuleSource,
    RequireSpecifier, require_spec::require_specifiers,
};
use crate::analyzer::VirtualModule;

/// One resolved literal require edge in a [`ResolverSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequireEdge<'a> {
    /// Module containing the literal require call.
    pub requester: &'a ModuleId,
    /// Original string literal passed to `require`.
    pub specifier: &'a str,
    /// Module selected by the resolver for this edge.
    pub dependency: &'a ModuleId,
}

/// Immutable resolved graph used by checked loading.
///
/// Snapshot resolution is for runtime-loadable module graphs. It walks only direct string-literal
/// `require(...)` calls. Checked loading rejects unsupported dynamic require expressions during
/// analysis rather than adding them to this graph. If the resolver returns a
/// [`super::ModuleSourceKind::Interface`] root or dependency, resolution fails with
/// [`ModuleResolveError::NotExecutable`]. Feed declaration-only modules through
/// [`crate::analyzer::ModuleInterfaceSet`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverSnapshot {
    /// Root module id.
    root: ModuleId,
    /// Resolved modules keyed by id.
    modules: BTreeMap<ModuleId, ModuleSource>,
    /// Resolved dependency edges keyed by requesting module and original specifier.
    edges: BTreeMap<ModuleId, BTreeMap<String, ModuleId>>,
}

impl ResolverSnapshot {
    /// Resolves a root module and its direct string-literal dependencies.
    ///
    /// The resolver is not called for dynamic `require` expressions, so embedders that disallow
    /// dynamic requires should inspect [`ResolverSnapshot::require_edges`] or
    /// [`super::required_specifiers_with_spans`] before executing user code.
    pub async fn resolve<R: ModuleResolver + ?Sized>(
        resolver: &R,
        root: impl Into<ModuleId>,
    ) -> StdResult<Self, ModuleResolveError> {
        let root = resolver.resolve(None, root.into().as_str()).await?;
        let mut builder = SnapshotBuilder::new(root)?;

        while let Some(source_id) = builder.pop_queued() {
            let requires = builder.requires(&source_id)?;
            for required in requires {
                let dep = resolver
                    .resolve(Some(&source_id), &required.specifier)
                    .await?;
                builder.add_dependency(&source_id, required.specifier, dep)?;
            }
        }

        Ok(builder.finish())
    }

    /// Returns the root module id.
    #[must_use]
    pub fn root(&self) -> &ModuleId {
        &self.root
    }

    /// Returns the root module source.
    #[must_use]
    pub fn root_source(&self) -> Option<&ModuleSource> {
        self.modules.get(&self.root)
    }

    /// Returns all resolved module sources in stable id order.
    pub fn modules(&self) -> impl Iterator<Item = &ModuleSource> {
        self.modules.values()
    }

    /// Returns the module source for `specifier` as resolved from `requester`.
    #[must_use]
    pub fn dependency(&self, requester: &ModuleId, specifier: &str) -> Option<&ModuleSource> {
        self.edges
            .get(requester)
            .and_then(|edges| edges.get(specifier))
            .and_then(|id| self.modules.get(id))
            .or_else(|| self.modules.get(&ModuleId::new(specifier)))
    }

    /// Returns all literal require edges resolved into this snapshot.
    pub fn require_edges(&self) -> impl Iterator<Item = RequireEdge<'_>> {
        self.edges.iter().flat_map(|(requester, edges)| {
            edges
                .iter()
                .map(move |(specifier, dependency)| RequireEdge {
                    requester,
                    specifier,
                    dependency,
                })
        })
    }

    /// Returns non-root modules as analyzer virtual modules.
    #[must_use]
    pub fn virtual_modules(&self) -> Vec<VirtualModule<'_>> {
        self.modules
            .iter()
            .filter(|(id, _)| **id != self.root)
            .map(|(id, module)| VirtualModule {
                name: id.as_str(),
                source: module.source(),
            })
            .collect()
    }
}

/// Mutable graph assembly state for [`ResolverSnapshot::resolve`].
struct SnapshotBuilder {
    /// Root module id.
    root: ModuleId,
    /// Resolved modules keyed by id.
    modules: BTreeMap<ModuleId, ModuleSource>,
    /// Resolved dependency edges keyed by requesting module and original specifier.
    edges: BTreeMap<ModuleId, BTreeMap<String, ModuleId>>,
    /// Modules whose literal requires still need to be walked.
    queue: VecDeque<ModuleId>,
    /// Module ids already queued or walked.
    queued: HashSet<ModuleId>,
}

impl SnapshotBuilder {
    /// Creates a builder seeded with an executable root module.
    fn new(root: ModuleSource) -> StdResult<Self, ModuleResolveError> {
        ensure_executable(&root)?;
        let root_id = root.id.clone();
        let mut modules = BTreeMap::new();
        modules.insert(root_id.clone(), root);

        Ok(Self {
            root: root_id.clone(),
            modules,
            edges: BTreeMap::new(),
            queue: VecDeque::from([root_id.clone()]),
            queued: HashSet::from([root_id]),
        })
    }

    /// Pops the next queued module id.
    fn pop_queued(&mut self) -> Option<ModuleId> {
        self.queue.pop_front()
    }

    /// Returns the literal require calls for a queued module.
    fn requires(&self, id: &ModuleId) -> StdResult<Vec<RequireSpecifier>, ModuleResolveError> {
        let source = self
            .modules
            .get(id)
            .ok_or_else(|| ModuleResolveError::NotFound(id.to_string()))?;
        require_specifiers(source.id(), source.source())
    }

    /// Records a resolved dependency and queues it if this is the first time it was seen.
    fn add_dependency(
        &mut self,
        requester: &ModuleId,
        specifier: String,
        dependency: ModuleSource,
    ) -> StdResult<(), ModuleResolveError> {
        ensure_executable(&dependency)?;
        let dependency_id = dependency.id.clone();
        self.edges
            .entry(requester.clone())
            .or_default()
            .insert(specifier, dependency_id.clone());

        if self.queued.insert(dependency_id.clone()) {
            self.modules.insert(dependency_id.clone(), dependency);
            self.queue.push_back(dependency_id);
        }

        Ok(())
    }

    /// Finishes graph assembly.
    fn finish(self) -> ResolverSnapshot {
        ResolverSnapshot {
            root: self.root,
            modules: self.modules,
            edges: self.edges,
        }
    }
}

/// Rejects interface-only modules in runtime snapshots.
fn ensure_executable(source: &ModuleSource) -> StdResult<(), ModuleResolveError> {
    if source.is_executable() {
        Ok(())
    } else {
        Err(ModuleResolveError::NotExecutable(source.id().to_string()))
    }
}

impl ModuleResolver for ResolverSnapshot {
    fn resolve<'a>(
        &'a self,
        requester: Option<&'a ModuleId>,
        specifier: &'a str,
    ) -> LocalResolveFuture<'a> {
        Box::pin(async move {
            // The snapshot was already produced by walking a real resolver, so a missing entry
            // is a resolution error here too.
            let module = match requester {
                Some(req) => self.dependency(req, specifier),
                None => self.modules.get(&ModuleId::new(specifier)),
            }
            .ok_or_else(|| ModuleResolveError::NotFound(specifier.to_owned()))?;
            Ok(module.clone())
        })
    }
}
