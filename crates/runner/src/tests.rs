use std::time::Duration;

use time::OffsetDateTime;

use crate::protocol::Refusal;
use crate::*;

const MACHINE: &str = "167eeb6836c44115aa084e7780e4328c";
const OTHER: &str = "ebb80f403ef641deaa486417f2b6992a";

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::from_secs(seconds as u64)
}

/// A fence whose clock is fixed, so lease rules are tested exactly.
fn fence() -> Fence {
    Fence::new(MACHINE, Box::new(SqliteFence::in_memory().unwrap())).with_clock(|| at(1_000))
}

fn envelope(op: Operation) -> Envelope {
    Envelope {
        protocol_version: PROTOCOL_VERSION,
        target_machine_id: Some(MACHINE.into()),
        epoch: 1,
        holder_id: "controller-a".into(),
        lease_expires_at: at(1_030),
        sent_at: at(1_000),
        request_id: "req-1".into(),
        object: None,
        op,
    }
}

fn instance() -> InstanceRef {
    InstanceRef {
        uuid: "uuid-web".into(),
        name: "web".into(),
    }
}

/// A change at one generation, wanting one thing.
fn change(generation: i64, digest: &str, request_id: &str) -> Envelope {
    Envelope {
        request_id: request_id.into(),
        object: Some(ObjectFence {
            object_id: instance().uuid,
            generation,
            digest: digest.into(),
        }),
        op: Operation::StartInstance {
            instance: instance(),
        },
        ..envelope(Operation::Health)
    }
}

struct FakeHost;

#[async_trait::async_trait]
impl Host for FakeHost {
    async fn health(&self) -> Result<Health, HostError> {
        Ok(Health {
            protocol_version: PROTOCOL_VERSION,
            machine_id: MACHINE.into(),
            hostname: "tsukasa.example.org".into(),
            accepted_epoch: 0,
        })
    }
    async fn capabilities(&self) -> Result<Capabilities, HostError> {
        Ok(Capabilities {
            arch: "aarch64".into(),
            cpu_count: 8,
            memory_total_mib: 7549,
            storage_total_gib: 165,
            storage_available_gib: 154,
            hypervisor_version: "QEMU 10.2.2".into(),
        })
    }
    async fn inventory(&self) -> Result<Inventory, HostError> {
        Ok(Inventory { domains: vec![] })
    }
    async fn sample(&self) -> Result<crate::Samples, HostError> {
        Ok(crate::Samples {
            host: crate::HostSample {
                cpu: Some(crate::CpuTimeSample {
                    total: 1000,
                    idle: 850,
                }),
                memory_total_bytes: 7549 * 1024 * 1024,
                memory_available_bytes: 6000 * 1024 * 1024,
                storage_total_bytes: 165 * 1024 * 1024 * 1024,
                storage_available_bytes: 154 * 1024 * 1024 * 1024,
                cpu_count: 8,
            },
            domains: vec![crate::DomainUsage {
                uuid: instance().uuid,
                cpu_time_ns: 30_000_000_000,
                vcpus: 2,
                rss_kib: Some(512 * 1024),
                storage_used_bytes: 3 * 1024 * 1024 * 1024,
            }],
        })
    }
    async fn ensure_image(&self, _: &crate::ImageRequest) -> Result<crate::Reply, HostError> {
        Ok(crate::Reply::ImageReady {
            name: "debian-13".into(),
            checksum: "sha256-00".into(),
            already_present: true,
            size: 0,
        })
    }
    async fn provision(&self, _: &crate::ProvisionRequest) -> Result<crate::Reply, HostError> {
        Ok(crate::Reply::Provisioned {
            state: bento_types::State::Running,
        })
    }
    async fn apply_network(
        &self,
        _: &bento_network::MachineNetwork,
    ) -> Result<crate::Reply, HostError> {
        Ok(crate::Reply::NetworkApplied {
            bridges: 0,
            routes_added: 0,
            routes_removed: 0,
            routes_unchanged: 0,
        })
    }
    async fn start(&self, _: &InstanceRef) -> Result<bento_types::State, HostError> {
        Ok(bento_types::State::Running)
    }
    async fn stop(&self, _: &InstanceRef) -> Result<bento_types::State, HostError> {
        Ok(bento_types::State::Stopped)
    }
    async fn reboot(&self, _: &InstanceRef) -> Result<bento_types::State, HostError> {
        Ok(bento_types::State::Running)
    }
    async fn remove(&self, _: &InstanceRef) -> Result<bento_types::State, HostError> {
        Ok(bento_types::State::Stopped)
    }
}

