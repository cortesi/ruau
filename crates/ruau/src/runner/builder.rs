use std::sync::Arc;

use ruau_abi::NativeModule;
use ruau_source::ModuleSource;
use ruau_vm::{Ambient, AmbientMode, ExecutionFeatures, Limits, Profile};

use super::{
    admission::{IngressAdmission, RunnerLaneAdmissionPolicy, TenantResourceAccounting},
    pipeline::{
        Runner, default_type_check_concurrency, runner_config_error_from_surface_compatibility,
        validate_surface_features,
    },
    types::{AggregateResourceLimits, FrontDoorLimits, IngressLimits},
};
use crate::{
    compile::CompileOptions,
    lanes::{AdmissionLimits, LanePool},
    surface::{ConfigError, SurfaceSpec},
};

/// Builder for a [`Runner`].
///
/// Required request-boundary fields are explicit; missing profile, limits,
/// ambient, source cap, features, or host surface settings are build errors.
#[derive(Default)]
pub struct Builder {
    surface: Option<SurfaceSpec>,
    profile: Option<Profile>,
    ambient: Option<Ambient>,
    base_limits: Option<Limits>,
    features: Option<ExecutionFeatures>,
    max_source_bytes: Option<usize>,
    modules: Option<Vec<Arc<dyn NativeModule>>>,
    module_source: Option<Arc<dyn ModuleSource>>,
    compile_options: Option<CompileOptions>,
    front_door: Option<FrontDoorLimits>,
    ingress: Option<IngressLimits>,
    aggregate_resources: Option<AggregateResourceLimits>,
    lane_count: Option<usize>,
    lane_admission: Option<AdmissionLimits>,
    max_concurrent_type_checks: Option<usize>,
}

impl Builder {
    /// Selects an already validated capability surface. This is mutually
    /// exclusive with [`profile`](Self::profile), [`module`](Self::module),
    /// [`no_host_modules`](Self::no_host_modules), and
    /// [`module_source`](Self::module_source).
    #[must_use]
    pub fn surface(mut self, surface: SurfaceSpec) -> Self {
        self.surface = Some(surface);
        self
    }

    /// Selects the VM profile.
    #[must_use]
    pub fn profile(mut self, profile: Profile) -> Self {
        self.profile = Some(profile);
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
    /// Runtime compilation requires a profile with `loadstring`; unsupported
    /// switches fail [`build`](Self::build).
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

    /// Registers an audited host module, available to every request.
    #[must_use]
    pub fn module(mut self, module: Arc<dyn NativeModule>) -> Self {
        self.modules.get_or_insert_with(Vec::new).push(module);
        self
    }

    /// Grants runtime `require` through the supplied module source.
    #[must_use]
    pub fn module_source(mut self, source: Arc<dyn ModuleSource>) -> Self {
        self.module_source = Some(source);
        self
    }

    /// Selects an explicit empty host environment.
    #[must_use]
    pub fn no_host_modules(mut self) -> Self {
        self.modules.get_or_insert_with(Vec::new);
        self
    }

    /// Overrides the compiler options. Defaults to
    /// [`CompileOptions::for_vm_execution`]; the profile restriction is applied
    /// on top at compile time regardless.
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
    /// Returns [`ConfigError`] for missing required settings, zero caps,
    /// incompatible runtime-compilation settings, or unsupported features.
    pub fn build(self) -> Result<Runner, ConfigError> {
        let (profile, prebuilt_surface) = match self.surface {
            Some(surface) => {
                if self.profile.is_some() || self.modules.is_some() || self.module_source.is_some()
                {
                    return Err(ConfigError::ConflictingSurfaceConfiguration);
                }
                (*surface.profile(), Some(surface))
            }
            None => (self.profile.ok_or(ConfigError::MissingProfile)?, None),
        };
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
        validate_surface_features(&profile, features)
            .map_err(runner_config_error_from_surface_compatibility)?;
        // Refuse to silently half-configure compatibility features the runner
        // cannot yet consume end to end.
        if features.fenv || features.harness_mode {
            return Err(ConfigError::UnsupportedFeature);
        }
        let surface = match prebuilt_surface {
            Some(surface) => surface,
            None => {
                let modules = match self.modules {
                    Some(modules) => modules,
                    None if self.module_source.is_some() => Vec::new(),
                    None => return Err(ConfigError::MissingHostEnvironment),
                };
                SurfaceSpec::from_parts(profile, modules, self.module_source)?
            }
        };
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
            #[cfg(any())]
            runtime_compiler_override: None,
        })
    }
}
