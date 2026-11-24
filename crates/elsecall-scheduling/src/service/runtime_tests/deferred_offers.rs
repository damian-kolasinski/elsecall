//! Deadline and ordering races driven at the configuration and member boundaries.
use super::*;
use crate::{
    placed::{DeferredOffer, OfferReply, PlacementOffer, Standing},
    quote::{QueuePriority, Quote, QuoteRequest},
};

type Offered = (PlacementOffer, oneshot::Sender<OfferReply>);
struct Peer(mpsc::UnboundedSender<Offered>);

impl Members for Peer {
    fn standing(&self) -> Standing {
        Standing {
            name: Some("caller".into()),
            master: Some("caller".into()),
            members: vec!["caller".into(), "peer".into()],
            awaiting_provisioning: vec![],
        }
    }
    fn offer(&self, _: &str, offer: &PlacementOffer) -> BoxFuture<'static, Result<OfferReply>> {
        let (answer, response) = oneshot::channel();
        self.0.send((offer.clone(), answer)).unwrap();
        Box::pin(async { Ok(response.await.unwrap()) })
    }
    fn quote(&self, _: &str, _: &QuoteRequest) -> BoxFuture<'static, Result<Quote>> {
        panic!("a required member does not need a quote")
    }
}

fn scheduler(home: &Path, members: Arc<dyn Members>, sync: Arc<dyn PreRunSync>) -> Arc<Scheduler> {
    let (_, exits) = mpsc::unbounded_channel();
    let (scheduler, _tasks) = Scheduler::start_with_runtime(
        &home.join("records.sqlite"),
        SchedulerConfiguration::default(),
        Adapters {
            machine: Arc::new(MachineBoundary),
            speed: Arc::new(SpeedBoundary),
            processes: Arc::new(ProcessBoundary),
            exits,
        },
        members,
        sync,
        Arc::new(LoadLog::open(&home.join("load.sqlite")).unwrap()),
        Runtime {
            clock: Arc::new(VirtualClock::new(Timestamp::from_millis(1000))),
            ids: Arc::new(IdBoundary(AtomicU64::new(1))),
            driving: Driving::Controlled,
        },
    )
    .unwrap();
    scheduler.tick().unwrap();
    scheduler
}
fn remote(urgency: Urgency) -> AdmissionRequest {
    AdmissionRequest {
        placement: Placement::Member("peer".into()),
        urgency,
        ..request(1000, 5000)
    }
}
fn submit(
    scheduler: &Arc<Scheduler>,
    pid: u32,
    req: AdmissionRequest,
) -> tokio::task::JoinHandle<Result<Grant>> {
    let scheduler = scheduler.clone();
    tokio::spawn(async move { scheduler.request(pid, req).await })
}
fn accepted(state: MemberState) -> OfferReply {
    OfferReply::Accepted {
        workload_contract: None,
        state,
        copy: "/copy".into(),
        location: "mapped".into(),
        warmth: None,
    }
}