#[test]
fn a_change_must_name_the_object_it_changes() {
    let fence = fence();
    let mut unfenced = change(1, "digest-a", "r1");
    unfenced.object = None;
    assert!(matches!(
        fence.admit(&unfenced).unwrap(),
        Admission::Refused(Refusal::UnfencedChange)
    ));
}

#[test]
fn a_late_order_from_an_overtaken_controller_is_refused() {
    // `request_id` catches a message that arrives twice. This catches a
    // different thing: a message that arrives late, carrying an older
    // idea of what the instance should be (MULTI-NODE 11.3).
    let fence = fence();
    assert!(matches!(
        fence.admit(&change(5, "digest-new", "r-new")).unwrap(),
        Admission::Allowed
    ));

    match fence.admit(&change(4, "digest-old", "r-old")).unwrap() {
        Admission::Refused(Refusal::StaleGeneration { yours, theirs, .. }) => {
            assert_eq!((yours, theirs), (4, 5));
        }
        other => panic!("expected a stale generation, got {other:?}"),
    }

    // A later generation supersedes it.
    assert!(matches!(
        fence.admit(&change(6, "digest-later", "r-later")).unwrap(),
        Admission::Allowed
    ));
}

#[test]
fn two_orders_at_one_generation_that_disagree_are_both_refused() {
    // One of them is wrong and the runner cannot tell which, so it takes
    // neither (MULTI-NODE 11.3).
    let fence = fence();
    assert!(matches!(
        fence.admit(&change(3, "digest-a", "r-a")).unwrap(),
        Admission::Allowed
    ));

    match fence.admit(&change(3, "digest-b", "r-b")).unwrap() {
        Admission::Refused(Refusal::GenerationConflict { generation, .. }) => {
            assert_eq!(generation, 3);
        }
        other => panic!("expected a generation conflict, got {other:?}"),
    }

    // The same generation wanting the same thing is a retry, not a
    // conflict. It is allowed through to the replay check.
    assert!(matches!(
        fence.admit(&change(3, "digest-a", "r-a2")).unwrap(),
        Admission::Allowed
    ));
}

#[tokio::test]
async fn a_change_is_recorded_before_its_answer_leaves() {
    // The controller may never see the reply. Its retry must be answered
    // from memory, not run a second time (MULTI-NODE 11.3).
    let fence = fence();
    let request = change(1, "digest-a", "r-once");

    let first = serve_one(&fence, &FakeHost, request.clone()).await.unwrap();
    assert!(matches!(
        first,
        Outcome::Done(Reply::Changed {
            state: bento_types::State::Running
        })
    ));

    match serve_one(&fence, &FakeHost, request).await.unwrap() {
        Outcome::Replayed(recorded) => {
            assert!(recorded.contains("\"reply\":\"changed\""), "{recorded}");
        }
        other => panic!("expected a replay, got {other:?}"),
    }
}

#[tokio::test]
async fn a_read_is_not_recorded_and_runs_every_time() {
    let fence = fence();
    let request = envelope(Operation::Inventory);
    for _ in 0..2 {
        let outcome = serve_one(&fence, &FakeHost, request.clone()).await.unwrap();
        assert!(matches!(outcome, Outcome::Done(Reply::Inventory(_))));
    }
}

