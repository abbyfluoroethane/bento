//! `NetworkSync` and runner reboots (MULTI-NODE 8.2).

use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;

use bento_store::HostSeen;
use time::OffsetDateTime;

use super::*;

const KONATA: &str = "ebb80f403ef641deaa486417f2b6992a";
const TSUKASA: &str = "167eeb6836c44115aa084e7780e4328c";

struct CountingClient {
    applies: StdMutex<usize>,
    replies: StdMutex<VecDeque<Reply>>,
}

#[async_trait]
impl Client for CountingClient {
    async fn call(&self, _endpoint: &str, envelope: Envelope) -> Result<Reply, String> {
        if matches!(envelope.op, Operation::ApplyNetwork { .. }) {
            *self.applies.lock().unwrap() += 1;
        }
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Reply::NetworkApplied {
                bridges: 0,
                routes_added: 0,
                routes_removed: 0,
                routes_unchanged: 0,
            }))
    }
}

async fn setup() -> (tempfile::TempDir, NetworkSync, Arc<CountingClient>, Host) {
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
        store,
        lease,
        bento_network::Plan::new("10.100.0.0/16").unwrap(),
        bento_network::PortRange {
            from: 3000,
            to: 9999,
        },
        controller.id,
    )
    .with_client(client.clone());
    (directory, sync, client, runner)
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

    sync.note_boot(runner.id, "boot-2").await;
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
