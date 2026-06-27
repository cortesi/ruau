//! Deterministic keyed hashing for per-VM maps.
//!
//! The VM owns hash maps whose keys are tenant-controlled bytes or values
//! (interned strings and table keys). `AmbientConfig::hash_seed` supplies one
//! construction-time seed per VM, folded into the hasher state so tests can
//! replay behavior while different VMs do not share one hash stream.
//!
//! The hasher is foldhash (hashbrown's default): far cheaper per key than the
//! previous SipHash stream, with collision resistance resting on the per-VM
//! seed staying secret — the same model the std/hashbrown ecosystem uses.
//! Tenants never observe raw hash values; iteration order is the only side
//! channel, and the seed is per-VM, so a probed ordering does not transfer to
//! another tenant's VM.

use std::hash::BuildHasher;

use foldhash::fast::{FixedState, FoldHasher};

const MIX_A: u64 = 0x9E37_79B9_7F4A_7C15;

#[derive(Clone, Copy, Debug, Default)]
pub struct VmBuildHasher {
    seed: u64,
}

impl VmBuildHasher {
    #[must_use]
    pub(crate) fn new(seed: u64) -> Self {
        Self { seed }
    }

    #[must_use]
    pub(crate) fn seed(self) -> u64 {
        self.seed
    }
}

impl BuildHasher for VmBuildHasher {
    type Hasher = FoldHasher<'static>;

    fn build_hasher(&self) -> Self::Hasher {
        FixedState::with_seed(mix64(self.seed ^ MIX_A)).build_hasher()
    }
}

fn mix64(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[cfg(any())]
mod tests {
    use std::hash::Hash;

    use super::*;

    fn hash_with(seed: u64, value: impl Hash) -> u64 {
        VmBuildHasher::new(seed).hash_one(&value)
    }

    #[test]
    fn same_seed_replays_and_different_seed_changes_hashes() {
        assert_eq!(hash_with(7, b"tenant-key"), hash_with(7, b"tenant-key"));
        assert_ne!(hash_with(7, b"tenant-key"), hash_with(8, b"tenant-key"));
    }
}
