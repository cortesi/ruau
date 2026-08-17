use std::sync::Arc;

use ruau_bytecode::CompileOptions;
use ruau_surface::{ConfigError, Surface};
use ruau_vm::{Ambient, AmbientMode, ExecutionFeatures, Limits};

use super::{
    admission::{IngressAdmission, TenantResourceAccounting},
    pipeline::{Executor, compile_policy_fingerprint, default_type_check_concurrency},
    preflight::{DEFAULT_FRONT_DOOR_CACHE_BYTES, PreflightCache},
    types::{AggregateResourceLimits, IngressLimits, PreflightLimits},
};
use crate::lanes::{
    AdmissionLimits, AdmissionPolicy, DefaultAdmissionPolicy, LanePool, LaneStartupError,
};

/// Executor configuration error returned by [`Builder::build`].
#[derive(Debug, PartialEq, Eq)]
pub enum BuildError {
    /// No [`Surface`] was selected.
    MissingSurface,
    /// No [`Ambient`] was selected.
    MissingAmbient,
    /// The executor requires [`Ambient::production`].
    NonProductionAmbient,
    /// No source byte cap was set.
    MissingSourceCap,
    /// The configured source byte cap was zero.
    ZeroSourceCap,
    /// The base limits left the gas budget unbounded.
    MissingGasLimit,
    /// The configured gas budget was zero.
    ZeroGasLimit,
    /// The base limits left the memory cap unbounded.
    MissingMemoryLimit,
    /// The configured memory cap was zero.
    ZeroMemoryLimit,
    /// The configured lane count was zero.
    ZeroLaneCount,
    /// A lane runtime or OS thread could not be started.
    LaneStartup(LaneStartupError),
    /// No execution feature set was selected.
    MissingFeatures,
    /// No aggregate resource policy was selected.
    MissingAggregateResourceLimits,
    /// The configured preflight-cache byte budget was zero.
    ZeroPreflightCacheBytes,
    /// A compatibility feature is not supported by this executor.
    UnsupportedFeature,
    /// The capability surface itself failed validation.
    Surface(ConfigError),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "executor configuration error: ")?;
        let reason = match self {
            Self::MissingSurface => "no surface selected",
            Self::MissingAmbient => "no production ambient seam selected",
            Self::NonProductionAmbient => "production executor requires a production ambient seam",
            Self::MissingSourceCap => "no source byte cap configured",
            Self::ZeroSourceCap => "source byte cap is zero",
            Self::MissingGasLimit => "limits left the gas budget unbounded",
            Self::ZeroGasLimit => "gas budget is zero",
            Self::MissingMemoryLimit => "limits left the memory cap unbounded",
            Self::ZeroMemoryLimit => "memory cap is zero",
            Self::ZeroLaneCount => "lane count is zero",
            Self::LaneStartup(error) => return write!(f, "lane startup failed: {error}"),
            Self::MissingFeatures => "no execution feature set selected",
            Self::MissingAggregateResourceLimits => {
                "no aggregate resource limits selected (use AggregateResourceLimits::unlimited() deliberately)"
            }
            Self::ZeroPreflightCacheBytes => "preflight cache byte budget is zero",
            Self::UnsupportedFeature => {
                "a compatibility feature is enabled but not yet wired into the pipeline"
            }
            Self::Surface(error) => return write!(f, "invalid surface: {error}"),
        };
        f.write_str(reason)
    }
}

