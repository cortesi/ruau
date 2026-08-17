use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use super::{
    TenantId,
    types::{
        AggregateResourceLimit, AggregateResourceLimits, IngressLimits, IngressScope, RequestError,
        RequestMetrics, TenantResourceTotals,
    },
};

pub const MAX_TRACKED_TENANTS: usize = 4096;

#[derive(Default)]
struct IngressState {
    total: usize,
    per_tenant: HashMap<TenantId, usize>,
}

pub struct IngressAdmission {
    pub(super) limits: IngressLimits,
    state: Mutex<IngressState>,
}

impl IngressAdmission {
    pub(super) fn new(limits: IngressLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(IngressState::default()),
        }
    }

    pub(super) fn try_enter(
        self: &Arc<Self>,
        tenant: TenantId,
    ) -> Result<IngressGuard, RequestError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.total >= self.limits.max_in_flight {
            return Err(RequestError::IngressRejected {
                tenant,
                in_flight: state.total,
                cap: self.limits.max_in_flight,
                scope: IngressScope::Pool,
            });
        }
        let tenant_in_flight = state.per_tenant.get(&tenant).copied().unwrap_or(0);
        if tenant_in_flight >= self.limits.max_in_flight_per_tenant {
            return Err(RequestError::IngressRejected {
                tenant,
                in_flight: tenant_in_flight,
                cap: self.limits.max_in_flight_per_tenant,
                scope: IngressScope::Tenant,
            });
        }
        state.total += 1;
        *state.per_tenant.entry(tenant).or_insert(0) += 1;
        Ok(IngressGuard {
            admission: Arc::clone(self),
            tenant,
        })
    }

    fn release(&self, tenant: TenantId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.total = state.total.saturating_sub(1);
        if let Some(count) = state.per_tenant.get_mut(&tenant) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.per_tenant.remove(&tenant);
            }
        }
    }
}

pub struct IngressGuard {
    admission: Arc<IngressAdmission>,
    tenant: TenantId,
}

impl Drop for IngressGuard {
    fn drop(&mut self) {
        self.admission.release(self.tenant);
    }
}

/// Bound on distinct tenants the aggregate accounting tracks. At the cap, the
/// least-recently-recorded tenant is evicted: bounded memory in a long-lived
/// multi-tenant process wins over a dormant tenant's lifetime totals (an
/// evicted tenant restarts its aggregate budget on its next request).
#[allow(clippy::cfg_not_test)] // tests need `pub(crate)` field visibility
#[derive(Default)]
pub struct TenantAccountingState {
    sequence: u64,
    #[cfg(any())]
    pub(crate) evictions: u64,
    #[cfg(not(any()))]
    evictions: u64,
    #[cfg(any())]
    pub(crate) per_tenant: HashMap<TenantId, TrackedTenantTotals>,
    #[cfg(not(any()))]
    per_tenant: HashMap<TenantId, TrackedTenantTotals>,
}

pub struct TrackedTenantTotals {
    last_recorded: u64,
    totals: TenantResourceTotals,
    pending: AccountedResources,
}

pub struct TenantResourceReservation {
    accounting: Arc<TenantResourceAccounting>,
    tenant: TenantId,
    resources: AccountedResources,
    settled: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AccountedResources {
    pub(crate) requests: u64,
    pub(crate) source_bytes: u64,
    pub(crate) preflight_time: std::time::Duration,
    pub(crate) run_time: std::time::Duration,
    pub(crate) gas_spent: u64,
    pub(crate) charged_bytes: u64,
}

impl AccountedResources {
    const LIMITS: [AggregateResourceLimit; 6] = [
        AggregateResourceLimit::Requests,
        AggregateResourceLimit::SourceBytes,
        AggregateResourceLimit::PreflightTime,
        AggregateResourceLimit::RunTime,
        AggregateResourceLimit::GasSpent,
        AggregateResourceLimit::ChargedBytes,
    ];

    fn saturating_add(self, other: Self) -> Self {
        Self {
            requests: self.requests.saturating_add(other.requests),
            source_bytes: self.source_bytes.saturating_add(other.source_bytes),
            preflight_time: self.preflight_time.saturating_add(other.preflight_time),
            run_time: self.run_time.saturating_add(other.run_time),
            gas_spent: self.gas_spent.saturating_add(other.gas_spent),
            charged_bytes: self.charged_bytes.saturating_add(other.charged_bytes),
        }
    }

