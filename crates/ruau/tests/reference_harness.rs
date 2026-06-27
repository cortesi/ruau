//! Cross-demo reference harness coverage for public Ruau integration contracts.

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        future::ready,
        sync::{Arc, Mutex},
        task::{Context, Poll},
        time::{Duration, Instant},
    };

    use ruau::{
        abi::{HostReturn, ModuleBinding, ModuleBuilder, NativeModule, OwnedValue},
        analysis::resolve::config::EmptyResolver,
        compile::{CompileOptions, compile_for},
        durable::{
            ActorId, CommitOutcome, LeaseToken, QueuedWake, StartOutcome, StateLease, StateStore,
            StateStoreError, StateStoreFuture, StateStoreResult, WakeRequest,
        },
        lanes::{AdmissionDecision, AdmissionLimits, AdmissionPolicy, AdmissionSnapshot, LanePool},
        runner::{
            AggregateResourceLimit, AggregateResourceLimits, Budget, FailureCategory,
            IngressLimits, IngressScope, Request, RequestError, RequestReport,
            RequestReportOutcome, ResultValue, Runner, TenantId,
        },
        source::{InMemorySource, ModuleId, ModuleName, ModuleSource},
        surface::{ConfigError, SurfaceSpec},
        typecheck::{
            diagnostic::{TypeDiagnostic, render_diagnostic_summary},
            frontend::GraphChecker,
        },
        vm::{
            Ambient, Cancel, ExecutionFeatures, Limits, MarshaledPair, MarshaledValue,
            ModuleBuilderExt, RuntimeError as Error, Vm, async_host_fn,
        },
    };
    // The raw host ABI is intentionally not part of the curated umbrella
    // surface; harness host functions take it from the ABI crate.
    use ruau_vm_api::{HostCall, HostContext, HostError, HostFunction};

    #[tokio::test]
    async fn durable_state_store_rejects_overlapping_actor_claims() {
        let store = ruau::durable::memory::InMemoryStore::new();
        let actor = ActorId::new("harness/actor");

        let (first, second) = tokio::join!(
            store.try_start(actor.clone()),
            store.try_start(actor.clone())
        );

        let outcomes = [
            first.expect("first claim returns"),
            second.expect("second claim returns"),
        ];
        let lease = single_started_lease(&outcomes);
        assert_eq!(single_busy_generation(&outcomes), 0);

        let commit = store
            .commit(
                lease.clone(),
                MarshaledValue::Number(1.0),
                vec![WakeRequest::new(actor.clone(), "continue")],
            )
            .await
            .expect("winning actor commits");
        assert_eq!(commit.generation().value(), 1);
        assert_eq!(commit.wakes().len(), 1);
        assert_eq!(
            store.state(&actor).expect("state reads"),
            Some(MarshaledValue::Number(1.0))
        );
        assert!(matches!(
            store.heartbeat(lease).await,
            Err(StateStoreError::StaleLease { .. })
        ));
    }

    #[tokio::test]
    async fn multi_tenant_runner_uses_distinct_surfaces_and_per_tenant_aggregate_caps() {
        let runner = Runner::builder()
            .profile(ruau::vm::Profile::full().without_runtime_compilation())
            .ambient(Ambient::production(0))
            .features(ExecutionFeatures::all_off())
            .no_host_modules()
            .max_source_bytes(1024)
            .limits(Limits {
                gas: Some(100_000),
                max_memory_bytes: Some(1 << 20),
                ..Limits::unlimited()
            })
            .lane_count(2)
            .lane_admission_limits(AdmissionLimits {
                max_in_flight: 2,
                max_in_flight_per_tenant: 1,
                max_queued: 2,
                max_queued_per_tenant: 1,
                max_total: 4,
            })
            .ingress_limits(IngressLimits {
                max_in_flight: 2,
                max_in_flight_per_tenant: 1,
            })
            .aggregate_resource_limits(AggregateResourceLimits {
                max_requests: Some(1),
                ..AggregateResourceLimits::default()
            })
            .build()
            .expect("runner validates");
        let alpha = TenantId(1);
        let beta = TenantId(2);
        let alpha_surface = tenant_surface("return { value = 11, label = 'alpha' }");
        let beta_surface = tenant_surface("return { value = 22, label = 'beta' }");
        let source = br#"
local tenant = require("tenant")
return tenant.value, tenant.label
"#;

        let alpha_report = run_tenant(&runner, alpha, &alpha_surface, source).await;
        assert_report_values(
            alpha_report,
            alpha,
            &[
                ResultValue::Number(11.0),
                ResultValue::String(b"alpha".to_vec()),
            ],
        );

        let rejected = run_tenant(&runner, alpha, &alpha_surface, source).await;
        match rejected.outcome {
            RequestReportOutcome::Failure {
                error:
                    RequestError::AggregateResourceLimitExceeded {
                        tenant,
                        limit,
                        used,
                        cap,
                    },
            } => {
                assert_eq!(tenant, alpha);
                assert_eq!(limit, AggregateResourceLimit::Requests);
                assert_eq!(used, 1);
                assert_eq!(cap, 1);
            }
            other => panic!("expected alpha aggregate rejection, got {other:?}"),
        }
        assert_eq!(
            rejected.failure_category,
            Some(FailureCategory::AggregateResourceLimit)
        );
        assert_eq!(rejected.metrics.parse_time, Duration::ZERO);
        assert_eq!(rejected.metrics.check_time, Duration::ZERO);
        assert_eq!(rejected.metrics.compile_time, Duration::ZERO);
        assert_eq!(rejected.metrics.vm_build_time, Duration::ZERO);
        assert_eq!(rejected.metrics.run_time, Duration::ZERO);

        let beta_report = run_tenant(&runner, beta, &beta_surface, source).await;
        assert_report_values(
            beta_report,
            beta,
            &[
                ResultValue::Number(22.0),
                ResultValue::String(b"beta".to_vec()),
            ],
        );

        assert_eq!(runner.tenant_resource_totals(alpha).requests, 1);
        assert_eq!(runner.tenant_resource_totals(beta).requests, 1);
        assert_eq!(runner.lane_metrics().lanes, 2);
    }

    #[tokio::test]
    async fn multi_tenant_runner_enforces_per_tenant_ingress_and_pool_caps() {
        let ingress_runner = capped_runner(
            IngressLimits {
                max_in_flight: 1,
                max_in_flight_per_tenant: 0,
            },
            AdmissionLimits::unlimited(),
        );
        let ingress_report = ingress_runner
            .run_report(
                Request::new(
                    b"return 1",
                    Budget::with_timeout(Duration::from_secs(5)).expect("future deadline"),
                )
                .tenant(TenantId(7)),
            )
            .await;
        match ingress_report.outcome {
            RequestReportOutcome::Failure {
                error:
                    RequestError::IngressRejected {
                        tenant,
                        in_flight,
                        cap,
                        scope,
                    },
            } => {
                assert_eq!(tenant, TenantId(7));
                assert_eq!(in_flight, 0);
                assert_eq!(cap, 0);
                assert_eq!(scope, IngressScope::Tenant);
            }
            other => panic!("expected per-tenant ingress rejection, got {other:?}"),
        }
        assert_eq!(
            ingress_report.failure_category,
            Some(FailureCategory::IngressRejected)
        );
        assert_eq!(ingress_report.metrics.parse_time, Duration::ZERO);
        assert_eq!(ingress_report.metrics.run_time, Duration::ZERO);

        let pool_runner = capped_runner(
            IngressLimits {
                max_in_flight: 1,
                max_in_flight_per_tenant: 1,
            },
            AdmissionLimits {
                max_in_flight: 0,
                max_in_flight_per_tenant: 0,
                max_queued: 0,
                max_queued_per_tenant: 0,
                max_total: 0,
            },
        );
        let pool_report = pool_runner
            .run_report(
                Request::new(
                    b"return 2",
                    Budget::with_timeout(Duration::from_secs(5)).expect("future deadline"),
                )
                .tenant(TenantId(8)),
            )
            .await;
        match pool_report.outcome {
            RequestReportOutcome::Failure {
                error: RequestError::LaneAdmissionRejected { tenant },
            } => assert_eq!(tenant, TenantId(8)),
            other => panic!("expected lane-pool rejection, got {other:?}"),
        }
        assert_eq!(
            pool_report.failure_category,
            Some(FailureCategory::LaneAdmissionRejected)
        );
        assert_eq!(pool_report.metrics.vm_build_time, Duration::ZERO);
        assert_eq!(pool_report.metrics.run_time, Duration::ZERO);
        assert_eq!(pool_runner.lane_metrics().rejected, 1);
    }

    #[tokio::test]
    async fn multi_tenant_runner_serializes_aggregate_budget_with_ingress_policy() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let runner = Runner::builder()
            .profile(ruau::vm::Profile::full().without_runtime_compilation())
            .ambient(Ambient::production(0))
            .features(ExecutionFeatures::all_off())
            .module(Arc::new(HarnessWaitHostModule(HarnessWaitHost {
                started: Arc::new(Mutex::new(Some(started_tx))),
                release: Arc::new(Mutex::new(Some(release_rx))),
            })))
            .max_source_bytes(1024)
            .limits(Limits {
                gas: Some(100_000),
                max_memory_bytes: Some(1 << 20),
                ..Limits::unlimited()
            })
            .ingress_limits(IngressLimits {
                max_in_flight: 2,
                max_in_flight_per_tenant: 1,
            })
            .aggregate_resource_limits(AggregateResourceLimits {
                max_requests: Some(1),
                ..AggregateResourceLimits::default()
            })
            .build()
            .expect("runner validates");
        let tenant = TenantId(33);
        let sibling = TenantId(34);
        let first = runner.run_report(
            Request::new(
                b"wait_host()\nreturn 1",
                Budget::with_timeout(Duration::from_secs(5)).expect("future deadline"),
            )
            .tenant(tenant),
        );
        tokio::pin!(first);
        tokio::select! {
            report = &mut first => panic!("first request should be parked, got {report:?}"),
            started = started_rx => started.expect("wait host is reached"),
        }

        let concurrent = runner
            .run_report(
                Request::new(
                    b"return 2",
                    Budget::with_timeout(Duration::from_secs(5)).expect("future deadline"),
                )
                .tenant(tenant),
            )
            .await;
        match concurrent.outcome {
            RequestReportOutcome::Failure {
                error:
                    RequestError::AggregateResourceLimitExceeded {
                        tenant,
                        limit,
                        used,
                        cap,
                    },
            } => {
                assert_eq!(tenant, TenantId(33));
                assert_eq!(limit, AggregateResourceLimit::Requests);
                assert_eq!(used, 1);
                assert_eq!(cap, 1);
            }
            other => panic!("expected pending aggregate reservation rejection, got {other:?}"),
        }
        assert_eq!(
            concurrent.failure_category,
            Some(FailureCategory::AggregateResourceLimit)
        );

        release_tx.send(()).expect("release first request");
        assert_report_values(first.await, tenant, &[ResultValue::Number(1.0)]);
        assert_eq!(runner.tenant_resource_totals(tenant).requests, 1);

        let exhausted = runner
            .run_report(
                Request::new(
                    b"return 3",
                    Budget::with_timeout(Duration::from_secs(5)).expect("future deadline"),
                )
                .tenant(tenant),
            )
            .await;
        match exhausted.outcome {
            RequestReportOutcome::Failure {
                error:
                    RequestError::AggregateResourceLimitExceeded {
                        tenant,
                        limit,
                        used,
                        cap,
                    },
            } => {
                assert_eq!(tenant, TenantId(33));
                assert_eq!(limit, AggregateResourceLimit::Requests);
                assert_eq!(used, 1);
                assert_eq!(cap, 1);
            }
            other => panic!("expected aggregate request-budget rejection, got {other:?}"),
        }
        assert_eq!(
            exhausted.failure_category,
            Some(FailureCategory::AggregateResourceLimit)
        );

        assert_report_values(
            runner
                .run_report(
                    Request::new(
                        b"return 4",
                        Budget::with_timeout(Duration::from_secs(5)).expect("future deadline"),
                    )
                    .tenant(sibling),
                )
                .await,
            sibling,
            &[ResultValue::Number(4.0)],
        );
    }

    #[test]
    fn lane_pool_orders_ready_queue_by_fair_share_policy() {
        let policy = Arc::new(LeastServedTenantPolicy {
            served: BTreeMap::from([(1, 3), (2, 0)]),
            seen_lanes: Mutex::new(Vec::new()),
        });
        let start_order = Arc::new(Mutex::new(Vec::new()));
        let pool = LanePool::with_admission_policy(
            1,
            AdmissionLimits {
                max_in_flight: 1,
                max_in_flight_per_tenant: 1,
                max_queued: 2,
                max_queued_per_tenant: 1,
                max_total: 3,
            },
            policy.clone(),
        );
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let busy = pool
            .submit(TenantId(0), move || async move {
                drop(release_rx.await);
                0u32
            })
            .expect("busy run starts");
        let more_served_order = Arc::clone(&start_order);
        let more_served = pool
            .submit(TenantId(1), move || async move {
                more_served_order.lock().expect("start order").push(1);
                10u32
            })
            .expect("more-served tenant queues first");
        let less_served_order = Arc::clone(&start_order);
        let less_served = pool
            .submit(TenantId(2), move || async move {
                less_served_order.lock().expect("start order").push(2);
                20u32
            })
            .expect("less-served tenant queues second");

        release_tx.send(()).expect("release busy run");
        assert_eq!(busy.blocking_recv().expect("busy result"), 0);
        assert_eq!(
            less_served.blocking_recv().expect("less-served result"),
            20,
            "fair-share policy should beat FIFO queue order"
        );
        assert_eq!(more_served.blocking_recv().expect("more-served result"), 10);
        assert_eq!(
            start_order.lock().expect("start order").as_slice(),
            &[2, 1],
            "less-served tenant must actually start before the FIFO predecessor"
        );
        assert!(
            policy
                .seen_lanes
                .lock()
                .expect("seen lane hints")
                .iter()
                .all(|lane| *lane == Some(0)),
            "ready-order snapshots carry the policy-selected lane"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multi_tenant_runner_serves_quick_sibling_while_cpu_request_runs() {
        let runner = Arc::new(
            Runner::builder()
                .profile(ruau::vm::Profile::full().without_runtime_compilation())
                .ambient(Ambient::production(0))
                .features(ExecutionFeatures::all_off())
                .no_host_modules()
                .max_source_bytes(1024)
                .limits(Limits {
                    gas: Some(1 << 60),
                    max_memory_bytes: Some(16 * 1024 * 1024),
                    ..Limits::unlimited()
                })
                .lane_count(2)
                .lane_admission_limits(AdmissionLimits {
                    max_in_flight: 2,
                    max_in_flight_per_tenant: 1,
                    max_queued: 0,
                    max_queued_per_tenant: 0,
                    max_total: 2,
                })
                .ingress_limits(IngressLimits {
                    max_in_flight: 2,
                    max_in_flight_per_tenant: 1,
                })
                .build()
                .expect("runner validates"),
        );
        let heavy_runner = Arc::clone(&runner);
        let heavy_cancel = Cancel::manual();
        let heavy_budget = Budget::new(
            Instant::now() + Duration::from_secs(3600),
            heavy_cancel.clone(),
        )
        .expect("future deadline");
        let heavy = tokio::spawn(async move {
            heavy_runner
                .run_report(
                    Request::new(b"--!nocheck\nwhile true do end", heavy_budget)
                        .tenant(TenantId(51)),
                )
                .await
        });
        wait_for_in_flight(&runner, 1).await;

        let quick = runner
            .run_report(
                Request::new(
                    b"return 7",
                    Budget::with_timeout(Duration::from_secs(2)).expect("future deadline"),
                )
                .tenant(TenantId(52)),
            )
            .await;
        assert_report_values(quick, TenantId(52), &[ResultValue::Number(7.0)]);
        heavy_cancel.cancel();

        let heavy = heavy.await.expect("heavy task joins");
        assert_eq!(heavy.failure_category, Some(FailureCategory::Cancelled));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_lane_runner_exposes_long_sync_host_call_residual() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let runner = Arc::new(
            Runner::builder()
                .profile(ruau::vm::Profile::full().without_runtime_compilation())
                .ambient(Ambient::production(0))
                .features(ExecutionFeatures::all_off())
                .module(Arc::new(HarnessBlockingHostModule(HarnessBlockingHost {
                    started: started_tx,
                    release: Arc::new(Mutex::new(Some(release_rx))),
                })))
                .max_source_bytes(1024)
                .limits(Limits {
                    gas: Some(100_000),
                    max_memory_bytes: Some(1 << 20),
                    ..Limits::unlimited()
                })
                .lane_count(1)
                .lane_admission_limits(AdmissionLimits {
                    max_in_flight: 2,
                    max_in_flight_per_tenant: 1,
                    max_queued: 0,
                    max_queued_per_tenant: 0,
                    max_total: 2,
                })
                .ingress_limits(IngressLimits {
                    max_in_flight: 2,
                    max_in_flight_per_tenant: 1,
                })
                .build()
                .expect("runner validates"),
        );
        let blocked_runner = Arc::clone(&runner);
        let blocked = tokio::spawn(async move {
            blocked_runner
                .run_report(
                    Request::new(
                        b"blocking_host()\nreturn 1",
                        Budget::with_timeout(Duration::from_secs(2)).expect("future deadline"),
                    )
                    .tenant(TenantId(61)),
                )
                .await
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("blocking host started");

        let quick = runner.run_report(
            Request::new(
                b"return 2",
                Budget::with_timeout(Duration::from_secs(2)).expect("future deadline"),
            )
            .tenant(TenantId(62)),
        );
        tokio::pin!(quick);
        tokio::select! {
            report = &mut quick => panic!("one-lane sibling should wait behind sync host residual, got {report:?}"),
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }

        release_tx.send(()).expect("release blocking host");
        assert_report_values(
            blocked.await.expect("blocked task joins"),
            TenantId(61),
            &[ResultValue::Number(1.0)],
        );
        assert_report_values(quick.await, TenantId(62), &[ResultValue::Number(2.0)]);
    }

    #[test]
    fn reference_harness_rejects_runner_build_time_bombs() {
        let zero_source_cap = Runner::builder()
            .profile(ruau::vm::Profile::full().without_runtime_compilation())
            .ambient(Ambient::production(0))
            .features(ExecutionFeatures::all_off())
            .no_host_modules()
            .max_source_bytes(0)
            .limits(Limits {
                gas: Some(100_000),
                max_memory_bytes: Some(1 << 20),
                ..Limits::unlimited()
            })
            .build()
            .expect_err("zero source cap fails closed");
        assert_eq!(zero_source_cap, ConfigError::ZeroSourceCap);

        let zero_lane_count = Runner::builder()
            .profile(ruau::vm::Profile::full().without_runtime_compilation())
            .ambient(Ambient::production(0))
            .features(ExecutionFeatures::all_off())
            .no_host_modules()
            .max_source_bytes(1024)
            .limits(Limits {
                gas: Some(100_000),
                max_memory_bytes: Some(1 << 20),
                ..Limits::unlimited()
            })
            .lane_count(0)
            .build()
            .expect_err("zero lane count fails closed");
        assert_eq!(zero_lane_count, ConfigError::ZeroLaneCount);
    }

    #[tokio::test]
    async fn durable_wake_generation_race_is_a_noop_and_releases_the_claim() {
        let store = ruau::durable::memory::InMemoryStore::new();
        let actor = ActorId::new("harness/wake");

        let stale_wake = commit_step(&store, actor.clone(), 1.0).await.wakes()[0].clone();
        let current_wake = commit_step(&store, actor.clone(), 2.0).await.wakes()[0].clone();

        assert_eq!(process_wake(&store, stale_wake).await, HarnessWake::Stale);
        assert_eq!(
            store.state(&actor).expect("state reads"),
            Some(MarshaledValue::Number(2.0))
        );

        let HarnessWake::Ran(current) = process_wake(&store, current_wake).await else {
            panic!("current generation wake should run");
        };
        assert_eq!(current.generation().value(), 3);
        assert_eq!(
            store.state(&actor).expect("state reads"),
            Some(MarshaledValue::Number(3.0))
        );
    }

    #[tokio::test]
    async fn durable_busy_wake_requests_retry_without_releasing_the_claim() {
        let store = ruau::durable::memory::InMemoryStore::new();
        let actor = ActorId::new("harness/busy");
        let wake = commit_step(&store, actor.clone(), 1.0).await.wakes()[0].clone();
        let active = start(&store, actor).await;

        assert_eq!(process_wake(&store, wake).await, HarnessWake::BusyRetry);
        store
            .heartbeat(active.clone())
            .await
            .expect("busy wake did not release active lease");
        store
            .abandon(active)
            .await
            .expect("busy wake test releases active lease");
    }

    #[tokio::test]
    async fn durable_duplicate_in_flight_wake_retries_then_runs_once_after_release() {
        let store = ruau::durable::memory::InMemoryStore::new();
        let actor = ActorId::new("harness/duplicate-wake");
        let wake = commit_step(&store, actor.clone(), 1.0).await.wakes()[0].clone();
        let active = start(&store, actor.clone()).await;

        assert_eq!(
            process_wake(&store, wake.clone()).await,
            HarnessWake::BusyRetry
        );
        assert_eq!(
            process_wake(&store, wake.clone()).await,
            HarnessWake::BusyRetry
        );
        store
            .heartbeat(active.clone())
            .await
            .expect("duplicate wake did not release active lease");
        store
            .abandon(active)
            .await
            .expect("active lease releases after duplicate delivery");

        let HarnessWake::Ran(commit) = process_wake(&store, wake.clone()).await else {
            panic!("released duplicate wake should run");
        };
        assert_eq!(commit.generation().value(), 2);
        assert_eq!(
            store.state(&actor).expect("state reads"),
            Some(MarshaledValue::Number(2.0))
        );
        assert_eq!(process_wake(&store, wake).await, HarnessWake::Stale);
    }

    #[tokio::test]
    async fn durable_backend_policy_expires_and_renews_leases() {
        let store = LogicalTtlStateStore::new(2);
        let actor = ActorId::new("harness/ttl");
        let renewed = start_any(&store, actor.clone()).await;

        store.advance(1);
        store
            .heartbeat(renewed.clone())
            .await
            .expect("heartbeat renews the lease");
        store.advance(1);
        store
            .heartbeat(renewed.clone())
            .await
            .expect("renewed lease is still current");

        store.advance(2);
        assert!(matches!(
            store.heartbeat(renewed).await,
            Err(StateStoreError::StaleLease { .. })
        ));

        let expired_commit = start_any(&store, actor.clone()).await;
        store.advance(2);
        assert!(matches!(
            store
                .commit(expired_commit, MarshaledValue::Number(99.0), Vec::new())
                .await,
            Err(StateStoreError::StaleLease { .. })
        ));

        let current = start_any(&store, actor.clone()).await;
        store
            .commit(current, MarshaledValue::Number(1.0), Vec::new())
            .await
            .expect("fresh lease commits after expired lease is reclaimed");
        assert_eq!(
            store.inner.state(&actor).expect("state reads"),
            Some(MarshaledValue::Number(1.0))
        );
    }

    #[tokio::test]
    async fn async_agent_host_composes_over_in_memory_state_store_backend() {
        let store = ruau::durable::memory::InMemoryStore::new();
        let actor = ActorId::new("harness/agent");

        let first = run_harness_agent_step(&store, actor.clone()).await;
        assert_eq!(first.summary(), (0.0, 1.0, 1, 1));
        assert_eq!(
            store.state(&actor).expect("agent state reads"),
            Some(MarshaledValue::Number(1.0))
        );

        let second = run_harness_agent_wake(&store, first.wakes[0].clone()).await;
        assert_eq!(second.summary(), (1.0, 2.0, 2, 1));
        assert_eq!(
            store.state(&actor).expect("agent state reads"),
            Some(MarshaledValue::Number(2.0))
        );
    }

    #[test]
    fn state_store_policy_rejects_oversized_commit_without_state_or_wakes() {
        let store = CappedStateStorePolicy::new(4, 8);
        let public_store: &dyn StateStore = &store;
        let actor = ActorId::new("tenant-a/actor");
        let StartOutcome::Started { lease, state } =
            ready_now(public_store.try_start(actor.clone())).expect("actor starts")
        else {
            panic!("first actor claim should start");
        };
        assert_eq!(state, MarshaledValue::Nil);

        let error = ready_now(public_store.commit(
            lease.clone(),
            MarshaledValue::String(b"large".to_vec()),
            vec![WakeRequest::new(actor.clone(), "oversized")],
        ))
        .expect_err("oversized durable state is rejected");
        assert_eq!(error, StateStoreError::ValueSizeLimit { bytes: 5, cap: 4 });
        ready_now(public_store.heartbeat(lease.clone()))
            .expect("oversized rejection leaves the public lease current");
        assert_eq!(
            store.inner.state(&actor).expect("state reads"),
            Some(MarshaledValue::Nil)
        );
        assert_eq!(
            store.inner.queued_wakes().expect("wakes read"),
            Vec::<QueuedWake>::new()
        );

        let commit = ready_now(public_store.commit(
            lease,
            MarshaledValue::String(b"ok".to_vec()),
            vec![WakeRequest::new(actor.clone(), "small")],
        ))
        .expect("lease remains valid after policy rejection");
        assert_eq!(commit.generation().value(), 1);
        assert_eq!(commit.wakes().len(), 1);
        let StartOutcome::Started {
            lease: final_lease,
            state,
        } = ready_now(public_store.try_start(actor)).expect("actor restarts")
        else {
            panic!("committed actor should be claimable again");
        };
        assert_eq!(state, MarshaledValue::String(b"ok".to_vec()));
        ready_now(public_store.abandon(final_lease)).expect("final lease abandons");
    }

    #[test]
    fn state_store_policy_caps_actor_slots_per_tenant() {
        let store = CappedStateStorePolicy::new(1024, 1);
        let public_store: &dyn StateStore = &store;
        let tenant_a_first = ActorId::new("tenant-a/first");
        let tenant_a_second = ActorId::new("tenant-a/second");
        let tenant_b_first = ActorId::new("tenant-b/first");

        let StartOutcome::Started { lease: a_lease, .. } =
            ready_now(public_store.try_start(tenant_a_first.clone()))
                .expect("tenant a first actor starts")
        else {
            panic!("first tenant actor should start");
        };
        let StartOutcome::Busy { actor, generation } =
            ready_now(public_store.try_start(tenant_a_first.clone()))
                .expect("same actor still reaches the backend busy contract")
        else {
            panic!("same active actor should be busy, not a tenant-cap rejection");
        };
        assert_eq!(actor, tenant_a_first);
        assert_eq!(generation.value(), 0);

        let error = ready_now(public_store.try_start(tenant_a_second))
            .expect_err("tenant a actor cap rejects a new actor slot");
        assert_eq!(
            error,
            StateStoreError::TenantActorLimit {
                tenant: "tenant-a".to_owned(),
                actors: 1,
                cap: 1,
            }
        );

        let StartOutcome::Started { lease: b_lease, .. } =
            ready_now(public_store.try_start(tenant_b_first))
                .expect("tenant b still has its own actor slot")
        else {
            panic!("another tenant should start independently");
        };
        ready_now(public_store.heartbeat(a_lease)).expect("tenant a lease remains current");
        ready_now(public_store.heartbeat(b_lease)).expect("tenant b lease remains current");
    }

    #[tokio::test]
    async fn durable_cross_actor_wake_uses_target_generation() {
        let store = ruau::durable::memory::InMemoryStore::new();
        let source = ActorId::new("harness/source");
        let target = ActorId::new("harness/target");
        commit_step(&store, target.clone(), 1.0).await;

        let source_lease = start(&store, source.clone()).await;
        let commit = store
            .commit(
                source_lease,
                MarshaledValue::Number(9.0),
                vec![WakeRequest::new(target.clone(), "cross-actor")],
            )
            .await
            .expect("source actor commits cross-actor wake");
        let wake = &commit.wakes()[0];
        assert_eq!(wake.actor(), &target);
        assert_eq!(wake.generation().value(), 1);

        let HarnessWake::Ran(current) = process_wake(&store, wake.clone()).await else {
            panic!("cross-actor wake should match target generation");
        };
        assert_eq!(current.generation().value(), 2);
        assert_eq!(
            store.state(&target).expect("target state reads"),
            Some(MarshaledValue::Number(2.0))
        );
        assert_eq!(
            store.state(&source).expect("source state reads"),
            Some(MarshaledValue::Number(9.0))
        );
    }

    #[tokio::test]
    async fn durable_commit_records_state_and_wakes_in_one_fenced_step() {
        let store = ruau::durable::memory::InMemoryStore::new();
        let actor = ActorId::new("harness/atomic");
        let target = ActorId::new("harness/atomic-target");
        commit_step(&store, target.clone(), 1.0).await;
        let baseline_wakes = store.queued_wakes().expect("baseline wakes read");

        let lease = start(&store, actor.clone()).await;
        let commit = store
            .commit(
                lease,
                MarshaledValue::Number(42.0),
                vec![
                    WakeRequest::new(actor.clone(), "self"),
                    WakeRequest::new(target.clone(), "target"),
                ],
            )
            .await
            .expect("fenced commit writes state and wakes");

        assert_eq!(commit.generation().value(), 1);
        assert_eq!(
            store.state(&actor).expect("committed state reads"),
            Some(MarshaledValue::Number(42.0))
        );
        let queued_wakes = store.queued_wakes().expect("queued wakes read");
        assert_eq!(
            &queued_wakes[..baseline_wakes.len()],
            baseline_wakes.as_slice()
        );
        assert_eq!(&queued_wakes[baseline_wakes.len()..], commit.wakes());
        assert_eq!(commit.wakes()[0].actor(), &actor);
        assert_eq!(commit.wakes()[0].generation().value(), 1);
        assert_eq!(commit.wakes()[1].actor(), &target);
        assert_eq!(commit.wakes()[1].generation().value(), 1);
    }

    #[tokio::test]
    async fn durable_stale_commit_writes_neither_state_nor_wakes() {
        let store = ruau::durable::memory::InMemoryStore::new();
        let actor = ActorId::new("harness/fence");
        let first = commit_step(&store, actor.clone(), 1.0).await;
        assert_eq!(first.generation().value(), 1);
        let baseline_wakes = store.queued_wakes().expect("wakes read");

        let stale = start(&store, actor.clone()).await;
        store
            .abandon(stale.clone())
            .await
            .expect("stale lease abandons so a later claim can continue");

        let current = start(&store, actor.clone()).await;
        let error = store
            .commit(
                stale,
                MarshaledValue::Number(99.0),
                vec![WakeRequest::new(actor.clone(), "must-not-queue")],
            )
            .await
            .expect_err("stale commit is fenced");
        assert!(matches!(error, StateStoreError::StaleLease { .. }));
        assert_eq!(
            store.state(&actor).expect("state reads"),
            Some(MarshaledValue::Number(1.0))
        );
        assert_eq!(
            store.queued_wakes().expect("wakes read"),
            baseline_wakes,
            "stale commit must not enqueue wakes"
        );
        store
            .abandon(current)
            .await
            .expect("current lease releases cleanly");
    }

    async fn commit_step(
        store: &ruau::durable::memory::InMemoryStore,
        actor: ActorId,
        counter: f64,
    ) -> ruau::durable::CommitOutcome {
        let lease = start(store, actor.clone()).await;
        store
            .commit(
                lease,
                MarshaledValue::Number(counter),
                vec![WakeRequest::new(actor, "continue")],
            )
            .await
            .expect("step commits")
    }

    async fn run_harness_agent_step(
        store: &ruau::durable::memory::InMemoryStore,
        actor: ActorId,
    ) -> HarnessAgentRun {
        match store.try_start(actor).await.expect("agent claim returns") {
            StartOutcome::Started { lease, state } => {
                finish_harness_agent_step(store, lease, state).await
            }
            StartOutcome::Busy { actor, .. } => panic!("agent {} was busy", actor.as_str()),
        }
    }

    async fn run_harness_agent_wake(
        store: &ruau::durable::memory::InMemoryStore,
        wake: QueuedWake,
    ) -> HarnessAgentRun {
        match store
            .try_start(wake.actor().clone())
            .await
            .expect("agent wake claim returns")
        {
            StartOutcome::Started { lease, state } if lease.generation() == wake.generation() => {
                finish_harness_agent_step(store, lease, state).await
            }
            StartOutcome::Started { lease, .. } => {
                store.abandon(lease).await.expect("stale wake abandons");
                panic!("agent wake was stale")
            }
            StartOutcome::Busy { actor, .. } => panic!("agent {} was busy", actor.as_str()),
        }
    }

    async fn finish_harness_agent_step(
        store: &ruau::durable::memory::InMemoryStore,
        lease: StateLease,
        state: MarshaledValue,
    ) -> HarnessAgentRun {
        let invocation = Arc::new(Mutex::new(HarnessAgentInvocation {
            actor: lease.actor().clone(),
            counter: harness_agent_counter(&state),
            wakes: Vec::new(),
        }));
        let module_source: Arc<dyn ModuleSource> = Arc::new(InMemorySource::new().with_module(
            "harness/agent-main",
            r#"
local before = durable.get_counter()
durable.set_counter(before + 1)
durable.wake("continue")
return {
    before = before,
    after = durable.get_counter(),
}
"#,
        ));
        let profile = ruau::vm::Profile::full().without_runtime_compilation();
        let surface = SurfaceSpec::builder(profile)
            .module_source(Arc::clone(&module_source))
            .module(Arc::new(HarnessAgentDurableModule {
                invocation: Arc::clone(&invocation),
            }))
            .build()
            .expect("agent surface validates");
        let root = ModuleId::from("harness/agent-main");
        check_harness_agent_graph(&surface, module_source.as_ref(), &root).await;
        let source = module_source.read(&root).await.expect("agent source reads");
        let mut builder = Vm::builder()
            .ambient(Ambient::production(0))
            .limits(Limits {
                gas: Some(100_000),
                max_memory_bytes: Some(1 << 20),
                ..Limits::unlimited()
            })
            .profile(*surface.profile())
            .module_source(Arc::clone(&module_source));
        for module in surface.native_modules() {
            builder = builder.module(Arc::clone(module));
        }
        let mut vm = builder.build().expect("agent VM builds");
        let chunk = compile_for(surface.profile(), &source, &CompileOptions::default())
            .expect("agent source compiles");
        let module = vm
            .load_named(&chunk, root.to_lossy_string().as_bytes())
            .expect("agent module loads");
        let values = vm
            .exec_async(&module, Default::default())
            .await
            .expect("agent module returns successfully");
        let (stored_counter, wakes) = {
            let invocation_state = invocation.lock().expect("agent invocation state");
            (invocation_state.counter, invocation_state.wakes.clone())
        };
        assert_eq!(
            harness_agent_result_numbers(&values),
            (harness_agent_counter(&state), stored_counter)
        );
        let commit = store
            .commit(lease, MarshaledValue::Number(stored_counter), wakes)
            .await
            .expect("agent step commits");
        HarnessAgentRun {
            observed_counter: harness_agent_counter(&state),
            stored_counter,
            generation: commit.generation().value(),
            wakes: commit.wakes().to_vec(),
        }
    }

    fn harness_agent_counter(state: &MarshaledValue) -> f64 {
        match state {
            MarshaledValue::Nil => 0.0,
            MarshaledValue::Number(counter) => *counter,
            other => panic!("unexpected harness agent state: {other:?}"),
        }
    }

    async fn check_harness_agent_graph(
        surface: &SurfaceSpec,
        module_source: &dyn ModuleSource,
        root: &ModuleId,
    ) {
        let config = EmptyResolver;
        let mut frontend =
            GraphChecker::with_checker(module_source, &config, surface.new_checker());
        let root_name = root
            .as_str()
            .map(ModuleName::from)
            .unwrap_or_else(|| panic!("agent root is not UTF-8: {}", root.to_lossy_string()));
        let result = frontend.check_async(root_name).await;
        let mut module_names = BTreeSet::new();
        module_names.insert(result.root.clone());
        module_names.extend(result.build_queue.iter().cloned());

        let mut diagnostics = Vec::new();
        for module_name in module_names {
            diagnostics.extend(
                frontend
                    .frontend()
                    .resolver_diagnostics(&module_name)
                    .iter()
                    .map(|diagnostic| {
                        TypeDiagnostic::from_resolver_diagnostic_with_display_name(
                            diagnostic,
                            Some(&frontend.frontend().module_display_name(&module_name)),
                        )
                    }),
            );
            if let Some(checked) = frontend.checked_module(&module_name) {
                diagnostics.extend(checked.diagnostics().iter().cloned());
            }
        }
        assert!(
            diagnostics.is_empty(),
            "{}",
            render_diagnostic_summary(&root.to_lossy_string(), &diagnostics)
        );
    }

    fn harness_agent_result_numbers(values: &[MarshaledValue]) -> (f64, f64) {
        let [MarshaledValue::Table(entries)] = values else {
            panic!("unexpected agent result shape: {values:?}");
        };
        (
            marshaled_table_number(entries, "before"),
            marshaled_table_number(entries, "after"),
        )
    }

    fn marshaled_table_number(entries: &[MarshaledPair], key: &str) -> f64 {
        entries
            .iter()
            .find_map(|pair| match (&pair.key, &pair.value) {
                (MarshaledValue::String(name), MarshaledValue::Number(value))
                    if name.as_slice() == key.as_bytes() =>
                {
                    Some(*value)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing numeric field `{key}` in {entries:?}"))
    }

    #[derive(Debug, PartialEq)]
    struct HarnessAgentRun {
        observed_counter: f64,
        stored_counter: f64,
        generation: u64,
        wakes: Vec<QueuedWake>,
    }

    impl HarnessAgentRun {
        fn summary(&self) -> (f64, f64, u64, usize) {
            (
                self.observed_counter,
                self.stored_counter,
                self.generation,
                self.wakes.len(),
            )
        }
    }

    struct HarnessAgentInvocation {
        actor: ActorId,
        counter: f64,
        wakes: Vec<WakeRequest>,
    }

    struct HarnessAgentDurableModule {
        invocation: Arc<Mutex<HarnessAgentInvocation>>,
    }

    impl NativeModule for HarnessAgentDurableModule {
        fn name(&self) -> &'static str {
            "harness_agent_durable"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text({
                r#"declare durable: {
    get_counter: () -> number,
    set_counter: (number) -> (),
    wake: (string) -> (),
}"#
            })
        }

        fn build(&self, builder: &mut dyn ModuleBuilder) {
            let invocation = Arc::clone(&self.invocation);
            builder.async_function(
                "get_counter",
                ModuleBinding::library("durable"),
                async_host_fn(move |_ctx, (): ()| {
                    let invocation = Arc::clone(&invocation);
                    async move {
                        let counter = invocation
                            .lock()
                            .map_err(|_| Error::runtime("agent state lock is poisoned"))?
                            .counter;
                        Ok(HostReturn {
                            values: vec![OwnedValue::Number(counter)],
                        })
                    }
                }),
            );

            let invocation = Arc::clone(&self.invocation);
            builder.async_function(
                "set_counter",
                ModuleBinding::library("durable"),
                async_host_fn(move |_ctx, counter: f64| {
                    let invocation = Arc::clone(&invocation);
                    async move {
                        invocation
                            .lock()
                            .map_err(|_| Error::runtime("agent state lock is poisoned"))?
                            .counter = counter;
                        Ok(HostReturn::default())
                    }
                }),
            );

            let invocation = Arc::clone(&self.invocation);
            builder.async_function(
                "wake",
                ModuleBinding::library("durable"),
                async_host_fn(move |_ctx, reason: String| {
                    let invocation = Arc::clone(&invocation);
                    async move {
                        let mut invocation = invocation
                            .lock()
                            .map_err(|_| Error::runtime("agent state lock is poisoned"))?;
                        let actor = invocation.actor.clone();
                        invocation.wakes.push(WakeRequest::new(actor, reason));
                        Ok(HostReturn::default())
                    }
                }),
            );
        }
    }

    #[derive(Debug, PartialEq)]
    enum HarnessWake {
        Ran(ruau::durable::CommitOutcome),
        Stale,
        BusyRetry,
    }

    async fn process_wake(
        store: &ruau::durable::memory::InMemoryStore,
        wake: QueuedWake,
    ) -> HarnessWake {
        let outcome = store
            .try_start(wake.actor().clone())
            .await
            .expect("wake claim returns");
        let StartOutcome::Started { lease, state } = outcome else {
            return HarnessWake::BusyRetry;
        };
        if lease.generation() != wake.generation() {
            store.abandon(lease).await.expect("stale wake abandons");
            return HarnessWake::Stale;
        }
        let MarshaledValue::Number(counter) = state else {
            panic!("unexpected state shape: {state:?}");
        };
        HarnessWake::Ran(
            store
                .commit(
                    lease,
                    MarshaledValue::Number(counter + 1.0),
                    vec![WakeRequest::new(wake.actor().clone(), "continue")],
                )
                .await
                .expect("wake step commits"),
        )
    }

    async fn start(store: &ruau::durable::memory::InMemoryStore, actor: ActorId) -> StateLease {
        start_any(store, actor).await
    }

    async fn start_any(store: &impl StateStore, actor: ActorId) -> StateLease {
        match store.try_start(actor).await.expect("claim returns") {
            StartOutcome::Started { lease, .. } => lease,
            StartOutcome::Busy { actor, .. } => panic!("actor {} was busy", actor.as_str()),
        }
    }

    fn single_started_lease(outcomes: &[StartOutcome]) -> StateLease {
        let mut leases = outcomes.iter().filter_map(|outcome| match outcome {
            StartOutcome::Started { lease, .. } => Some(lease.clone()),
            StartOutcome::Busy { .. } => None,
        });
        let lease = leases.next().expect("one actor claim starts");
        assert!(leases.next().is_none(), "only one actor claim may start");
        lease
    }

    fn single_busy_generation(outcomes: &[StartOutcome]) -> u64 {
        let mut generations = outcomes.iter().filter_map(|outcome| match outcome {
            StartOutcome::Started { .. } => None,
            StartOutcome::Busy { generation, .. } => Some(generation.value()),
        });
        let generation = generations.next().expect("one actor claim is busy");
        assert!(
            generations.next().is_none(),
            "only one actor claim should be busy"
        );
        generation
    }

    fn capped_runner(ingress: IngressLimits, lane_admission: AdmissionLimits) -> Runner {
        Runner::builder()
            .profile(ruau::vm::Profile::full().without_runtime_compilation())
            .ambient(Ambient::production(0))
            .features(ExecutionFeatures::all_off())
            .no_host_modules()
            .max_source_bytes(1024)
            .limits(Limits {
                gas: Some(100_000),
                max_memory_bytes: Some(1 << 20),
                ..Limits::unlimited()
            })
            .lane_count(1)
            .lane_admission_limits(lane_admission)
            .ingress_limits(ingress)
            .build()
            .expect("capped runner validates")
    }

    fn tenant_surface(source: &'static str) -> SurfaceSpec {
        let modules = Arc::new(InMemorySource::new().with_module("tenant", source));
        SurfaceSpec::builder(ruau::vm::Profile::full().without_runtime_compilation())
            .module_source(modules)
            .build()
            .expect("tenant surface validates")
    }

    async fn run_tenant(
        runner: &Runner,
        tenant: TenantId,
        surface: &SurfaceSpec,
        source: &[u8],
    ) -> RequestReport {
        runner
            .run_report(
                Request::new(
                    source,
                    Budget::with_timeout(Duration::from_secs(5)).expect("future deadline"),
                )
                .tenant(tenant)
                .surface(surface),
            )
            .await
    }

    fn assert_report_values(report: RequestReport, tenant: TenantId, expected: &[ResultValue]) {
        assert_eq!(report.tenant, tenant);
        match report.outcome {
            RequestReportOutcome::Success { values } => assert_eq!(values, expected),
            other => panic!("tenant {tenant:?} should succeed, got {other:?}"),
        }
    }

    async fn wait_for_in_flight(runner: &Runner, target: usize) {
        for _ in 0..100 {
            if runner.lane_metrics().in_flight >= target {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "runner did not reach {target} in-flight request(s); metrics = {:?}",
            runner.lane_metrics()
        );
    }

    #[derive(Clone)]
    struct HarnessWaitHost {
        started: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
        release: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    }

    impl HostFunction for HarnessWaitHost {
        fn call(&self, _ctx: &mut dyn HostContext) -> HostCall {
            if let Some(started) = self.started.lock().expect("started mutex").take() {
                assert!(started.send(()).is_ok(), "started receiver is alive");
            }
            let release = self
                .release
                .lock()
                .expect("release mutex")
                .take()
                .expect("wait host is called once");
            HostCall::Pending(Box::pin(async move {
                drop(release.await);
                Ok::<HostReturn, HostError>(HostReturn {
                    values: vec![OwnedValue::Nil],
                })
            }))
        }
    }

    struct HarnessWaitHostModule(HarnessWaitHost);

    impl NativeModule for HarnessWaitHostModule {
        fn name(&self) -> &'static str {
            "harness_wait_host"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("declare function wait_host(): nil")
        }

        fn build(&self, builder: &mut dyn ModuleBuilder) {
            builder.function("wait_host", ModuleBinding::Global, Box::new(self.0.clone()));
        }
    }

    #[derive(Clone)]
    struct HarnessBlockingHost {
        started: std::sync::mpsc::Sender<()>,
        release: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
    }

    impl HostFunction for HarnessBlockingHost {
        fn call(&self, _ctx: &mut dyn HostContext) -> HostCall {
            self.started
                .send(())
                .expect("blocking host start receiver is alive");
            let release = self
                .release
                .lock()
                .expect("release mutex")
                .take()
                .expect("blocking host is called once");
            release.recv().expect("blocking host is released");
            HostCall::Ready(Ok(vec![OwnedValue::Nil]))
        }
    }

    struct HarnessBlockingHostModule(HarnessBlockingHost);

    impl NativeModule for HarnessBlockingHostModule {
        fn name(&self) -> &'static str {
            "harness_blocking_host"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("declare function blocking_host(): nil")
        }

        fn build(&self, builder: &mut dyn ModuleBuilder) {
            builder.function(
                "blocking_host",
                ModuleBinding::Global,
                Box::new(self.0.clone()),
            );
        }
    }

    struct LeastServedTenantPolicy {
        served: BTreeMap<u64, usize>,
        seen_lanes: Mutex<Vec<Option<usize>>>,
    }

    impl AdmissionPolicy for LeastServedTenantPolicy {
        fn decide(&self, snapshot: &AdmissionSnapshot) -> AdmissionDecision {
            if snapshot.pool_in_flight == 0 {
                AdmissionDecision::Admit { lane_hint: Some(0) }
            } else {
                AdmissionDecision::Defer { lane_hint: Some(0) }
            }
        }

        fn compare_ready(
            &self,
            left: &AdmissionSnapshot,
            right: &AdmissionSnapshot,
        ) -> std::cmp::Ordering {
            let mut seen_lanes = self.seen_lanes.lock().expect("seen lanes");
            seen_lanes.push(left.lane);
            seen_lanes.push(right.lane);
            let left_served = self.served.get(&left.tenant.0).copied().unwrap_or(0);
            let right_served = self.served.get(&right.tenant.0).copied().unwrap_or(0);
            left_served
                .cmp(&right_served)
                .then_with(|| left.sequence.cmp(&right.sequence))
        }
    }

    struct LogicalTtlStateStore {
        inner: ruau::durable::memory::InMemoryStore,
        ttl_ticks: u64,
        now: Mutex<u64>,
        leases: Mutex<BTreeMap<(ActorId, LeaseToken), u64>>,
    }

    impl LogicalTtlStateStore {
        fn new(ttl_ticks: u64) -> Self {
            assert!(ttl_ticks > 0, "TTL must be positive");
            Self {
                inner: ruau::durable::memory::InMemoryStore::new(),
                ttl_ticks,
                now: Mutex::new(0),
                leases: Mutex::new(BTreeMap::new()),
            }
        }

        fn advance(&self, ticks: u64) {
            let mut now = self.now.lock().expect("clock mutex");
            *now = now.saturating_add(ticks);
        }

        fn now(&self) -> u64 {
            *self.now.lock().expect("clock mutex")
        }

        fn remember(&self, lease: &StateLease) {
            self.leases.lock().expect("lease mutex").insert(
                (lease.actor().clone(), lease.token()),
                self.now().saturating_add(self.ttl_ticks),
            );
        }

        fn forget(&self, lease: &StateLease) {
            self.leases
                .lock()
                .expect("lease mutex")
                .remove(&(lease.actor().clone(), lease.token()));
        }

        fn stale_error(lease: &StateLease) -> StateStoreError {
            StateStoreError::StaleLease {
                actor: lease.actor().clone(),
                token: lease.token(),
            }
        }

        fn expire_if_needed(&self, lease: &StateLease) -> Option<StateStoreError> {
            let key = (lease.actor().clone(), lease.token());
            let mut leases = self.leases.lock().expect("lease mutex");
            let Some(deadline) = leases.get(&key).copied() else {
                return Some(Self::stale_error(lease));
            };
            if self.now() < deadline {
                return None;
            }
            leases.remove(&key);
            drop(leases);
            match ready_now(self.inner.abandon(lease.clone())) {
                Ok(()) | Err(StateStoreError::StaleLease { .. }) => {}
                Err(error) => return Some(error),
            }
            Some(Self::stale_error(lease))
        }

        fn ready<T: Send + 'static>(result: StateStoreResult<T>) -> StateStoreFuture<T> {
            Box::pin(ready(result))
        }
    }

    impl StateStore for LogicalTtlStateStore {
        fn try_start(&self, actor: ActorId) -> StateStoreFuture<StartOutcome> {
            let result = ready_now(self.inner.try_start(actor));
            if let Ok(StartOutcome::Started { lease, .. }) = &result {
                self.remember(lease);
            }
            Self::ready(result)
        }

        fn heartbeat(&self, lease: StateLease) -> StateStoreFuture<()> {
            let result = if let Some(error) = self.expire_if_needed(&lease) {
                Err(error)
            } else {
                let result = ready_now(self.inner.heartbeat(lease.clone()));
                if result.is_ok() {
                    self.remember(&lease);
                }
                result
            };
            Self::ready(result)
        }

        fn abandon(&self, lease: StateLease) -> StateStoreFuture<()> {
            self.forget(&lease);
            Self::ready(ready_now(self.inner.abandon(lease)))
        }

        fn commit(
            &self,
            lease: StateLease,
            state: MarshaledValue,
            wakes: Vec<WakeRequest>,
        ) -> StateStoreFuture<CommitOutcome> {
            let result = if let Some(error) = self.expire_if_needed(&lease) {
                Err(error)
            } else {
                self.forget(&lease);
                ready_now(self.inner.commit(lease, state, wakes))
            };
            Self::ready(result)
        }
    }

    struct CappedStateStorePolicy {
        inner: ruau::durable::memory::InMemoryStore,
        max_state_bytes: usize,
        max_actors_per_tenant: usize,
        tenant_actors: Mutex<BTreeMap<String, BTreeSet<ActorId>>>,
    }

    impl CappedStateStorePolicy {
        fn new(max_state_bytes: usize, max_actors_per_tenant: usize) -> Self {
            Self {
                inner: ruau::durable::memory::InMemoryStore::new(),
                max_state_bytes,
                max_actors_per_tenant,
                tenant_actors: Mutex::new(BTreeMap::new()),
            }
        }

        fn try_start_policy(&self, actor: &ActorId) -> StateStoreResult<StartOutcome> {
            let tenant = tenant_key(actor);
            let inserted = self.reserve_actor_slot(&tenant, actor.clone())?;
            match ready_now(self.inner.try_start(actor.clone())) {
                Ok(outcome) => Ok(outcome),
                Err(error) => {
                    if inserted {
                        self.release_actor_slot(&tenant, actor);
                    }
                    Err(error)
                }
            }
        }

        fn commit_policy(
            &self,
            lease: StateLease,
            state: MarshaledValue,
            wakes: Vec<WakeRequest>,
        ) -> StateStoreResult<CommitOutcome> {
            let bytes = marshaled_value_bytes(&state);
            if bytes > self.max_state_bytes {
                return Err(StateStoreError::ValueSizeLimit {
                    bytes,
                    cap: self.max_state_bytes,
                });
            }
            ready_now(self.inner.commit(lease, state, wakes))
        }

        fn reserve_actor_slot(&self, tenant: &str, actor: ActorId) -> StateStoreResult<bool> {
            let mut tenant_actors = self.tenant_actors.lock().expect("tenant actor cap mutex");
            let actors = tenant_actors.entry(tenant.to_owned()).or_default();
            if actors.contains(&actor) {
                return Ok(false);
            }
            if actors.len() >= self.max_actors_per_tenant {
                return Err(StateStoreError::TenantActorLimit {
                    tenant: tenant.to_owned(),
                    actors: actors.len(),
                    cap: self.max_actors_per_tenant,
                });
            }
            actors.insert(actor);
            Ok(true)
        }

        fn release_actor_slot(&self, tenant: &str, actor: &ActorId) {
            let mut tenant_actors = self.tenant_actors.lock().expect("tenant actor cap mutex");
            if let Some(actors) = tenant_actors.get_mut(tenant) {
                actors.remove(actor);
                if actors.is_empty() {
                    tenant_actors.remove(tenant);
                }
            }
        }

        fn ready<T: Send + 'static>(result: StateStoreResult<T>) -> StateStoreFuture<T> {
            Box::pin(ready(result))
        }
    }

    impl StateStore for CappedStateStorePolicy {
        fn try_start(&self, actor: ActorId) -> StateStoreFuture<StartOutcome> {
            Self::ready(self.try_start_policy(&actor))
        }

        fn heartbeat(&self, lease: StateLease) -> StateStoreFuture<()> {
            self.inner.heartbeat(lease)
        }

        fn abandon(&self, lease: StateLease) -> StateStoreFuture<()> {
            self.inner.abandon(lease)
        }

        fn commit(
            &self,
            lease: StateLease,
            state: MarshaledValue,
            wakes: Vec<WakeRequest>,
        ) -> StateStoreFuture<CommitOutcome> {
            Self::ready(self.commit_policy(lease, state, wakes))
        }
    }

    fn tenant_key(actor: &ActorId) -> String {
        actor.as_str().split_once('/').map_or_else(
            || actor.as_str().to_owned(),
            |(tenant, _)| tenant.to_owned(),
        )
    }

    fn marshaled_value_bytes(value: &MarshaledValue) -> usize {
        match value {
            MarshaledValue::Nil => 0,
            MarshaledValue::Boolean(_) => 1,
            MarshaledValue::Number(_) | MarshaledValue::Integer(_) => 8,
            MarshaledValue::Vector(_) => 12,
            MarshaledValue::LightUserdata { .. } => 5,
            MarshaledValue::String(bytes) | MarshaledValue::Buffer(bytes) => bytes.len(),
            MarshaledValue::Table(pairs) => pairs.iter().map(marshaled_pair_bytes).sum(),
            MarshaledValue::Opaque(kind) => kind.len(),
        }
    }

    fn marshaled_pair_bytes(pair: &MarshaledPair) -> usize {
        marshaled_value_bytes(&pair.key).saturating_add(marshaled_value_bytes(&pair.value))
    }

    fn ready_now<T>(mut future: StateStoreFuture<T>) -> StateStoreResult<T> {
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("test state store future unexpectedly pending"),
        }
    }
}
