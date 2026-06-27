//! Luau debug magic type names that affect checker behavior.

/// Upstream magic annotation that forces a constraint-solving-incomplete
/// diagnostic while lowering to `any`.
pub const LUAU_FORCE_CONSTRAINT_SOLVING_INCOMPLETE: &str =
    "_luau_force_constraint_solving_incomplete";

/// Stable diagnostic payload marker for
/// [`LUAU_FORCE_CONSTRAINT_SOLVING_INCOMPLETE`].
pub const FORCED_CONSTRAINT_SOLVING_INCOMPLETE_KIND: &str = "constraint-solving-incomplete-forced";