#[test]
fn a_request_this_runner_cannot_read_is_refused_before_anything_else() {
    let fence = fence();
    let mut envelope = envelope(Operation::Health);
    envelope.protocol_version = PROTOCOL_VERSION + 1;
    // Also addressed to the wrong host: the version check must win, so a
    // runner never reasons about a message it does not understand.
    envelope.target_machine_id = Some(OTHER.into());
    assert!(matches!(
        fence.admit(&envelope).unwrap(),
        Admission::Refused(Refusal::ProtocolVersion { .. })
    ));
}

#[test]
fn a_request_addressed_to_another_machine_is_refused() {
    let fence = fence();
    let mut envelope = envelope(Operation::Health);
    envelope.target_machine_id = Some(OTHER.into());
    assert!(matches!(
        fence.admit(&envelope).unwrap(),
        Admission::Refused(Refusal::WrongMachine { yours, theirs })
            if yours == OTHER && theirs == MACHINE
    ));
}

#[test]
fn an_unaddressed_read_is_answered_and_an_unaddressed_change_is_not() {
    // First contact: an operator wrote an endpoint, and only the machine
    // can say which machine it is (MULTI-NODE 11.1). A read answers. A
    // change must never act on a machine nobody could name.
    let fence = fence();
    let mut request = envelope(Operation::Health);
    request.target_machine_id = None;
    assert!(matches!(fence.admit(&request).unwrap(), Admission::Allowed));

    let mut unaddressed = change(1, "digest-a", "r1");
    unaddressed.target_machine_id = None;
    assert!(matches!(
        fence.admit(&unaddressed).unwrap(),
        Admission::Refused(Refusal::UnaddressedChange)
    ));
}

#[test]
fn an_expired_lease_is_refused() {
    let fence = fence();
    let mut envelope = envelope(Operation::Health);
    envelope.lease_expires_at = at(999);
    match fence.admit(&envelope).unwrap() {
        Admission::Refused(Refusal::LeaseExpired { at }) => {
            // The same shape as every other time on the wire, so the
            // controller can parse what it is told.
            assert_eq!(at, "1970-01-01T00:16:39Z");
        }
        other => panic!("expected an expired lease, got {other:?}"),
    }
}

#[test]
fn a_clock_that_disagrees_in_either_direction_fails_closed() {
    // The runner clock reads 1000. A controller whose clock is far from
    // that cannot have its lease judged, so the runner refuses rather
    // than guesses. Both directions must be caught: a runner running
    // fast would otherwise read every live lease as expired and report
    // the wrong reason (MULTI-NODE 11.3).
    let fence = fence().with_max_skew(Duration::from_secs(60));

    // The controller is far behind this runner.
    let mut behind = envelope(Operation::Health);
    behind.sent_at = at(400);
    behind.lease_expires_at = at(430);
    match fence.admit(&behind).unwrap() {
        Admission::Refused(Refusal::ClockUntrusted { skew_seconds }) => {
            assert_eq!(
                skew_seconds, 600,
                "the runner is ahead, so the skew is positive"
            );
        }
        other => panic!("expected an untrusted clock, got {other:?}"),
    }

    // The controller is far ahead of this runner. Before `sent_at`
    // existed, this case reported an expired lease and sent the operator
    // after the wrong problem.
    let mut ahead = envelope(Operation::Health);
    ahead.sent_at = at(9_000);
    ahead.lease_expires_at = at(9_030);
    match fence.admit(&ahead).unwrap() {
        Admission::Refused(Refusal::ClockUntrusted { skew_seconds }) => {
            assert_eq!(
                skew_seconds, -8_000,
                "the runner is behind, so the skew is negative"
            );
        }
        other => panic!("expected an untrusted clock, got {other:?}"),
    }

    // Inside the skew, the lease is judged normally.
    let mut close = envelope(Operation::Health);
    close.sent_at = at(1_030);
    close.lease_expires_at = at(1_060);
    assert!(matches!(fence.admit(&close).unwrap(), Admission::Allowed));
}

