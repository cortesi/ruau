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
use crate::lanes::{AdmissionDecision, AdmissionPolicy, AdmissionSnapshot, DefaultAdmissionPolicy};

pub const MAX_TRACKED_TENANTS: usize = 4096;

#[derive(Default)]
pub struct IngressState {
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
        let Ok(mut state) = self.state.lock() else {
            return;
        };
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
    pending: TenantResourceTotals,
}

pub struct TenantResourceReservation {
    accounting: Arc<TenantResourceAccounting>,
    tenant: TenantId,
    totals: TenantResourceTotals,
    settled: bool,
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
        reservation: TenantResourceTotals,
    ) -> Result<TenantResourceReservation, RequestError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.ensure_tenant_slot(tenant);
        let projected = {
            let tracked = state.entry_for_tenant(tenant);
            let current = add_totals(tracked.totals, tracked.pending);
            check_aggregate_current(tenant, limits, current)?;
            add_totals(current, reservation)
        };
        check_aggregate_projection(tenant, limits, projected)?;
        state.sequence = state.sequence.wrapping_add(1);
        let sequence = state.sequence;
        let tracked = state.entry_for_tenant(tenant);
        tracked.last_recorded = sequence;
        tracked.pending = add_totals(tracked.pending, reservation);
        Ok(TenantResourceReservation {
            accounting: Arc::clone(self),
            tenant,
            totals: reservation,
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
                pending: TenantResourceTotals::default(),
            });
        tracked.last_recorded = sequence;
        record_metrics(&mut tracked.totals, metrics);
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
    #[cfg(any())]
    #[cfg(any())]
    pub(super) fn evictions(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .evictions
    }

    fn release_reservation(&self, tenant: TenantId, reservation: TenantResourceTotals) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some(tracked) = state.per_tenant.get_mut(&tenant) {
            tracked.pending = subtract_totals(tracked.pending, reservation);
        }
    }

    fn settle_reservation(
        &self,
        tenant: TenantId,
        reservation: TenantResourceTotals,
        metrics: &RequestMetrics,
    ) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.sequence = state.sequence.wrapping_add(1);
        let sequence = state.sequence;
        let tracked = state.entry_for_tenant(tenant);
        tracked.last_recorded = sequence;
        tracked.pending = subtract_totals(tracked.pending, reservation);
        record_metrics(&mut tracked.totals, metrics);
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
                pending: TenantResourceTotals::default(),
            })
    }
}

impl TenantResourceReservation {
    pub(super) fn settle(mut self, metrics: &RequestMetrics) {
        self.accounting
            .settle_reservation(self.tenant, self.totals, metrics);
        self.settled = true;
    }
}

