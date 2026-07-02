//! Concurrency and failure-pressure checks for the production request runner.

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use ruau::{
        runner::{
            AdmissionLimits, Budget, IngressLimits, Request, RequestError, ResultValue, Runner,
        },
        surface::Surface,
        vm::{Ambient, Cancel, CancellationToken, ExecutionFeatures, Limits},
        vm_api::{HostReturn, HostValue, ModuleBinding, ModuleBuilder, NativeModule, OwnedValue},
    };
    // These test-only host functions exercise the engine-facing trait directly;
    // ordinary facade callers name host/native-module types through ruau::vm_api.
    use ruau_vm_api::{HostCall, HostContext, HostError, HostFunction};

    fn default_surface() -> Surface {
        Surface::new()
    }

    fn surface_with_module(module: Arc<dyn NativeModule>) -> Surface {
        Surface::builder()
            .module(module)
            .build()
            .expect("module surface validates")
    }

    fn runner_with_limits(gas: u64, max_memory_bytes: usize) -> Runner {
        Runner::builder()
            .surface(default_surface())
            .ambient(Ambient::production(0))
            .features(ExecutionFeatures::all_off())
            // These tests stress raw concurrency; opt out of the fail-closed
            // admission defaults explicitly.
            .ingress_limits(IngressLimits {
                max_in_flight: usize::MAX,
                max_in_flight_per_tenant: usize::MAX,
            })
            .lane_admission_limits(AdmissionLimits::unlimited())
            .max_source_bytes(256 * 1024)
            .limits(Limits {
                gas: Some(gas),
                max_memory_bytes: Some(max_memory_bytes),
                ..Limits::unlimited()
            })
            .build()
            .expect("runner builds")
    }

    fn budget(timeout: Duration) -> Budget {
        Budget::with_timeout(timeout).expect("test budget has a future deadline")
    }

    fn assert_number(values: &[ResultValue], expected: f64) {
        match values {
            [ResultValue::Number(actual)] => {
                assert!(
                    (*actual - expected).abs() < f64::EPSILON,
                    "expected {expected}, got {actual}"
                );
            }
            other => panic!("expected one number result, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_thousand_concurrent_tiny_requests_complete() {
        let runner = Arc::new(runner_with_limits(1_000_000, 8 * 1024 * 1024));
        let mut tasks = Vec::new();
        for _ in 0..1_000 {
            let runner = Arc::clone(&runner);
            tasks.push(tokio::spawn(async move {
                // Generous budget: type checks run on a bounded pool, so a
                // thousand-deep burst drains in series under full-suite CPU
                // contention; this asserts completion, not latency.
                runner
                    .run(Request::new(
                        b"--!nocheck\nreturn 1 + 1",
                        budget(Duration::from_secs(60)),
                    ))
                    .await
            }));
        }

        for task in tasks {
            let outcome = task
                .await
                .expect("request task joins")
                .expect("request succeeds");
            assert_number(&outcome.values, 2.0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancellation_storms_do_not_poison_the_runner() {
        let runner = Arc::new(runner_with_limits(1_000_000, 8 * 1024 * 1024));
        let mut tasks = Vec::new();
        for _ in 0..128 {
            let runner = Arc::clone(&runner);
            tasks.push(tokio::spawn(async move {
                // Built from the facade-re-exported token type: embedders link
                // external cancellation without a direct tokio-util dependency.
                let cancel = Cancel::new(CancellationToken::new());
                cancel.cancel();
                runner
                    .run(Request::new(
                        b"--!nocheck\nreturn 1",
                        Budget::new(Instant::now() + Duration::from_secs(5), cancel)
                            .expect("future cancellation budget"),
                    ))
                    .await
            }));
        }

        for task in tasks {
            let error = task
                .await
                .expect("request task joins")
                .expect_err("pre-cancelled request fails");
            assert!(matches!(error, RequestError::Cancelled), "got {error:?}");
        }

        let outcome = runner
            .run(Request::new(
                b"--!nocheck\nreturn 7",
                budget(Duration::from_secs(5)),
            ))
            .await
            .expect("runner remains healthy");
        assert_number(&outcome.values, 7.0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn jittered_mid_flight_cancellation_storm_stays_healthy() {
        // Mixed CPU and host-await workloads on a multi-lane pool, each with a
        // jittered mid-flight cancel: every outcome must be Ok or Cancelled,
        // and the runner must stay healthy afterwards.
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = Arc::new(
            Runner::builder()
                .surface(surface_with_module(Arc::new(DelayModule {
                    delay: Duration::from_millis(40),
                    calls: Arc::clone(&calls),
                })))
                .ambient(Ambient::production(0))
                .features(ExecutionFeatures::all_off())
                .ingress_limits(IngressLimits {
                    max_in_flight: usize::MAX,
                    max_in_flight_per_tenant: usize::MAX,
                })
                .lane_admission_limits(AdmissionLimits::unlimited())
                .lane_count(4)
                .max_source_bytes(256 * 1024)
                .limits(Limits {
                    gas: Some(50_000_000),
                    max_memory_bytes: Some(16 * 1024 * 1024),
                    ..Limits::unlimited()
                })
                .build()
                .expect("runner builds"),
        );

        let mut tasks = Vec::new();
        for index in 0..96_u64 {
            let runner = Arc::clone(&runner);
            tasks.push(tokio::spawn(async move {
                let cancel = Cancel::manual();
                let canceller = cancel.clone();
                // Jitter across the whole request lifetime: parse/check,
                // mid-dispatch, and mid-host-await all get hit.
                let cancel_after = Duration::from_micros((index % 29) * 700);
                tokio::spawn(async move {
                    tokio::time::sleep(cancel_after).await;
                    canceller.cancel();
                });
                let source: &[u8] = if index % 2 == 0 {
                    b"--!nocheck\nlocal s = 0\nfor i = 1, 400000 do s = s + i end\nreturn s"
                } else {
                    b"--!nocheck\nreturn host_delay(1)"
                };
                runner
                    .run(Request::new(
                        source,
                        Budget::new(Instant::now() + Duration::from_secs(10), cancel)
                            .expect("future storm budget"),
                    ))
                    .await
            }));
        }

        let mut completed = 0_usize;
        let mut cancelled = 0_usize;
        for task in tasks {
            match task.await.expect("storm task joins") {
                Ok(_) => completed += 1,
                Err(RequestError::Cancelled) => cancelled += 1,
                Err(other) => panic!("storm outcome must be Ok or Cancelled, got {other:?}"),
            }
        }
        assert_eq!(completed + cancelled, 96);

        let outcome = runner
            .run(Request::new(
                b"--!nocheck\nreturn 21",
                budget(Duration::from_secs(5)),
            ))
            .await
            .expect("runner remains healthy after the storm");
        assert_number(&outcome.values, 21.0);
    }

    struct DelayModule {
        delay: Duration,
        calls: Arc<AtomicUsize>,
    }

    impl NativeModule for DelayModule {
        fn name(&self) -> &'static str {
            "delay"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("declare function host_delay(value: number): number")
        }

        fn build(&self, builder: &mut dyn ModuleBuilder) {
            builder.function(
                "host_delay",
                ModuleBinding::Global,
                Box::new(DelayHost {
                    delay: self.delay,
                    calls: Arc::clone(&self.calls),
                }),
            );
        }
    }

    struct DelayHost {
        delay: Duration,
        calls: Arc<AtomicUsize>,
    }

    impl HostFunction for DelayHost {
        fn call(&self, ctx: &mut dyn HostContext) -> HostCall {
            let value = match ctx.arg(0) {
                Some(HostValue::Integer(value)) => value as f64,
                Some(HostValue::Number(value)) => value,
                _ => 0.0,
            };
            self.calls.fetch_add(1, Ordering::SeqCst);
            let delay = self.delay;
            HostCall::Pending(Box::pin(async move {
                tokio::time::sleep(delay).await;
                Ok::<HostReturn, HostError>(HostReturn {
                    values: vec![OwnedValue::Number(value + 1.0)],
                })
            }))
        }
    }

    fn delay_runner(delay: Duration, calls: Arc<AtomicUsize>) -> Runner {
        Runner::builder()
            .surface(surface_with_module(Arc::new(DelayModule { delay, calls })))
            .ambient(Ambient::production(0))
            .features(ExecutionFeatures::all_off())
            .ingress_limits(IngressLimits {
                max_in_flight: usize::MAX,
                max_in_flight_per_tenant: usize::MAX,
            })
            .lane_admission_limits(AdmissionLimits::unlimited())
            .max_source_bytes(256 * 1024)
            .limits(Limits {
                gas: Some(2_000_000),
                max_memory_bytes: Some(8 * 1024 * 1024),
                ..Limits::unlimited()
            })
            .build()
            .expect("runner builds")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn long_host_awaits_do_not_starve_ordinary_requests() {
        let host_calls = Arc::new(AtomicUsize::new(0));
        let runner = Arc::new(delay_runner(
            Duration::from_secs(2),
            Arc::clone(&host_calls),
        ));
        let mut waits = Vec::new();
        for _ in 0..16 {
            let runner = Arc::clone(&runner);
            waits.push(tokio::spawn(async move {
                runner
                    .run(Request::new(
                        b"--!nocheck\nreturn host_delay(41)",
                        budget(Duration::from_secs(10)),
                    ))
                    .await
            }));
        }

        let wait_started = Instant::now();
        while host_calls.load(Ordering::SeqCst) < 16 {
            assert!(
                wait_started.elapsed() < Duration::from_secs(2),
                "host awaits should reach their parked futures"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let started = Instant::now();
        let ordinary = runner
            .run(Request::new(
                b"--!nocheck\nreturn 3",
                budget(Duration::from_secs(3)),
            ))
            .await
            .expect("ordinary request succeeds while host awaits are parked");
        let ordinary_elapsed = started.elapsed();
        assert!(
            ordinary_elapsed < Duration::from_secs(1),
            "ordinary request should run while host awaits are parked; took {ordinary_elapsed:?}"
        );
        assert_number(&ordinary.values, 3.0);

        for task in waits {
            let outcome = task
                .await
                .expect("host-await task joins")
                .expect("host await succeeds");
            assert_number(&outcome.values, 42.0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cpu_bound_tenants_hit_deadlines_without_starving_tiny_requests() {
        let runner = Arc::new(runner_with_limits(1 << 60, 8 * 1024 * 1024));
        let mut loops = Vec::new();
        for _ in 0..8 {
            let runner = Arc::clone(&runner);
            loops.push(tokio::spawn(async move {
                runner
                    .run(Request::new(
                        b"--!nocheck\nwhile true do end",
                        budget(Duration::from_millis(50)),
                    ))
                    .await
            }));
        }

        let started = Instant::now();
        let ordinary = runner
            .run(Request::new(
                b"--!nocheck\nreturn 5",
                budget(Duration::from_secs(2)),
            ))
            .await
            .expect("ordinary request succeeds while CPU tenants are bounded");
        let ordinary_elapsed = started.elapsed();
        assert!(
            ordinary_elapsed < Duration::from_millis(1_500),
            "ordinary request should not starve behind CPU-bound tenants; took {ordinary_elapsed:?}"
        );
        assert_number(&ordinary.values, 5.0);

        for task in loops {
            let error = task
                .await
                .expect("CPU tenant task joins")
                .expect_err("CPU loop hits deadline");
            assert!(
                matches!(error, RequestError::DeadlineExceeded),
                "got {error:?}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn hostile_memory_pressure_fails_closed_without_worker_damage() {
        let runner = Arc::new(runner_with_limits(1 << 60, 1024 * 1024));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let runner = Arc::clone(&runner);
            tasks.push(tokio::spawn(async move {
                runner
                    .run(Request::new(
                        b"--!nocheck\nlocal t = {}\nwhile true do t[#t + 1] = string.rep('x', 1024) end",
                        budget(Duration::from_secs(2))))
                    .await
            }));
        }

        for task in tasks {
            let error = task
                .await
                .expect("memory-pressure task joins")
                .expect_err("memory pressure fails closed");
            assert!(
                matches!(
                    error,
                    RequestError::OutOfMemory(_) | RequestError::DeadlineExceeded
                ),
                "got {error:?}"
            );
        }

        let outcome = runner
            .run(Request::new(
                b"--!nocheck\nreturn 11",
                budget(Duration::from_secs(2)),
            ))
            .await
            .expect("runner remains healthy after memory pressure");
        assert_number(&outcome.values, 11.0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pattern_pressure_fails_closed_without_worker_damage() {
        let runner = Arc::new(
            Runner::builder()
                .surface(default_surface())
                .ambient(Ambient::production(0))
                .features(ExecutionFeatures::all_off())
                .ingress_limits(IngressLimits {
                    max_in_flight: usize::MAX,
                    max_in_flight_per_tenant: usize::MAX,
                })
                .lane_admission_limits(AdmissionLimits::unlimited())
                .max_source_bytes(256 * 1024)
                .limits(Limits {
                    gas: Some(1 << 60),
                    max_memory_bytes: Some(8 * 1024 * 1024),
                    max_pattern_steps: Some(128),
                    ..Limits::unlimited()
                })
                .build()
                .expect("runner builds"),
        );

        let started = Instant::now();
        let error = runner
            .run(Request::new(
                b"--!nocheck\nreturn string.match(string.rep('a', 128), '.*.*.*.*.*.*.*.*z')",
                budget(Duration::from_secs(2)),
            ))
            .await
            .expect_err("pattern pressure fails closed");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "pattern pressure should stop at the 128-step limit, not run until the deadline"
        );
        match error {
            RequestError::Runtime(ResultValue::String(bytes)) => {
                let message = String::from_utf8_lossy(&bytes);
                assert!(
                    message.contains("pattern match is too complex"),
                    "got runtime error {message:?}"
                );
            }
            other => panic!("expected pattern runtime failure, got {other:?}"),
        }

        let outcome = runner
            .run(Request::new(
                b"--!nocheck\nreturn 13",
                budget(Duration::from_secs(2)),
            ))
            .await
            .expect("runner remains healthy after pattern pressure");
        assert_number(&outcome.values, 13.0);
    }

    struct PanicModule;

    impl NativeModule for PanicModule {
        fn name(&self) -> &'static str {
            "panic_probe"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("declare function panic_probe(): ()")
        }

        fn build(&self, builder: &mut dyn ModuleBuilder) {
            builder.function("panic_probe", ModuleBinding::Global, Box::new(PanicHost));
        }
    }

    struct PanicHost;

    impl HostFunction for PanicHost {
        fn call(&self, _ctx: &mut dyn HostContext) -> HostCall {
            panic!("host panic probe");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn host_panic_pressure_fails_closed_without_worker_damage() {
        let runner = Arc::new(
            Runner::builder()
                .surface(surface_with_module(Arc::new(PanicModule)))
                .ambient(Ambient::production(0))
                .features(ExecutionFeatures::all_off())
                .ingress_limits(IngressLimits {
                    max_in_flight: usize::MAX,
                    max_in_flight_per_tenant: usize::MAX,
                })
                .lane_admission_limits(AdmissionLimits::unlimited())
                .max_source_bytes(256 * 1024)
                .limits(Limits {
                    gas: Some(1_000_000),
                    max_memory_bytes: Some(8 * 1024 * 1024),
                    ..Limits::unlimited()
                })
                .build()
                .expect("runner builds"),
        );

        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut panic_tasks = Vec::new();
        for _ in 0..8 {
            let runner = Arc::clone(&runner);
            panic_tasks.push(tokio::spawn(async move {
                runner
                    .run(Request::new(
                        b"--!nocheck\npanic_probe()\nreturn 99",
                        budget(Duration::from_secs(2)),
                    ))
                    .await
            }));
        }

        let ordinary = runner
            .run(Request::new(
                b"--!nocheck\nreturn 17",
                budget(Duration::from_secs(2)),
            ))
            .await
            .expect("ordinary request runs during host-panic pressure");
        assert_number(&ordinary.values, 17.0);

        for task in panic_tasks {
            let error = task
                .await
                .expect("host-panic task joins")
                .expect_err("host panic fails closed");
            assert!(
                matches!(error, RequestError::PanicPoison(_)),
                "got {error:?}"
            );
        }
        std::panic::set_hook(prev);

        let outcome = runner
            .run(Request::new(
                b"--!nocheck\nreturn 19",
                budget(Duration::from_secs(2)),
            ))
            .await
            .expect("runner remains healthy after host panic");
        assert_number(&outcome.values, 19.0);
    }
}

#[cfg(test)]
mod fail_closed_admission {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use ruau::{
        runner::{Budget, Request, RequestError, Runner},
        surface::Surface,
        vm::{Ambient, ExecutionFeatures, Limits},
        vm_api::{HostReturn, ModuleBinding, ModuleBuilder, NativeModule, OwnedValue},
    };
    use ruau_vm_api::{HostCall, HostContext, HostError, HostFunction};

    fn surface_with_module(module: Arc<dyn NativeModule>) -> Surface {
        Surface::builder()
            .module(module)
            .build()
            .expect("module surface validates")
    }

    struct SlowModule;

    impl NativeModule for SlowModule {
        fn name(&self) -> &'static str {
            "slow"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("declare function slow_probe(): number")
        }

        fn build(&self, builder: &mut dyn ModuleBuilder) {
            builder.function("slow_probe", ModuleBinding::Global, Box::new(SlowHost));
        }
    }

    struct SlowHost;

    impl HostFunction for SlowHost {
        fn call(&self, _ctx: &mut dyn HostContext) -> HostCall {
            HostCall::Pending(Box::pin(async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok::<HostReturn, HostError>(HostReturn {
                    values: vec![OwnedValue::Number(1.0)],
                })
            }))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unconfigured_admission_rejects_a_same_tenant_burst() {
        // No explicit ingress/lane admission: the builder's fail-closed
        // defaults (derived from the single lane) must bound a same-tenant
        // burst instead of admitting it unbounded.
        let runner = Arc::new(
            Runner::builder()
                .surface(surface_with_module(Arc::new(SlowModule)))
                .ambient(Ambient::production(0))
                .features(ExecutionFeatures::all_off())
                .max_source_bytes(64 * 1024)
                .limits(Limits {
                    gas: Some(10_000_000),
                    max_memory_bytes: Some(8 * 1024 * 1024),
                    ..Limits::unlimited()
                })
                .build()
                .expect("runner builds"),
        );

        let rejections = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let runner = Arc::clone(&runner);
            let rejections = Arc::clone(&rejections);
            tasks.push(tokio::spawn(async move {
                let budget = Budget::with_timeout(Duration::from_secs(5))
                    .expect("test budget has a future deadline");
                match runner
                    .run(Request::new(b"--!nocheck\nreturn slow_probe()", budget))
                    .await
                {
                    Ok(_) => {}
                    Err(RequestError::IngressRejected { .. }) => {
                        rejections.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(other) => panic!("unexpected error under burst: {other:?}"),
                }
            }));
        }
        for task in tasks {
            task.await.expect("burst task completes");
        }

        assert!(
            rejections.load(Ordering::Relaxed) > 0,
            "the fail-closed defaults bound a 16-deep same-tenant burst"
        );

        let budget = Budget::with_timeout(Duration::from_secs(5))
            .expect("test budget has a future deadline");
        let outcome = runner
            .run(Request::new(b"--!nocheck\nreturn 7", budget))
            .await
            .expect("the runner stays healthy after rejections");
        assert!(!outcome.values.is_empty());
    }
}
