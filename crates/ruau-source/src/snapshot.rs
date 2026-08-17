//! Captured and sealed module-source graphs.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

use crate::{
    InstanceKey, ModuleId, ReadContext, Source, SourceError, SourceFuture, SourceProvider,
    SourceRead, SourceResult,
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ResolveKey {
    requester: Option<ModuleId>,
    request: Vec<u8>,
}

impl ResolveKey {
    fn new(requester: Option<&ModuleId>, request: &[u8]) -> Self {
        Self {
            requester: requester.cloned(),
            request: request.to_vec(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct ReadKey {
    id: ModuleId,
    requester: Option<ModuleId>,
}

impl ReadKey {
    fn new(request: ReadContext<'_>) -> Self {
        Self {
            id: request.id().clone(),
            requester: request.requester().cloned(),
        }
    }

    fn context(&self) -> ReadContext<'_> {
        ReadContext::with_requester(&self.id, self.requester.as_ref())
    }
}

/// One captured require resolution in first-success order.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SourceResolutionEdge {
    requester: Option<ModuleId>,
    request: Vec<u8>,
    resolved: ModuleId,
}

impl SourceResolutionEdge {
    /// Creates an exact observed resolution.
    #[must_use]
    pub fn new(
        requester: Option<ModuleId>,
        request: impl Into<Vec<u8>>,
        resolved: ModuleId,
    ) -> Self {
        Self {
            requester,
            request: request.into(),
            resolved,
        }
    }

    /// Returns the module issuing the request, or `None` for an entry request.
    #[must_use]
    pub const fn requester(&self) -> Option<&ModuleId> {
        self.requester.as_ref()
    }

    /// Returns the byte-exact captured request.
    #[must_use]
    pub fn request(&self) -> &[u8] {
        &self.request
    }

    /// Returns the canonical module id produced by resolution.
    #[must_use]
    pub const fn resolved(&self) -> &ModuleId {
        &self.resolved
    }
}

/// Immutable source closure captured while checking one module graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceGraphSnapshot {
    root: Source,
    sources: BTreeMap<InstanceKey, SourceRead>,
    reads: BTreeMap<ReadKey, SourceRead>,
    edges: Vec<SourceResolutionEdge>,
    epoch: u64,
}

impl SourceGraphSnapshot {
    /// Returns the graph root exactly as checked and compiled.
    #[must_use]
    pub const fn root(&self) -> &Source {
        &self.root
    }

    /// Returns captured sources keyed by VM instance identity.
    #[must_use]
    pub const fn sources(&self) -> &BTreeMap<InstanceKey, SourceRead> {
        &self.sources
    }

    /// Returns successful resolutions in capture order.
    #[must_use]
    pub fn edges(&self) -> &[SourceResolutionEdge] {
        &self.edges
    }

    /// Returns the provider epoch pinned by this graph.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }
}

type SharedResolve = SharedOperation<(ModuleId, Option<SourceRead>)>;
type SharedRead = SharedOperation<SourceRead>;

#[derive(Default)]
struct SnapshotState {
    pinned_epoch: Option<u64>,
    resolutions: HashMap<ResolveKey, ModuleId>,
    reads: HashMap<ReadKey, SourceRead>,
    edges: Vec<SourceResolutionEdge>,
    sealed: Option<SourceGraphSnapshot>,
    resolving: OperationTable<ResolveKey, (ModuleId, Option<SourceRead>)>,
    reading: OperationTable<ReadKey, SourceRead>,
}

enum Admission<V> {
    Ready(V),
    Follow(SharedOperation<V>),
    Lead {
        operation: SharedOperation<V>,
        epoch: u64,
    },
}

struct OperationTable<K, V> {
    operations: HashMap<K, SharedOperation<V>>,
}

impl<K, V> Default for OperationTable<K, V> {
    fn default() -> Self {
        Self {
            operations: HashMap::new(),
        }
    }
}

impl<K, V> OperationTable<K, V>
where
    K: Clone + Eq + std::hash::Hash,
    V: Clone,
{
    fn admit(&mut self, key: &K, ready: Option<V>, epoch: u64) -> Admission<V> {
        if let Some(value) = ready {
            return Admission::Ready(value);
        }
        if let Some(operation) = self.operations.get(key) {
            return Admission::Follow(operation.clone());
        }
        let operation = SharedOperation::new();
        self.operations.insert(key.clone(), operation.clone());
        Admission::Lead { operation, epoch }
    }

    fn finish(&mut self, key: &K, operation: &SharedOperation<V>) -> SourceResult<()> {
        if !self
            .operations
            .get(key)
            .is_some_and(|current| current.is_same(operation))
        {
            return Err(SourceError::other("source operation lost its admission"));
        }
        self.operations.remove(key);
        Ok(())
    }

    fn cancel(&mut self, key: &K, operation: &SharedOperation<V>) -> bool {
        if self
            .operations
            .get(key)
            .is_some_and(|current| current.is_same(operation))
        {
            self.operations.remove(key);
            true
        } else {
            false
        }
    }

    fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }
}

/// Source provider that captures one epoch-consistent graph and can seal it.
///
/// Successful resolutions and reads are cached. A sealed source serves only
/// operations captured during checking, so later compilation and execution
/// cannot observe delegate mutations or import new modules.
#[derive(Clone)]
pub struct SnapshotSource {
    source: Arc<dyn SourceProvider>,
    state: Arc<Mutex<SnapshotState>>,
}

impl SnapshotSource {
    /// Wraps a source provider without reading or pinning it.
    #[must_use]
    pub fn new(source: Arc<dyn SourceProvider>) -> Self {
        Self {
            source,
            state: Arc::new(Mutex::new(SnapshotState::default())),
        }
    }

    fn locked(&self) -> SourceResult<std::sync::MutexGuard<'_, SnapshotState>> {
        self.state
            .lock()
            .map_err(|_| SourceError::other("source snapshot lock was poisoned"))
    }

    fn compatible_read_from<'a>(
        exact: Option<&'a SourceRead>,
        mut candidates: impl Iterator<Item = &'a SourceRead>,
        id: &ModuleId,
    ) -> SourceResult<Option<SourceRead>> {
        if let Some(read) = exact {
            return Ok(Some(read.clone()));
        }
        let Some(first) = candidates.next() else {
            return Ok(None);
        };
        if candidates.all(|candidate| candidate == first) {
            Ok(Some(first.clone()))
        } else {
            Err(SourceError::AmbiguousInstance { id: id.clone() })
        }
    }

    fn compatible_read(state: &SnapshotState, key: &ReadKey) -> SourceResult<Option<SourceRead>> {
        Self::compatible_read_from(
            state.reads.get(key),
            state
                .reads
                .iter()
                .filter(|(candidate, _)| candidate.id == key.id)
                .map(|(_, read)| read),
            &key.id,
        )
    }

    fn compatible_snapshot_read(
        snapshot: &SourceGraphSnapshot,
        key: &ReadKey,
    ) -> SourceResult<Option<SourceRead>> {
        Self::compatible_read_from(
            snapshot.reads.get(key),
            snapshot
                .reads
                .iter()
                .filter(|(candidate, _)| candidate.id == key.id)
                .map(|(_, read)| read),
            &key.id,
        )
    }

    fn sealed_resolution(
        snapshot: &SourceGraphSnapshot,
        key: &ResolveKey,
    ) -> SourceResult<ModuleId> {
        snapshot
            .edges
            .iter()
            .find(|edge| {
                edge.requester.as_ref() == key.requester.as_ref() && edge.request == key.request
            })
            .map(|edge| edge.resolved.clone())
            .ok_or_else(|| SourceError::UncapturedOperation {
                operation: format!(
                    "resolution '{}' from {}",
                    String::from_utf8_lossy(&key.request),
                    key.requester
                        .as_ref()
                        .map_or_else(|| "the entry".to_owned(), ToString::to_string)
                ),
            })
    }

    fn sealed_read(snapshot: &SourceGraphSnapshot, key: &ReadKey) -> SourceResult<SourceRead> {
        if let Some(read) = snapshot.reads.get(key) {
            return Ok(read.clone());
        }
        let edge_was_checked = key.requester.as_ref().is_some_and(|requester| {
            snapshot
                .edges
                .iter()
                .any(|edge| edge.requester.as_ref() == Some(requester) && edge.resolved == key.id)
        });
        if edge_was_checked && let Some(read) = Self::compatible_snapshot_read(snapshot, key)? {
            return Ok(read);
        }
        Err(SourceError::UncapturedOperation {
            operation: format!("source read for module '{}'", key.id),
        })
    }

    fn admit_resolution(
        &self,
        key: &ResolveKey,
    ) -> SourceResult<Admission<(ModuleId, Option<SourceRead>)>> {
        let observed = self.source.epoch();
        let mut state = self.locked()?;
        if let Some(snapshot) = &state.sealed {
            return Self::sealed_resolution(snapshot, key).map(|id| Admission::Ready((id, None)));
        }
        let expected = *state.pinned_epoch.get_or_insert(observed);
        if observed != expected {
            return Err(SourceError::EpochChanged { expected, observed });
        }
        let ready = state.resolutions.get(key).cloned().map(|id| (id, None));
        Ok(state.resolving.admit(key, ready, expected))
    }

    fn admit_read(&self, key: &ReadKey) -> SourceResult<Admission<SourceRead>> {
        let observed = self.source.epoch();
        let mut state = self.locked()?;
        if let Some(snapshot) = &state.sealed {
            return Self::sealed_read(snapshot, key).map(Admission::Ready);
        }
        let expected = *state.pinned_epoch.get_or_insert(observed);
        if observed != expected {
            return Err(SourceError::EpochChanged { expected, observed });
        }
        let ready = Self::compatible_read(&state, key)?;
        Ok(state.reading.admit(key, ready, expected))
    }

    fn finish_resolution(
        &self,
        key: &ResolveKey,
        operation: &SharedResolve,
        result: &SourceResult<(ModuleId, Option<SourceRead>)>,
    ) -> SourceResult<()> {
        let mut state = self.locked()?;
        state.resolving.finish(key, operation)?;

        if state.sealed.is_some() {
            Err(SourceError::UncapturedOperation {
                operation: "resolution completed after source sealing".to_owned(),
            })
        } else if let Ok((id, read)) = result {
            if let Some(existing) = state.resolutions.get(key)
                && existing != id
            {
                Err(SourceError::other(
                    "coalesced source resolution produced incompatible module ids",
                ))
            } else {
                let read_key = read.as_ref().map(|read| ReadKey {
                    id: read.source().id().clone(),
                    requester: key.requester.clone(),
                });
                if let Some((read_key, read)) = read_key.as_ref().zip(read.as_ref())
                    && state
                        .reads
                        .get(read_key)
                        .is_some_and(|existing| existing != read)
                {
                    return Err(SourceError::AmbiguousInstance {
                        id: read.source().id().clone(),
                    });
                }
                state.resolutions.insert(key.clone(), id.clone());
                state.edges.push(SourceResolutionEdge {
                    requester: key.requester.clone(),
                    request: key.request.clone(),
                    resolved: id.clone(),
                });
                if let Some((read_key, read)) = read_key.zip(read.as_ref()) {
                    state.reads.insert(read_key, read.clone());
                }
                Ok(())
            }
        } else {
            Ok(())
        }
    }

    fn finish_read(
        &self,
        key: &ReadKey,
        operation: &SharedRead,
        result: &SourceResult<SourceRead>,
    ) -> SourceResult<()> {
        let mut state = self.locked()?;
        state.reading.finish(key, operation)?;

        if state.sealed.is_some() {
            Err(SourceError::UncapturedOperation {
                operation: "source read completed after source sealing".to_owned(),
            })
        } else if let Ok(read) = result {
            if let Some(existing) = state.reads.get(key)
                && existing != read
            {
                Err(SourceError::AmbiguousInstance { id: key.id.clone() })
            } else {
                state.reads.insert(key.clone(), read.clone());
                Ok(())
            }
        } else {
            Ok(())
        }
    }

    fn cancel_operation<T>(
        &self,
        operation: &SharedOperation<T>,
        remove: impl FnOnce(&mut SnapshotState) -> bool,
        message: &'static str,
    ) {
        if let Ok(mut state) = self.state.lock() {
            remove(&mut state);
        }
        operation.complete(Err(SourceError::other(message)));
    }

    fn verify_existing_snapshot(
        snapshot: &SourceGraphSnapshot,
        root: &Source,
        modules: &BTreeSet<ModuleId>,
        edges: &[SourceResolutionEdge],
    ) -> SourceResult<()> {
        let snapshot_modules = snapshot
            .sources
            .values()
            .map(|read| read.source().id().clone())
            .collect::<BTreeSet<_>>();
        let snapshot_edges = snapshot
            .edges
            .iter()
            .filter(|edge| edge.requester.is_some())
            .cloned()
            .collect::<BTreeSet<_>>();
        let expected_edges = edges
            .iter()
            .filter(|edge| edge.requester.is_some())
            .cloned()
            .collect::<BTreeSet<_>>();
        if snapshot.root != *root
            || snapshot_modules != *modules
            || snapshot_edges != expected_edges
        {
            return Err(SourceError::UncapturedOperation {
                operation: "repeat seal requested a different source closure".to_owned(),
            });
        }
        Ok(())
    }

    /// Seals the captured operations against a checked root, module set, and
    /// requester-to-module edge set.
    ///
    /// # Errors
    /// Returns [`SourceError`] when the provider changed, a checked source was
    /// not captured, the closure disagrees with captured operations, or an
    /// operation is still in flight.
    pub fn seal(
        &self,
        root: &Source,
        modules: &BTreeSet<ModuleId>,
        edges: &[SourceResolutionEdge],
    ) -> SourceResult<SourceGraphSnapshot> {
        let observed = self.source.epoch();
        let mut state = self.locked()?;
        if let Some(snapshot) = &state.sealed {
            Self::verify_existing_snapshot(snapshot, root, modules, edges)?;
            return Ok(snapshot.clone());
        }
        let expected = *state.pinned_epoch.get_or_insert(observed);
        if observed != expected {
            return Err(SourceError::EpochChanged { expected, observed });
        }
        if !state.resolving.is_empty() || !state.reading.is_empty() {
            return Err(SourceError::other(
                "cannot seal a source graph while operations are in flight",
            ));
        }

        let mut captured_modules = BTreeSet::new();
        let mut sources = BTreeMap::new();
        for read in state.reads.values() {
            let id = read.source().id();
            if !modules.contains(id) {
                return Err(SourceError::UncapturedOperation {
                    operation: format!("source read for module '{id}' outside the checked graph"),
                });
            }
            captured_modules.insert(id.clone());
            if let Some(existing) = sources.insert(read.instance_key().clone(), read.clone())
                && existing != *read
            {
                return Err(SourceError::AmbiguousInstance { id: id.clone() });
            }
        }
        if captured_modules != *modules {
            let missing = modules
                .difference(&captured_modules)
                .next()
                .expect("unequal module sets have a missing member");
            return Err(SourceError::UncapturedOperation {
                operation: format!("source read for checked module '{missing}'"),
            });
        }
        if !state.reads.values().any(|read| read.source() == root) {
            return Err(SourceError::UncapturedOperation {
                operation: format!("exact root source '{}'", root.id()),
            });
        }

        let entry_edges = state
            .edges
            .iter()
            .filter(|edge| edge.requester.is_none())
            .cloned()
            .collect::<Vec<_>>();
        if entry_edges.iter().any(|edge| edge.resolved != *root.id()) {
            return Err(SourceError::UncapturedOperation {
                operation: "entry resolution outside the checked root".to_owned(),
            });
        }

        let captured_edges = state
            .edges
            .iter()
            .filter_map(|edge| {
                edge.requester.as_ref().map(|requester| {
                    (
                        requester.clone(),
                        edge.request.clone(),
                        edge.resolved.clone(),
                    )
                })
            })
            .collect::<BTreeSet<_>>();
        let expected_edges = edges
            .iter()
            .filter_map(|edge| {
                edge.requester.as_ref().map(|requester| {
                    (
                        requester.clone(),
                        edge.request.clone(),
                        edge.resolved.clone(),
                    )
                })
            })
            .collect::<BTreeSet<_>>();
        if !captured_edges.is_subset(&expected_edges) {
            return Err(SourceError::UncapturedOperation {
                operation: "resolution edge outside the checked graph".to_owned(),
            });
        }

        let mut snapshot_edges = state.edges.clone();
        for edge in edges {
            let Some(requester) = edge.requester.as_ref() else {
                continue;
            };
            let exact = (
                requester.clone(),
                edge.request.clone(),
                edge.resolved.clone(),
            );
            if captured_edges.contains(&exact) {
                continue;
            }
            let resolved = crate::resolve_request(Some(requester), &edge.request)?;
            if resolved != edge.resolved || edge.resolved != *root.id() {
                return Err(SourceError::UncapturedOperation {
                    operation: format!(
                        "checked resolution '{}' from '{}' was not captured by the snapshot",
                        String::from_utf8_lossy(&edge.request),
                        requester
                    ),
                });
            }
            snapshot_edges.push(edge.clone());
        }

        let snapshot = SourceGraphSnapshot {
            root: root.clone(),
            sources,
            reads: state
                .reads
                .iter()
                .map(|(key, read)| (key.clone(), read.clone()))
                .collect(),
            edges: snapshot_edges,
            epoch: expected,
        };
        let observed = self.source.epoch();
        if observed != expected {
            return Err(SourceError::EpochChanged { expected, observed });
        }
        state.sealed = Some(snapshot.clone());
        state.resolutions.clear();
        state.reads.clear();
        state.edges.clear();
        Ok(snapshot)
    }
}

