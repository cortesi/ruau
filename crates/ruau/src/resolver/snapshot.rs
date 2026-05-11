//! Resolved module graph snapshots.

use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    result::Result as StdResult,
};

use super::{
    LocalResolveFuture, ModuleId, ModuleResolveError, ModuleResolver, ModuleSource,
    require_spec::require_specifiers,
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
        if !root.is_executable() {
            return Err(ModuleResolveError::NotExecutable(root.id().to_string()));
        }
        let root_id = root.id.clone();
        let mut modules = BTreeMap::new();
        modules.insert(root.id.clone(), root);
        let mut edges = BTreeMap::new();
        let mut queue = VecDeque::from([root_id.clone()]);
        let mut queued = HashSet::from([root_id.clone()]);

        while let Some(id) = queue.pop_front() {
            let (source_id, requires) = {
                let source = modules
                    .get(&id)
                    .expect("queued resolver snapshot module is missing");
                (
                    source.id.clone(),
                    require_specifiers(source.id(), source.source())?,
                )
            };
            for required in requires {
                let dep = resolver
                    .resolve(Some(&source_id), &required.specifier)
                    .await?;
                if !dep.is_executable() {
                    return Err(ModuleResolveError::NotExecutable(dep.id().to_string()));
                }
                edges
                    .entry(source_id.clone())
                    .or_insert_with(BTreeMap::new)
                    .insert(required.specifier, dep.id.clone());
                if queued.insert(dep.id.clone()) {
                    let dep_id = dep.id.clone();
                    modules.insert(dep.id.clone(), dep);
                    queue.push_back(dep_id);
                }
            }
        }

        Ok(Self {
            root: root_id,
            modules,
            edges,
        })
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
