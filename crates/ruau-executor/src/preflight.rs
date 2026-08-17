use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use ruau_bytecode::BytecodeChunk;
use ruau_typecheck::Diagnostics;

/// What the front door learned about one source under one surface: either the
/// verdict failed with diagnostics, or it passed and compiled to a chunk.
#[derive(Clone)]
pub struct PreflightVerdict {
    pub(super) ast_nodes: usize,
    pub(super) type_arena_nodes: usize,
    pub(super) retained_bytes: usize,
    pub(super) outcome: PreflightOutcome,
}

#[derive(Clone)]
pub enum PreflightOutcome {
    TypeErrors(Diagnostics),
    Chunk(Arc<BytecodeChunk>),
}

/// Bounded source-verdict cache shared across tenants by design: every cached
/// value derives from source bytes, compile options, module-source epoch, and
/// the surface identity.
pub struct PreflightCache {
    entries: Mutex<PreflightCacheMap>,
    max_bytes: usize,
}

#[derive(Default)]
struct PreflightCacheMap {
    map: HashMap<[u8; 32], PreflightVerdict>,
    order: VecDeque<[u8; 32]>,
    retained_bytes: usize,
}

const FRONT_DOOR_CACHE_ENTRIES: usize = 256;
pub const DEFAULT_FRONT_DOOR_CACHE_BYTES: usize = 64 * 1024 * 1024;

impl Default for PreflightCache {
    fn default() -> Self {
        Self::new(DEFAULT_FRONT_DOOR_CACHE_BYTES)
    }
}

impl PreflightCache {
    pub(super) fn new(max_bytes: usize) -> Self {
        Self {
            entries: Mutex::default(),
            max_bytes,
        }
    }

    pub(super) fn get(&self, key: &[u8; 32]) -> Option<PreflightVerdict> {
        let inner = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.map.get(key).cloned()
    }

    pub(super) fn insert(&self, key: [u8; 32], verdict: PreflightVerdict) {
        let mut inner = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let retained_bytes = verdict.retained_bytes;
        if let Some(previous) = inner.map.insert(key, verdict) {
            inner.retained_bytes = inner.retained_bytes.saturating_sub(previous.retained_bytes);
        } else {
            inner.order.push_back(key);
        }
        inner.retained_bytes = inner.retained_bytes.saturating_add(retained_bytes);
        while inner.order.len() > FRONT_DOOR_CACHE_ENTRIES || inner.retained_bytes > self.max_bytes
        {
            if let Some(evicted) = inner.order.pop_front()
                && let Some(verdict) = inner.map.remove(&evicted)
            {
                inner.retained_bytes = inner.retained_bytes.saturating_sub(verdict.retained_bytes);
            }
        }
    }
}

#[cfg(any())]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;

    #[test]
    fn cache_recovers_a_poisoned_lock() {
        let cache = PreflightCache::default();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = cache.entries.lock().expect("cache lock starts healthy");
            panic!("poison cache lock");
        }));
        assert!(result.is_err());

        let key = [7; 32];
        cache.insert(
            key,
            PreflightVerdict {
                ast_nodes: 1,
                type_arena_nodes: 2,
                retained_bytes: 0,
                outcome: PreflightOutcome::TypeErrors(Diagnostics::new()),
            },
        );

        let cached = cache.get(&key).expect("cache remains usable");
        assert_eq!(cached.ast_nodes, 1);
        assert_eq!(cached.type_arena_nodes, 2);
        assert!(matches!(cached.outcome, PreflightOutcome::TypeErrors(_)));
    }

    #[test]
    fn cache_evicts_until_under_the_byte_budget() {
        let cache = PreflightCache::new(10);
        for key in 0..3 {
            cache.insert(
                [key; 32],
                PreflightVerdict {
                    ast_nodes: 1,
                    type_arena_nodes: 1,
                    retained_bytes: 6,
                    outcome: PreflightOutcome::TypeErrors(Diagnostics::new()),
                },
            );
        }

        assert!(cache.get(&[0; 32]).is_none());
        assert!(cache.get(&[1; 32]).is_none());
        assert!(cache.get(&[2; 32]).is_some());
    }
}
