use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use ruau_bytecode::BytecodeChunk;
use ruau_typecheck::Diagnostics;

/// What the front door learned about one source under one surface: either the
/// verdict failed with diagnostics, or it passed and compiled to a chunk.
#[derive(Clone)]
pub struct FrontDoorVerdict {
    pub(super) ast_nodes: usize,
    pub(super) type_arena_nodes: usize,
    pub(super) outcome: FrontDoorOutcome,
}

#[derive(Clone)]
pub enum FrontDoorOutcome {
    TypeErrors(Diagnostics),
    Chunk(Arc<BytecodeChunk>),
}

/// Bounded source-verdict cache shared across tenants by design: every cached
/// value derives from source bytes, compile options, module-source epoch, and
/// the surface identity.
#[derive(Default)]
pub struct FrontDoorCache {
    entries: Mutex<FrontDoorCacheMap>,
}

#[derive(Default)]
struct FrontDoorCacheMap {
    map: HashMap<[u8; 32], FrontDoorVerdict>,
    order: VecDeque<[u8; 32]>,
}

const FRONT_DOOR_CACHE_ENTRIES: usize = 256;

impl FrontDoorCache {
    pub(super) fn get(&self, key: &[u8; 32]) -> Option<FrontDoorVerdict> {
        let inner = self.entries.lock().ok()?;
        inner.map.get(key).cloned()
    }

    pub(super) fn insert(&self, key: [u8; 32], verdict: FrontDoorVerdict) {
        let Ok(mut inner) = self.entries.lock() else {
            return;
        };
        if inner.map.insert(key, verdict).is_none() {
            inner.order.push_back(key);
            while inner.order.len() > FRONT_DOOR_CACHE_ENTRIES {
                if let Some(evicted) = inner.order.pop_front() {
                    inner.map.remove(&evicted);
                }
            }
        }
    }
}