#[test]
fn a_later_controller_fences_out_the_one_before_it() {
    let fence = fence();

    // A mutation at epoch 5 is what records the epoch. Nothing else does.
    let mut newer = change(1, "digest-a", "req-new");
    newer.epoch = 5;
    assert!(matches!(fence.admit(&newer).unwrap(), Admission::Allowed));
    assert_eq!(fence.accepted_epoch().unwrap(), 5);

    // The controller that held epoch 4 is refused, even for work it
    // started before it was replaced.
    let mut older = envelope(Operation::Health);
    older.epoch = 4;
    assert!(matches!(
        fence.admit(&older).unwrap(),
        Admission::Refused(Refusal::StaleEpoch {
            yours: 4,
            theirs: 5
        })
    ));

    // The same epoch is still allowed: a lease renewal does not raise it.
    let mut same = envelope(Operation::Health);
    same.epoch = 5;
    assert!(matches!(fence.admit(&same).unwrap(), Admission::Allowed));
}

#[test]
fn a_retried_mutation_answers_from_memory_instead_of_running_twice() {
    let fence = fence();
    let request = change(1, "digest-a", "req-once");

    assert!(matches!(fence.admit(&request).unwrap(), Admission::Allowed));
    fence.finish("req-once", "{\"reply\":\"changed\"}").unwrap();

    // The controller retried because it never saw the answer. The work
    // must not happen a second time.
    match fence.admit(&request).unwrap() {
        Admission::AlreadyDone(recorded) => {
            assert_eq!(recorded, "{\"reply\":\"changed\"}");
        }
        other => panic!("expected the recorded outcome, got {other:?}"),
    }

    // A different attempt at the same work is new work.
    let again = change(1, "digest-a", "req-twice");
    assert!(matches!(fence.admit(&again).unwrap(), Admission::Allowed));
}

#[test]
fn a_runner_cannot_be_talked_backwards() {
    // MULTI-NODE 11.4: a controller with a restored, older database must
    // not be able to lower what a runner has accepted.
    let store = SqliteFence::in_memory().unwrap();
    store.accept_epoch(9).unwrap();
    store.accept_epoch(2).unwrap();
    assert_eq!(store.accepted_epoch().unwrap(), 9);
}

#[test]
fn a_read_only_operation_never_moves_the_epoch() {
    // This is what lets a recovering controller read fencing state before
    // it is allowed to change anything (MULTI-NODE 11.4).
    let store = SqliteFence::in_memory().unwrap();
    let fence = Fence::new(MACHINE, Box::new(store)).with_clock(|| at(1_000));
    let mut envelope = envelope(Operation::Health);
    envelope.epoch = 42;
    assert!(!envelope.op.mutates());
    assert!(matches!(
        fence.admit(&envelope).unwrap(),
        Admission::Allowed
    ));
    assert_eq!(
        fence.accepted_epoch().unwrap(),
        0,
        "a read recorded an epoch"
    );
}

#[tokio::test]
async fn a_permitted_request_reaches_the_host() {
    let fence = fence();
    let outcome = serve_one(&fence, &FakeHost, envelope(Operation::Capabilities))
        .await
        .unwrap();
    let Outcome::Done(Reply::Capabilities(capabilities)) = outcome else {
        panic!("expected capabilities, got {outcome:?}");
    };
    assert_eq!(capabilities.arch, "aarch64");
    assert_eq!(capabilities.storage_available_gib, 154);
}

