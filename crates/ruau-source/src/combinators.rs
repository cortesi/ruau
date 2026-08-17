//! Composable routing and sealed-graph module sources.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    InstanceKey, ModuleId, ReadContext, Source, SourceEpoch, SourceError, SourceFuture,
    SourceMetadata, SourceProvider, SourceRead, resolve_request,
};

/// Acquire a shared lock after recovering data from a prior panic.
fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Source router that mounts independent providers behind qualified routes.
pub struct Router {
    /// Default source for ids outside a mounted route.
    fallback: Arc<dyn SourceProvider>,
    /// Prefix used to qualify mounted module ids.
    route_prefix: String,
    /// Mounted sources keyed by their qualified prefix.
    routes: Arc<Mutex<HashMap<String, SourceRoute>>>,
    /// Local invalidation state for mount-table changes.
    route_epoch: SourceEpoch,
}

/// One immutable mounted source.
struct SourceRoute {
    /// Filesystem or in-memory provider serving this route.
    provider: Arc<dyn SourceProvider>,
    /// Unqualified entry identity used to resolve root-relative requests.
    root: ModuleId,
}

impl Router {
    /// Wrap the fallback provider.
    pub fn new(fallback: Arc<dyn SourceProvider>) -> Self {
        Self::with_namespace(fallback, "route")
    }

    /// Wrap the fallback provider and select the qualified route namespace.
    pub fn with_namespace(fallback: Arc<dyn SourceProvider>, namespace: impl Into<String>) -> Self {
        Self {
            fallback,
            route_prefix: format!("@{}/", namespace.into()),
            routes: Arc::new(Mutex::new(HashMap::new())),
            route_epoch: SourceEpoch::default(),
        }
    }

    /// Mount one independent source and return its qualified root id.
    pub fn mount(
        &self,
        route: &str,
        provider: Arc<dyn SourceProvider>,
        root: ModuleId,
    ) -> ModuleId {
        lock_unpoisoned(&self.routes).insert(route.to_owned(), SourceRoute { provider, root });
        self.route_epoch.bump();
        ModuleId::new(format!("{}{route}/root", self.route_prefix))
    }

    /// Release one mounted route.
    pub fn unmount(&self, route: &str) {
        lock_unpoisoned(&self.routes).remove(route);
        self.route_epoch.bump();
    }

    /// Split a qualified id into route and unqualified module id.
    fn split<'a>(&self, id: &'a ModuleId) -> Option<(&'a str, &'a str)> {
        let value = id.as_str()?.strip_prefix(&self.route_prefix)?;
        value.split_once('/')
    }

    fn combined_epoch(
        fallback: &dyn SourceProvider,
        routes: &Mutex<HashMap<String, SourceRoute>>,
        route_epoch: &SourceEpoch,
    ) -> u64 {
        let mut parts = lock_unpoisoned(routes)
            .iter()
            .map(|(name, route)| {
                (
                    name.clone(),
                    route.root.as_bytes().to_vec(),
                    route.provider.epoch(),
                )
            })
            .collect::<Vec<_>>();
        parts.sort_by(|left, right| left.0.cmp(&right.0));
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in fallback
            .epoch()
            .to_le_bytes()
            .into_iter()
            .chain(route_epoch.get().to_le_bytes())
            .chain(parts.iter().flat_map(|(name, root, epoch)| {
                name.as_bytes()
                    .iter()
                    .copied()
                    .chain(root.iter().copied())
                    .chain(epoch.to_le_bytes())
            }))
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }
}

impl SourceProvider for Router {
    fn resolve(&self, requester: Option<&ModuleId>, request: &[u8]) -> SourceFuture<ModuleId> {
        let Some((route_name, requester_name)) = requester.and_then(|id| self.split(id)) else {
            return self.fallback.resolve(requester, request);
        };
        let routes = lock_unpoisoned(&self.routes);
        let Some(route) = routes.get(route_name) else {
            return Box::pin(async { Err(SourceError::other("module route has been released")) });
        };
        let requester = if requester_name == "root" {
            route.root.clone()
        } else {
            ModuleId::new(requester_name.as_bytes().to_vec())
        };
        let future = route.provider.resolve(Some(&requester), request);
        let prefix = format!("{}{route_name}/", self.route_prefix);
        Box::pin(async move {
            let id = future.await?;
            Ok(ModuleId::new([prefix.as_bytes(), id.as_bytes()].concat()))
        })
    }

    fn read(&self, id: &ModuleId) -> SourceFuture<Vec<u8>> {
        let Some((route_name, module_name)) = self.split(id) else {
            return self.fallback.read(id);
        };
        let routes = lock_unpoisoned(&self.routes);
        let Some(route) = routes.get(route_name) else {
            return Box::pin(async { Err(SourceError::other("module route has been released")) });
        };
        route
            .provider
            .read(&ModuleId::new(module_name.as_bytes().to_vec()))
    }

