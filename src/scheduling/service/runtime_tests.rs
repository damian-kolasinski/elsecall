//! The real scheduler driven by virtual boundary adapters, without service tasks.
use super::*;
use crate::{
    inspection::{LoadSample, Machine, MemoryPressure, ThermalState},
    scheduling::{
        clock::VirtualClock,
        messages::RunEnded,
        placed::Alone,
        pre_run_sync::{PreRunSync, Synced},
    },
};
use futures_util::future::BoxFuture;
use std::sync::atomic::{AtomicU64, Ordering};

struct MachineBoundary;
impl MachineInspector for MachineBoundary {
    fn inspect(&self) -> Result<Machine> {
        Err(Failure::new(
            "unavailable",
            "No host inspection in simulation.",
            "",
        ))
    }
    fn sample_load(&self) -> Result<LoadSample> {
        Ok(LoadSample {
            cores: CoreCounts {
                performance: 2,
                efficiency: 0,
            },
            cpu_utilization: Some(0.0),
            memory_pressure: MemoryPressure::Normal,
            thermal_state: ThermalState::Nominal,
        })
    }
}
struct SpeedBoundary;
impl SpeedMeter for SpeedBoundary {
    fn measure(&self) -> Result<f64> {
        panic!("controlled driving must never start the blocking speed meter")
    }
}
struct ProcessBoundary;
impl ProcessTable for ProcessBoundary {
    fn describe(&self, pid: u32) -> Option<ProcessInfo> {
        Some(ProcessInfo {
            identity: ProcessIdentity { pid, started_us: 1 },
            parent: 0,
        })
    }
    fn watch(&self, _: ProcessIdentity) -> bool {
        true
    }
    fn kill_group(&self, _: ProcessIdentity, _: u32) -> bool {
        false
    }
}
struct SyncBoundary;
impl PreRunSync for SyncBoundary {
    fn bring_up_to_date(&self, _: &str) -> BoxFuture<'static, Result<Synced>> {
        Box::pin(async { Ok(Synced::NothingSynced) })
    }
}
struct IdBoundary(AtomicU64);
impl RunIds for IdBoundary {
    fn next(&self) -> RunId {
        RunId::parse(&format!(
            "r-{:012x}",
            self.0.fetch_add(1, Ordering::Relaxed)
        ))
        .unwrap()
    }
}

fn request(size: u32, hold_ms: u64) -> AdmissionRequest {
    AdmissionRequest {
        workload: None,
        kind: Kind::Run,
        holder_pid: None,
        size: Some(Cores::from_millis(size)),
        label: Some("runtime probe".into()),
        placement: Placement::Pinned,
        urgency: Urgency::Interactive {
            hold_limit_ms: Some(hold_ms),
        },
        workspace: None,
        settings: None,
    }
}

