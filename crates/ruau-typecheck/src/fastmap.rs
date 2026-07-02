//! Deterministically seeded hash containers for checker-internal id tables.
//!
//! These replace ordered maps on hot lookup-only paths: every table using
//! them is probed by key and never iterated for output ordering (branch-join
//! and diagnostic ordering normalize through explicit sorted sets), so the
//! fixed-seed foldhash state keeps builds reproducible while dropping the
//! ordered-map comparison and rebalancing cost.

use std::collections::{HashMap, HashSet};

use foldhash::fast::FixedState;

/// Fixed-seed hash map for checker-internal fact tables.
pub type FastMap<K, V> = HashMap<K, V, FixedState>;

/// Fixed-seed hash set sibling of [`FastMap`].
pub type FastSet<T> = HashSet<T, FixedState>;
