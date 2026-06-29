use std::sync::Arc;

use ruau_bytecode::CompileOptions;
use ruau_surface::{ConfigError, Surface};
use ruau_vm::{Ambient, AmbientMode, ExecutionFeatures, Limits};

use super::{
    admission::{IngressAdmission, RunnerLaneAdmissionPolicy, TenantResourceAccounting},
    pipeline::{Runner, default_type_check_concurrency},
    types::{AggregateResourceLimits, FrontDoorLimits, IngressLimits},
};
use crate::lanes::{AdmissionLimits, LanePool};

/// Builder for a [`Runner`].
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
    compile_options: Option<CompileOptions>,
    front_door: Option<FrontDoorLimits>,
    ingress: Option<IngressLimits>,
    aggregate_resources: Option<AggregateResourceLimits>,
    lane_count: Option<usize>,
    lane_admission: Option<AdmissionLimits>,
    max_concurrent_type_checks: Option<usize>,
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
    /// per-request deadline and cancellation come from [`super::Budget`].
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
    pub fn front_door_limits(mut self, limits: FrontDoorLimits) -> Self {
        self.front_door = Some(limits);
        self
    }

    /// Sets request caps checked before parser/checker/compiler work starts.
    #[must_use]
    pub fn ingress_limits(mut self, limits: IngressLimits) -> Self {
        self.ingress = Some(limits);
        self
    }

    /// Sets per-tenant aggregate resource caps enforced across requests. Defaults
    /// to unlimited aggregate budgets.
    #[must_use]
    pub fn aggregate_resource_limits(mut self, limits: AggregateResourceLimits) -> Self {
        self.aggregate_resources = Some(limits);
        self
    }

    /// Sets how many worker lanes run VM-owned request work.
    ///
    /// Each lane owns one OS thread. If unset, the runner uses one lane.
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

    /// Overrides the compiler options. Defaults to
    /// [`CompileOptions::for_vm_execution`]; surface library restrictions are
    /// applied on top at compile time regardless.
    #[must_use]
    pub fn compile_options(mut self, options: CompileOptions) -> Self {
        self.compile_options = Some(options);
        self
    }

    /// Caps how many type-check stages may run concurrently (minimum 1).
    /// Defaults to the host's available parallelism, clamped to at most 8.
    #[must_use]
    pub fn max_concurrent_type_checks(mut self, checks: usize) -> Self {
        self.max_concurrent_type_checks = Some(checks);
        self
    }

    /// Builds the runner.
    ///
    /// # Errors
    /// Returns [`ConfigError`] for missing required settings, zero caps, or
    /// unsupported features.
    pub fn build(self) -> Result<Runner, ConfigError> {
        let surface = self.surface.ok_or(ConfigError::MissingSurface)?;
        let ambient = self.ambient.ok_or(ConfigError::MissingAmbient)?;
        if !matches!(ambient.mode, AmbientMode::Production) {
            return Err(ConfigError::NonProductionAmbient);
        }
        let max_source_bytes = self.max_source_bytes.ok_or(ConfigError::MissingSourceCap)?;
        if max_source_bytes == 0 {
            return Err(ConfigError::ZeroSourceCap);
        }
        let base_limits = self.base_limits.unwrap_or_else(Limits::unlimited);
        match base_limits.gas {
            None => return Err(ConfigError::MissingGasLimit),
            Some(0) => return Err(ConfigError::ZeroGasLimit),
            Some(_) => {}
        }
        match base_limits.max_memory_bytes {
            None => return Err(ConfigError::MissingMemoryLimit),
            Some(0) => return Err(ConfigError::ZeroMemoryLimit),
            Some(_) => {}
        }
        let features = self.features.ok_or(ConfigError::MissingFeatures)?;
        // Refuse to silently half-configure compatibility features the runner
        // cannot yet consume end to end.
        if features.fenv || features.harness_mode {
            return Err(ConfigError::UnsupportedFeature);
        }
        let compile_options = self
            .compile_options
            .unwrap_or_else(CompileOptions::for_vm_execution);
        let front_door = self.front_door.unwrap_or_default();
        let lane_count = self.lane_count.unwrap_or(1);
        if lane_count == 0 {
            return Err(ConfigError::ZeroLaneCount);
        }
        // Admission limits fail closed: an unconfigured runner gets finite,
        // lane-derived caps instead of `usize::MAX`. Set explicit limits to
        // widen (or deliberately un-cap) them.
        let ingress = self
            .ingress
            .unwrap_or_else(|| IngressLimits::fail_closed(lane_count));
        let aggregate_limits = self.aggregate_resources.unwrap_or_default();
        let lane_admission = self
            .lane_admission
            .unwrap_or_else(|| AdmissionLimits::fail_closed(lane_count));
        let resource_accounting = Arc::new(TenantResourceAccounting::default());
        let lane_policy = Arc::new(RunnerLaneAdmissionPolicy);
        let front_door_concurrency = self
            .max_concurrent_type_checks
            .unwrap_or_else(default_type_check_concurrency)
            .max(1);
        Ok(Runner {
            surface,
            ambient,
            base_limits,
            features,
            max_source_bytes,
            compile_options,
            front_door,
            ingress: Arc::new(IngressAdmission::new(ingress)),
            aggregate_limits,
            resource_accounting,
            lane_pool: LanePool::with_admission_policy(lane_count, lane_admission, lane_policy),
            front_door_permits: Arc::new(tokio::sync::Semaphore::new(front_door_concurrency)),
            front_door_cache: Default::default(),
            #[cfg(any())]
            runtime_compiler_override: None,
        })
    }
}
