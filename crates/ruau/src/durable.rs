//! Durable actor state primitives.
//!
//! These types describe the store contract used by durable agent hosts. The VM
//! never sees the store as an ambient global; host modules decide what capped
//! state surface a script can read or update.

use std::{
    collections::BTreeMap,
    future::{Future, ready},
    pin::Pin,
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::vm::MarshaledValue;

/// Result returned by durable state-store operations.
pub type StateStoreResult<T> = Result<T, StateStoreError>;

/// `Send + Sync` on native targets; no bound on wasm32, whose executors are
/// single-threaded and whose JS-backed stores are `!Send`.
#[cfg(not(target_arch = "wasm32"))]
#[doc(hidden)]
pub trait MaybeSendSync: Send + Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + Sync> MaybeSendSync for T {}
/// `Send + Sync` on native targets; no bound on wasm32, whose executors are
/// single-threaded and whose JS-backed stores are `!Send`.
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub trait MaybeSendSync {}
#[cfg(target_arch = "wasm32")]
impl<T> MaybeSendSync for T {}

#[cfg(not(target_arch = "wasm32"))]
/// Boxed future returned by durable state stores.
pub type StateStoreFuture<T> = Pin<Box<dyn Future<Output = StateStoreResult<T>> + Send + 'static>>;
/// The boxed future a state store returns (wasm: no `Send` bound; Durable
/// Object storage futures are JS-backed and `!Send`).
#[cfg(target_arch = "wasm32")]
pub type StateStoreFuture<T> = Pin<Box<dyn Future<Output = StateStoreResult<T>> + 'static>>;

/// A durable actor identifier.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct ActorId(String);

impl ActorId {
    /// Creates an actor id.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The actor id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ActorId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ActorId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Monotonic state generation for one actor.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct StateGeneration(u64);

impl StateGeneration {
    /// Wraps a raw generation value (backend constructor).
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw generation value.
    #[must_use]
    pub fn value(self) -> u64 {
        self.0
    }
}

/// Backend-generated monotonic lease token.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct LeaseToken(u64);

impl LeaseToken {
    /// Wraps a raw token value (backend constructor).
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw token value.
    #[must_use]
    pub fn value(self) -> u64 {
        self.0
    }
}

/// A fenced lease on one actor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateLease {
    actor: ActorId,
    token: LeaseToken,
    generation: StateGeneration,
}

impl StateLease {
    /// Mints a lease. Backend constructor: only a [`StateStore`]
    /// implementation should create leases, with a token it generated and
    /// the generation it observed — embedders treat leases as opaque.
    #[must_use]
    pub fn new(actor: ActorId, token: LeaseToken, generation: StateGeneration) -> Self {
        Self {
            actor,
            token,
            generation,
        }
    }

    /// The leased actor.
    #[must_use]
    pub fn actor(&self) -> &ActorId {
        &self.actor
    }

    /// The backend-generated lease token.
    #[must_use]
    pub fn token(&self) -> LeaseToken {
        self.token
    }

    /// The state generation observed when the lease started.
    #[must_use]
    pub fn generation(&self) -> StateGeneration {
        self.generation
    }
}

/// Outcome of trying to start one actor invocation.
#[derive(Clone, Debug, PartialEq)]
pub enum StartOutcome {
    /// The actor lease was acquired.
    Started {
        /// Fenced lease for heartbeat and commit.
        lease: StateLease,
        /// Current durable state snapshot.
        state: MarshaledValue,
    },
    /// Another invocation already owns this actor.
    Busy {
        /// Actor that could not be claimed.
        actor: ActorId,
        /// Current state generation.
        generation: StateGeneration,
    },
}

/// A wake requested by a committed invocation.
///
/// Wakes may target the committing actor or another actor. The store stamps the
/// queued wake with the target actor's current generation so a wake runner can
/// drop stale redeliveries without guessing from the source actor's generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WakeRequest {
    actor: ActorId,
    reason: String,
}

impl WakeRequest {
    /// Creates a wake request for `actor`.
    #[must_use]
    pub fn new(actor: impl Into<ActorId>, reason: impl Into<String>) -> Self {
        Self {
            actor: actor.into(),
            reason: reason.into(),
        }
    }

    /// The actor to wake.
    #[must_use]
    pub fn actor(&self) -> &ActorId {
        &self.actor
    }

