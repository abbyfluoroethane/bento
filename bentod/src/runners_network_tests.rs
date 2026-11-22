//! `NetworkSync` and runner reboots (MULTI-NODE 8.2).

use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;

use bento_store::HostSeen;
use time::OffsetDateTime;

use super::*;

const KONATA: &str = "ebb80f403ef641deaa486417f2b6992a";
const TSUKASA: &str = "167eeb6836c44115aa084e7780e4328c";

fn applied() -> Result<Reply, String> {
    Ok(Reply::NetworkApplied {
        bridges: 0,
        routes_added: 0,
        routes_removed: 0,
        routes_unchanged: 0,
    })
}

struct CountingClient {
    applies: StdMutex<usize>,
    replies: StdMutex<VecDeque<Result<Reply, String>>>,
}

#[async_trait]
impl Client for CountingClient {
    async fn call(&self, _endpoint: &str, envelope: Envelope) -> Result<Reply, String> {
        if matches!(envelope.op, Operation::ApplyNetwork { .. }) {
            *self.applies.lock().unwrap() += 1;
        }
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(applied)
    }
}

async fn setup() -> (tempfile::TempDir, NetworkSync, Arc<CountingClient>, Host) {
    let (directory, sync, client, runner, _) = setup_with_store().await;
    (directory, sync, client, runner)
}

async fn setup_with_store() -> (
    tempfile::TempDir,
    NetworkSync,
    Arc<CountingClient>,
    Host,
    Store,
) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("bento.db"))
        .await
        .unwrap();
    let controller = store
        .ensure_host(KONATA, "konata", "qemu:///system")
        .await
        .unwrap();
    let runner = store
        .register_runner("tsukasa", "http://10.0.0.97:10443", "10.0.0.97")
        .await
        .unwrap();
    store
        .observe_host(
            runner.id,
            HostSeen {
                machine_id: Some(TSUKASA.into()),
                ..HostSeen::default()
            },
        )
        .await
        .unwrap();
    let runner = store.host(runner.id).await.unwrap();
    let (_, lease) = watch::channel(Lease {
        holder_id: "controller-a".into(),
        epoch: 1,
        expires_at: OffsetDateTime::now_utc() + Duration::from_secs(60),
    });
    let client = Arc::new(CountingClient {
        applies: StdMutex::new(0),
        replies: StdMutex::new(VecDeque::new()),
    });
    let sync = NetworkSync::new(
        store.clone(),
        lease,
        bento_network::Plan::new("10.100.0.0/16").unwrap(),
        bento_network::PortRange {
            from: 3000,
            to: 9999,
        },
        controller.id,
    )
    .with_client(client.clone());
    (directory, sync, client, runner, store)
}

#[tokio::test]
async fn unchanged_state_and_an_unchanged_boot_are_not_resent() {
    let (_directory, sync, client, runner) = setup().await;
    sync.note_boot(runner.id, "boot-1").await;
    sync.sync_host(&runner).await.unwrap();
    sync.note_boot(runner.id, "boot-1").await;
    sync.sync_host(&runner).await.unwrap();
    assert_eq!(*client.applies.lock().unwrap(), 1);
}

#[tokio::test]
async fn a_rebooted_runner_gets_its_network_again() {
    // A reboot erases routes, proxy ARP, and masquerade while the
    // database stays the same (MULTI-NODE 8.2).
    let (_directory, sync, client, runner) = setup().await;
    sync.note_boot(runner.id, "boot-1").await;
    sync.sync_host(&runner).await.unwrap();

    assert!(sync.note_boot(runner.id, "boot-2").await, "a new boot ID");
    sync.sync_host(&runner).await.unwrap();
    assert_eq!(*client.applies.lock().unwrap(), 2, "resent after a reboot");

    // And it settles again once the new boot is applied.
    sync.note_boot(runner.id, "boot-2").await;
    sync.sync_host(&runner).await.unwrap();
    assert_eq!(*client.applies.lock().unwrap(), 2);
}

#[tokio::test]
async fn a_runner_that_reports_no_boot_id_is_not_resent_to() {
    let (_directory, sync, client, runner) = setup().await;
    sync.note_boot(runner.id, "").await;
    sync.sync_host(&runner).await.unwrap();
    sync.note_boot(runner.id, "").await;
    sync.sync_host(&runner).await.unwrap();
    assert_eq!(*client.applies.lock().unwrap(), 1);
}

