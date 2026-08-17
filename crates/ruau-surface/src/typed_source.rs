//! Require sources for typed native modules.

use std::sync::Arc;

use ruau_source::{
    InMemorySource, InstanceKey, ModuleId, ReadContext, SourceError, SourceFuture, SourceMetadata,
    SourceProvider, SourceRead, SyncSourceProvider, ready,
};

/// One typed native module's require source entry.
#[derive(Clone, Debug)]
pub struct TypedModuleEntry {
    /// The module's canonical require id, from the native module name.
    pub(crate) name: String,
    /// The declaration-erased, compilable module source.
    pub(crate) source: String,
    /// Require aliases that resolve to the canonical id.
    pub(crate) aliases: Vec<String>,
}

/// Serves typed native-module sources beneath an optional host source.
///
/// The host source keeps resolution policy: a request reaches a typed module
/// only when the host reports the module missing. A host error other than a
/// missing module, such as a policy refusal, stands. Without a host source,
/// the typed set answers directly.
pub struct TypedModuleSource {
    typed: InMemorySource,
    host: Option<Arc<dyn SourceProvider>>,
}

impl TypedModuleSource {
    pub(crate) fn new(entries: &[TypedModuleEntry], host: Option<Arc<dyn SourceProvider>>) -> Self {
        let mut typed = InMemorySource::new();
        for entry in entries {
            let canonical = ModuleId::canonicalized(entry.name.as_str());
            let display = entry
                .aliases
                .first()
                .cloned()
                .unwrap_or_else(|| entry.name.clone());
            typed = typed
                .with_module(canonical.clone(), entry.source.as_bytes())
                .with_metadata(canonical.clone(), SourceMetadata::new(display.clone()));
            for alias in &entry.aliases {
                let alias_id = ModuleId::canonicalized(alias.as_str());
                typed = typed
                    .with_alias(alias_id.clone(), canonical.clone())
                    .with_metadata(alias_id, SourceMetadata::new(display.clone()));
            }
        }
        Self { typed, host }
    }

    /// Returns whether `id` names a typed module source.
    fn owns(&self, id: &ModuleId) -> bool {
        self.typed.read_sync(id).is_ok()
    }
}

/// Returns the typed result when the host reports a missing module.
#[cfg(not(target_arch = "wasm32"))]
fn host_then_typed<T: 'static + Send>(
    host: SourceFuture<T>,
    typed: Result<T, SourceError>,
) -> SourceFuture<T> {
    Box::pin(async move {
        match host.await {
            Err(SourceError::MissingModule { .. }) => typed,
            other => other,
        }
    })
}

/// Returns the typed result when the host reports a missing module.
#[cfg(target_arch = "wasm32")]
fn host_then_typed<T: 'static>(
    host: SourceFuture<T>,
    typed: Result<T, SourceError>,
) -> SourceFuture<T> {
    Box::pin(async move {
        match host.await {
            Err(SourceError::MissingModule { .. }) => typed,
            other => other,
        }
    })
}

impl SourceProvider for TypedModuleSource {
    fn resolve(&self, requester: Option<&ModuleId>, request: &[u8]) -> SourceFuture<ModuleId> {
        let typed = self.typed.resolve_sync(requester, request);
        let Some(host) = &self.host else {
            return ready(typed);
        };
        let typed_owned = typed
            .as_ref()
            .is_ok_and(|id| self.owns(id))
            .then_some(typed);
        match typed_owned {
            Some(typed) => host_then_typed(host.resolve(requester, request), typed),
            None => host.resolve(requester, request),
        }
    }

    fn read(&self, id: &ModuleId) -> SourceFuture<Vec<u8>> {
        let Some(host) = &self.host else {
            return ready(self.typed.read_sync(id));
        };
        if !self.owns(id) {
            return host.read(id);
        }
        host_then_typed(host.read(id), self.typed.read_sync(id))
    }

    fn read_request(&self, request: ReadContext<'_>) -> SourceFuture<Vec<u8>> {
        let Some(host) = &self.host else {
            return ready(self.typed.read_request_sync(request));
        };
        if !self.owns(request.id()) {
            return host.read_request(request);
        }
        let typed = self.typed.read_request_sync(request);
        host_then_typed(host.read_request(request), typed)
    }

    fn read_observation(&self, request: ReadContext<'_>) -> SourceFuture<SourceRead> {
        let typed = self.typed.read_observation_sync(request).map(|read| {
            let (source, _, _, origin) = read.into_parts();
            SourceRead::new(source, self.instance_key(request), self.epoch(), origin)
        });
        let Some(host) = &self.host else {
            return ready(typed);
        };
        if !self.owns(request.id()) {
            return host.read_observation(request);
        }
        host_then_typed(host.read_observation(request), typed)
    }

    fn instance_key(&self, request: ReadContext<'_>) -> InstanceKey {
        match &self.host {
            Some(host) => host.instance_key(request),
            None => InstanceKey::shared(request.id().clone()),
        }
    }

    fn metadata(&self, id: &ModuleId) -> SourceMetadata {
        if self.owns(id) {
            return SyncSourceProvider::metadata(&self.typed, id);
        }
        match &self.host {
            Some(host) => host.metadata(id),
            None => SyncSourceProvider::metadata(&self.typed, id),
        }
    }

    fn epoch(&self) -> u64 {
        self.host.as_ref().map_or(0, |host| host.epoch())
    }
}