    /// Human-readable wake reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// A wake recorded by the store after a successful commit.
///
/// The generation is the target actor generation observed by the store when the
/// wake was queued. For self-wakes this is the committing actor's generation
/// after the commit; for cross-actor wakes it is the target actor's current
/// generation at enqueue time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedWake {
    actor: ActorId,
    generation: StateGeneration,
    reason: String,
}

impl QueuedWake {
    fn from_request(request: WakeRequest, generation: StateGeneration) -> Self {
        Self {
            actor: request.actor,
            generation,
            reason: request.reason,
        }
    }

    /// The actor to wake.
    #[must_use]
    pub fn actor(&self) -> &ActorId {
        &self.actor
    }

    /// Generation that scheduled this wake.
    #[must_use]
    pub fn generation(&self) -> StateGeneration {
        self.generation
    }

    /// Human-readable wake reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Outcome of a successful fenced commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitOutcome {
    generation: StateGeneration,
    wakes: Vec<QueuedWake>,
}

impl CommitOutcome {
    /// Builds a commit outcome. Backend constructor: only a [`StateStore`]
    /// implementation should create these, after its fenced compare passed.
    #[must_use]
    pub fn new(generation: StateGeneration, wakes: Vec<QueuedWake>) -> Self {
        Self { generation, wakes }
    }

    /// The actor generation after the commit.
    #[must_use]
    pub fn generation(&self) -> StateGeneration {
        self.generation
    }

    /// Wakes recorded by this commit.
    #[must_use]
    pub fn wakes(&self) -> &[QueuedWake] {
        &self.wakes
    }
}

/// Durable state-store contract.
///
/// Leases carry a backend-policy time-to-live: `try_start` may reclaim an
/// actor whose active lease has expired (issuing a fresh fencing token), and
/// a valid `heartbeat` extends the holder's lease. An expired holder's
/// heartbeat/commit/abandon reject as stale — fencing means a crashed or
/// stalled holder can never wedge an actor as Busy forever, and can never
/// commit over a reclaimed generation.
pub trait StateStore: MaybeSendSync + 'static {
    /// Attempts to claim an actor for one invocation.
    fn try_start(&self, actor: ActorId) -> StateStoreFuture<StartOutcome>;

    /// Heartbeats an active lease.
    fn heartbeat(&self, lease: StateLease) -> StateStoreFuture<()>;

    /// Releases an active lease without committing state or wakes.
    fn abandon(&self, lease: StateLease) -> StateStoreFuture<()>;

    /// Commits state and wakes if the lease still owns the actor generation.
    fn commit(
        &self,
        lease: StateLease,
        state: MarshaledValue,
        wakes: Vec<WakeRequest>,
    ) -> StateStoreFuture<CommitOutcome>;
}

/// Error returned by durable state stores.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StateStoreError {
    /// The backend failed internally (a poisoned lock, a storage I/O error,
    /// a serialization failure, …).
    Internal {
        /// Backend-specific failure description.
        detail: String,
    },
    /// The store exhausted its monotonic lease token space.
    TokenExhausted,
    /// An actor exhausted its monotonic generation space.
    GenerationExhausted {
        /// Actor whose generation could not advance.
        actor: ActorId,
    },
    /// The lease no longer owns the actor.
    StaleLease {
        /// Leased actor.
        actor: ActorId,
        /// Rejected token.
        token: LeaseToken,
    },
    /// A backend or embedder policy rejected an oversized state snapshot.
    ValueSizeLimit {
        /// Size of the rejected snapshot in policy-defined bytes.
        bytes: usize,
        /// Maximum accepted snapshot size.
        cap: usize,
    },
    /// A backend or embedder policy rejected a new actor slot for a tenant.
    TenantActorLimit {
        /// Tenant whose actor slot cap was reached.
        tenant: String,
        /// Number of actor slots already reserved for the tenant.
        actors: usize,
        /// Maximum actor slots accepted for the tenant.
        cap: usize,
    },
}

