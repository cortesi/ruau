/// The PCG32 stream constant and increment used by `math.random` (upstream
/// `lmathlib.cpp`: multiplier `6364136223846793005`, `PCG32_INC = 105`). The
/// increment must be odd, so the dispatch formula uses `PCG32_INC | 1`.
const PCG32_MULT: u64 = 6_364_136_223_846_793_005;
const PCG32_INC: u64 = 105;

/// Advances a PCG32 state one step (upstream `*state = oldstate * MULT + INC`).
pub(super) fn pcg32_step(state: u64) -> u64 {
    state.wrapping_mul(PCG32_MULT).wrapping_add(PCG32_INC | 1)
}

/// Derives the initial stream state from a seed (upstream `pcg32_seed`: zero the
/// state, advance, add the seed, advance again).
pub(super) fn pcg32_seed(seed: u64) -> u64 {
    let state = pcg32_step(0).wrapping_add(seed);
    pcg32_step(state)
}

/// The PCG32 output word for a state (upstream `pcg32_random`'s xorshift-rotate). The state
/// is advanced separately by [`pcg32_step`].
pub(super) fn pcg32_output(state: u64) -> u32 {
    let xorshifted = (((state >> 18) ^ state) >> 27) as u32;
    let rot = (state >> 59) as u32;
    // upstream `(xorshifted >> rot) | (xorshifted << ((-rot) & 31))`.
    xorshifted.rotate_right(rot)
}

/// Salt mixed into the seed for the GC-stress PRNG so `GcPolicy::RandomizedSteps` draws an
/// independent stream from `math.random`'s `rngstate` — the collection schedule must not
/// perturb (or be perturbable through) program-visible randomness.
pub(super) const GC_RNG_SEED_SALT: u64 = 0x9E37_79B9_7F4A_7C15;

/// `GcPolicy::RandomizedSteps` collects at a top-level safepoint when the GC PRNG output is
/// divisible by this stride — a seeded ~1-in-N schedule that varies *when* collection lands
/// relative to allocation and mutation, surfacing GC-timing-dependent bugs a fixed cadence
/// misses, while staying cheaper than collecting every step.
pub(super) const GC_STRESS_STRIDE: u32 = 3;