    fn read_observation(&self, request: ReadContext<'_>) -> SourceFuture<SourceRead> {
        let expected = self.epoch();
        let id = request.id().clone();
        let metadata = self.metadata(&id);
        let instance = self.instance_key(request);
        let future = self.read_request(request);
        let fallback = Arc::clone(&self.fallback);
        let routes = Arc::clone(&self.routes);
        let route_epoch = self.route_epoch.clone();
        Box::pin(async move {
            let bytes = future.await?;
            let observed = Self::combined_epoch(fallback.as_ref(), &routes, &route_epoch);
            if observed != expected {
                return Err(SourceError::EpochChanged { expected, observed });
            }
            Ok(SourceRead::new(
                Source::bytes(id, bytes).with_metadata(metadata),
                instance,
                expected,
                None,
            ))
        })
    }

    fn metadata(&self, id: &ModuleId) -> SourceMetadata {
        let Some((route_name, module_name)) = self.split(id) else {
            return self.fallback.metadata(id);
        };
        lock_unpoisoned(&self.routes).get(route_name).map_or_else(
            || SourceMetadata::new(id.to_lossy_string()),
            |route| {
                route
                    .provider
                    .metadata(&ModuleId::new(module_name.as_bytes().to_vec()))
            },
        )
    }

    fn epoch(&self) -> u64 {
        Self::combined_epoch(self.fallback.as_ref(), &self.routes, &self.route_epoch)
    }
}

/// Requester and literal request bytes used to cache one resolution edge.
type ResolutionKey = (Option<ModuleId>, Vec<u8>);

/// Module source that freezes one checked graph into a runtime allowlist.
pub struct SealedGraphSource {
    /// Filesystem resolver used during graph preparation and activation.
    delegate: Arc<dyn SourceProvider>,
    /// Entry module identity used when a root request has no requester.
    graph_root: ModuleId,
    /// Canonical security root applied by the filesystem delegate.
    requested_root: PathBuf,
    /// Resolution edges observed while checking and activating the graph.
    resolutions: Arc<Mutex<HashMap<ResolutionKey, ModuleId>>>,
    /// Source bytes cached for stable repeated reads during entry evaluation.
    bytes: Arc<Mutex<HashMap<ModuleId, Vec<u8>>>>,
    /// Checked module identities; absent only while the graph is being built.
    allowed: Arc<Mutex<Option<HashSet<ModuleId>>>>,
    /// Whether entry evaluation has completed and new source reads are forbidden.
    sealed: Arc<AtomicBool>,
}