impl std::fmt::Display for StateStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Internal { detail } => write!(f, "state store backend failure: {detail}"),
            Self::TokenExhausted => f.write_str("state store lease token space is exhausted"),
            Self::GenerationExhausted { actor } => {
                write!(
                    f,
                    "state generation for actor `{}` is exhausted",
                    actor.as_str()
                )
            }
            Self::StaleLease { actor, token } => write!(
                f,
                "lease token {} no longer owns actor `{}`",
                token.0,
                actor.as_str()
            ),
            Self::ValueSizeLimit { bytes, cap } => {
                write!(f, "state snapshot is {bytes} byte(s), exceeding cap {cap}")
            }
            Self::TenantActorLimit {
                tenant,
                actors,
                cap,
            } => write!(
                f,
                "tenant `{tenant}` has {actors} actor slot(s), exceeding cap {cap}"
            ),
        }
    }
}

impl std::error::Error for StateStoreError {}

/// In-memory backends for the durable contract.
///
/// Use this module for examples, local tools, and tests that need a
/// [`StateStore`] without external storage.
pub mod memory {
    use super::*;

    /// In-memory durable state store for examples, local tools, and lightweight hosts.
    pub struct InMemoryStore {
        inner: Mutex<InMemoryState>,
        /// How long a lease stays valid past its acquisition or last heartbeat.
        /// An expired lease is reclaimable by the next `try_start`, and its
        /// holder's heartbeat/commit/abandon reject as stale — so a crashed
        /// holder cannot wedge an actor as Busy forever.
        lease_ttl: Duration,
    }

    impl Default for InMemoryStore {
        fn default() -> Self {
            Self {
                inner: Mutex::default(),
                lease_ttl: DEFAULT_LEASE_TTL,
            }
        }
    }

    /// Default lease time-to-live.
    pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(30);

    #[derive(Default)]
    struct InMemoryState {
        next_token: u64,
        actors: BTreeMap<ActorId, ActorEntry>,
        wakes: Vec<QueuedWake>,
    }

    #[derive(Clone, Debug)]
    struct ActorEntry {
        generation: StateGeneration,
        state: MarshaledValue,
        active: Option<LeaseToken>,
        /// The instant the active lease stops being valid.
        lease_expires_at: Option<Instant>,
    }

    impl Default for ActorEntry {
        fn default() -> Self {
            Self {
                generation: StateGeneration::default(),
                state: MarshaledValue::Nil,
                active: None,
                lease_expires_at: None,
            }
        }
    }

    impl ActorEntry {
        /// Whether `lease` currently owns this actor: token and generation match
        /// and the lease has not expired.
        fn owned_by(&self, lease: &StateLease, now: Instant) -> bool {
            self.active == Some(lease.token)
                && self.generation == lease.generation
                && self.lease_expires_at.is_none_or(|expires| now < expires)
        }
    }

    impl InMemoryStore {
        /// Locks the store, mapping a poisoned lock to
        /// [`StateStoreError::Internal`].
        fn locked(&self) -> StateStoreResult<std::sync::MutexGuard<'_, InMemoryState>> {
            self.inner.lock().map_err(|_| StateStoreError::Internal {
                detail: "state store lock is poisoned".to_owned(),
            })
        }

        /// Creates an empty in-memory store with the default lease TTL.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Creates an empty in-memory store with an explicit lease TTL.
        #[must_use]
        pub fn with_lease_ttl(lease_ttl: Duration) -> Self {
            Self {
                inner: Mutex::default(),
                lease_ttl,
            }
        }

        /// Returns the current state snapshot for `actor`.
        ///
        /// # Errors
        /// Returns [`StateStoreError::Internal`] if the store lock was poisoned.
        pub fn state(&self, actor: &ActorId) -> StateStoreResult<Option<MarshaledValue>> {
            let inner = self.locked()?;
            Ok(inner.actors.get(actor).map(|entry| entry.state.clone()))
        }

        /// Returns all wakes recorded so far.
        ///
        /// # Errors
        /// Returns [`StateStoreError::Internal`] if the store lock was poisoned.
        pub fn queued_wakes(&self) -> StateStoreResult<Vec<QueuedWake>> {
            let inner = self.locked()?;
            Ok(inner.wakes.clone())
        }

