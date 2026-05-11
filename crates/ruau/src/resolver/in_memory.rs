//! In-memory module resolution for tests and simple embedders.

use std::{collections::HashMap, path::Path};

use super::{
    LocalResolveFuture, ModuleId, ModuleResolveError, ModuleResolver, ModuleSource,
    path_util::normalize_path,
};

/// In-memory resolver for tests and embedders.
#[derive(Debug, Clone, Default)]
pub struct InMemoryResolver {
    /// Source text keyed by stable module id.
    modules: HashMap<ModuleId, String>,
}

impl InMemoryResolver {
    /// Creates an empty in-memory resolver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style insertion of a module by id.
    ///
    /// Replaces any module previously registered under the same id and discards the prior source.
    /// Use [`InMemoryResolver::insert_module`] when the prior source is needed or when working
    /// with a long-lived resolver after construction.
    #[must_use]
    pub fn with_module(mut self, id: impl Into<ModuleId>, source: impl Into<String>) -> Self {
        self.insert_module(id, source);
        self
    }

    /// Inserts a module by id, returning the previous source for that id if one was registered.
    ///
    /// Mirrors [`std::collections::HashMap::insert`]. Use [`InMemoryResolver::with_module`] for
    /// builder-style construction where the previous value is not needed.
    pub fn insert_module(
        &mut self,
        id: impl Into<ModuleId>,
        source: impl Into<String>,
    ) -> Option<String> {
        self.modules.insert(id.into(), source.into())
    }
}

impl ModuleResolver for InMemoryResolver {
    fn resolve<'a>(
        &'a self,
        requester: Option<&'a ModuleId>,
        specifier: &'a str,
    ) -> LocalResolveFuture<'a> {
        Box::pin(async move {
            let id = if specifier.starts_with("./") || specifier.starts_with("../") {
                let requester =
                    requester.ok_or_else(|| ModuleResolveError::NotFound(specifier.into()))?;
                let parent = Path::new(requester.as_str())
                    .parent()
                    .unwrap_or_else(|| Path::new(""));
                let path = parent.join(specifier);
                ModuleId::new(normalize_path(&path).display().to_string())
            } else {
                ModuleId::new(specifier)
            };
            let source = self
                .modules
                .get(&id)
                .ok_or_else(|| ModuleResolveError::NotFound(id.as_str().to_owned()))?;
            Ok(ModuleSource::new(id, source.clone()))
        })
    }
}