impl SealedGraphSource {
    /// Wrap a resolver for one config graph.
    pub fn new(
        delegate: Arc<dyn SourceProvider>,
        graph_root: ModuleId,
        requested_root: PathBuf,
    ) -> Self {
        Self {
            delegate,
            graph_root,
            requested_root,
            resolutions: Arc::new(Mutex::new(HashMap::new())),
            bytes: Arc::new(Mutex::new(HashMap::new())),
            allowed: Arc::new(Mutex::new(None)),
            sealed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Restrict runtime resolution and reads to the checked graph.
    pub fn allow_only(&self, modules: impl IntoIterator<Item = ModuleId>) {
        *lock_unpoisoned(&self.allowed) = Some(modules.into_iter().collect());
    }

    /// Prevent later entrypoints from reading any module source. Previously
    /// observed resolution edges remain available only for export-cache lookup.
    pub fn seal(&self) {
        self.sealed.store(true, Ordering::Release);
    }

    /// Return true for relative request spellings inside a checked graph.
    fn request_is_relative(request: &[u8]) -> bool {
        request.starts_with(b"./") || request.starts_with(b"../")
    }
}

impl SourceProvider for SealedGraphSource {
    fn resolve(&self, requester: Option<&ModuleId>, request: &[u8]) -> SourceFuture<ModuleId> {
        if !Self::request_is_relative(request) {
            let request = String::from_utf8_lossy(request).into_owned();
            return Box::pin(async move {
                Err(SourceError::other(format!(
                    "module request '{request}' must begin with ./ or ../"
                )))
            });
        }

        let requester = requester.or(Some(&self.graph_root));
        let key = (requester.cloned(), request.to_vec());
        if self.sealed.load(Ordering::Acquire) {
            let cached = lock_unpoisoned(&self.resolutions).get(&key).cloned();
            return Box::pin(async move {
                cached.ok_or_else(|| {
                    SourceError::other("module source is sealed after config entry evaluation")
                })
            });
        }

        let requested_root = self.requested_root.display().to_string();
        if let Some(allowed) = lock_unpoisoned(&self.allowed).as_ref() {
            let candidate = match resolve_request(requester, request) {
                Ok(candidate) => candidate,
                Err(error) => return Box::pin(async move { Err(error) }),
            };
            if !allowed.contains(&candidate) {
                return Box::pin(async move {
                    Err(SourceError::other(format!(
                        "module '{candidate}' is outside the checked config graph rooted at {}",
                        requested_root
                    )))
                });
            }
        }

        let future = self.delegate.resolve(requester, request);
        let allowed = Arc::clone(&self.allowed);
        let resolutions = Arc::clone(&self.resolutions);
        Box::pin(async move {
            let id = future.await?;
            if lock_unpoisoned(&allowed)
                .as_ref()
                .is_some_and(|allowed| !allowed.contains(&id))
            {
                return Err(SourceError::other(format!(
                    "module '{id}' is outside the checked config graph rooted at {}",
                    requested_root
                )));
            }
            lock_unpoisoned(&resolutions).insert(key, id.clone());
            Ok(id)
        })
    }

    fn read(&self, id: &ModuleId) -> SourceFuture<Vec<u8>> {
        if self.sealed.load(Ordering::Acquire) {
            return Box::pin(async {
                Err(SourceError::other(
                    "module source is sealed after config entry evaluation",
                ))
            });
        }
        if let Some(source) = lock_unpoisoned(&self.bytes).get(id).cloned() {
            return Box::pin(async move { Ok(source) });
        }
        if lock_unpoisoned(&self.allowed)
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(id))
        {
            let id = id.clone();
            return Box::pin(async move {
                Err(SourceError::other(format!(
                    "module '{id}' is outside the checked config graph"
                )))
            });
        }

        let future = self.delegate.read(id);
        let id = id.clone();
        let bytes = Arc::clone(&self.bytes);
        Box::pin(async move {
            let source = future.await?;
            lock_unpoisoned(&bytes).insert(id, source.clone());
            Ok(source)
        })
    }

    fn read_request(&self, request: ReadContext<'_>) -> SourceFuture<Vec<u8>> {
        self.read(request.id())
    }

    fn instance_key(&self, request: ReadContext<'_>) -> InstanceKey {
        self.delegate.instance_key(request)
    }

    fn metadata(&self, id: &ModuleId) -> SourceMetadata {
        self.delegate.metadata(id)
    }

    fn epoch(&self) -> u64 {
        self.delegate.epoch()
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use crate::{
        InMemorySource, ModuleId, ReadySourceFutureExt, Router, SealedGraphSource, SourceProvider,
    };

    #[test]
    fn router_qualifies_mounted_resolutions_and_releases_routes() {
        let mut mounted = InMemorySource::new();
        mounted.insert(ModuleId::new(b"entry".to_vec()), b"return require('./dep')");
        mounted.insert(ModuleId::new(b"dep".to_vec()), b"return 42");
        let mounted: Arc<dyn SourceProvider> = Arc::new(mounted);
        let fallback: Arc<dyn SourceProvider> = Arc::new(InMemorySource::new());
        let router = Router::with_namespace(fallback, "script");
        let initial_epoch = router.epoch();
        let root = router.mount("caller", mounted, ModuleId::new(b"entry".to_vec()));
        assert_ne!(router.epoch(), initial_epoch);

        let dependency = router
            .resolve(Some(&root), b"./dep")
            .ready_only("resolve mounted dependency")
            .expect("mounted dependency resolves");
        assert_eq!(dependency, ModuleId::new(b"@script/caller/dep".to_vec()));
        assert_eq!(
            router
                .read(&dependency)
                .ready_only("read mounted dependency")
                .expect("mounted dependency reads"),
            b"return 42"
        );
        let read = router
            .read_observation(crate::ReadContext::new(&dependency))
            .ready_only("observe mounted dependency")
            .expect("mounted dependency observation");
        assert_eq!(read.epoch(), router.epoch());

        let mounted_epoch = router.epoch();
        router.unmount("caller");
        assert_ne!(router.epoch(), mounted_epoch);
        let error = router
            .read(&dependency)
            .ready_only("read released route")
            .expect_err("released route must fail");
        assert!(error.to_string().contains("route has been released"));
    }

    #[test]
    fn sealed_graph_replays_checked_resolutions_and_rejects_new_edges() {
        let mut delegate = InMemorySource::new();
        delegate.insert(
            ModuleId::new(b"config".to_vec()),
            b"return require('./dep')",
        );
        delegate.insert(ModuleId::new(b"dep".to_vec()), b"return 42");
        delegate.insert(ModuleId::new(b"other".to_vec()), b"return 0");
        let source = SealedGraphSource::new(
            Arc::new(delegate),
            ModuleId::new(b"config".to_vec()),
            PathBuf::from("/config"),
        );
        source.allow_only([
            ModuleId::new(b"config".to_vec()),
            ModuleId::new(b"dep".to_vec()),
        ]);

        let dependency = source
            .resolve(None, b"./dep")
            .ready_only("resolve checked dependency")
            .expect("checked dependency resolves");
        assert_eq!(dependency, ModuleId::new(b"dep".to_vec()));
        source.seal();
        assert_eq!(
            source
                .resolve(None, b"./dep")
                .ready_only("replay checked resolution")
                .expect("checked resolution remains available"),
            dependency
        );
        let error = source
            .resolve(None, b"./other")
            .ready_only("resolve new edge after sealing")
            .expect_err("new edge must fail after sealing");
        assert!(error.to_string().contains("sealed"));
    }
}