    fn saturating_sub(self, other: Self) -> Self {
        Self {
            requests: self.requests.saturating_sub(other.requests),
            source_bytes: self.source_bytes.saturating_sub(other.source_bytes),
            preflight_time: self.preflight_time.saturating_sub(other.preflight_time),
            run_time: self.run_time.saturating_sub(other.run_time),
            gas_spent: self.gas_spent.saturating_sub(other.gas_spent),
            charged_bytes: self.charged_bytes.saturating_sub(other.charged_bytes),
        }
    }

    fn value(self, limit: AggregateResourceLimit) -> u128 {
        match limit {
            AggregateResourceLimit::Requests => u128::from(self.requests),
            AggregateResourceLimit::SourceBytes => u128::from(self.source_bytes),
            AggregateResourceLimit::PreflightTime => self.preflight_time.as_nanos(),
            AggregateResourceLimit::RunTime => self.run_time.as_nanos(),
            AggregateResourceLimit::GasSpent => u128::from(self.gas_spent),
            AggregateResourceLimit::ChargedBytes => u128::from(self.charged_bytes),
        }
    }
}

impl TenantResourceTotals {
    pub(crate) fn accounted(self) -> AccountedResources {
        AccountedResources {
            requests: self.requests,
            source_bytes: self.source_bytes,
            preflight_time: self.preflight_time,
            run_time: self.run_time,
            gas_spent: self.gas_spent,
            charged_bytes: self.charged_bytes,
        }
    }

    fn record(&mut self, metrics: &RequestMetrics) {
        self.requests = self.requests.saturating_add(1);
        self.source_bytes = self
            .source_bytes
            .saturating_add(u64::try_from(metrics.source_bytes).unwrap_or(u64::MAX));
        self.preflight_time = self
            .preflight_time
            .saturating_add(metrics.parse_time)
            .saturating_add(metrics.check_time)
            .saturating_add(metrics.compile_time);
        self.run_time = self.run_time.saturating_add(metrics.run_time);
        self.parse_ast_nodes = self
            .parse_ast_nodes
            .saturating_add(u64::try_from(metrics.parse_ast_nodes).unwrap_or(u64::MAX));
        self.type_arena_nodes = self
            .type_arena_nodes
            .saturating_add(u64::try_from(metrics.type_arena_nodes).unwrap_or(u64::MAX));
        self.compiled_instructions = self
            .compiled_instructions
            .saturating_add(u64::try_from(metrics.compiled_instructions).unwrap_or(u64::MAX));
        self.compiled_bytecode_bytes = self
            .compiled_bytecode_bytes
            .saturating_add(u64::try_from(metrics.compiled_bytecode_bytes).unwrap_or(u64::MAX));
        self.gas_spent = self.gas_spent.saturating_add(metrics.gas_spent);
        self.vm_execution_count = self
            .vm_execution_count
            .saturating_add(metrics.vm_execution_count);
        self.heap_bytes = u64::try_from(metrics.heap_bytes).unwrap_or(u64::MAX);
        self.peak_heap_bytes = self
            .peak_heap_bytes
            .max(u64::try_from(metrics.peak_heap_bytes).unwrap_or(u64::MAX));
        self.charged_bytes = self.charged_bytes.saturating_add(charged_bytes(metrics));
    }
}

#[allow(clippy::cfg_not_test)] // tests need `pub(crate)` field visibility
#[derive(Default)]
pub struct TenantResourceAccounting {
    #[cfg(any())]
    pub(crate) state: Mutex<TenantAccountingState>,
    #[cfg(not(any()))]
    state: Mutex<TenantAccountingState>,
}

impl TenantResourceAccounting {
    pub(super) fn try_reserve(
        self: &Arc<Self>,
        tenant: TenantId,
        limits: AggregateResourceLimits,
        reservation: AccountedResources,
    ) -> Result<TenantResourceReservation, RequestError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.ensure_tenant_slot(tenant);
        let projected = {
            let tracked = state.entry_for_tenant(tenant);
            let current = tracked.totals.accounted().saturating_add(tracked.pending);
            check_aggregate_current(tenant, limits, current)?;
            current.saturating_add(reservation)
        };
        check_aggregate_projection(tenant, limits, projected)?;
        state.sequence = state.sequence.wrapping_add(1);
        let sequence = state.sequence;
        let tracked = state.entry_for_tenant(tenant);
        tracked.last_recorded = sequence;
        tracked.pending = tracked.pending.saturating_add(reservation);
        Ok(TenantResourceReservation {
            accounting: Arc::clone(self),
            tenant,
            resources: reservation,
            settled: false,
        })
    }