#[tokio::test(start_paused = true)]
async fn controlled_runtime_uses_real_admission_timers_records_and_supplied_ids() {
    let home = tempfile::tempdir().unwrap();
    let clock = Arc::new(VirtualClock::new(Timestamp::from_millis(1_000)));
    let (_, exits) = mpsc::unbounded_channel();
    let (scheduler, _tasks) = Scheduler::start_with_runtime(
        &home.path().join("records.sqlite"),
        ServiceConfiguration::default(),
        Adapters {
            machine: Arc::new(MachineBoundary),
            speed: Arc::new(SpeedBoundary),
            processes: Arc::new(ProcessBoundary),
            exits,
        },
        Arc::new(Alone),
        Arc::new(SyncBoundary),
        Arc::new(LoadLog::open(&home.path().join("load.sqlite")).unwrap()),
        Runtime {
            clock: clock.clone(),
            ids: Arc::new(IdBoundary(AtomicU64::new(1))),
            driving: Driving::Controlled,
        },
    )
    .unwrap();
    scheduler.tick().unwrap();
    scheduler.observe_speed(2.0);
    let holding = scheduler.request(100, request(2_000, 5_000)).await.unwrap();
    assert_eq!(holding.run_id.as_str(), "r-000000000001");
    let rejected = scheduler
        .run_ended(
            100,
            &holding.run_id,
            RunEnded::PeerFailed {
                remote_failure_phase: None,
                failure: crate::remote_run::protocol::StreamFailure {
                    phase: crate::peer_diagnostics::Phase::Receive,
                    kind: crate::peer_diagnostics::Kind::Receive,
                    duration_ms: 1,
                },
            },
        )
        .unwrap_err();
    assert_eq!(rejected.code, "invalid_request");
    assert_eq!(scheduler.member_state().in_use, Cores::from_millis(2_000));
    // Outside pressure detection reads the same gate, and the active holder
    // and the service as the roots of Elsecall's own work.
    use crate::outside_pressure::GateSource;
    let observed = crate::outside_pressure_gate::SchedulerGate(scheduler.clone()).observe();
    assert_eq!(observed.at.millis(), 1_000);
    assert_eq!(observed.gate, Gate::Open);
    assert_eq!(observed.own.roots, [100, std::process::id()]);
    assert_eq!(
        observed.own.granted_since,
        Some(Timestamp::from_millis(1_000))
    );
    let waiting = tokio::spawn({
        let scheduler = scheduler.clone();
        async move { scheduler.request(101, request(1_000, 750)).await }
    });
    while scheduler.member_state().queued.interactive != 1 {
        tokio::task::yield_now().await;
    }
    let waiting_id = RunId::parse("r-000000000002").unwrap();
    assert!(scheduler.is_awaiting_verdict(&waiting_id));
    assert!(!scheduler.has_immediate_work());
    assert_eq!(scheduler.next_event_at().unwrap().millis(), 1_750);
    tokio::time::advance(Duration::from_millis(750)).await;
    let refused = waiting.await.unwrap().unwrap_err();
    assert_eq!(refused.code, "fleet_busy");
    assert!(!scheduler.is_awaiting_verdict(&waiting_id));
    assert!(scheduler.next_event_at().is_none());
    let id = RunId::parse("r-000000000002").unwrap();
    let record = scheduler.record(&id).unwrap();
    assert_eq!(record.requested_at.millis(), 1_000);
    assert_eq!(record.ended_at.unwrap().millis(), 1_750);
    assert_eq!(record.decision.unwrap().decided_at.millis(), 1_750);
    scheduler
        .run_ended(
            100,
            &holding.run_id,
            RunEnded::Exited {
                plan_observation: None,
                exit_code: 0,
                cpu_ms: None,
                run_phases: None,
            },
        )
        .unwrap();
    assert_eq!(scheduler.member_state().in_use, Cores::ZERO);
    assert_eq!(scheduler.record(&holding.run_id).unwrap().run_ms, Some(750));
    assert!(scheduler.own_load().holders.is_empty(), "the grant ended");
    assert_eq!(clock.now().millis(), 1_750);
}

#[tokio::test(start_paused = true)]
async fn rehearsal_defers_internal_work_extends_deadlines_and_preserves_production_guard() {
    let home = tempfile::tempdir().unwrap();
    let clock = Arc::new(VirtualClock::new(Timestamp::from_millis(1_000)));
    let (_, exits) = mpsc::unbounded_channel();
    let (scheduler, _tasks) = Scheduler::start_with_runtime(
        &home.path().join("records.sqlite"),
        ServiceConfiguration::default(),
        Adapters {
            machine: Arc::new(MachineBoundary),
            speed: Arc::new(SpeedBoundary),
            processes: Arc::new(ProcessBoundary),
            exits,
        },
        Arc::new(Alone),
        Arc::new(SyncBoundary),
        Arc::new(LoadLog::open(&home.path().join("load.sqlite")).unwrap()),
        Runtime {
            clock: clock.clone(),
            ids: Arc::new(IdBoundary(AtomicU64::new(1))),
            driving: Driving::Controlled,
        },
    )
    .unwrap();
    scheduler.tick().unwrap();
    let pause = scheduler.pause_internal().unwrap();
    let mut jobs = Vec::new();
    for label in [
        "elsecall housekeeping",
        "elsecall fidelity drift",
        "elsecall future internal task",
    ] {
        let scheduler = scheduler.clone();
        let mut r = request(500, 1_000);
        r.label = Some(label.into());
        r.urgency = Urgency::Deferrable {
            deadline: clock.now().after(Duration::from_secs(1)),
            at_deadline: AtDeadline::Skip,
        };
        jobs.push(tokio::spawn(async move {
            scheduler.request(std::process::id(), r).await
        }));
    }
    while scheduler.book().entries.len() < 3 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(3)).await;
    scheduler.tick().unwrap();
    assert_eq!(
        scheduler
            .production_activity(Timestamp::from_millis(1_000))
            .unwrap(),
        (0, 0)
    );
    assert!(jobs.iter().all(|j| !j.is_finished()));
    // A consumer cannot bypass safety by borrowing the service's label.
    let mut external = request(500, 1_000);
    external.label = Some("elsecall housekeeping".into());
    let production = scheduler.request(100, external).await.unwrap();
    assert_eq!(
        scheduler
            .production_activity(Timestamp::from_millis(1_000))
            .unwrap(),
        (1, 1)
    );
    scheduler
        .run_ended(
            100,
            &production.run_id,
            RunEnded::Exited {
                plan_observation: None,
                exit_code: 0,
                cpu_ms: None,
                run_phases: None,
            },
        )
        .unwrap();
    drop(pause);
    for job in jobs {
        let grant = job.await.unwrap().unwrap();
        let record = scheduler.record(&grant.run_id).unwrap();
        assert!(record.internal);
        assert!(
            record
                .warnings
                .iter()
                .any(|w| w.contains("extended by 3000 ms"))
        );
        assert_eq!(
            record.urgency,
            Urgency::Deferrable {
                deadline: Timestamp::from_millis(5_000),
                at_deadline: AtDeadline::Skip
            }
        );
        scheduler
            .run_ended(
                std::process::id(),
                &grant.run_id,
                RunEnded::Exited {
                    plan_observation: None,
                    exit_code: 0,
                    cpu_ms: None,
                    run_phases: None,
                },
            )
            .unwrap();
    }
    assert_eq!(
        scheduler
            .production_activity(Timestamp::from_millis(1_000))
            .unwrap(),
        (0, 1)
    );
}

