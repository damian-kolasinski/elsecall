//! Replay against records made the way the service makes them: scored by
//! `placement::decide`, recorded by `Candidate::scored` and `Arrival::of`, and
//! read back from JSON, in each generation of the record format.
use super::*;
use crate::{
    inspection::{CoreCounts, LoadSample, MemoryPressure, ThermalState},
    scheduling::{
        admission::QueueCounts,
        capacity::{Capacity, Cores},
        learning::Conversions,
        load,
        messages::MemberState,
        parameters::{Admission, Parameters, Placement as PlacementParameters},
        placed::Warmth,
        placement::{
            self, Ahead, Copy, Expectation, Held, Learned, LearnedRun, LearnedTime, Level,
            Observation, Source, Wanted,
        },
        processes::ProcessIdentity,
        record::{Arrival, Candidate, Decision, Outcome, Scoring, State, Verdict},
        request::{Kind, Placement, Urgency},
        run_id::RunId,
        timestamp::Timestamp,
    },
};
use std::time::Duration;

fn cores(n: f64) -> Cores {
    Cores::from_f64(n).unwrap()
}

/// A ten-core member whose gate is what its own service derives from its load
/// under the default thresholds.
fn member(
    name: &str,
    caller: bool,
    speed: Option<f64>,
    copy: Copy,
    utilization: f64,
) -> Observation {
    let capacity = Capacity::of(
        CoreCounts {
            performance: 10,
            efficiency: 0,
        },
        Cores::ZERO,
    );
    let mut observation = Observation {
        state: MemberState {
            member: name.into(),
            capacity,
            in_use: Cores::ZERO,
            free: capacity.capacity,
            queued: QueueCounts::default(),
            load: Some(LoadSample {
                cores: CoreCounts {
                    performance: 10,
                    efficiency: 0,
                },
                cpu_utilization: Some(utilization),
                memory_pressure: MemoryPressure::Normal,
                thermal_state: ThermalState::Nominal,
            }),
            gate: load::Gate::Open,
            speed,
        },
        caller,
        utilization: Some(utilization),
        holding: Vec::new(),
        ahead: Vec::new(),
        copy,
        learned: Learned::default(),
        admission: Some(Admission::default()),
    };
    regate(&mut observation);
    observation
}

fn regate(observation: &mut Observation) {
    let state = &observation.state;
    observation.state.gate = load::gate_of(
        state.load.as_ref(),
        observation.utilization,
        state.in_use,
        state.capacity.weighted_cores,
        &Admission::default(),
    );
}

fn remote(warmth: Warmth) -> Copy {
    Copy::Remote {
        warmth,
        busy: false,
    }
}

/// A warm devbox at speed 1.2 completes in 50 s + 3 s against 60 s on the
/// caller: it wins by default, by more than the 6 s margin.
fn warm_faster() -> Vec<Observation> {
    vec![
        member("mini", true, Some(1.0), Copy::Caller, 0.0),
        member("devbox", false, Some(1.2), remote(Warmth::Warm), 0.0),
    ]
}

/// The caller is saturated by outside load (95%), so the work goes to a cold
/// devbox: 120 s sync + 180 s cold run.
fn saturated_caller() -> Vec<Observation> {
    vec![
        member("mini", true, Some(1.0), Copy::Caller, 0.95),
        member("devbox", false, Some(1.0), remote(Warmth::Cold), 0.0),
    ]
}

/// Both members are saturated: no member can take the work.
fn both_saturated() -> Vec<Observation> {
    vec![
        member("mini", true, Some(1.0), Copy::Caller, 0.95),
        member("devbox", false, Some(1.0), remote(Warmth::Cold), 0.96),
    ]
}

/// The faster warm devbox is under memory pressure at warning level.
fn devbox_under_pressure() -> Vec<Observation> {
    let mut observations = warm_faster();
    if let Some(load) = &mut observations[1].state.load {
        load.memory_pressure = MemoryPressure::Warning;
    }
    regate(&mut observations[1]);
    observations
}