    #[cfg(any())]
    pub(super) fn record(&self, tenant: TenantId, metrics: &RequestMetrics) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.sequence = state.sequence.wrapping_add(1);
        let sequence = state.sequence;
        if !state.per_tenant.contains_key(&tenant) && state.per_tenant.len() >= MAX_TRACKED_TENANTS
        {
            // Evict the least-recently-recorded tenant to admit the new one.
            if let Some(evict) = state
                .per_tenant
                .iter()
                .min_by_key(|(_, tracked)| tracked.last_recorded)
                .map(|(tenant, _)| *tenant)
            {
                state.per_tenant.remove(&evict);
                state.evictions = state.evictions.saturating_add(1);
            }
        }
        let tracked = state
            .per_tenant
            .entry(tenant)
            .or_insert_with(|| TrackedTenantTotals {
                last_recorded: sequence,
                totals: TenantResourceTotals::default(),
                pending: AccountedResources::default(),
            });
        tracked.last_recorded = sequence;
        tracked.totals.record(metrics);
    }

    pub(super) fn totals(&self, tenant: TenantId) -> TenantResourceTotals {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .per_tenant
            .get(&tenant)
            .map(|tracked| tracked.totals)
            .unwrap_or_default()
    }

    #[cfg(any())]
    pub(super) fn evictions(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .evictions
    }

    fn release_reservation(&self, tenant: TenantId, reservation: AccountedResources) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(tracked) = state.per_tenant.get_mut(&tenant) {
            tracked.pending = tracked.pending.saturating_sub(reservation);
        }
    }

    fn settle_reservation(
        &self,
        tenant: TenantId,
        reservation: AccountedResources,
        metrics: &RequestMetrics,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.sequence = state.sequence.wrapping_add(1);
        let sequence = state.sequence;
        let tracked = state.entry_for_tenant(tenant);
        tracked.last_recorded = sequence;
        tracked.pending = tracked.pending.saturating_sub(reservation);
        tracked.totals.record(metrics);
    }
}

impl TenantAccountingState {
    fn ensure_tenant_slot(&mut self, tenant: TenantId) {
        if self.per_tenant.contains_key(&tenant) || self.per_tenant.len() < MAX_TRACKED_TENANTS {
            return;
        }
        // Evict only dormant tenants: pending reservations represent live
        // requests and must not disappear while their guard can still settle.
        if let Some(evict) = self
            .per_tenant
            .iter()
            .filter(|(_, tracked)| tracked.pending.requests == 0)
            .min_by_key(|(_, tracked)| tracked.last_recorded)
            .map(|(tenant, _)| *tenant)
        {
            self.per_tenant.remove(&evict);
            self.evictions = self.evictions.saturating_add(1);
        }
    }

    fn entry_for_tenant(&mut self, tenant: TenantId) -> &mut TrackedTenantTotals {
        self.per_tenant
            .entry(tenant)
            .or_insert_with(|| TrackedTenantTotals {
                last_recorded: self.sequence,
                totals: TenantResourceTotals::default(),
                pending: AccountedResources::default(),
            })
    }
}

impl TenantResourceReservation {
    pub(super) fn settle(mut self, metrics: &RequestMetrics) {
        self.accounting
            .settle_reservation(self.tenant, self.resources, metrics);
        self.settled = true;
    }
}

impl Drop for TenantResourceReservation {
    fn drop(&mut self) {
        if !self.settled {
            self.accounting
                .release_reservation(self.tenant, self.resources);
        }
    }
}