#[tokio::test(start_paused = true)]
async fn master_grant_controls_the_skip_deadline_even_while_an_offer_is_in_flight() {
    for late in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let (peer, mut offers) = mpsc::unbounded_channel();
        let scheduler = scheduler(home.path(), Arc::new(Peer(peer)), Arc::new(SyncBoundary));
        let deadline = scheduler.clock.now().after(Duration::from_secs(1));
        let caller = submit(
            &scheduler,
            100,
            remote(Urgency::Deferrable {
                deadline,
                at_deadline: AtDeadline::Skip,
            }),
        );
        let (offer, response) = offers.recv().await.unwrap();
        let reply = accepted(scheduler.member_state());
        if late {
            tokio::time::advance(Duration::from_secs(1)).await;
            assert_eq!(caller.await.unwrap().unwrap_err().code, "deadline_skipped");
            assert_eq!(
                scheduler.record(&offer.run_id).unwrap().ended_at,
                Some(deadline)
            );
            response.send(reply).unwrap();
            // Synchronize with the next offer to this member after the late reply.
            let next = submit(
                &scheduler,
                101,
                remote(Urgency::Interactive {
                    hold_limit_ms: Some(5000),
                }),
            );
            offers
                .recv()
                .await
                .unwrap()
                .1
                .send(accepted(scheduler.member_state()))
                .unwrap();
            next.await.unwrap().unwrap();
            let record = scheduler.record(&offer.run_id).unwrap();
            assert!(record.started_at.is_none() && record.placed.is_none());
            assert_eq!(record.decision.unwrap().verdict, Verdict::DeadlineSkipped);
        } else {
            tokio::time::advance(Duration::from_millis(999)).await;
            response.send(reply).unwrap();
            let grant = caller.await.unwrap().unwrap();
            assert_eq!(grant.run_id, offer.run_id);
            let started = scheduler.record(&offer.run_id).unwrap().started_at.unwrap();
            assert_eq!(deadline.since(started), Duration::from_millis(1));
            assert_eq!(
                grant.placed.unwrap().start_by,
                started.after(request::MAXIMUM_HOLD_LIMIT)
            );
        }
    }
}

struct FirstSync(Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>);
impl PreRunSync for FirstSync {
    fn bring_up_to_date(&self, _: &str) -> BoxFuture<'static, Result<Synced>> {
        let gate = self.0.lock().unwrap().take();
        Box::pin(async move {
            if let Some((started, release)) = gate {
                started.send(()).unwrap();
                release.await.unwrap();
            }
            Ok(Synced::NothingSynced)
        })
    }
}