/// Everything else scoring reads: running and queued work, learned history in
/// sub-millisecond durations, an unmeasured member, a member with a copy it
/// cannot host, and contention.
fn busy_and_learned() -> Vec<Observation> {
    let mut caller = member("mini", true, Some(0.9137), Copy::Caller, 0.6180339887498949);
    caller.state.in_use = cores(7.0);
    caller.state.free = cores(3.0);
    caller.holding = vec![
        held(cores(4.5), Some(Duration::from_nanos(12_345_678_901))),
        held(cores(2.5), None),
    ];
    caller.ahead = vec![Ahead {
        size: cores(1.5),
        run: Some(Duration::from_nanos(3_000_000_499)),
    }];
    caller.learned.warm_run = Some(LearnedTime {
        time: Duration::from_nanos(9_876_543_210),
        source: Source::MemberRuns { samples: 4 },
    });
    regate(&mut caller);
    let mut devbox = member(
        "devbox",
        false,
        Some(0.7234),
        remote(Warmth::Warm),
        0.31415926535,
    );
    devbox.learned = Learned {
        warm_run: Some(LearnedTime {
            time: Duration::from_nanos(7_123_456_789),
            source: Source::FleetRuns {
                samples: 3,
                conversions: Conversions {
                    learned: 2,
                    measured: 0,
                },
            },
        }),
        cold_run: None,
        sync: Some(LearnedTime {
            time: Duration::from_nanos(412_345_678),
            source: Source::WorkspaceSyncs { samples: 3 },
        }),
        losses: Some(crate::scheduling::loss_history::Losses {
            lost: 3,
            run: Duration::from_millis(7_654_321),
        }),
    };
    let mut laptop = member("laptop", false, None, remote(Warmth::ColdWithHistory), 0.1);
    laptop.admission = None;
    // Estimated from the label's family, standing in for its one run.
    laptop.learned.warm_run = Some(LearnedTime {
        time: Duration::from_nanos(21_000_000_333),
        source: Source::FleetFamilyRuns {
            samples: 5,
            label_samples: 1,
            conversions: Conversions {
                learned: 3,
                measured: 3,
            },
        },
    });
    let other = member("other", false, Some(1.3), Copy::Unavailable, 0.0);
    vec![caller, devbox, laptop, other]
}

/// The caller is full: 9 cores held by work without an estimate, 644 s in, and
/// 1 core by work whose family history expects 300 s, 100 s in. It frees 2
/// cores after 322 s (half the elapsed time), so the cold devbox (30 s sync +
/// 180 s cold run) completes first; with the assumption fixed at 60 s, the
/// caller (60 s + 60 s) would.
fn behind_long_running_work() -> Vec<Observation> {
    let mut caller = member("mini", true, Some(1.0), Copy::Caller, 0.0);
    caller.state.in_use = cores(10.0);
    caller.state.free = Cores::ZERO;
    caller.holding = vec![
        Held {
            size: cores(9.0),
            elapsed: Some(Duration::from_nanos(644_000_000_499)),
            expectation: Expectation::Unknown,
        },
        Held {
            size: cores(1.0),
            elapsed: Some(Duration::from_millis(100_000)),
            expectation: Expectation::Learned(LearnedRun {
                learned: LearnedTime {
                    time: Duration::from_nanos(300_000_333_333),
                    source: Source::FamilyRuns {
                        samples: 3,
                        label_samples: 0,
                        conversions: Conversions::default(),
                    },
                },
                cold_from_warm: false,
            }),
        },
    ];
    regate(&mut caller);
    vec![
        caller,
        member(
            "devbox",
            false,
            Some(1.0),
            remote(Warmth::ColdWithHistory),
            0.0,
        ),
    ]
}

fn cases() -> Vec<Vec<Observation>> {
    vec![
        warm_faster(),
        saturated_caller(),
        both_saturated(),
        devbox_under_pressure(),
        busy_and_learned(),
        behind_long_running_work(),
    ]
}

/// Running work as records from before elapsed time kept it.
fn held(size: Cores, remaining: Option<Duration>) -> Held {
    Held {
        size,
        elapsed: None,
        expectation: remaining.map_or(Expectation::Unknown, |remaining| Expectation::Member {
            remaining,
        }),
    }
}

/// How far the record format had come when a decision was recorded.
#[derive(Clone, Copy, Debug)]
enum Form {
    /// Arrival scoring with its inputs.
    Current,
    /// Inputs for the last scoring only (the tuning item).
    BeforeArrivalInputs,
    /// Estimates only (S4).
    BeforeInputs,
}
const FORMS: [Form; 3] = [Form::Current, Form::BeforeArrivalInputs, Form::BeforeInputs];