pub fn check_aggregate_limit_reached(
    tenant: TenantId,
    limit: AggregateResourceLimit,
    used: u128,
    cap: Option<u128>,
) -> Result<(), RequestError> {
    if let Some(cap) = cap
        && used >= cap
    {
        return Err(RequestError::AggregateResourceLimitExceeded {
            tenant,
            limit,
            used,
            cap,
        });
    }
    Ok(())
}

pub fn check_aggregate_limit_exceeded(
    tenant: TenantId,
    limit: AggregateResourceLimit,
    used: u128,
    cap: Option<u128>,
) -> Result<(), RequestError> {
    if let Some(cap) = cap
        && used > cap
    {
        return Err(RequestError::AggregateResourceLimitExceeded {
            tenant,
            limit,
            used,
            cap,
        });
    }
    Ok(())
}

pub fn charged_bytes(metrics: &RequestMetrics) -> u64 {
    u64::try_from(metrics.source_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(u64::try_from(metrics.compiled_bytecode_bytes).unwrap_or(u64::MAX))
        .saturating_add(u64::try_from(metrics.peak_heap_bytes).unwrap_or(u64::MAX))
}

fn check_aggregate_current(
    tenant: TenantId,
    limits: AggregateResourceLimits,
    current: AccountedResources,
) -> Result<(), RequestError> {
    for limit in AccountedResources::LIMITS {
        let used = current.value(limit);
        let cap = aggregate_cap(limits, limit);
        if matches!(
            limit,
            AggregateResourceLimit::SourceBytes | AggregateResourceLimit::ChargedBytes
        ) {
            check_aggregate_limit_exceeded(tenant, limit, used, cap)?;
        } else {
            check_aggregate_limit_reached(tenant, limit, used, cap)?;
        }
    }
    Ok(())
}

fn check_aggregate_projection(
    tenant: TenantId,
    limits: AggregateResourceLimits,
    projected: AccountedResources,
) -> Result<(), RequestError> {
    for limit in AccountedResources::LIMITS {
        check_aggregate_limit_exceeded(
            tenant,
            limit,
            projected.value(limit),
            aggregate_cap(limits, limit),
        )?;
    }
    Ok(())
}

fn aggregate_cap(limits: AggregateResourceLimits, limit: AggregateResourceLimit) -> Option<u128> {
    match limit {
        AggregateResourceLimit::Requests => limits.max_requests.map(u128::from),
        AggregateResourceLimit::SourceBytes => limits.max_source_bytes.map(u128::from),
        AggregateResourceLimit::PreflightTime => {
            limits.max_preflight_time.map(|value| value.as_nanos())
        }
        AggregateResourceLimit::RunTime => limits.max_run_time.map(|value| value.as_nanos()),
        AggregateResourceLimit::GasSpent => limits.max_gas_spent.map(u128::from),
        AggregateResourceLimit::ChargedBytes => limits.max_charged_bytes.map(u128::from),
    }
}

#[cfg(any())]
mod accounted_tests {
    use super::*;

    fn resources(limit: AggregateResourceLimit, value: u64) -> AccountedResources {
        let mut resources = AccountedResources::default();
        match limit {
            AggregateResourceLimit::Requests => resources.requests = value,
            AggregateResourceLimit::SourceBytes => resources.source_bytes = value,
            AggregateResourceLimit::PreflightTime => {
                resources.preflight_time = std::time::Duration::from_nanos(value);
            }
            AggregateResourceLimit::RunTime => {
                resources.run_time = std::time::Duration::from_nanos(value);
            }
            AggregateResourceLimit::GasSpent => resources.gas_spent = value,
            AggregateResourceLimit::ChargedBytes => resources.charged_bytes = value,
        }
        resources
    }

    fn limits(limit: AggregateResourceLimit, cap: u64) -> AggregateResourceLimits {
        let mut limits = AggregateResourceLimits::default();
        match limit {
            AggregateResourceLimit::Requests => limits.max_requests = Some(cap),
            AggregateResourceLimit::SourceBytes => limits.max_source_bytes = Some(cap),
            AggregateResourceLimit::PreflightTime => {
                limits.max_preflight_time = Some(std::time::Duration::from_nanos(cap));
            }
            AggregateResourceLimit::RunTime => {
                limits.max_run_time = Some(std::time::Duration::from_nanos(cap));
            }
            AggregateResourceLimit::GasSpent => limits.max_gas_spent = Some(cap),
            AggregateResourceLimit::ChargedBytes => limits.max_charged_bytes = Some(cap),
        }
        limits
    }

    #[test]
    fn every_accounted_dimension_keeps_current_and_projected_boundaries() {
        let tenant = TenantId(1);
        for limit in AccountedResources::LIMITS {
            let limits = limits(limit, 10);
            check_aggregate_current(tenant, limits, resources(limit, 9))
                .expect("below-cap current use is admitted");
            check_aggregate_projection(tenant, limits, resources(limit, 9))
                .expect("below-cap projection is admitted");

            let current_at_cap = check_aggregate_current(tenant, limits, resources(limit, 10));
            if matches!(
                limit,
                AggregateResourceLimit::SourceBytes | AggregateResourceLimit::ChargedBytes
            ) {
                current_at_cap.expect("byte totals may remain exactly at cap");
            } else {
                current_at_cap.expect_err("consumable totals reject the next request at cap");
            }
            check_aggregate_projection(tenant, limits, resources(limit, 10))
                .expect("a projection may land exactly at cap");

            check_aggregate_current(tenant, limits, resources(limit, 11))
                .expect_err("current use over cap is rejected");
            check_aggregate_projection(tenant, limits, resources(limit, 11))
                .expect_err("projected use over cap is rejected");
        }
    }

    #[test]
    fn telemetry_only_totals_do_not_enter_admission_accounting() {
        let totals = TenantResourceTotals {
            parse_ast_nodes: u64::MAX,
            type_arena_nodes: u64::MAX,
            compiled_instructions: u64::MAX,
            compiled_bytecode_bytes: u64::MAX,
            vm_execution_count: u64::MAX,
            heap_bytes: u64::MAX,
            peak_heap_bytes: u64::MAX,
            ..TenantResourceTotals::default()
        };
        assert_eq!(totals.accounted(), AccountedResources::default());
    }
}

#[cfg(any())]
mod poison_tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;

    fn poison<T>(mutex: &Mutex<T>) {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = mutex.lock().expect("test mutex starts healthy");
            panic!("poison test mutex");
        }));
        assert!(result.is_err());
    }

    #[test]
    fn ingress_release_recovers_a_poisoned_lock() {
        let admission = Arc::new(IngressAdmission::new(IngressLimits {
            max_in_flight: 1,
            max_in_flight_per_tenant: 1,
        }));
        let guard = admission
            .try_enter(TenantId(1))
            .expect("first request enters");
        poison(&admission.state);

        drop(guard);

        admission
            .try_enter(TenantId(1))
            .expect("released capacity remains usable after lock poisoning");
    }

    #[test]
    fn reservation_release_recovers_a_poisoned_lock() {
        let accounting = Arc::new(TenantResourceAccounting::default());
        let tenant = TenantId(2);
        let limits = AggregateResourceLimits {
            max_requests: Some(1),
            ..AggregateResourceLimits::default()
        };
        let reservation = AccountedResources {
            requests: 1,
            ..AccountedResources::default()
        };
        let guard = accounting
            .try_reserve(tenant, limits, reservation)
            .expect("first request reserves capacity");
        poison(&accounting.state);

        drop(guard);

        accounting
            .try_reserve(tenant, limits, reservation)
            .expect("released reservation remains usable after lock poisoning");
    }

    #[test]
    fn reservation_settlement_recovers_a_poisoned_lock() {
        let accounting = Arc::new(TenantResourceAccounting::default());
        let tenant = TenantId(3);
        let reservation = accounting
            .try_reserve(
                tenant,
                AggregateResourceLimits::default(),
                AccountedResources {
                    requests: 1,
                    ..AccountedResources::default()
                },
            )
            .expect("request reserves capacity");
        poison(&accounting.state);

        reservation.settle(&RequestMetrics {
            source_bytes: 7,
            ..RequestMetrics::default()
        });

        let totals = accounting.totals(tenant);
        assert_eq!(totals.requests, 1);
        assert_eq!(totals.source_bytes, 7);
    }
}