impl SourceProvider for SnapshotSource {
    fn resolve(&self, requester: Option<&ModuleId>, request: &[u8]) -> SourceFuture<ModuleId> {
        let key = ResolveKey::new(requester, request);
        let admission = match self.admit_resolution(&key) {
            Ok(admission) => admission,
            Err(error) => return crate::ready(Err(error)),
        };
        match admission {
            Admission::Ready((id, _)) => crate::ready(Ok(id)),
            Admission::Follow(operation) => {
                Box::pin(async move { operation.await.map(|(id, _)| id) })
            }
            Admission::Lead { operation, epoch } => {
                let snapshot = self.clone();
                let source = Arc::clone(&self.source);
                let guard = LeaderGuard::new(operation.clone(), {
                    let snapshot = snapshot.clone();
                    let key = key.clone();
                    move |operation| {
                        snapshot.cancel_operation(
                            operation,
                            |state| state.resolving.cancel(&key, operation),
                            "source resolution operation was cancelled",
                        );
                    }
                });
                Box::pin(async move {
                    let mut guard = guard;
                    let mut result = async {
                        let id = source.resolve(key.requester.as_ref(), &key.request).await?;
                        let observed = source.epoch();
                        if observed != epoch {
                            return Err(SourceError::EpochChanged {
                                expected: epoch,
                                observed,
                            });
                        }
                        let read = source
                            .read_observation(ReadContext::with_requester(
                                &id,
                                key.requester.as_ref(),
                            ))
                            .await;
                        let read = match read {
                            Ok(read) => {
                                if read.epoch() != epoch {
                                    return Err(SourceError::EpochChanged {
                                        expected: epoch,
                                        observed: read.epoch(),
                                    });
                                }
                                Some(read)
                            }
                            Err(SourceError::EpochChanged { expected, observed }) => {
                                return Err(SourceError::EpochChanged { expected, observed });
                            }
                            Err(_) => None,
                        };
                        let observed = source.epoch();
                        if observed != epoch {
                            return Err(SourceError::EpochChanged {
                                expected: epoch,
                                observed,
                            });
                        }
                        Ok((id, read))
                    }
                    .await;
                    if let Err(error) = snapshot.finish_resolution(&key, &operation, &result) {
                        result = Err(error);
                    }
                    operation.complete(result.clone());
                    guard.disarm();
                    result.map(|(id, _)| id)
                })
            }
        }
    }