impl Drop for TenantResourceReservation {
    fn drop(&mut self) {
        if !self.settled {
            self.accounting
                .release_reservation(self.tenant, self.totals);
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
    current: TenantResourceTotals,
) -> Result<(), RequestError> {
    check_aggregate_limit_reached(
        tenant,
        AggregateResourceLimit::Requests,
        u128::from(current.requests),
        limits.max_requests.map(u128::from),
    )?;
    check_aggregate_limit_exceeded(
        tenant,
        AggregateResourceLimit::SourceBytes,
        u128::from(current.source_bytes),
        limits.max_source_bytes.map(u128::from),
    )?;
    check_aggregate_limit_reached(
        tenant,
        AggregateResourceLimit::FrontDoorTime,
        current.front_door_time.as_nanos(),
        limits
            .max_front_door_time
            .map(|duration| duration.as_nanos()),
    )?;
    check_aggregate_limit_reached(
        tenant,
        AggregateResourceLimit::RunTime,
        current.run_time.as_nanos(),
        limits.max_run_time.map(|duration| duration.as_nanos()),
    )?;
    check_aggregate_limit_reached(
        tenant,
        AggregateResourceLimit::GasSpent,
        u128::from(current.gas_spent),
        limits.max_gas_spent.map(u128::from),
    )?;
    check_aggregate_limit_exceeded(
        tenant,
        AggregateResourceLimit::ChargedBytes,
        u128::from(current.charged_bytes),
        limits.max_charged_bytes.map(u128::from),
    )?;
    Ok(())
}

fn check_aggregate_projection(
    tenant: TenantId,
    limits: AggregateResourceLimits,
    projected: TenantResourceTotals,
) -> Result<(), RequestError> {
    check_aggregate_limit_exceeded(
        tenant,
        AggregateResourceLimit::Requests,
        u128::from(projected.requests),
        limits.max_requests.map(u128::from),
    )?;
    check_aggregate_limit_exceeded(
        tenant,
        AggregateResourceLimit::SourceBytes,
        u128::from(projected.source_bytes),
        limits.max_source_bytes.map(u128::from),
    )?;
    check_aggregate_limit_exceeded(
        tenant,
        AggregateResourceLimit::FrontDoorTime,
        projected.front_door_time.as_nanos(),
        limits
            .max_front_door_time
            .map(|duration| duration.as_nanos()),
    )?;
    check_aggregate_limit_exceeded(
        tenant,
        AggregateResourceLimit::RunTime,
        projected.run_time.as_nanos(),
        limits.max_run_time.map(|duration| duration.as_nanos()),
    )?;
    check_aggregate_limit_exceeded(
        tenant,
        AggregateResourceLimit::GasSpent,
        u128::from(projected.gas_spent),
        limits.max_gas_spent.map(u128::from),
    )?;
    check_aggregate_limit_exceeded(
        tenant,
        AggregateResourceLimit::ChargedBytes,
        u128::from(projected.charged_bytes),
        limits.max_charged_bytes.map(u128::from),
    )?;
    Ok(())
}

fn add_totals(left: TenantResourceTotals, right: TenantResourceTotals) -> TenantResourceTotals {
    TenantResourceTotals {
        requests: left.requests.saturating_add(right.requests),
        source_bytes: left.source_bytes.saturating_add(right.source_bytes),
        front_door_time: left.front_door_time.saturating_add(right.front_door_time),
        run_time: left.run_time.saturating_add(right.run_time),
        parse_ast_nodes: left.parse_ast_nodes.saturating_add(right.parse_ast_nodes),
        type_arena_nodes: left.type_arena_nodes.saturating_add(right.type_arena_nodes),
        compiled_instructions: left
            .compiled_instructions
            .saturating_add(right.compiled_instructions),
        compiled_bytecode_bytes: left
            .compiled_bytecode_bytes
            .saturating_add(right.compiled_bytecode_bytes),
        gas_spent: left.gas_spent.saturating_add(right.gas_spent),
        vm_execution_count: left
            .vm_execution_count
            .saturating_add(right.vm_execution_count),
        heap_bytes: left.heap_bytes.saturating_add(right.heap_bytes),
        peak_heap_bytes: left.peak_heap_bytes.saturating_add(right.peak_heap_bytes),
        charged_bytes: left.charged_bytes.saturating_add(right.charged_bytes),
    }
}

fn subtract_totals(
    left: TenantResourceTotals,
    right: TenantResourceTotals,
) -> TenantResourceTotals {
    TenantResourceTotals {
        requests: left.requests.saturating_sub(right.requests),
        source_bytes: left.source_bytes.saturating_sub(right.source_bytes),
        front_door_time: left.front_door_time.saturating_sub(right.front_door_time),
        run_time: left.run_time.saturating_sub(right.run_time),
        parse_ast_nodes: left.parse_ast_nodes.saturating_sub(right.parse_ast_nodes),
        type_arena_nodes: left.type_arena_nodes.saturating_sub(right.type_arena_nodes),
        compiled_instructions: left
            .compiled_instructions
            .saturating_sub(right.compiled_instructions),
        compiled_bytecode_bytes: left
            .compiled_bytecode_bytes
            .saturating_sub(right.compiled_bytecode_bytes),
        gas_spent: left.gas_spent.saturating_sub(right.gas_spent),
        vm_execution_count: left
            .vm_execution_count
            .saturating_sub(right.vm_execution_count),
        heap_bytes: left.heap_bytes.saturating_sub(right.heap_bytes),
        peak_heap_bytes: left.peak_heap_bytes.saturating_sub(right.peak_heap_bytes),
        charged_bytes: left.charged_bytes.saturating_sub(right.charged_bytes),
    }
}

fn record_metrics(totals: &mut TenantResourceTotals, metrics: &RequestMetrics) {
    totals.requests = totals.requests.saturating_add(1);
    totals.source_bytes = totals
        .source_bytes
        .saturating_add(u64::try_from(metrics.source_bytes).unwrap_or(u64::MAX));
    totals.front_door_time = totals
        .front_door_time
        .saturating_add(metrics.parse_time)
        .saturating_add(metrics.check_time)
        .saturating_add(metrics.compile_time);
    totals.run_time = totals.run_time.saturating_add(metrics.run_time);
    totals.parse_ast_nodes = totals
        .parse_ast_nodes
        .saturating_add(u64::try_from(metrics.parse_ast_nodes).unwrap_or(u64::MAX));
    totals.type_arena_nodes = totals
        .type_arena_nodes
        .saturating_add(u64::try_from(metrics.type_arena_nodes).unwrap_or(u64::MAX));
    totals.compiled_instructions = totals
        .compiled_instructions
        .saturating_add(u64::try_from(metrics.compiled_instructions).unwrap_or(u64::MAX));
    totals.compiled_bytecode_bytes = totals
        .compiled_bytecode_bytes
        .saturating_add(u64::try_from(metrics.compiled_bytecode_bytes).unwrap_or(u64::MAX));
    totals.gas_spent = totals.gas_spent.saturating_add(metrics.gas_spent);
    totals.vm_execution_count = totals
        .vm_execution_count
        .saturating_add(metrics.vm_execution_count);
    totals.heap_bytes = u64::try_from(metrics.heap_bytes).unwrap_or(u64::MAX);
    totals.peak_heap_bytes = totals
        .peak_heap_bytes
        .max(u64::try_from(metrics.peak_heap_bytes).unwrap_or(u64::MAX));
    totals.charged_bytes = totals.charged_bytes.saturating_add(charged_bytes(metrics));
}

pub struct RunnerLaneAdmissionPolicy;

impl AdmissionPolicy for RunnerLaneAdmissionPolicy {
    fn decide(&self, snapshot: &AdmissionSnapshot) -> AdmissionDecision {
        DefaultAdmissionPolicy.decide(snapshot)
    }

    fn compare_ready(
        &self,
        left: &AdmissionSnapshot,
        right: &AdmissionSnapshot,
    ) -> std::cmp::Ordering {
        DefaultAdmissionPolicy.compare_ready(left, right)
    }
}