impl std::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::LaneStartup(error) => Some(error),
            Self::Surface(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ConfigError> for BuildError {
    fn from(error: ConfigError) -> Self {
        Self::Surface(error)
    }
}

/// Builder for a [`Executor`].
///
/// Required request-boundary fields are explicit; missing surface, limits,
/// ambient, source cap, or features are build errors.
#[derive(Default)]
pub struct Builder {
    surface: Option<Surface>,
    ambient: Option<Ambient>,
    base_limits: Option<Limits>,
    features: Option<ExecutionFeatures>,
    max_source_bytes: Option<usize>,
    compile_policy: Option<CompileOptions>,
    preflight: Option<PreflightLimits>,
    ingress: Option<IngressLimits>,
    aggregate_resources: Option<AggregateResourceLimits>,
    lane_count: Option<usize>,
    lane_admission: Option<AdmissionLimits>,
    lane_policy: Option<Arc<dyn AdmissionPolicy>>,
    max_concurrent_type_checks: Option<usize>,
    preflight_cache_bytes: Option<usize>,
}

impl Builder {
    /// Selects an already validated capability surface.
    #[must_use]
    pub fn surface(mut self, surface: Surface) -> Self {
        self.surface = Some(surface);
        self
    }

    /// Selects the clock, cancellation, and seed source.
    #[must_use]
    pub fn ambient(mut self, ambient: Ambient) -> Self {
        self.ambient = Some(ambient);
        self
    }

    /// Sets the base per-request limits. Gas and the memory cap are required; the
    /// per-request deadline and cancellation come from [`super::RunControl`].
    #[must_use]
    pub fn limits(mut self, limits: Limits) -> Self {
        self.base_limits = Some(limits);
        self
    }

    /// Sets per-invocation compatibility features.
    ///
    /// Unsupported switches fail [`build`](Self::build).
    #[must_use]
    pub fn features(mut self, features: ExecutionFeatures) -> Self {
        self.features = Some(features);
        self
    }

    /// Sets the source byte cap.
    #[must_use]
    pub fn max_source_bytes(mut self, bytes: usize) -> Self {
        self.max_source_bytes = Some(bytes);
        self
    }

    /// Sets pre-VM work limits such as diagnostic-output caps.
    #[must_use]
    pub fn preflight_limits(mut self, limits: PreflightLimits) -> Self {
        self.preflight = Some(limits);
        self
    }

    /// Sets request caps checked before parser/checker/compiler work starts.
    #[must_use]
    pub fn ingress_limits(mut self, limits: IngressLimits) -> Self {
        self.ingress = Some(limits);
        self
    }

    /// Sets per-tenant aggregate resource caps enforced across requests.
    /// This policy is required; use [`AggregateResourceLimits::unlimited`] to
    /// select uncapped accounting deliberately.
    #[must_use]
    pub fn aggregate_resource_limits(mut self, limits: AggregateResourceLimits) -> Self {
        self.aggregate_resources = Some(limits);
        self
    }

    /// Sets how many worker lanes run VM-owned request work.
    ///
    /// Each lane owns one OS thread. If unset, the executor uses one lane.
    #[must_use]
    pub fn lane_count(mut self, lanes: usize) -> Self {
        self.lane_count = Some(lanes);
        self
    }

    /// Sets lane-pool admission limits checked before VM execution.
    #[must_use]
    pub fn lane_admission_limits(mut self, limits: AdmissionLimits) -> Self {
        self.lane_admission = Some(limits);
        self
    }

    /// Sets the lane-pool admission policy.
    ///
    /// The executor still enforces [`AdmissionLimits`] after the policy's
    /// decision. Use [`crate::DefaultAdmissionPolicy`] from custom policies to
    /// delegate to the built-in FIFO behaviour.
    #[must_use]
    pub fn admission_policy(mut self, policy: Arc<dyn AdmissionPolicy>) -> Self {
        self.lane_policy = Some(policy);
        self
    }

    /// Overrides the VM compile policy. Surface library restrictions are
    /// applied on top at compile time regardless.
    #[must_use]
    pub fn compile_policy(mut self, policy: CompileOptions) -> Self {
        self.compile_policy = Some(policy);
        self
    }

    /// Caps how many type-check stages may run concurrently (minimum 1).
    /// Defaults to the host's available parallelism, clamped to at most 8.
    #[must_use]
    pub fn max_concurrent_type_checks(mut self, checks: usize) -> Self {
        self.max_concurrent_type_checks = Some(checks);
        self
    }

    /// Sets the maximum retained-byte estimate for shared preflight verdicts.
    /// Defaults to 64 MiB.
    #[must_use]
    pub fn preflight_cache_bytes(mut self, bytes: usize) -> Self {
        self.preflight_cache_bytes = Some(bytes);
        self
    }

    /// Builds the executor.
    ///
    /// # Errors
    /// Returns [`BuildError`] for missing required settings, zero caps, or
    /// unsupported features.
    pub fn build(self) -> Result<Executor, BuildError> {
        let surface = self.surface.ok_or(BuildError::MissingSurface)?;
        let ambient = self.ambient.ok_or(BuildError::MissingAmbient)?;
        if !matches!(ambient.mode, AmbientMode::Production) {
            return Err(BuildError::NonProductionAmbient);
        }
        let max_source_bytes = self.max_source_bytes.ok_or(BuildError::MissingSourceCap)?;
        if max_source_bytes == 0 {
            return Err(BuildError::ZeroSourceCap);
        }
        let base_limits = self.base_limits.unwrap_or_else(Limits::unlimited);
        match base_limits.gas {
            None => return Err(BuildError::MissingGasLimit),
            Some(0) => return Err(BuildError::ZeroGasLimit),
            Some(_) => {}
        }
        match base_limits.max_memory_bytes {
            None => return Err(BuildError::MissingMemoryLimit),
            Some(0) => return Err(BuildError::ZeroMemoryLimit),
            Some(_) => {}
        }
        let features = self.features.ok_or(BuildError::MissingFeatures)?;
        // Refuse to silently half-configure compatibility features the executor
        // cannot yet consume end to end.
        if features.fenv || features.harness_mode {
            return Err(BuildError::UnsupportedFeature);
        }
        let compile_policy = self.compile_policy.unwrap_or_default();
        let preflight = self.preflight.unwrap_or_default();
        let lane_count = self.lane_count.unwrap_or(1);
        if lane_count == 0 {
            return Err(BuildError::ZeroLaneCount);
        }
        // Admission limits fail closed: an unconfigured executor gets finite,
        // lane-derived caps instead of `usize::MAX`. Set explicit limits to
        // widen (or deliberately un-cap) them.
        let ingress = self
            .ingress
            .unwrap_or_else(|| IngressLimits::fail_closed(lane_count));
        let aggregate_limits = self
            .aggregate_resources
            .ok_or(BuildError::MissingAggregateResourceLimits)?;
        let preflight_cache_bytes = self
            .preflight_cache_bytes
            .unwrap_or(DEFAULT_FRONT_DOOR_CACHE_BYTES);
        if preflight_cache_bytes == 0 {
            return Err(BuildError::ZeroPreflightCacheBytes);
        }
        let lane_admission = self
            .lane_admission
            .unwrap_or_else(|| AdmissionLimits::fail_closed(lane_count));
        let resource_accounting = Arc::new(TenantResourceAccounting::default());
        let lane_policy = self
            .lane_policy
            .unwrap_or_else(|| Arc::new(DefaultAdmissionPolicy));
        let preflight_concurrency = self
            .max_concurrent_type_checks
            .unwrap_or_else(default_type_check_concurrency)
            .max(1);
        let lane_pool =
            LanePool::try_with_admission_policy(lane_count, lane_admission, lane_policy)
                .map_err(BuildError::LaneStartup)?;
        Ok(Executor {
            surface: Arc::new(surface),
            ambient,
            base_limits,
            features,
            max_source_bytes,
            compile_policy_fingerprint: compile_policy_fingerprint(&compile_policy),
            compile_policy,
            preflight,
            ingress: Arc::new(IngressAdmission::new(ingress)),
            aggregate_limits,
            resource_accounting,
            lane_pool,
            preflight_permits: Arc::new(tokio::sync::Semaphore::new(preflight_concurrency)),
            preflight_cache: PreflightCache::new(preflight_cache_bytes),
            #[cfg(any())]
            runtime_compiler_override: None,
        })
    }
}