#[tokio::test]
async fn an_old_apply_is_sent_again_and_a_new_one_is_not() {
    // Kernel state can change with no restart, so an acknowledged apply
    // is trusted for a bound only (MULTI-NODE 8.2).
    let (_directory, sync, client, runner) = setup().await;
    let start = std::time::Instant::now();
    let offset = Arc::new(StdMutex::new(Duration::ZERO));
    let clock = offset.clone();
    let sync = sync.with_clock(move || start + *clock.lock().unwrap());

    sync.sync_host(&runner).await.unwrap();
    *offset.lock().unwrap() = NETWORK_RESEND_AFTER - Duration::from_secs(1);
    sync.sync_host(&runner).await.unwrap();
    assert_eq!(*client.applies.lock().unwrap(), 1, "inside the bound");

    *offset.lock().unwrap() = NETWORK_RESEND_AFTER + Duration::from_secs(1);
    sync.sync_host(&runner).await.unwrap();
    assert_eq!(*client.applies.lock().unwrap(), 2, "past the bound");

    // The new acknowledgement starts a new bound.
    sync.sync_host(&runner).await.unwrap();
    assert_eq!(*client.applies.lock().unwrap(), 2);
}

#[tokio::test]
async fn note_boot_says_whether_the_boot_id_changed() {
    let (_directory, sync, _client, runner) = setup().await;
    assert!(!sync.note_boot(runner.id, "boot-1").await, "first sight");
    assert!(!sync.note_boot(runner.id, "boot-1").await, "same ID");
    assert!(
        !sync.note_boot(runner.id, "").await,
        "an empty ID infers nothing"
    );
    assert!(
        !sync.note_boot(runner.id, "boot-1").await,
        "empty did not reset"
    );
    assert!(sync.note_boot(runner.id, "boot-2").await, "different ID");
}

// ---- recording the result of an apply (MULTI-NODE 15) ----

async fn network_error(store: &Store, host_id: i64) -> Option<Option<String>> {
    store
        .host_network(host_id)
        .await
        .unwrap()
        .map(|result| result.error)
}

#[tokio::test]
async fn a_failed_apply_records_the_error_and_a_later_success_clears_it() {
    let (_directory, sync, client, runner, store) = setup_with_store().await;
    assert_eq!(network_error(&store, runner.id).await, None, "nothing yet");
    client
        .replies
        .lock()
        .unwrap()
        .push_back(Err("runner transport: connection refused".into()));

    assert!(!sync.converge(&runner).await);
    let error = network_error(&store, runner.id).await.unwrap().unwrap();
    assert!(error.contains("connection refused"), "{error}");

    // The failed digest is not cached, so the next tick sends again.
    assert!(sync.converge(&runner).await);
    assert_eq!(*client.applies.lock().unwrap(), 2);
    assert_eq!(network_error(&store, runner.id).await, Some(None));
}

#[tokio::test]
async fn a_skipped_send_leaves_a_recorded_error_in_place() {
    let (_directory, sync, client, runner, store) = setup_with_store().await;
    assert!(sync.converge(&runner).await);
    // Something else recorded an error. A skip must not clear it, and
    // must not write at all.
    store
        .record_network_result(runner.id, Err("route table full".into()))
        .await
        .unwrap();
    let before = store.host_network(runner.id).await.unwrap().unwrap();

    assert!(sync.converge(&runner).await, "the skip is converged");

    assert_eq!(*client.applies.lock().unwrap(), 1, "nothing was sent");
    let after = store.host_network(runner.id).await.unwrap().unwrap();
    assert_eq!(after.error.as_deref(), Some("route table full"));
    assert_eq!(after.checked_at, before.checked_at, "no write");
}

