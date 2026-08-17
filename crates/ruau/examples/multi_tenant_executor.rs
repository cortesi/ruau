//! Bounded request execution with tenants, reports, and executor caching.

use std::time::{Duration, Instant};

use ruau::{
    executor::{
        Executor, IngressLimits, PreflightLimits, Request, RunControl, RunReport, TenantId,
    },
    surface::Surface,
    vm::{Ambient, Cancel, ExecutionFeatures, Limits, ValueSnapshot},
};

const SOURCE: &[u8] = b"--!strict\nlocal answer: number = 40 + 2\nreturn answer";
const BAD_SOURCE: &[u8] = b"--!strict\nlocal answer: number = 'wrong'\nreturn answer";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    let surface = Surface::new();
    let executor = Executor::builder()
        .aggregate_resource_limits(ruau::executor::AggregateResourceLimits::unlimited())
        .surface(surface)
        .ambient(Ambient::production(0))
        .features(ExecutionFeatures::all_off())
        .max_source_bytes(8 * 1024)
        .limits(Limits::metered(1_000_000, 16 * 1024 * 1024))
        .preflight_limits(PreflightLimits {
            max_type_diagnostics: 8,
            ..PreflightLimits::default()
        })
        .ingress_limits(IngressLimits {
            max_in_flight: 4,
            max_in_flight_per_tenant: 2,
        })
        .lane_count(1)
        .build()
        .map_err(|error| format!("executor: {error}"))?;

    let first = executor
        .run_report(Request::new(TenantId(0), SOURCE, run_control()?).with_tenant(TenantId(7)))
        .await;
    let second = executor
        .run_report(Request::new(TenantId(0), SOURCE, run_control()?).with_tenant(TenantId(7)))
        .await;
    let rejected = executor
        .run_report(Request::new(TenantId(0), BAD_SOURCE, run_control()?).with_tenant(TenantId(9)))
        .await;

    assert_eq!(success_values(&first)?, vec![ValueSnapshot::Number(42.0)]);
    assert_eq!(success_values(&second)?, vec![ValueSnapshot::Number(42.0)]);
    assert_eq!(second.metrics.compile_time, Duration::ZERO);
    assert!(rejected.result.is_err());

    println!("first run metrics: {:?}", first.metrics);
    println!("cached run metrics: {:?}", second.metrics);
    println!("bad tenant category: {:?}", rejected.failure_category);
    Ok(())
}

fn run_control() -> Result<RunControl, String> {
    RunControl::new(Instant::now() + Duration::from_secs(2), Cancel::manual())
        .map_err(|error| error.to_string())
}

fn success_values(report: &RunReport) -> Result<Vec<ValueSnapshot>, String> {
    match &report.result {
        Ok(values) => Ok(values.clone()),
        Err(error) => Err(format!("request failed: {error:?}")),
    }
}