        fn ready<T: Send + 'static>(result: StateStoreResult<T>) -> StateStoreFuture<T> {
            Box::pin(ready(result))
        }

        fn reject_stale(lease: &StateLease) -> StateStoreError {
            StateStoreError::StaleLease {
                actor: lease.actor.clone(),
                token: lease.token,
            }
        }
    }

    impl StateStore for InMemoryStore {
        fn try_start(&self, actor: ActorId) -> StateStoreFuture<StartOutcome> {
            let result = (|| {
                let now = Instant::now();
                let mut inner = self.locked()?;
                if let Some(entry) = inner.actors.get(&actor)
                    && entry.active.is_some()
                    && entry.lease_expires_at.is_none_or(|expires| now < expires)
                {
                    return Ok(StartOutcome::Busy {
                        actor,
                        generation: entry.generation,
                    });
                }
                // Any previously active lease here has expired: issuing a fresh
                // token reclaims the actor, and the old holder's fenced
                // heartbeat/commit reject on the token mismatch.
                inner.next_token = inner
                    .next_token
                    .checked_add(1)
                    .ok_or(StateStoreError::TokenExhausted)?;
                let token = LeaseToken::new(inner.next_token);
                let lease_ttl = self.lease_ttl;
                let entry = inner.actors.entry(actor.clone()).or_default();
                entry.active = Some(token);
                entry.lease_expires_at = now.checked_add(lease_ttl);
                Ok(StartOutcome::Started {
                    lease: StateLease::new(actor, token, entry.generation),
                    state: entry.state.clone(),
                })
            })();
            Self::ready(result)
        }

        fn heartbeat(&self, lease: StateLease) -> StateStoreFuture<()> {
            let result = (|| {
                let now = Instant::now();
                let mut inner = self.locked()?;
                let lease_ttl = self.lease_ttl;
                let Some(entry) = inner.actors.get_mut(&lease.actor) else {
                    return Err(Self::reject_stale(&lease));
                };
                if entry.owned_by(&lease, now) {
                    // A valid heartbeat extends the lease.
                    entry.lease_expires_at = now.checked_add(lease_ttl);
                    Ok(())
                } else {
                    Err(Self::reject_stale(&lease))
                }
            })();
            Self::ready(result)
        }

        fn abandon(&self, lease: StateLease) -> StateStoreFuture<()> {
            let result = (|| {
                let mut inner = self.locked()?;
                let entry = inner
                    .actors
                    .get_mut(&lease.actor)
                    .ok_or_else(|| Self::reject_stale(&lease))?;
                if !entry.owned_by(&lease, Instant::now()) {
                    return Err(Self::reject_stale(&lease));
                }
                entry.active = None;
                entry.lease_expires_at = None;
                Ok(())
            })();
            Self::ready(result)
        }

        fn commit(
            &self,
            lease: StateLease,
            state: MarshaledValue,
            wakes: Vec<WakeRequest>,
        ) -> StateStoreFuture<CommitOutcome> {
            let result = (|| {
                let mut inner = self.locked()?;
                let entry = inner
                    .actors
                    .get_mut(&lease.actor)
                    .ok_or_else(|| Self::reject_stale(&lease))?;
                if !entry.owned_by(&lease, Instant::now()) {
                    return Err(Self::reject_stale(&lease));
                }
                entry.generation =
                    StateGeneration::new(entry.generation.value().checked_add(1).ok_or_else(
                        || StateStoreError::GenerationExhausted {
                            actor: lease.actor.clone(),
                        },
                    )?);
                entry.state = state;
                entry.active = None;
                entry.lease_expires_at = None;
                let generation = entry.generation;
                let queued: Vec<_> = wakes
                    .into_iter()
                    .map(|wake| {
                        let wake_generation = if wake.actor == lease.actor {
                            generation
                        } else {
                            inner
                                .actors
                                .get(&wake.actor)
                                .map_or(StateGeneration::default(), |entry| entry.generation)
                        };
                        QueuedWake::from_request(wake, wake_generation)
                    })
                    .collect();
                inner.wakes.extend(queued.iter().cloned());
                Ok(CommitOutcome::new(generation, queued))
            })();
            Self::ready(result)
        }
    }
}

#[cfg(any())]
mod tests {
    use super::*;

    async fn start(store: &memory::InMemoryStore, actor: &str) -> (StateLease, MarshaledValue) {
        match store
            .try_start(ActorId::from(actor))
            .await
            .expect("start succeeds")
        {
            StartOutcome::Started { lease, state } => (lease, state),
            StartOutcome::Busy { .. } => panic!("actor should be claimable"),
        }
    }