#[tokio::test]
async fn a_failed_resend_does_not_leave_the_old_digest_trusted() {
    // After a success, a resend past the bound fails. The next tick must
    // send again and not skip on the old digest.
    let (_directory, sync, client, runner, store) = setup_with_store().await;
    let start = std::time::Instant::now();
    let offset = Arc::new(StdMutex::new(Duration::ZERO));
    let clock = offset.clone();
    let sync = sync.with_clock(move || start + *clock.lock().unwrap());
    assert!(sync.converge(&runner).await);
    *offset.lock().unwrap() = NETWORK_RESEND_AFTER + Duration::from_secs(1);
    client.replies.lock().unwrap().push_back(Err("down".into()));
    assert!(!sync.converge(&runner).await);
    *offset.lock().unwrap() = NETWORK_RESEND_AFTER + Duration::from_secs(2);
    assert!(sync.converge(&runner).await);
    assert_eq!(*client.applies.lock().unwrap(), 3);
    assert_eq!(network_error(&store, runner.id).await, Some(None));
}

// ---- restore after a runner restart (MULTI-NODE 14.3) ----

use bento_runner::{Capabilities, Health};
use tokio::sync::Notify;

/// One scripted runner: health, capabilities, inventory, and network.
struct ScriptedRunner {
    boot_id: StdMutex<String>,
    apply_fails: StdMutex<bool>,
}

#[async_trait]
impl Client for ScriptedRunner {
    async fn call(&self, _endpoint: &str, envelope: Envelope) -> Result<Reply, String> {
        match envelope.op {
            Operation::Health => Ok(Reply::Health(Health {
                protocol_version: PROTOCOL_VERSION,
                machine_id: TSUKASA.into(),
                hostname: "tsukasa.example.org".into(),
                accepted_epoch: 1,
                boot_id: self.boot_id.lock().unwrap().clone(),
            })),
            Operation::Capabilities => Ok(Reply::Capabilities(Capabilities {
                arch: "aarch64".into(),
                cpu_count: 8,
                memory_total_mib: 7549,
                storage_total_gib: 164,
                storage_available_gib: 153,
                hypervisor_version: "libvirt local RPC".into(),
            })),
            Operation::ApplyNetwork { .. } if *self.apply_fails.lock().unwrap() => {
                Err("runner transport: connection refused".into())
            }
            Operation::ApplyNetwork { .. } => applied(),
            Operation::Inventory => Ok(Reply::Inventory(bento_runner::Inventory {
                domains: vec![],
            })),
            other => Err(format!("unscripted {other:?}")),
        }
    }
}

#[derive(Default)]
struct RecordingRestore {
    calls: StdMutex<Vec<i64>>,
    results: StdMutex<VecDeque<anyhow::Result<()>>>,
    /// When set, a restore waits here until the test releases it.
    gate: StdMutex<Option<Arc<Notify>>>,
}

#[async_trait]
impl HostRestore for RecordingRestore {
    async fn restore_host(&self, host_id: i64) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(host_id);
        let gate = self.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.results.lock().unwrap().pop_front().unwrap_or(Ok(()))
    }
}

struct Poll {
    _directory: tempfile::TempDir,
    task: PollTask,
    sweep: Sweep,
    runner: Arc<ScriptedRunner>,
    restore: Arc<RecordingRestore>,
    host: Host,
}

async fn poll() -> Poll {
    let (directory, sync, _client, host, store) = setup_with_store().await;
    let runner = Arc::new(ScriptedRunner {
        boot_id: StdMutex::new("boot-1".into()),
        apply_fails: StdMutex::new(false),
    });
    let restore = Arc::new(RecordingRestore::default());
    let (_, lease) = watch::channel(Lease {
        holder_id: "controller-a".into(),
        epoch: 1,
        expires_at: OffsetDateTime::now_utc() + Duration::from_secs(60),
    });
    let task = PollTask {
        store,
        client: runner.clone(),
        lease,
        request_timeout: REQUEST_TIMEOUT,
        network: Some(sync.with_client(runner.clone())),
        local_host_id: None,
        restore: Some(restore.clone()),
        restoring: Arc::default(),
    };
    Poll {
        _directory: directory,
        task,
        sweep: Sweep::default(),
        runner,
        restore,
        host,
    }
}

impl Poll {
    /// One tick, then every restore task it started runs to its end.
    async fn tick(&mut self) {
        self.start_tick().await;
        while let Some(result) = self.sweep.restore_tasks.join_next_with_id().await {
            self.task.finish_restore(result, &mut self.sweep);
        }
    }