    fn read(&self, id: &ModuleId) -> SourceFuture<Vec<u8>> {
        self.read_request(ReadContext::new(id))
    }

    fn read_request(&self, request: ReadContext<'_>) -> SourceFuture<Vec<u8>> {
        let future = self.read_observation(request);
        Box::pin(async move { Ok(future.await?.into_parts().0.into_bytes()) })
    }

    fn read_observation(&self, request: ReadContext<'_>) -> SourceFuture<SourceRead> {
        let key = ReadKey::new(request);
        let admission = match self.admit_read(&key) {
            Ok(admission) => admission,
            Err(error) => return crate::ready(Err(error)),
        };
        match admission {
            Admission::Ready(read) => crate::ready(Ok(read)),
            Admission::Follow(operation) => Box::pin(operation),
            Admission::Lead { operation, epoch } => {
                let snapshot = self.clone();
                let source = Arc::clone(&self.source);
                let guard = LeaderGuard::new(operation.clone(), {
                    let snapshot = snapshot.clone();
                    let key = key.clone();
                    move |operation| {
                        snapshot.cancel_operation(
                            operation,
                            |state| state.reading.cancel(&key, operation),
                            "source read operation was cancelled",
                        );
                    }
                });
                Box::pin(async move {
                    let mut guard = guard;
                    let mut result = source.read_observation(key.context()).await;
                    if let Ok(read) = &result
                        && read.epoch() != epoch
                    {
                        result = Err(SourceError::EpochChanged {
                            expected: epoch,
                            observed: read.epoch(),
                        });
                    }
                    if result.is_ok() {
                        let observed = source.epoch();
                        if observed != epoch {
                            result = Err(SourceError::EpochChanged {
                                expected: epoch,
                                observed,
                            });
                        }
                    }
                    if let Err(error) = snapshot.finish_read(&key, &operation, &result) {
                        result = Err(error);
                    }
                    operation.complete(result.clone());
                    guard.disarm();
                    result
                })
            }
        }
    }

