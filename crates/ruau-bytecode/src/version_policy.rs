//! Shared bytecode version policy for public and upstream-fixture paths.

use crate::builder::{DEFAULT_VERSION, FIXTURE_TOOLING_CLASS_VERSION, FIXTURE_TOOLING_MAX_VERSION};

/// Bytecode version acceptance policy.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BytecodeVersionPolicy {
    /// Public APIs accept only the pinned current bytecode version.
    Public,
    /// Repository-owned upstream fixture tooling accepts current-baseline
    /// sidecar versions emitted by the pinned upstream compiler.
    UpstreamFixture,
}

impl BytecodeVersionPolicy {
    /// Returns whether `version` is accepted by this policy.
    #[must_use]
    pub fn accepts(self, version: u8) -> bool {
        match self {
            Self::Public => version == DEFAULT_VERSION,
            Self::UpstreamFixture => {
                (DEFAULT_VERSION..=FIXTURE_TOOLING_MAX_VERSION).contains(&version)
                    || version == FIXTURE_TOOLING_CLASS_VERSION
            }
        }
    }

    /// Returns the shared unsupported-version diagnostic for this policy.
    #[must_use]
    pub fn unsupported_version_message(self, version: u8) -> String {
        match self {
            Self::Public => format!(
                "unsupported bytecode version {version}; Ruau supports only current public bytecode version {DEFAULT_VERSION}"
            ),
            Self::UpstreamFixture => format!(
                "unsupported bytecode version {version}; upstream fixture tooling accepts versions {DEFAULT_VERSION}..={FIXTURE_TOOLING_MAX_VERSION} and {FIXTURE_TOOLING_CLASS_VERSION}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_policy_accepts_only_default_version() {
        assert!(BytecodeVersionPolicy::Public.accepts(DEFAULT_VERSION));
        assert!(!BytecodeVersionPolicy::Public.accepts(DEFAULT_VERSION - 1));
        assert!(!BytecodeVersionPolicy::Public.accepts(DEFAULT_VERSION + 1));
    }

    #[test]
    fn fixture_policy_accepts_current_upstream_sidecar_range_only() {
        assert!(BytecodeVersionPolicy::UpstreamFixture.accepts(DEFAULT_VERSION));
        assert!(BytecodeVersionPolicy::UpstreamFixture.accepts(FIXTURE_TOOLING_MAX_VERSION));
        assert!(BytecodeVersionPolicy::UpstreamFixture.accepts(FIXTURE_TOOLING_CLASS_VERSION));
        assert!(!BytecodeVersionPolicy::UpstreamFixture.accepts(DEFAULT_VERSION - 1));
        assert!(!BytecodeVersionPolicy::UpstreamFixture.accepts(FIXTURE_TOOLING_MAX_VERSION + 1));
        assert!(!BytecodeVersionPolicy::UpstreamFixture.accepts(FIXTURE_TOOLING_CLASS_VERSION - 1));
    }

    #[test]
    fn unsupported_version_messages_are_policy_specific() {
        assert_eq!(
            BytecodeVersionPolicy::Public.unsupported_version_message(99),
            format!(
                "unsupported bytecode version 99; Ruau supports only current public bytecode version {DEFAULT_VERSION}"
            )
        );
        assert_eq!(
            BytecodeVersionPolicy::UpstreamFixture.unsupported_version_message(3),
            format!(
                "unsupported bytecode version 3; upstream fixture tooling accepts versions {DEFAULT_VERSION}..={FIXTURE_TOOLING_MAX_VERSION} and {FIXTURE_TOOLING_CLASS_VERSION}"
            )
        );
    }
}