    #[tokio::test]
    async fn in_memory_store_claims_one_actor_serially_and_commits_state() {
        let store = memory::InMemoryStore::new();
        let (lease, state) = start(&store, "agent/a").await;
        assert_eq!(state, MarshaledValue::Nil);
        assert!(matches!(
            store
                .try_start(ActorId::from("agent/a"))
                .await
                .expect("busy is not an error"),
            StartOutcome::Busy { generation, .. } if generation.value() == 0
        ));
        store
            .heartbeat(lease.clone())
            .await
            .expect("active lease heartbeats");
        let outcome = store
            .commit(
                lease,
                MarshaledValue::Table(vec![]),
                vec![WakeRequest::new("agent/a", "continue")],
            )
            .await
            .expect("commit succeeds");
        assert_eq!(outcome.generation(), StateGeneration::new(1));
        assert_eq!(outcome.wakes().len(), 1);
        assert_eq!(
            store.state(&ActorId::from("agent/a")).expect("state reads"),
            Some(MarshaledValue::Table(vec![]))
        );
    }

    #[tokio::test]
    async fn in_memory_store_rejects_stale_leases_and_uses_monotonic_tokens() {
        let store = memory::InMemoryStore::new();
        let (first, _) = start(&store, "agent/a").await;
        store
            .commit(first.clone(), MarshaledValue::Number(1.0), vec![])
            .await
            .expect("first commit succeeds");
        assert!(matches!(
            store.heartbeat(first.clone()).await,
            Err(StateStoreError::StaleLease { .. })
        ));

        let (second, state) = start(&store, "agent/a").await;
        assert_eq!(state, MarshaledValue::Number(1.0));
        assert!(second.token().value() > first.token().value());
        assert_eq!(second.generation(), StateGeneration::new(1));
        assert!(matches!(
            store
                .commit(first, MarshaledValue::Number(2.0), vec![])
                .await,
            Err(StateStoreError::StaleLease { .. })
        ));
    }

    #[tokio::test]
    async fn in_memory_store_abandons_leases_without_advancing_state() {
        let store = memory::InMemoryStore::new();
        let (first, _) = start(&store, "agent/a").await;
        store
            .abandon(first.clone())
            .await
            .expect("active lease can be abandoned");
        assert_eq!(
            store.state(&ActorId::from("agent/a")).expect("state reads"),
            Some(MarshaledValue::Nil)
        );

        let (second, state) = start(&store, "agent/a").await;
        assert_eq!(state, MarshaledValue::Nil);
        assert_eq!(second.generation(), StateGeneration::new(0));
        assert!(second.token().value() > first.token().value());
        assert!(matches!(
            store.abandon(first).await,
            Err(StateStoreError::StaleLease { .. })
        ));
    }

    #[tokio::test]
    async fn expired_leases_are_reclaimed_and_fenced() {
        // A zero TTL expires every lease immediately, making expiry
        // deterministic: the next try_start reclaims the actor with a fresh
        // fencing token, and the crashed holder's lease is stale everywhere.
        let store = memory::InMemoryStore::with_lease_ttl(Duration::ZERO);
        let (crashed, _state) = start(&store, "actor").await;

        let reclaimed = match store
            .try_start(ActorId::from("actor"))
            .await
            .expect("reclaim succeeds")
        {
            StartOutcome::Started { lease, .. } => lease,
            StartOutcome::Busy { .. } => panic!("an expired lease must not wedge the actor"),
        };
        assert_ne!(
            reclaimed.token, crashed.token,
            "reclaim issues a fresh token"
        );

        assert!(matches!(
            store.heartbeat(crashed.clone()).await,
            Err(StateStoreError::StaleLease { .. })
        ));
        assert!(matches!(
            store.commit(crashed, MarshaledValue::Nil, Vec::new()).await,
            Err(StateStoreError::StaleLease { .. })
        ));
    }

    #[tokio::test]
    async fn live_leases_stay_exclusive_and_heartbeats_extend() {
        let store = memory::InMemoryStore::with_lease_ttl(Duration::from_secs(3600));
        let (lease, _state) = start(&store, "actor").await;

        assert!(matches!(
            store
                .try_start(ActorId::from("actor"))
                .await
                .expect("second start resolves"),
            StartOutcome::Busy { .. }
        ));
        store
            .heartbeat(lease.clone())
            .await
            .expect("a live lease heartbeats");
        store
            .commit(lease, MarshaledValue::Nil, Vec::new())
            .await
            .expect("a live lease commits");
    }
}