    fn instance_key(&self, request: ReadContext<'_>) -> InstanceKey {
        let key = ReadKey::new(request);
        self.state
            .lock()
            .ok()
            .and_then(|state| {
                state.sealed.as_ref().map_or_else(
                    || Self::compatible_read(&state, &key).ok().flatten(),
                    |snapshot| Self::sealed_read(snapshot, &key).ok(),
                )
            })
            .map_or_else(
                || self.source.instance_key(request),
                |read| read.instance_key().clone(),
            )
    }

    fn metadata(&self, id: &ModuleId) -> crate::SourceMetadata {
        self.state
            .lock()
            .ok()
            .and_then(|state| match &state.sealed {
                Some(snapshot) => snapshot
                    .reads
                    .values()
                    .find(|read| read.source().id() == id)
                    .map(|read| read.source().metadata().clone()),
                None => state
                    .reads
                    .values()
                    .find(|read| read.source().id() == id)
                    .map(|read| read.source().metadata().clone()),
            })
            .unwrap_or_else(|| self.source.metadata(id))
    }

    fn epoch(&self) -> u64 {
        self.state.lock().ok().map_or_else(
            || self.source.epoch(),
            |state| {
                if state.sealed.is_some() {
                    state.pinned_epoch.unwrap_or_else(|| self.source.epoch())
                } else {
                    self.source.epoch()
                }
            },
        )
    }
}