/// A request scored once, on arrival, and started at once on the winner (or
/// failed with `fleet_busy`), recorded as `form` records it and read back.
fn recorded(observations: Vec<Observation>, parameters: &Parameters, form: Form) -> Record {
    let at = Timestamp::parse_rfc3339("2026-11-03T12:00:00Z").unwrap();
    let wanted = Wanted {
        size: cores(2.0),
        hold_left: Duration::from_millis(587_321),
        parameters: &parameters.placement,
    };
    let verdict = placement::decide(observations, Vec::new(), &wanted);
    let scoring = Scoring {
        scored_at: at,
        size: wanted.size,
        hold_left: wanted.hold_left,
        winner: verdict.winner.clone(),
    };
    let mut arrival = Arrival::of(&verdict, &scoring);
    let mut candidates: Vec<Candidate> = verdict
        .candidates
        .iter()
        .map(|scored| {
            let chosen = verdict.winner.as_deref() == Some(&scored.observation.state.member);
            Candidate::scored(scored, chosen, at)
        })
        .collect();
    let started = verdict.winner.is_some();
    let mut decision = Decision {
        decided_at: at,
        verdict: if started {
            Verdict::Started
        } else {
            Verdict::FleetBusy
        },
        reason: verdict.reason.clone(),
        candidates: Vec::new(),
        unreachable: Vec::new(),
        parameters: Some(parameters.clone()),
        scoring: Some(scoring),
        arrival: None,
    };
    match form {
        Form::Current => {}
        Form::BeforeArrivalInputs => {
            arrival.size = None;
            arrival.hold_left = None;
            for candidate in &mut arrival.candidates {
                candidate.state = None;
                candidate.inputs = None;
            }
        }
        Form::BeforeInputs => {
            assert_eq!(parameters, &Parameters::default(), "S4 used the defaults");
            decision.parameters = None;
            decision.scoring = None;
            for candidate in &mut candidates {
                candidate.inputs = None;
            }
        }
    }
    decision.candidates = candidates;
    if !matches!(form, Form::BeforeInputs) {
        decision.arrival = Some(arrival);
    }
    let record = Record {
        plan_observation: None,
        workload: None,
        run_id: RunId::generate(),
        kind: Kind::Run,
        synthetic: false,
        internal: false,
        peer_io: None,
        label: Some("test App".into()),
        placement: Placement::Placeable,
        urgency: Urgency::Interactive {
            hold_limit_ms: None,
        },
        size: cores(2.0),
        size_basis: None,
        calling_member: "mini".into(),
        workspace: None,
        workspace_recorded: true,
        member: verdict.winner.clone(),
        placement_note: None,
        nested_in: None,
        shares_capacity_of: None,
        state: State::Ended,
        outcome: Some(if started {
            Outcome::Exited { exit_code: 0 }
        } else {
            Outcome::Failed {
                code: "fleet_busy".into(),
            }
        }),
        requested_at: at,
        started_at: started.then(|| at.after(Duration::from_millis(400))),
        ended_at: Some(at.after(Duration::from_secs(41))),
        waited_ms: started.then_some(400),
        run_ms: started.then_some(40_600),
        run_phases: None,
        cpu_ms: None,
        hold_limit: None,
        decision: Some(decision),
        warnings: Vec::new(),
        requested_by: ProcessIdentity {
            pid: 1,
            started_us: 1,
        },
        processes: Vec::new(),
        placed: None,
        settings: None,
        reacquired: 0,
        config_sync: None,
        pre_run_history: Some(Default::default()),
        fallbacks: Vec::new(),
    };
    serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap()
}