#[tokio::test(start_paused = true)]
async fn promoted_internal_deadline_keeps_priority_after_pause() {
    let home = tempfile::tempdir().unwrap();
    let clock = Arc::new(VirtualClock::new(Timestamp::from_millis(1000)));
    let (_, exits) = mpsc::unbounded_channel();
    let (scheduler, _tasks) = Scheduler::start_with_runtime(
        &home.path().join("records.sqlite"),
        ServiceConfiguration::default(),
        Adapters {
            machine: Arc::new(MachineBoundary),
            speed: Arc::new(SpeedBoundary),
            processes: Arc::new(ProcessBoundary),
            exits,
        },
        Arc::new(Alone),
        Arc::new(SyncBoundary),
        Arc::new(LoadLog::open(&home.path().join("load.sqlite")).unwrap()),
        Runtime {
            clock: clock.clone(),
            ids: Arc::new(IdBoundary(AtomicU64::new(1))),
            driving: Driving::Controlled,
        },
    )
    .unwrap();
    scheduler.tick().unwrap();
    let holding = scheduler.request(100, request(2000, 10000)).await.unwrap();
    let mut internal = request(2000, 1000);
    internal.label = Some("elsecall housekeeping".into());
    internal.urgency = Urgency::Deferrable {
        deadline: clock.now().after(Duration::from_secs(1)),
        at_deadline: AtDeadline::Start,
    };
    let job = tokio::spawn({
        let s = scheduler.clone();
        async move { s.request(std::process::id(), internal).await }
    });
    while scheduler.member_state().queued.deferrable != 1 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    scheduler.tick().unwrap();
    let pause = scheduler.pause_internal().unwrap();
    let consumer = tokio::spawn({
        let s = scheduler.clone();
        async move { s.request(101, request(2000, 10000)).await }
    });
    while scheduler.member_state().queued.interactive != 1 {
        tokio::task::yield_now().await;
    }
    drop(pause);
    scheduler
        .run_ended(
            100,
            &holding.run_id,
            RunEnded::Exited {
                plan_observation: None,
                exit_code: 0,
                cpu_ms: None,
                run_phases: None,
            },
        )
        .unwrap();
    tokio::task::yield_now().await;
    assert!(
        job.is_finished(),
        "promoted internal work must keep deadline priority"
    );
    assert!(!consumer.is_finished());
    let grant = job.await.unwrap().unwrap();
    scheduler
        .run_ended(
            std::process::id(),
            &grant.run_id,
            RunEnded::Exited {
                plan_observation: None,
                exit_code: 0,
                cpu_ms: None,
                run_phases: None,
            },
        )
        .unwrap();
    let grant = consumer.await.unwrap().unwrap();
    scheduler
        .run_ended(
            101,
            &grant.run_id,
            RunEnded::Exited {
                plan_observation: None,
                exit_code: 0,
                cpu_ms: None,
                run_phases: None,
            },
        )
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn draining_active_internal_work_respects_short_preparation_limit_and_never_stops_it() {
    let home = tempfile::tempdir().unwrap();
    let clock = Arc::new(VirtualClock::new(Timestamp::from_millis(1000)));
    let (_, exits) = mpsc::unbounded_channel();
    let (scheduler, _tasks) = Scheduler::start_with_runtime(
        &home.path().join("records.sqlite"),
        ServiceConfiguration::default(),
        Adapters {
            machine: Arc::new(MachineBoundary),
            speed: Arc::new(SpeedBoundary),
            processes: Arc::new(ProcessBoundary),
            exits,
        },
        Arc::new(Alone),
        Arc::new(SyncBoundary),
        Arc::new(LoadLog::open(&home.path().join("load.sqlite")).unwrap()),
        Runtime {
            clock,
            ids: Arc::new(IdBoundary(AtomicU64::new(1))),
            driving: Driving::Controlled,
        },
    )
    .unwrap();
    scheduler.tick().unwrap();
    let mut r = request(1000, 10000);
    r.label = Some("elsecall long internal pass".into());
    let holding = scheduler.request(std::process::id(), r).await.unwrap();
    let pause = scheduler.pause_internal().unwrap();
    let started = tokio::time::Instant::now();
    assert!(
        scheduler
            .drain_internal(started + Duration::from_millis(100))
            .await
            .is_err()
    );
    assert_eq!(started.elapsed(), Duration::from_millis(100));
    assert!(scheduler.internal_active());
    drop(pause);
    scheduler
        .run_ended(
            std::process::id(),
            &holding.run_id,
            RunEnded::Exited {
                plan_observation: None,
                exit_code: 0,
                cpu_ms: None,
                run_phases: None,
            },
        )
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn workload_reports_obey_pending_nested_requester_barriers_and_pid_identity() {
    use crate::{
        scheduling::{binding::fake::FakeProcesses, messages::CommandStarted},
        workload_metadata::{Observation, Workload},
    };
    let home = tempfile::tempdir().unwrap();
    let clock = Arc::new(VirtualClock::new(Timestamp::from_millis(1_000)));
    let processes = Arc::new(FakeProcesses::default());
    processes.start(100, 1, 0); // outer requester
    processes.start(101, 1, 100); // outer command
    processes.start(102, 1, 101); // nested requester
    processes.start(103, 1, 102); // nested command, not bound yet
    processes.start(104, 1, 103); // observer
    let (_, exits) = mpsc::unbounded_channel();
    let (scheduler, _tasks) = Scheduler::start_with_runtime(
        &home.path().join("records.sqlite"),
        ServiceConfiguration::default(),
        Adapters {
            machine: Arc::new(MachineBoundary),
            speed: Arc::new(SpeedBoundary),
            processes: processes.clone(),
            exits,
        },
        Arc::new(Alone),
        Arc::new(SyncBoundary),
        Arc::new(LoadLog::open(&home.path().join("load.sqlite")).unwrap()),
        Runtime {
            clock,
            ids: Arc::new(IdBoundary(AtomicU64::new(1))),
            driving: Driving::Controlled,
        },
    )
    .unwrap();
    scheduler.tick().unwrap();
    let mut declared = request(1000, 5000);
    declared.workload = Some(Workload::Opaque {});
    let outer = scheduler.request(100, declared.clone()).await.unwrap();
    scheduler
        .command_started(100, &outer.run_id, CommandStarted { pid: 101 })
        .unwrap();
    let inner = scheduler.request(102, declared).await.unwrap();
    assert_eq!(inner.nested_in, Some(outer.run_id.clone()));
    let before = scheduler.record(&outer.run_id).unwrap();
    for id in [&outer.run_id, &inner.run_id] {
        assert!(
            scheduler
                .observe_workload(104, id, Observation::Unknown {})
                .is_err()
        );
    }
    assert_eq!(before, scheduler.record(&outer.run_id).unwrap());
    scheduler
        .command_started(102, &inner.run_id, CommandStarted { pid: 103 })
        .unwrap();
    assert!(
        scheduler
            .observe_workload(104, &outer.run_id, Observation::Unknown {})
            .is_err()
    );
    scheduler
        .observe_workload(104, &inner.run_id, Observation::Unknown {})
        .unwrap();
    assert!(
        scheduler
            .observe_workload(104, &inner.run_id, Observation::Unknown {})
            .is_err()
    );
    // A PID reused by a different process cannot reuse the command binding.
    processes.start(103, 2, 102);
    assert!(
        scheduler
            .observe_workload(104, &inner.run_id, Observation::Unknown {})
            .is_err()
    );
    assert!(
        scheduler
            .record(&outer.run_id)
            .unwrap()
            .plan_observation
            .is_none()
    );
    // A queued requester is also a barrier when the boundary later discovers
    // its ancestry. Queued state must not authorize a fall-through report.
    processes.start(200, 1, 0);
    processes.start(201, 1, 200);
    let queued = tokio::spawn({
        let scheduler = scheduler.clone();
        async move {
            let mut declared = request(2000, 5000);
            declared.workload = Some(Workload::Opaque {});
            scheduler.request(200, declared).await
        }
    });
    while scheduler.member_state().queued.interactive == 0 {
        tokio::task::yield_now().await;
    }
    processes.start(200, 1, 101);
    assert!(
        scheduler
            .observe_workload(201, &outer.run_id, Observation::Unknown {})
            .is_err()
    );
    processes.end(200);
    scheduler.process_exited(200);
    assert!(queued.await.unwrap().is_err());
    let queued_id = RunId::parse("r-000000000003").unwrap();
    assert_eq!(
        scheduler.record(&queued_id).unwrap().plan_observation,
        Some(Observation::Unknown {})
    );
}

#[tokio::test(start_paused = true)]
async fn workload_metadata_does_not_change_learned_size_hold_or_admission_decisions() {
    use crate::workload_metadata::parse_workload;
    let mut decisions = Vec::new();
    for structured in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let clock = Arc::new(VirtualClock::new(Timestamp::from_millis(1_000)));
        let (_, exits) = mpsc::unbounded_channel();
        let (scheduler, _tasks) = Scheduler::start_with_runtime(
            &home.path().join("records.sqlite"),
            ServiceConfiguration::default(),
            Adapters {
                machine: Arc::new(MachineBoundary),
                speed: Arc::new(SpeedBoundary),
                processes: Arc::new(ProcessBoundary),
                exits,
            },
            Arc::new(Alone),
            Arc::new(SyncBoundary),
            Arc::new(LoadLog::open(&home.path().join("load.sqlite")).unwrap()),
            Runtime {
                clock: clock.clone(),
                ids: Arc::new(IdBoundary(AtomicU64::new(1))),
                driving: Driving::Controlled,
            },
        )
        .unwrap();
        scheduler.tick().unwrap();
        scheduler.observe_speed(1.0);
        for n in 0..4 {
            let mut r = request(1000, 5000);
            r.size = None;
            r.urgency = Urgency::Interactive {
                hold_limit_ms: None,
            };
            if structured {
                r.workload=Some(parse_workload(&serde_json::json!({"kind":"sequential","planned_units":100,"stop_policy":"continue_all","estimate_key":"a".repeat(64),"variant_key":"b".repeat(64)}).to_string()).unwrap());
            }
            let grant = scheduler.request(100, r).await.unwrap();
            if n == 3 {
                decisions.push(scheduler.record(&grant.run_id).unwrap().decision);
            }
            tokio::time::advance(Duration::from_millis(1000)).await;
            scheduler
                .run_ended(
                    100,
                    &grant.run_id,
                    RunEnded::Exited {
                        exit_code: 1,
                        cpu_ms: Some(1000),
                        run_phases: None,
                        plan_observation: None,
                    },
                )
                .unwrap();
        }
    }
    assert_eq!(decisions[0], decisions[1]);
    assert!(
        decisions[0].as_ref().unwrap().candidates[0]
            .expected_run_seconds
            .is_some()
    );
}

#[tokio::test(start_paused = true)]
async fn structured_placement_requires_acknowledgement_without_retry_or_attach() {
    use crate::scheduling::{
        messages::MemberState,
        placed::{Members, OfferReply, PlacementOffer, Standing, Warmth, WorkspaceRef},
        quote::{Quote, QuoteRequest},
    };
    struct Peers {
        ack: Option<u32>,
        offers: Arc<Mutex<Vec<PlacementOffer>>>,
    }
    impl Members for Peers {
        fn standing(&self) -> Standing {
            Standing {
                name: Some("caller".into()),
                master: Some("caller".into()),
                members: vec!["caller".into(), "peer".into()],
            }
        }
        fn offer(&self, _: &str, offer: &PlacementOffer) -> BoxFuture<'static, Result<OfferReply>> {
            self.offers.lock().unwrap().push(offer.clone());
            let ack = self.ack;
            Box::pin(async move {
                Ok(OfferReply::Accepted {
                    workload_contract: ack,
                    state: MemberState {
                        member: "peer".into(),
                        capacity: Capacity::of(
                            CoreCounts {
                                performance: 2,
                                efficiency: 0,
                            },
                            Cores::ZERO,
                        ),
                        in_use: Cores::ONE,
                        free: Cores::ONE,
                        queued: Default::default(),
                        load: None,
                        gate: Gate::Open,
                        speed: None,
                    },
                    copy: "/copy".into(),
                    location: "mapped".into(),
                    warmth: Some(Warmth::Warm),
                })
            })
        }
        fn quote(&self, _: &str, _: &QuoteRequest) -> BoxFuture<'static, Result<Quote>> {
            Box::pin(async { panic!("required-member admission does not quote") })
        }
    }
    for ack in [None, Some(99), Some(crate::workload_metadata::VERSION)] {
        let home = tempfile::tempdir().unwrap();
        let offers = Arc::new(Mutex::new(Vec::new()));
        let (_, exits) = mpsc::unbounded_channel();
        let (scheduler, _tasks) = Scheduler::start_with_runtime(
            &home.path().join("records.sqlite"),
            ServiceConfiguration::default(),
            Adapters {
                machine: Arc::new(MachineBoundary),
                speed: Arc::new(SpeedBoundary),
                processes: Arc::new(ProcessBoundary),
                exits,
            },
            Arc::new(Peers {
                ack,
                offers: offers.clone(),
            }),
            Arc::new(SyncBoundary),
            Arc::new(LoadLog::open(&home.path().join("load.sqlite")).unwrap()),
            Runtime {
                clock: Arc::new(VirtualClock::new(Timestamp::from_millis(1000))),
                ids: Arc::new(IdBoundary(AtomicU64::new(1))),
                driving: Driving::Controlled,
            },
        )
        .unwrap();
        scheduler.tick().unwrap();
        let mut declared = request(1000, 5000);
        declared.placement = Placement::Member("peer".into());
        declared.workspace = Some(WorkspaceRef {
            root: "/repo".into(),
            common_dir: "/repo/.git".into(),
        });
        declared.workload = Some(crate::workload_metadata::Workload::Opaque {});
        let result = scheduler.request(100, declared).await;
        let id = RunId::parse("r-000000000001").unwrap();
        assert_eq!(
            offers.lock().unwrap().len(),
            1,
            "no metadata stripping or admission retry"
        );
        assert!(offers.lock().unwrap()[0].workload.is_some());
        if ack == Some(crate::workload_metadata::VERSION) {
            assert!(result.is_ok());
            assert_eq!(scheduler.placed_member(100, &id).unwrap(), "peer");
        } else {
            assert_eq!(result.unwrap_err().code, "workload_contract_unsupported");
            assert!(
                scheduler.placed_member(100, &id).is_err(),
                "no attach permitted"
            );
            assert_eq!(
                scheduler.record(&id).unwrap().outcome,
                Some(Outcome::Failed {
                    code: "workload_contract_unsupported".into()
                })
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_peer_that_lost_its_placed_runs_keeps_short_work_and_not_long_work() {
    use crate::scheduling::{
        messages::MemberState,
        placed::{Members, OfferReply, PlacementOffer, Standing, Warmth, WorkspaceRef},
        quote::{CopyQuote, Quote, QuoteRequest},
        record::Record,
        records::Records,
    };
    /// A peer twice as fast as the caller, with a warm copy of `/repo`.
    struct Peer;
    fn state() -> MemberState {
        MemberState {
            member: "peer".into(),
            capacity: Capacity::of(
                CoreCounts {
                    performance: 2,
                    efficiency: 0,
                },
                Cores::ZERO,
            ),
            in_use: Cores::ZERO,
            free: Cores::from_millis(2000),
            queued: Default::default(),
            load: None,
            gate: Gate::Open,
            speed: Some(2.0),
        }
    }
    impl Members for Peer {
        fn standing(&self) -> Standing {
            Standing {
                name: Some("caller".into()),
                master: Some("caller".into()),
                members: vec!["caller".into(), "peer".into()],
            }
        }
        fn offer(&self, _: &str, _: &PlacementOffer) -> BoxFuture<'static, Result<OfferReply>> {
            Box::pin(async {
                Ok(OfferReply::Accepted {
                    workload_contract: None,
                    state: state(),
                    copy: "/copy".into(),
                    location: "mapped".into(),
                    warmth: Some(Warmth::Warm),
                })
            })
        }
        fn quote(&self, _: &str, _: &QuoteRequest) -> BoxFuture<'static, Result<Quote>> {
            Box::pin(async {
                Ok(Quote {
                    state: state(),
                    utilization: Some(0.0),
                    holding: Vec::new(),
                    waiting: Vec::new(),
                    copies: vec![CopyQuote {
                        workspace: "/repo".into(),
                        warmth: Warmth::Warm,
                        busy: false,
                        unavailable: false,
                    }],
                    admission: None,
                })
            })
        }
    }
    let now = Timestamp::parse_rfc3339("2026-11-10T00:00:00Z").unwrap();
    let minute = 60_000;
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("records.sqlite");
    {
        let records = Records::open(&path).unwrap();
        let record =
            |n: u64, label: &str, run_ms: u64, outcome: serde_json::Value, placed: bool| {
                let mut record: Record = serde_json::from_value(serde_json::json!({
                    "run_id": format!("r-{:012x}", 0x1000 + n), "kind": "run", "label": label,
                    "placement": "placeable", "urgency": {"interactive": {"hold_limit_ms": null}},
                    "size": 1.0, "calling_member": "caller", "workspace_recorded": true,
                    "member": if placed { "peer" } else { "caller" }, "state": "ended",
                    "outcome": outcome, "requested_at": "2026-11-09T00:00:00Z", "run_ms": run_ms,
                    "requested_by": {"pid": 1, "started_us": 1}, "processes": [], "reacquired": 0,
                "warnings": [],
                    "placed": placed.then(|| serde_json::json!({
                        "side": "caller", "calling_member": "caller", "member": "peer",
                        "caller_workspace": "/repo", "reached": "started"})),
                }))
                .unwrap();
                record.requested_at = Timestamp::from_millis(now.millis() - (n + 10) * minute);
                record
            };
        let exited = serde_json::json!({"kind": "exited", "exit_code": 0});
        // 20 placed runs on the peer lost their connection within 10 hours of
        // placed work there: two an hour.
        for n in 0..20 {
            let lost = serde_json::json!({"kind": "failed", "code": "connection_lost"});
            records
                .save(&record(n, "other", 30 * minute, lost, true))
                .unwrap();
        }
        // Here, "long job" takes 2 hours and "short job" a minute.
        for n in 20..23 {
            records
                .save(&record(n, "long job", 120 * minute, exited.clone(), false))
                .unwrap();
            records
                .save(&record(n + 3, "short job", minute, exited.clone(), false))
                .unwrap();
        }
    }
    let (_, exits) = mpsc::unbounded_channel();
    let (scheduler, _tasks) = Scheduler::start_with_runtime(
        &path,
        ServiceConfiguration::default(),
        Adapters {
            machine: Arc::new(MachineBoundary),
            speed: Arc::new(SpeedBoundary),
            processes: Arc::new(ProcessBoundary),
            exits,
        },
        Arc::new(Peer),
        Arc::new(SyncBoundary),
        Arc::new(LoadLog::open(&home.path().join("load.sqlite")).unwrap()),
        Runtime {
            clock: Arc::new(VirtualClock::new(now)),
            ids: Arc::new(IdBoundary(AtomicU64::new(1))),
            driving: Driving::Controlled,
        },
    )
    .unwrap();
    scheduler.tick().unwrap();
    scheduler.observe_speed(1.0);
    let placeable = |label: &str| {
        let mut r = request(1000, 600_000);
        r.label = Some(label.into());
        r.placement = Placement::Placeable;
        r.workspace = Some(WorkspaceRef {
            root: "/repo".into(),
            common_dir: "/repo/.git".into(),
        });
        r
    };
    let peer_estimate = |id: &RunId| {
        let decision = scheduler.record(id).unwrap().decision.unwrap();
        let peer = decision
            .candidates
            .iter()
            .find(|c| c.member == "peer")
            .unwrap()
            .clone();
        let winner = decision
            .candidates
            .iter()
            .find(|c| c.chosen)
            .map(|c| c.member.clone());
        (winner, peer)
    };

    // A minute's work goes to the faster peer; its losses add under 1%.
    let short = scheduler
        .request(100, placeable("short job"))
        .await
        .unwrap();
    let (winner, peer) = peer_estimate(&short.run_id);
    assert_eq!(winner.as_deref(), Some("peer"), "{}", peer.why);
    assert!(
        peer.estimate.as_ref().unwrap().loss_factor.unwrap() < 1.01,
        "{}",
        peer.why
    );

    // Two hours of work stays here: an hour there, but over three hours with
    // the runs it loses.
    let long = scheduler.request(100, placeable("long job")).await.unwrap();
    let (winner, peer) = peer_estimate(&long.run_id);
    assert_eq!(winner.as_deref(), Some("caller"), "{}", peer.why);
    assert_eq!(
        serde_json::to_value(peer.inputs.as_ref().unwrap().learned.losses).unwrap(),
        serde_json::json!({"lost": 20, "run_ms": 36_000_000})
    );
    let factor = peer.estimate.as_ref().unwrap().loss_factor.unwrap();
    assert!((3.1..3.3).contains(&factor), "{factor}: {}", peer.why);
    assert!(
        peer.why
            .contains("for 20 lost of its placed runs in 10.0 h of them"),
        "{}",
        peer.why
    );
}

#[tokio::test(start_paused = true)]
async fn held_internal_work_does_not_start_when_room_frees_and_starts_once_released() {
    let home = tempfile::tempdir().unwrap();
    let clock = Arc::new(VirtualClock::new(Timestamp::from_millis(1000)));
    let (_, exits) = mpsc::unbounded_channel();
    let (scheduler, _tasks) = Scheduler::start_with_runtime(
        &home.path().join("records.sqlite"),
        ServiceConfiguration::default(),
        Adapters {
            machine: Arc::new(MachineBoundary),
            speed: Arc::new(SpeedBoundary),
            processes: Arc::new(ProcessBoundary),
            exits,
        },
        Arc::new(Alone),
        Arc::new(SyncBoundary),
        Arc::new(LoadLog::open(&home.path().join("load.sqlite")).unwrap()),
        Runtime {
            clock: clock.clone(),
            ids: Arc::new(IdBoundary(AtomicU64::new(1))),
            driving: Driving::Controlled,
        },
    )
    .unwrap();
    scheduler.tick().unwrap();
    let ended = || RunEnded::Exited {
        plan_observation: None,
        exit_code: 0,
        cpu_ms: None,
        run_phases: None,
    };
    let internal = |label: &str| {
        let mut r = request(2000, 1000);
        r.label = Some(label.into());
        r.urgency = Urgency::Deferrable {
            deadline: clock.now().after(Duration::from_secs(60)),
            at_deadline: AtDeadline::Skip,
        };
        r
    };
    let holding = scheduler.request(100, request(2000, 10000)).await.unwrap();
    let housekeeping = tokio::spawn({
        let (s, r) = (scheduler.clone(), internal("elsecall housekeeping"));
        async move { s.request(std::process::id(), r).await }
    });
    while scheduler.in_flight().len() != 2 {
        tokio::task::yield_now().await;
    }

    // Held, queued internal work stays out of admission, and so does internal
    // work asked for meanwhile; an operator's request is admitted as usual.
    let hold = scheduler.hold_internal();
    let drift = tokio::spawn({
        let (s, r) = (scheduler.clone(), internal("elsecall fidelity drift"));
        async move { s.request(std::process::id(), r).await }
    });
    while scheduler.in_flight().len() != 3 {
        tokio::task::yield_now().await;
    }
    scheduler.run_ended(100, &holding.run_id, ended()).unwrap();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(!housekeeping.is_finished() && !drift.is_finished());
    assert!(
        scheduler
            .in_flight()
            .iter()
            .all(|w| w.internal && !w.running)
    );
    let operator = scheduler.request(101, request(2000, 10000)).await.unwrap();
    scheduler.run_ended(101, &operator.run_id, ended()).unwrap();
    assert!(!housekeeping.is_finished() && !drift.is_finished());

    // Released, it starts in the order it was asked for.
    drop(hold);
    let grant = housekeeping.await.unwrap().unwrap();
    let queued = |label: &str| {
        scheduler
            .in_flight()
            .iter()
            .any(|w| w.label.as_deref() == Some(label) && !w.running)
    };
    assert!(queued("elsecall fidelity drift"));
    scheduler
        .run_ended(std::process::id(), &grant.run_id, ended())
        .unwrap();
    let grant = drift.await.unwrap().unwrap();
    scheduler
        .run_ended(std::process::id(), &grant.run_id, ended())
        .unwrap();

    // Held and paused for a rehearsal at once, in either order, the work starts
    // only once both end.
    for hold_first in [true, false] {
        let hold = scheduler.hold_internal();
        let pause = scheduler.pause_internal().unwrap();
        let pass = tokio::spawn({
            let (s, r) = (scheduler.clone(), internal("elsecall housekeeping"));
            async move { s.request(std::process::id(), r).await }
        });
        while !queued("elsecall housekeeping") {
            tokio::task::yield_now().await;
        }
        let last: Box<dyn Send> = if hold_first {
            drop(hold);
            Box::new(pause)
        } else {
            drop(pause);
            Box::new(hold)
        };
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(queued("elsecall housekeeping"), "hold first: {hold_first}");
        drop(last);
        let grant = pass.await.unwrap().unwrap();
        scheduler
            .run_ended(std::process::id(), &grant.run_id, ended())
            .unwrap();
    }
}