struct SharedOperation<T> {
    state: Arc<Mutex<SharedOperationState<T>>>,
}

impl<T> Clone for SharedOperation<T> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

struct SharedOperationState<T> {
    result: Option<SourceResult<T>>,
    waiters: Vec<Waker>,
}

impl<T> SharedOperation<T> {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedOperationState {
                result: None,
                waiters: Vec::new(),
            })),
        }
    }

    fn is_same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    fn complete(&self, result: SourceResult<T>) {
        let (mut state, poisoned) = match self.state.lock() {
            Ok(state) => (state, false),
            Err(error) => (error.into_inner(), true),
        };
        if poisoned {
            state.result = Some(Err(SourceError::other(
                "shared source operation lock was poisoned",
            )));
        } else if state.result.is_some() {
            return;
        } else {
            state.result = Some(result);
        }
        let waiters = std::mem::take(&mut state.waiters);
        drop(state);
        for waker in waiters {
            waker.wake();
        }
    }
}

impl<T: Clone> Future for SharedOperation<T> {
    type Output = SourceResult<T>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let (mut state, poisoned) = match self.state.lock() {
            Ok(state) => (state, false),
            Err(error) => (error.into_inner(), true),
        };
        if poisoned {
            state.result = Some(Err(SourceError::other(
                "shared source operation lock was poisoned",
            )));
            let waiters = std::mem::take(&mut state.waiters);
            let result = state
                .result
                .as_ref()
                .expect("poison recovery installs a result")
                .clone();
            drop(state);
            for waker in waiters {
                waker.wake();
            }
            return Poll::Ready(result);
        }
        if let Some(result) = &state.result {
            return Poll::Ready(result.clone());
        }
        if !state
            .waiters
            .iter()
            .any(|waker| waker.will_wake(context.waker()))
        {
            state.waiters.push(context.waker().clone());
        }
        Poll::Pending
    }
}

