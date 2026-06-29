/// Per-invocation compatibility switches.
///
/// Every switch is off by default. Hosts pass the same value to the checker,
/// compiler, and VM for one execution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecutionFeatures {
    /// `getfenv` / `setfenv` / `safeenv` compatibility.
    pub fenv: bool,
    /// Conformance-harness helpers and compatibility-only globals. Never a
    /// tenant capability.
    pub harness_mode: bool,
}

impl ExecutionFeatures {
    /// Every compatibility switch off.
    #[must_use]
    pub const fn all_off() -> Self {
        Self {
            fenv: false,
            harness_mode: false,
        }
    }

    /// Whether any compatibility switch is enabled.
    #[must_use]
    pub const fn any_enabled(self) -> bool {
        self.fenv || self.harness_mode
    }
}
