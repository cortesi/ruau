//! Bounded request execution with tenants, reports, and runner caching.

use std::time::{Duration, Instant};

use ruau::{
    runner::{
        Budget, FrontDoorLimits, IngressLimits, Request, RequestReport, RequestReportOutcome,
        ResultValue, Runner, TenantId,
    },
    surface::Surface,
    vm::{Ambient, Cancel, ExecutionFeatures, Limits},
};

const SOURCE: &[u8] = b"--!strict\nlocal answer: number = 40 + 2\nreturn answer";
const BAD_SOURCE: &[u8] = b"--!strict\nlocal answer: number = 'wrong'\nreturn answer";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    let surface = Surface::new();
    let runner = Runner::builder()
        .surface(surface)
        .ambient(Ambient::production(0))
        .features(ExecutionFeatures::all_off())
        .max_source_bytes(8 * 1024)
        .limits(Limits::production(1_000_000, 16 * 1024 * 1024))
        .front_door_limits(FrontDoorLimits {
            max_type_diagnostics: 8,
            ..FrontDoorLimits::default()
        })
        .ingress_limits(IngressLimits {
            max_in_flight: 4,
            max_in_flight_per_tenant: 2,
        })
        .lane_count(1)
        .build()
        .map_err(|error| format!("runner: {error}"))?;

    let first = runner
        .run_report(Request::new(SOURCE, budget()?).tenant(TenantId(7)))
        .await;
    let second = runner
        .run_report(Request::new(SOURCE, budget()?).tenant(TenantId(7)))
        .await;
    let rejected = runner
        .run_report(Request::new(BAD_SOURCE, budget()?).tenant(TenantId(9)))
        .await;

    assert_eq!(success_values(&first)?, vec![ResultValue::Number(42.0)]);
    assert_eq!(success_values(&second)?, vec![ResultValue::Number(42.0)]);
    assert_eq!(second.metrics.compile_time, Duration::ZERO);
    assert!(matches!(
        rejected.outcome,
        RequestReportOutcome::Failure { .. }
    ));

    println!("first run metrics: {:?}", first.metrics);
    println!("cached run metrics: {:?}", second.metrics);
    println!("bad tenant category: {:?}", rejected.failure_category);
    Ok(())
}

fn budget() -> Result<Budget, String> {
    Budget::new(Instant::now() + Duration::from_secs(2), Cancel::manual())
        .map_err(|error| error.to_string())
}

fn success_values(report: &RequestReport) -> Result<Vec<ResultValue>, String> {
    match &report.outcome {
        RequestReportOutcome::Success { values } => Ok(values.clone()),
        RequestReportOutcome::Failure { error } => Err(format!("request failed: {error:?}")),
    }
}