fn change(settings: &[&str]) -> Change {
    Change::parse(&settings.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
}

fn defaults() -> Parameters {
    Parameters::default()
}

#[test]
fn replaying_unchanged_parameters_reproduces_every_recorded_decision() {
    for form in FORMS {
        let records: Vec<Record> = cases()
            .into_iter()
            .map(|case| recorded(case, &defaults(), form))
            .collect();
        // Settings that change nothing: none, a default margin, and for records
        // with inputs, the default gate thresholds, which derive every gate again.
        let mut unchanged = vec![change(&[]), change(&["keep_local_margin=0.1"])];
        if !matches!(form, Form::BeforeInputs) {
            unchanged.push(change(&[
                "admission.saturated_utilization=0.9",
                "memory_pressure=warning",
                "run_prior=60s",
            ]));
        }
        for unchanged in unchanged {
            let replay = replay(&records, &unchanged);
            let decisions = &replay.decisions;
            assert_eq!(decisions.placement, records.len(), "{form:?}");
            assert!(
                decisions.not_replayable.is_empty(),
                "{form:?} {unchanged:?}: {:?}",
                decisions.not_replayable
            );
            assert_eq!(decisions.replayed, records.len(), "{form:?}");
            assert_eq!(decisions.flipped, 0, "{form:?}: {:?}", replay.flips);
            let from = &decisions.replayed_from;
            let expected = match form {
                // Every request here started at once: its last scoring was its
                // arrival scoring.
                Form::Current | Form::BeforeArrivalInputs => (records.len(), 0, 0),
                Form::BeforeInputs => (0, 0, records.len()),
            };
            assert_eq!(
                (from.arrival, from.last_scoring, from.recorded_estimates),
                expected,
                "{form:?}"
            );
            assert_eq!(replay.effect.fleet_busy.recorded, 1);
            assert_eq!(replay.effect.fleet_busy.would_place, 0);
        }
    }
}

#[test]
fn replay_starts_from_each_decisions_own_recorded_parameters() {
    let wider = Parameters {
        placement: PlacementParameters {
            keep_local_margin: 0.2,
            ..Default::default()
        },
        ..Default::default()
    };
    for form in [Form::Current, Form::BeforeArrivalInputs] {
        let records = vec![recorded(warm_faster(), &wider, form)];
        let replay_unchanged = replay(&records, &change(&[]));
        assert_eq!(replay_unchanged.decisions.flipped, 0);
        assert_eq!(replay_unchanged.effect.kept_on_caller.recorded, 1);
        // Only the margin changes back: the run prior and the rest stay recorded.
        let narrower = replay(&records, &change(&["keep_local_margin=0.1"]));
        assert_eq!(narrower.decisions.flipped, 1, "{form:?}");
        assert_eq!(
            narrower.flips[0].replayed.choice.winner.as_deref(),
            Some("devbox")
        );
    }
}

#[test]
fn a_wider_keep_local_margin_keeps_the_faster_members_work_on_the_caller() {
    for form in FORMS {
        let records = vec![
            recorded(warm_faster(), &defaults(), form),
            recorded(saturated_caller(), &defaults(), form),
        ];
        let replay = replay(&records, &change(&["keep_local_margin=0.2"]));
        assert_eq!(replay.decisions.replayed, 2, "{form:?}");
        assert_eq!(replay.decisions.flipped, 1, "{form:?}");
        let flip = &replay.flips[0];
        assert_eq!(flip.run_id, records[0].run_id);
        assert_eq!(flip.recorded.winner.as_deref(), Some("devbox"));
        assert_eq!(
            flip.recorded.completion_seconds,
            Some(Duration::from_secs(53))
        );
        assert_eq!(flip.replayed.choice.winner.as_deref(), Some("mini"));
        assert_eq!(
            flip.replayed.choice.completion_seconds,
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            (flip.recorded.run_level, flip.replayed.choice.run_level),
            (Some(Level::Prior), Some(Level::Prior)),
            "{form:?}: neither had history"
        );
        assert_eq!(
            flip.replayed.recorded_winner_completion_seconds,
            Some(Duration::from_secs(53))
        );
        assert_eq!(flip.completion_difference_seconds, Some(7.0));
        assert!(
            flip.replayed
                .choice
                .reason
                .contains("not earlier by the 12 s margin"),
            "{}",
            flip.replayed.choice.reason
        );
        assert_eq!(flip.ran_on.as_deref(), Some("devbox"));
        let actual = flip.actual.as_ref().unwrap();
        assert_eq!(
            actual.started_after_seconds,
            Some(Duration::from_millis(400))
        );
        assert_eq!(actual.completion_seconds, Some(Duration::from_secs(41)));
        let effect = &replay.effect;
        assert_eq!(
            (
                effect.kept_on_caller.recorded,
                effect.kept_on_caller.replayed
            ),
            (0, 1)
        );
        let devbox = effect
            .winners
            .iter()
            .find(|w| w.member == "devbox")
            .unwrap();
        assert_eq!((devbox.recorded, devbox.replayed), (2, 1));
        let difference = &effect.completion_difference_seconds;
        assert_eq!((difference.total, difference.mean), (7.0, Some(7.0)));
        assert_eq!(
            (difference.earlier, difference.later, difference.unknown),
            (0, 1, 0)
        );
    }
}

#[test]
fn a_slower_warm_sync_prior_flips_only_decisions_recorded_with_inputs() {
    for form in FORMS {
        let records = vec![recorded(warm_faster(), &defaults(), form)];
        let replay = replay(&records, &change(&["sync_prior_warm=10s"]));
        match form {
            Form::BeforeInputs => {
                assert_eq!(replay.decisions.replayed, 0);
                let not = &replay.decisions.not_replayable;
                assert_eq!(not.len(), 1);
                assert_eq!((not[0].why, not[0].count), ("recorded_before_inputs", 1));
                assert!(not[0].run_ids.is_empty());
            }
            _ => {
                assert_eq!(replay.decisions.flipped, 1, "{form:?}");
                let flip = &replay.flips[0];
                assert_eq!(flip.replayed.choice.winner.as_deref(), Some("mini"));
                // 50 s + 10 s on the devbox now, against 60 s.
                assert_eq!(
                    flip.replayed.recorded_winner_completion_seconds,
                    Some(Duration::from_secs(60))
                );
                assert_eq!(flip.completion_difference_seconds, Some(0.0));
            }
        }
    }
}

#[test]
fn the_remaining_time_of_work_without_an_estimate_replays_from_its_recorded_elapsed_time() {
    for form in [Form::Current, Form::BeforeArrivalInputs] {
        let records = vec![recorded(behind_long_running_work(), &defaults(), form)];
        let candidate = |record: &Record| {
            let decision = record.decision.as_ref().unwrap();
            decision
                .candidates
                .iter()
                .find(|c| c.member == "mini")
                .unwrap()
                .clone()
        };
        let mini = candidate(&records[0]);
        assert_eq!(
            mini.estimate.as_ref().unwrap().wait_seconds,
            Some(Duration::from_millis(322_000))
        );
        assert!(
            mini.why.contains(
                "behind running work: 9 cores 644 s in, 322 s more (no estimate), \
                 1 core 100 s in, 200 s more (family: 3 runs of other labels there)"
            ),
            "{}",
            mini.why
        );
        let unchanged = replay(&records, &change(&[]));
        assert_eq!(unchanged.decisions.replayed, 1, "{form:?}");
        assert_eq!(unchanged.decisions.flipped, 0, "{form:?}");
        // With the assumption fixed again, the caller would have kept the work.
        let fixed = replay(&records, &change(&["unknown_remaining_per_elapsed=0"]));
        assert_eq!(fixed.decisions.flipped, 1, "{form:?}");
        assert_eq!(
            fixed.flips[0].replayed.choice.winner.as_deref(),
            Some("mini")
        );

        // A record from before elapsed time and the new parameter were recorded
        // replays as it scored then: 60 s for the work without an estimate.
        let mut json = serde_json::to_value(&records[0]).unwrap();
        let decision = &mut json["decision"];
        decision["parameters"]["placement"]
            .as_object_mut()
            .unwrap()
            .remove("unknown_remaining_per_elapsed");
        let strip = |candidates: Option<&mut serde_json::Value>| {
            let candidates = candidates.and_then(|c| c.as_array_mut());
            for candidate in candidates.into_iter().flatten() {
                let holding = candidate
                    .pointer_mut("/inputs/holding")
                    .and_then(|h| h.as_array_mut());
                for held in holding.into_iter().flatten() {
                    let held = held.as_object_mut().unwrap();
                    held.remove("elapsed_ms");
                    held.remove("learned_run");
                }
            }
        };
        strip(decision.get_mut("candidates"));
        strip(decision.pointer_mut("/arrival/candidates"));
        let earlier: Record = serde_json::from_value(json).unwrap();
        let rescored = RecordedScoring::of(&earlier).unwrap().reproduce(false);
        // Its recorded estimates were taken with elapsed time, so the stripped
        // record no longer matches them; scored again it waits 60 s.
        assert!(rescored.is_err(), "{form:?}");
        let scoring = RecordedScoring::of(&earlier).unwrap();
        let again = scoring.score(&defaults(), &change(&[])).unwrap();
        assert_eq!(again.winner.as_deref(), Some("mini"), "{form:?}");
    }
}

/// The arrival scoring of a real request (`r-b3832e93b295`, 2026-11-04): this
/// Mac's 9 cores were held by a run without an estimate that had started
/// 644.413 s before, which the record from then does not hold.
const REAL_ARRIVAL: &str = r#"{"scored_at":"2026-11-04T07:35:07.753Z","winner":"mac-mini","candidates":[{"member":"mac-mini","wait_seconds":120.0,"sync_seconds":0.0,"run_seconds":70.057,"completion_seconds":190.057,"eligible":true,"state":{"member":"mac-mini","capacity":{"weighted_cores":12.0,"reserved":3.0,"capacity":9.0},"in_use":9.0,"free":0.0,"queued":{"interactive":1,"deferrable":1},"load":{"cores":{"performance":10,"efficiency":4},"cpu_utilization":0.32511589050292966,"memory_pressure":"normal","thermal_state":"nominal"},"gate":{"verdict":"open"},"speed":1.147000666378712},"inputs":{"caller":true,"speed":1.147000666378712,"utilization":0.29207773208618165,"holding":[{"size":9.0,"remaining_ms":null}],"ahead":[{"size":9.0,"run_ms":null}],"copy":{"state":"caller"},"learned":{"warm_run":{"ms":70057,"from":"member_runs","samples":5}},"admission":{"saturated_utilization":0.9,"memory_pressure":"warning"}}},{"member":"macbook-pro","wait_seconds":0.0,"sync_seconds":4.707,"run_seconds":168.144,"completion_seconds":172.851,"eligible":true,"state":{"member":"macbook-pro","capacity":{"weighted_cores":9.0,"reserved":0.0,"capacity":9.0},"in_use":0.0,"free":9.0,"queued":{"interactive":0,"deferrable":0},"load":{"cores":{"performance":8,"efficiency":2},"cpu_utilization":0.2,"memory_pressure":"normal","thermal_state":"nominal"},"gate":{"verdict":"open"},"speed":0.7811775274754783},"inputs":{"caller":false,"speed":0.7811775274754783,"utilization":0.20232673009236657,"holding":[],"ahead":[],"copy":{"state":"remote","warmth":"cold_with_history","busy":false},"learned":{"warm_run":{"ms":56048,"from":"fleet_runs","samples":5,"conversions":{"learned":5,"measured":0}},"sync":{"ms":4707,"from":"repository_syncs","samples":1}},"admission":{"saturated_utilization":0.9,"memory_pressure":"warning"}}}],"size":3.0,"hold_left_ms":529895}"#;

#[test]
fn the_real_request_that_waited_410_s_goes_to_the_idle_member_once_elapsed_time_counts() {
    let arrival: Arrival = serde_json::from_str(REAL_ARRIVAL).unwrap();
    let observations = |elapsed: Option<Duration>| -> Vec<Observation> {
        arrival
            .candidates
            .iter()
            .map(|c| {
                let mut inputs = c.inputs.clone().unwrap();
                for held in &mut inputs.holding {
                    held.elapsed = elapsed;
                }
                Observation::from_inputs(c.state.clone().unwrap(), inputs)
            })
            .collect()
    };
    let parameters = defaults();
    let wanted = Wanted {
        size: arrival.size.unwrap(),
        hold_left: arrival.hold_left.unwrap(),
        parameters: &parameters.placement,
    };
    // As recorded, it reproduces: 60 s for the running work and 60 s for the
    // 9-core request queued ahead, 190 s against 173 s, within the margin.
    let recorded = placement::decide(observations(None), Vec::new(), &wanted);
    assert_eq!(recorded.winner.as_deref(), Some("mac-mini"));
    let mini = recorded.scored("mac-mini").unwrap();
    assert_eq!(
        mini.estimate.completion_seconds,
        Some(Duration::from_millis(190_057))
    );
    // With the elapsed time it had: 322.2 s for the running work, then 60 s for
    // the request ahead, 452 s in all, and the idle member completes first by far.
    let now = placement::decide(
        observations(Some(Duration::from_millis(644_413))),
        Vec::new(),
        &wanted,
    );
    assert_eq!(now.winner.as_deref(), Some("macbook-pro"), "{}", now.reason);
    let mini = now.scored("mac-mini").unwrap();
    assert_eq!(
        mini.estimate.wait_seconds,
        Some(Duration::from_micros(382_206_500))
    );
    assert!(
        mini.why
            .contains("behind running work: 9 cores 644 s in, 322 s more (no estimate)"),
        "{}",
        mini.why
    );
    assert_eq!(
        now.reason,
        "macbook-pro completes earliest, in 173 s, against 452 s on the calling member mac-mini."
    );
}

#[test]
fn a_higher_saturation_threshold_derives_gates_again_and_places_fleet_busy_work() {
    for form in [Form::Current, Form::BeforeArrivalInputs] {
        let records = vec![
            recorded(saturated_caller(), &defaults(), form),
            recorded(both_saturated(), &defaults(), form),
        ];
        let replay = replay(&records, &change(&["saturated_utilization=0.99"]));
        assert_eq!(
            replay.decisions.replayed, 2,
            "{:?}",
            replay.decisions.not_replayable
        );
        assert_eq!(replay.decisions.flipped, 2);
        // The caller is open again, 4× slower under its outside load: 240 s,
        // against 300 s on the cold devbox.
        let flip = &replay.flips[0];
        assert_eq!(flip.recorded.winner.as_deref(), Some("devbox"));
        assert_eq!(flip.replayed.choice.winner.as_deref(), Some("mini"));
        assert_eq!(flip.completion_difference_seconds, Some(-60.0));
        // The decision that failed with fleet_busy finds a member.
        let busy = &replay.flips[1];
        assert_eq!(busy.verdict, Verdict::FleetBusy);
        assert_eq!(busy.recorded.winner, None);
        assert_eq!(busy.replayed.choice.winner.as_deref(), Some("mini"));
        assert_eq!(busy.completion_difference_seconds, None);
        assert!(busy.actual.is_none(), "it never started");
        let effect = &replay.effect;
        assert_eq!(
            (effect.fleet_busy.recorded, effect.fleet_busy.would_place),
            (1, 1)
        );
        assert_eq!(
            (effect.no_winner.recorded, effect.no_winner.replayed),
            (1, 0)
        );
        let difference = &effect.completion_difference_seconds;
        assert_eq!(
            (difference.total, difference.earlier, difference.unknown),
            (-60.0, 1, 1)
        );
        assert!(
            replay
                .limits
                .iter()
                .any(|l| l.contains("as if every member"))
        );
    }
}

#[test]
fn blocking_only_at_critical_memory_pressure_opens_the_faster_member() {
    let records = vec![recorded(
        devbox_under_pressure(),
        &defaults(),
        Form::Current,
    )];
    assert_eq!(
        replay(&records, &change(&[])).flips.len(),
        0,
        "the devbox was blocked"
    );
    let replay = replay(&records, &change(&["memory_pressure=critical"]));
    assert_eq!(replay.decisions.flipped, 1);
    assert_eq!(
        replay.flips[0].replayed.choice.winner.as_deref(),
        Some("devbox")
    );
    assert_eq!(replay.flips[0].completion_difference_seconds, Some(-7.0));
}

#[test]
fn a_decision_that_does_not_reproduce_is_reported_and_never_replayed() {
    let mut tampered = recorded(warm_faster(), &defaults(), Form::Current);
    let arrival = tampered
        .decision
        .as_mut()
        .unwrap()
        .arrival
        .as_mut()
        .unwrap();
    arrival.candidates[1].run_seconds += Duration::from_millis(1);
    let mut inconsistent = recorded(saturated_caller(), &defaults(), Form::Current);
    // A gate its recorded load does not explain.
    let arrival = inconsistent
        .decision
        .as_mut()
        .unwrap()
        .arrival
        .as_mut()
        .unwrap();
    let devbox = arrival.candidates[1].state.as_mut().unwrap();
    devbox.gate = load::Gate::MemoryPressure {
        level: MemoryPressure::Warning,
    };
    // Its scoring then differs from the recorded one too.
    let records = vec![tampered.clone(), inconsistent.clone()];

    let replay_margin = replay(&records, &change(&["keep_local_margin=0.2"]));
    assert_eq!(replay_margin.decisions.replayed, 0);
    let not = &replay_margin.decisions.not_replayable;
    assert_eq!(not.len(), 1, "{not:?}");
    assert_eq!((not[0].why, not[0].count), ("not_reproduced", 2));
    assert_eq!(
        not[0].run_ids,
        vec![tampered.run_id.clone(), inconsistent.run_id.clone()]
    );
    assert!(replay_margin.flips.is_empty());

    let replay_gates = replay(&records, &change(&["saturated_utilization=0.95"]));
    let not = &replay_gates.decisions.not_replayable;
    assert_eq!(not.len(), 2, "{not:?}");
    assert_eq!(not[0].why, "not_reproduced");
    assert_eq!(not[0].run_ids, vec![tampered.run_id.clone()]);
    assert_eq!(not[1].why, "gate_not_reproduced");
    assert_eq!(not[1].run_ids, vec![inconsistent.run_id.clone()]);
}

/// A request whose arrival scoring sent it per `first`, and that waited until a
/// scoring per `last`, 5 s later, before it started (or failed).
fn waited(first: Vec<Observation>, last: Vec<Observation>, form: Form) -> Record {
    let mut record = recorded(last, &defaults(), form);
    let first = recorded(first, &defaults(), Form::Current);
    let mut arrival = first.decision.unwrap().arrival.unwrap();
    arrival.scored_at = Timestamp::from_millis(arrival.scored_at.millis() - 5000);
    if matches!(form, Form::BeforeArrivalInputs) {
        arrival.size = None;
        arrival.hold_left = None;
        for candidate in &mut arrival.candidates {
            candidate.state = None;
            candidate.inputs = None;
        }
    }
    record.decision.as_mut().unwrap().arrival = Some(arrival);
    record
}

#[test]
fn a_request_that_waited_replays_its_arrival_or_else_its_last_scoring() {
    // Sent to the faster devbox on arrival; by the time it started, the devbox
    // was under memory pressure and the caller won.
    let current = waited(warm_faster(), devbox_under_pressure(), Form::Current);
    let tuning = waited(
        warm_faster(),
        devbox_under_pressure(),
        Form::BeforeArrivalInputs,
    );
    let records = vec![current.clone(), tuning.clone()];
    let unchanged = replay(&records, &change(&[]));
    assert_eq!(unchanged.decisions.flipped, 0);
    let from = &unchanged.decisions.replayed_from;
    assert_eq!((from.arrival, from.last_scoring), (1, 1));
    let devbox = unchanged
        .effect
        .winners
        .iter()
        .find(|w| w.member == "devbox");
    assert_eq!(devbox.map(|w| w.recorded), Some(1), "the arrival winner");

    let wider = replay(&records, &change(&["keep_local_margin=0.2"]));
    assert_eq!(wider.decisions.flipped, 1);
    assert_eq!(wider.flips[0].run_id, current.run_id);
    assert_eq!(wider.flips[0].replayed_from, recorded::Origin::Arrival);
    assert_eq!(
        wider.flips[0].scored_at,
        current
            .decision
            .as_ref()
            .unwrap()
            .arrival
            .as_ref()
            .unwrap()
            .scored_at
    );
    assert!(wider.limits.iter().any(|l| l.contains("not a move")));
}

#[test]
fn fleet_busy_work_that_had_a_place_is_not_counted_as_placed_by_a_change() {
    // Its arrival scoring had a winner, but it failed later.
    let mut busy = recorded(warm_faster(), &defaults(), Form::Current);
    busy.decision.as_mut().unwrap().verdict = Verdict::FleetBusy;
    busy.member = None;
    busy.started_at = None;
    for settings in [&[][..], &["keep_local_margin=0.2"][..]] {
        let replay = replay(std::slice::from_ref(&busy), &change(settings));
        let fleet_busy = &replay.effect.fleet_busy;
        assert_eq!(
            (
                fleet_busy.recorded,
                fleet_busy.had_a_winner,
                fleet_busy.would_place
            ),
            (1, 1, 0),
            "{settings:?}"
        );
    }
}

#[test]
fn a_decision_recorded_before_inputs_that_started_elsewhere_has_its_own_reason() {
    let mut moved = recorded(warm_faster(), &defaults(), Form::BeforeInputs);
    moved.member = Some("mini".into());
    for candidate in &mut moved.decision.as_mut().unwrap().candidates {
        candidate.chosen = candidate.member == "mini";
    }
    let replay = replay(
        std::slice::from_ref(&moved),
        &change(&["keep_local_margin=0.2"]),
    );
    let not = &replay.decisions.not_replayable;
    assert_eq!(not.len(), 1);
    assert_eq!(not[0].why, "started_elsewhere_before_inputs");
    assert_eq!(not[0].run_ids, vec![moved.run_id.clone()]);
}

#[test]
fn recorded_parameters_the_change_makes_invalid_are_reported() {
    // A value outside today's range, as a later version might record.
    let recorded_with = Parameters {
        placement: PlacementParameters {
            run_prior: Duration::from_millis(500),
            ..Default::default()
        },
        ..Default::default()
    };
    let record = recorded(warm_faster(), &recorded_with, Form::Current);
    let replay = replay(
        std::slice::from_ref(&record),
        &change(&["keep_local_margin=0.2"]),
    );
    let not = &replay.decisions.not_replayable;
    assert_eq!(not.len(), 1);
    assert_eq!(not[0].why, "invalid_parameters");
    assert!(
        not[0].explanation.contains("placement.run_prior"),
        "{}",
        not[0].explanation
    );
}

#[test]
fn the_flips_listed_are_bounded_and_say_so() {
    let records: Vec<Record> = (0..summary::MAXIMUM_LISTED + 1)
        .map(|_| recorded(warm_faster(), &defaults(), Form::BeforeInputs))
        .collect();
    let replay = replay(&records, &change(&["keep_local_margin=0.2"]));
    assert_eq!(replay.decisions.flipped, summary::MAXIMUM_LISTED + 1);
    assert_eq!(replay.flips.len(), summary::MAXIMUM_LISTED);
    assert!(replay.flips_truncated);
    assert_eq!(
        replay.flips[0].run_id, records[1].run_id,
        "the latest are kept"
    );
    assert_eq!(
        replay.effect.completion_difference_seconds.total,
        7.0 * (summary::MAXIMUM_LISTED + 1) as f64,
        "aggregates count every flip"
    );
}

#[test]
fn records_without_a_placement_decision_are_left_out() {
    let mut pinned = recorded(warm_faster(), &defaults(), Form::Current);
    pinned.placement = Placement::Pinned;
    let mut undecided = recorded(warm_faster(), &defaults(), Form::Current);
    undecided.decision = None;
    let mut local = recorded(warm_faster(), &defaults(), Form::BeforeInputs);
    // A placeable request on a member that is not the master runs there, with
    // no candidates.
    local.decision.as_mut().unwrap().candidates.clear();
    let replay = replay(&[pinned, undecided, local], &change(&[]));
    assert_eq!(replay.decisions.placement, 0);
}
