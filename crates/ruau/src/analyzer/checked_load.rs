//! Checked-load integration between analyzer results and runtime require.

use std::rc::Rc;

use super::{AnalysisError, Checker, ModuleInterfaceSet};
use crate::{
    Chunk, Luau,
    resolver::{ModuleId, ModuleResolver, ResolverSnapshot},
    runtime::require::{RuntimeModuleCache, SharedResolver, resolver_environment},
};

impl Luau {
    /// Type-checks a resolver snapshot before returning a loadable root chunk.
    ///
    /// The snapshot uses the static require policy documented in [`crate::resolver`]. Unsupported
    /// dynamic require expressions are rejected by analysis before a chunk is returned.
    pub async fn checked_load(
        &self,
        checker: &mut Checker,
        snapshot: ResolverSnapshot,
    ) -> Result<Chunk<'static>, AnalysisError> {
        self.checked_load_with_interfaces(checker, snapshot, &ModuleInterfaceSet::new())
            .await
    }

    /// Type-checks a resolver snapshot with host interfaces before returning a loadable chunk.
    ///
    /// Use `interfaces` for declaration-only host modules. If a runtime resolver returns
    /// interface-only source, snapshot construction fails before this method runs.
    pub async fn checked_load_with_interfaces(
        &self,
        checker: &mut Checker,
        snapshot: ResolverSnapshot,
        interfaces: &ModuleInterfaceSet,
    ) -> Result<Chunk<'static>, AnalysisError> {
        let result = checker
            .check_snapshot_with_interfaces(&snapshot, interfaces)
            .await?;
        if !result.is_ok() {
            return Err(AnalysisError::CheckFailed(result));
        }

        let root = snapshot
            .root_source()
            .ok_or_else(|| AnalysisError::MissingSnapshotRoot(snapshot.root().to_string()))?;
        let root_id = root.id().clone();
        let root_source = root.source().to_owned();

        // Reuse the runtime resolver→`require` plumbing: ResolverSnapshot itself implements
        // ModuleResolver, so the same builder serves both checked load and live require.
        let resolver: SharedResolver = Rc::new(snapshot);
        let cache = RuntimeModuleCache::new();
        let env = resolver_environment(self, resolver, cache, Some(root_id.clone()))
            .map_err(|error| AnalysisError::Load(error.to_string()))?;

        Ok(self
            .load(root_source)
            .name(root_id.as_str())
            .environment(env))
    }

    /// Resolves, type-checks, and loads a root module in one step.
    pub async fn checked_load_resolved<R>(
        &self,
        checker: &mut Checker,
        resolver: &R,
        root: impl Into<ModuleId>,
    ) -> Result<Chunk<'static>, AnalysisError>
    where
        R: ModuleResolver + ?Sized,
    {
        let snapshot = ResolverSnapshot::resolve(resolver, root).await?;
        self.checked_load(checker, snapshot).await
    }
}