/// A sample is a read: the controller sends one to every machine every
/// 30 seconds, and it must not record a generation or take a fence slot
/// (SPEC 14.4, MULTI-NODE 11.5).
#[tokio::test]
async fn a_sample_is_a_read_and_carries_counters_rather_than_rates() {
    let fence = fence();
    let envelope = envelope(Operation::Sample);
    assert!(!envelope.op.mutates());
    let outcome = serve_one(&fence, &FakeHost, envelope).await.unwrap();
    let Outcome::Done(Reply::Samples(samples)) = outcome else {
        panic!("expected samples, got {outcome:?}");
    };
    assert_eq!(samples.host.cpu_count, 8);
    assert_eq!(samples.domains.len(), 1);
    let domain = &samples.domains[0];
    // The UUID identifies the guest, not the name, so a rename in flight
    // cannot move a reading onto another guest (SPEC 7.2).
    assert_eq!(domain.uuid, instance().uuid);
    assert_eq!(domain.cpu_time_ns, 30_000_000_000);
    assert_eq!(
        fence.accepted_epoch().unwrap(),
        0,
        "a sample recorded an epoch"
    );
}

#[tokio::test]
async fn a_refused_request_never_reaches_the_host() {
    struct Exploding;
    #[async_trait::async_trait]
    impl Host for Exploding {
        async fn health(&self) -> Result<Health, HostError> {
            panic!("the host was asked despite a refusal");
        }
        async fn capabilities(&self) -> Result<Capabilities, HostError> {
            panic!("the host was asked despite a refusal");
        }
        async fn inventory(&self) -> Result<Inventory, HostError> {
            panic!("the host was asked despite a refusal");
        }
        async fn sample(&self) -> Result<crate::Samples, HostError> {
            panic!("the host was asked despite a refusal");
        }
        async fn ensure_image(&self, _: &crate::ImageRequest) -> Result<crate::Reply, HostError> {
            panic!("the host was asked despite a refusal");
        }
        async fn provision(&self, _: &crate::ProvisionRequest) -> Result<crate::Reply, HostError> {
            panic!("the host was asked despite a refusal");
        }
        async fn apply_network(
            &self,
            _: &bento_network::MachineNetwork,
        ) -> Result<crate::Reply, HostError> {
            panic!("the host was asked despite a refusal");
        }
        async fn start(&self, _: &InstanceRef) -> Result<bento_types::State, HostError> {
            panic!("the host was asked despite a refusal");
        }
        async fn stop(&self, _: &InstanceRef) -> Result<bento_types::State, HostError> {
            panic!("the host was asked despite a refusal");
        }
        async fn reboot(&self, _: &InstanceRef) -> Result<bento_types::State, HostError> {
            panic!("the host was asked despite a refusal");
        }
        async fn remove(&self, _: &InstanceRef) -> Result<bento_types::State, HostError> {
            panic!("the host was asked despite a refusal");
        }
    }
    let fence = fence();
    let mut envelope = envelope(Operation::Inventory);
    envelope.target_machine_id = Some(OTHER.into());
    let outcome = serve_one(&fence, &Exploding, envelope).await.unwrap();
    assert!(matches!(
        outcome,
        Outcome::Refused(Refusal::WrongMachine { .. })
    ));
}

#[test]
fn what_a_runner_accepted_survives_a_restart() {
    // The whole reason this is on disk rather than in memory: a runner
    // that forgets on reboot would accept the controller it just fenced.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fence.db");
    {
        let store = SqliteFence::open(&path).unwrap();
        store.accept_epoch(11).unwrap();
        store
            .record_outcome("req-7", "{\"reply\":\"health\"}")
            .unwrap();
    }
    let reopened = SqliteFence::open(&path).unwrap();
    assert_eq!(reopened.accepted_epoch().unwrap(), 11);
    assert_eq!(
        reopened.outcome("req-7").unwrap().as_deref(),
        Some("{\"reply\":\"health\"}")
    );
    assert_eq!(reopened.outcome("req-8").unwrap(), None);
}

#[test]
fn the_envelope_is_json_both_ways() {
    let envelope = envelope(Operation::Inventory);
    let text = serde_json::to_string(&envelope).unwrap();
    assert!(text.contains("\"op\":\"inventory\""), "{text}");
    assert_eq!(
        serde_json::from_str::<Envelope>(&text).unwrap(),
        envelope,
        "an envelope must survive the wire unchanged"
    );
}