#[tokio::test(start_paused = true)]
async fn priority_change_during_sync_rescans_immediately_and_peer_refusal_backs_off() {
    let home = tempfile::tempdir().unwrap();
    let (peer, mut offers) = mpsc::unbounded_channel();
    let (started, syncing) = oneshot::channel();
    let (release, wait) = oneshot::channel();
    let scheduler = scheduler(
        home.path(),
        Arc::new(Peer(peer)),
        Arc::new(FirstSync(Mutex::new(Some((started, wait))))),
    );
    let now = scheduler.clock.now();
    let background = submit(
        &scheduler,
        100,
        remote(Urgency::Deferrable {
            deadline: now.after(Duration::from_secs(30)),
            at_deadline: AtDeadline::Start,
        }),
    );
    syncing.await.unwrap();
    let foreground = submit(
        &scheduler,
        101,
        remote(Urgency::Interactive {
            hold_limit_ms: Some(5000),
        }),
    );
    let interactive_id = RunId::parse("r-000000000002").unwrap();
    while !scheduler.is_awaiting_verdict(&interactive_id) {
        tokio::task::yield_now().await;
    }
    release.send(()).unwrap();
    let (first, response) = offers.recv().await.unwrap();
    assert_eq!(first.run_id, interactive_id);
    assert_eq!(
        scheduler.clock.now(),
        now,
        "queue changes consume no retry interval"
    );
    response
        .send(OfferReply::Busy {
            state: scheduler.member_state(),
            report: Failure::new("fleet_busy", "Busy", "Retry."),
        })
        .unwrap();
    let (retry, response) = offers.recv().await.unwrap();
    assert_eq!(retry.run_id, interactive_id);
    assert_eq!(scheduler.clock.now().since(now), Duration::from_millis(500));
    response.send(accepted(scheduler.member_state())).unwrap();
    foreground.await.unwrap().unwrap();
    let (_, response) = offers.recv().await.unwrap();
    response.send(accepted(scheduler.member_state())).unwrap();
    background.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn worker_checks_local_queue_priority_before_accepting_a_remote_offer() {
    use QueuePriority::{Deferrable, Interactive, Promoted};
    for local in [Promoted, Interactive, Deferrable] {
        let home = tempfile::tempdir().unwrap();
        let scheduler = scheduler(home.path(), Arc::new(Alone), Arc::new(SyncBoundary));
        let _holding = scheduler.request(100, request(1000, 5000)).await.unwrap();
        let mut queued = request(2000, 5000);
        let deadline = scheduler.clock.now().after(Duration::from_secs(1));
        if local != Interactive {
            queued.urgency = Urgency::Deferrable {
                deadline,
                at_deadline: if local == Promoted {
                    AtDeadline::Start
                } else {
                    AtDeadline::Skip
                },
            };
        }
        let _waiting = submit(&scheduler, 101, queued);
        let id = RunId::parse("r-000000000002").unwrap();
        while !scheduler.is_awaiting_verdict(&id) {
            tokio::task::yield_now().await;
        }
        if local == Promoted {
            tokio::time::advance(Duration::from_secs(1)).await;
            scheduler.tick().unwrap();
        }
        for (index, incoming) in [Promoted, Interactive, Deferrable].into_iter().enumerate() {
            let run_id = RunId::parse(&format!("r-{:012x}", index + 100)).unwrap();
            let offer = PlacementOffer {
                run_id: run_id.clone(),
                size: Cores::ONE,
                label: None,
                workspace: None,
                workload: None,
                start_within_ms: 1000,
                size_basis: None,
                expected_run_ms: None,
                deferred: match incoming {
                    Promoted => Some(DeferredOffer::Promoted(deadline)),
                    Interactive => None,
                    Deferrable => Some(DeferredOffer::StartAt(deadline)),
                },
            };
            let reply = scheduler
                .accept_placed("caller", &offer, true, "/home", "home", None)
                .unwrap();
            let allowed = matches!(
                (incoming, local),
                (Promoted, Interactive | Deferrable) | (Interactive, Deferrable)
            );
            if allowed {
                assert!(
                    matches!(reply, OfferReply::Accepted { .. }),
                    "incoming {incoming:?} with {local:?} waiting"
                );
                scheduler.hosted_ended(
                    &run_id,
                    HostedEnd::Exited {
                        code: 0,
                        cpu_ms: None,
                    },
                    None,
                );
            } else {
                assert!(
                    matches!(reply, OfferReply::Busy { .. }),
                    "incoming {incoming:?} with {local:?} waiting"
                );
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn promotion_or_skip_during_sync_offers_the_new_head_at_the_deadline() {
    for action in [AtDeadline::Start, AtDeadline::Skip] {
        let home = tempfile::tempdir().unwrap();
        let (peer, mut offers) = mpsc::unbounded_channel();
        let (started, syncing) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        let scheduler = scheduler(
            home.path(),
            Arc::new(Peer(peer)),
            Arc::new(FirstSync(Mutex::new(Some((started, wait))))),
        );
        let deadline = scheduler.clock.now().after(Duration::from_secs(1));
        let background = submit(
            &scheduler,
            100,
            remote(Urgency::Deferrable {
                deadline,
                at_deadline: action,
            }),
        );
        syncing.await.unwrap();
        let foreground = submit(
            &scheduler,
            101,
            remote(Urgency::Interactive {
                hold_limit_ms: Some(5_000),
            }),
        );
        let foreground_id = RunId::parse("r-000000000002").unwrap();
        while !scheduler.is_awaiting_verdict(&foreground_id) {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        release.send(()).unwrap();
        if action == AtDeadline::Start {
            let (first, reply) = offers.recv().await.unwrap();
            assert_eq!(first.deferred, Some(DeferredOffer::Promoted(deadline)));
            assert_eq!(scheduler.clock.now(), deadline);
            reply.send(accepted(scheduler.member_state())).unwrap();
            background.await.unwrap().unwrap();
        } else {
            assert_eq!(
                background.await.unwrap().unwrap_err().code,
                "deadline_skipped"
            );
        }
        let (next, reply) = offers.recv().await.unwrap();
        assert_eq!(next.run_id, foreground_id);
        assert_eq!(scheduler.clock.now(), deadline);
        reply.send(accepted(scheduler.member_state())).unwrap();
        foreground.await.unwrap().unwrap();
    }
}