struct LeaderGuard<T, C>
where
    C: FnOnce(&SharedOperation<T>),
{
    operation: SharedOperation<T>,
    cancel: Option<C>,
}

impl<T, C> LeaderGuard<T, C>
where
    C: FnOnce(&SharedOperation<T>),
{
    fn new(operation: SharedOperation<T>, cancel: C) -> Self {
        Self {
            operation,
            cancel: Some(cancel),
        }
    }

    fn disarm(&mut self) {
        self.cancel = None;
    }
}

impl<T, C> Drop for LeaderGuard<T, C>
where
    C: FnOnce(&SharedOperation<T>),
{
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel(&self.operation);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        panic::{AssertUnwindSafe, catch_unwind},
        task::{Context, Poll, Waker},
    };

    use super::{Admission, OperationTable, SharedOperation};

    #[test]
    fn operation_table_coalesces_followers_and_checks_completion_identity() {
        let mut table = OperationTable::<u8, u8>::default();
        let Admission::Lead { operation, epoch } = table.admit(&7, None, 11) else {
            panic!("first admission leads");
        };
        assert_eq!(epoch, 11);
        let Admission::Follow(first_follower) = table.admit(&7, None, 11) else {
            panic!("second admission follows");
        };
        let Admission::Follow(second_follower) = table.admit(&7, None, 11) else {
            panic!("third admission follows");
        };
        let unrelated = SharedOperation::new();
        assert!(table.finish(&7, &unrelated).is_err());
        table
            .finish(&7, &operation)
            .expect("the admitted leader completes");
        operation.complete(Ok(42));

        let mut first_follower = Box::pin(first_follower);
        let mut second_follower = Box::pin(second_follower);
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(
            first_follower.as_mut().poll(&mut context),
            Poll::Ready(Ok(42))
        ));
        assert!(matches!(
            second_follower.as_mut().poll(&mut context),
            Poll::Ready(Ok(42))
        ));
    }

    #[test]
    fn poisoned_shared_operation_completes_with_a_source_error() {
        let operation = SharedOperation::<u8>::new();
        let state = operation.state.clone();
        assert!(
            catch_unwind(AssertUnwindSafe(move || {
                let _state = state.lock().expect("operation starts healthy");
                panic!("poison operation state");
            }))
            .is_err()
        );

        operation.complete(Ok(42));
        let mut operation = Box::pin(operation);
        let mut context = Context::from_waker(Waker::noop());
        let Poll::Ready(Err(error)) = operation.as_mut().poll(&mut context) else {
            panic!("poisoned operation must complete with an error");
        };
        assert!(error.to_string().contains("poisoned"));
    }
}