    /// One tick; a restore task that waits at its gate keeps running.
    async fn start_tick(&mut self) {
        let image_sync = ImageSync {
            store: self.task.store.clone(),
            client: Arc::clone(&self.task.client),
            lease: self.task.lease.clone(),
            request_timeout: IMAGE_TIMEOUT,
        };
        let hosts = self.task.tick().await.unwrap();
        self.task.sweep(hosts, &image_sync, &mut self.sweep).await;
    }

    fn restores(&self) -> usize {
        self.restore.calls.lock().unwrap().len()
    }
}

#[tokio::test]
async fn a_new_boot_id_restores_once_and_the_same_id_does_not() {
    let mut poll = poll().await;
    poll.tick().await;
    assert_eq!(poll.restores(), 1, "the first healthy tick");

    poll.tick().await;
    poll.tick().await;
    assert_eq!(poll.restores(), 1, "the same boot ID later");

    *poll.runner.boot_id.lock().unwrap() = "boot-2".into();
    poll.tick().await;
    assert_eq!(*poll.restore.calls.lock().unwrap(), vec![poll.host.id; 2]);

    poll.tick().await;
    assert_eq!(poll.restores(), 2, "boot-2 is known now");
}

#[tokio::test]
async fn an_empty_boot_id_triggers_only_the_first_restore() {
    let mut poll = poll().await;
    *poll.runner.boot_id.lock().unwrap() = String::new();
    poll.tick().await;
    poll.tick().await;
    assert_eq!(poll.restores(), 1);
}

#[tokio::test]
async fn a_failed_network_apply_delays_the_restore_until_one_works() {
    let mut poll = poll().await;
    *poll.runner.apply_fails.lock().unwrap() = true;
    poll.tick().await;
    poll.tick().await;
    assert_eq!(poll.restores(), 0, "no start on a runner with no network");
    let recorded = poll.task.store.host_network(poll.host.id).await.unwrap();
    assert!(recorded.unwrap().error.is_some());

    *poll.runner.apply_fails.lock().unwrap() = false;
    poll.tick().await;
    assert_eq!(poll.restores(), 1);
    let recorded = poll.task.store.host_network(poll.host.id).await.unwrap();
    assert_eq!(recorded.unwrap().error, None, "the success cleared it");
}

#[tokio::test]
async fn a_restore_that_fails_is_tried_again() {
    let mut poll = poll().await;
    poll.restore
        .results
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!("libvirt busy")));
    poll.tick().await;
    assert_eq!(poll.restores(), 1);
    poll.tick().await;
    assert_eq!(poll.restores(), 2, "kept pending after the error");
    poll.tick().await;
    assert_eq!(poll.restores(), 2, "cleared after the success");
}

#[tokio::test]
async fn ticks_during_a_running_restore_start_no_second_task() {
    let mut poll = poll().await;
    let gate = Arc::new(Notify::new());
    *poll.restore.gate.lock().unwrap() = Some(gate.clone());
    poll.start_tick().await;
    tokio::task::yield_now().await;
    assert_eq!(poll.restores(), 1);
    assert!(poll.task.restoring.lock().unwrap().contains(&poll.host.id));

    // A new boot during the run makes a restore pending, but the host
    // has a task, so none starts.
    *poll.runner.boot_id.lock().unwrap() = "boot-2".into();
    poll.start_tick().await;
    poll.start_tick().await;
    tokio::task::yield_now().await;
    assert_eq!(poll.restores(), 1, "one task for one host");

    gate.notify_one();
    let result = poll.sweep.restore_tasks.join_next_with_id().await.unwrap();
    poll.task.finish_restore(result, &mut poll.sweep);
    assert!(poll.task.restoring.lock().unwrap().is_empty());

    // The boot change seen during the run is restored afterwards.
    *poll.restore.gate.lock().unwrap() = None;
    poll.tick().await;
    assert_eq!(poll.restores(), 2);
}

#[tokio::test]
async fn a_panicking_restore_frees_the_host() {
    struct Panics;
    #[async_trait]
    impl HostRestore for Panics {
        async fn restore_host(&self, _host_id: i64) -> anyhow::Result<()> {
            panic!("restore panicked");
        }
    }
    let mut poll = poll().await;
    poll.task.restore = Some(Arc::new(Panics));
    poll.tick().await;
    assert!(poll.task.restoring.lock().unwrap().is_empty());
    assert!(poll.sweep.restore_running.is_empty());
    assert!(poll.sweep.pending_restore.contains(&poll.host.id));
}
