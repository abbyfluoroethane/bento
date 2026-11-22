//! The one Bento nftables table (SPEC 6.3), built from database state and
//! applied atomically. The control plane applies it at startup, after every
//! mutation and every convergence tick. Applying never skips: two processes
//! write the one kernel table, so a per-process memory of the last text does
//! not say what the kernel holds (SPEC 6.3).

use std::sync::Arc;

use anyhow::Result;
use bento_network::{Applier, Plan, PortRange, Ruleset};
use bento_store::Store;
#[cfg(test)]
use bento_types::Visibility;
use tokio::sync::Mutex;

pub(crate) struct Firewall {
    store: Store,
    plan: Plan,
    applier: Arc<dyn Applier>,
    high_ports: PortRange,
    /// This machine. The policy describes the guests on this machine and
    /// admits the ones that reach it over the underlay (MULTI-NODE 8.4).
    host_id: i64,
    last: Mutex<String>,
}

impl Firewall {
    pub(crate) fn new(
        store: Store,
        plan: Plan,
        applier: Arc<dyn Applier>,
        high_ports: PortRange,
        host_id: i64,
    ) -> Self {
        Self {
            store,
            plan,
            applier,
            high_ports,
            host_id,
            last: Mutex::new(String::new()),
        }
    }

    /// The one constructor for the two processes that write the table
    /// (`serve` and `sshd`). Two hand-built values would differ in port range
    /// or options, and each would overwrite the table of the other with
    /// the wrong text (SPEC 6.3).
    pub(crate) fn for_config(
        store: Store,
        plan: Plan,
        cfg: &bento_config::Config,
        applier: Arc<dyn Applier>,
        host_id: i64,
    ) -> Self {
        Self::new(
            store,
            plan,
            applier,
            PortRange {
                from: cfg.listen.proxy_port_min,
                to: cfg.listen.proxy_port_max,
            },
            host_id,
        )
    }

    /// Rebuilds the ruleset and applies it. The apply is atomic and
    /// idempotent, and it always runs: another process or `nft flush
    /// ruleset` can change the kernel table without a change of the store,
    /// so an equal text is no proof that the kernel holds it (SPEC 6.2, 6.3).
    pub(crate) async fn reload(&self) -> Result<()> {
        // One apply at a time in this process, so a slow older build cannot
        // land after a newer one.
        let mut last = self.last.lock().await;
        let ruleset = build_ruleset(&self.store, self.plan, self.high_ports, self.host_id).await?;
        let text = ruleset.render()?;
        self.applier
            .apply_ruleset(&text)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        // The memory of the last text only picks the log level.
        let changed = text != *last;
        *last = text;
        if changed {
            tracing::info!(
                users = ruleset.users.len(),
                "firewall: nftables table reloaded"
            );
        } else {
            tracing::debug!("firewall: nftables table reapplied");
        }
        Ok(())
    }
}

/// Derives this machine's SPEC 6.3 policy from its desired network state.
///
/// The policy and the routes come from one description of the machine
/// (MULTI-NODE 8), so the controller's own firewall and the firewall it
/// sends a runner cannot disagree. Two builders would drift, and a drift
/// makes two machines overwrite each other's table on every tick.
pub(crate) async fn build_ruleset(
    store: &Store,
    plan: Plan,
    mut high_ports: PortRange,
    host_id: i64,
) -> Result<Ruleset> {
    if high_ports.from == 0 && high_ports.to == 0 {
        high_ports = PortRange {
            from: bento_proxy::HIGH_PORT_MIN,
            to: bento_proxy::HIGH_PORT_MAX,
        };
    }
    let network =
        crate::netstate::machine_network(store, plan, high_ports, host_id, host_id).await?;
    Ok(network.ruleset()?)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use bento_network::DynError;
    use bento_types::{Image, ImageVersion, User};
    use time::OffsetDateTime;

    use super::*;

    #[derive(Default)]
    pub(crate) struct RecordingApplier(pub(crate) StdMutex<Vec<String>>);

    #[async_trait]
    impl Applier for RecordingApplier {
        async fn apply_ruleset(&self, ruleset: &str) -> Result<(), DynError> {
            self.0.lock().unwrap().push(ruleset.to_owned());
            Ok(())
        }
    }

    async fn store_with_user() -> (tempfile::TempDir, Store, Plan, User) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bento.db")).await.unwrap();
        let plan = Plan::new("10.100.0.0/16").unwrap();
        let user = store
            .register_user("amber", "amber@example.org", None, plan.range())
            .await
            .unwrap();
        store
            .ensure_host(
                "00000000000000000000000000000001",
                "testhost",
                "qemu:///system",
            )
            .await
            .unwrap();
        (dir, store, plan, user)
    }

    async fn create_web_instance(store: &Store, user: &User, visibility: Visibility) {
        store
            .upsert_image(Image {
                name: "debian-13".into(),
                url: "https://example.test/image".into(),
                kind: Default::default(),
                pinned_checksum: None,
                current_checksum: None,
            })
            .await
            .unwrap();
        store
            .add_image_version(ImageVersion {
                checksum: "aa11".into(),
                image_name: "debian-13".into(),
                path: "/images/aa11".into(),
                size: 1,
                kind: Default::default(),
                source_digest: None,
                fetched_at: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();
        store
            .create_instance(
                bento_types::Instance {
                    uuid: "uuid-1".into(),
                    name: "web".into(),
                    owner_id: user.id,
                    host_id: 1,
                    image_name: "debian-13".into(),
                    base_checksum: "aa11".into(),
                    state: bento_types::State::Stopped,
                    desired_state: bento_types::DesiredState::Stopped,
                    address: "10.100.0.2".into(),
                    mac: "52:54:00:00:00:01".into(),
                    vcpu: 2,
                    memory_mib: 2048,
                    disk_gib: 20,
                    nested: false,
                    ksm: true,
                    http_port: 0,
                    visibility,
                    created_at: OffsetDateTime::now_utc(),
                    last_seen_at: None,
                    slot: None,
                },
                std::time::Duration::from_secs(1),
                bento_types::Capacity::unbounded(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn off_instance_keeps_ssh_only() {
        let (_dir, store, plan, user) = store_with_user().await;
        create_web_instance(&store, &user, Visibility::Off).await;
        let text = build_ruleset(&store, plan, PortRange { from: 0, to: 0 }, 1)
            .await
            .unwrap()
            .render()
            .unwrap();
        assert!(text.contains("ip daddr 10.100.0.2 tcp dport { 22 } accept"));
        assert!(!text.contains("3000-9999"));
    }

    #[tokio::test]
    async fn reload_always_applies() {
        let (_dir, store, plan, _) = store_with_user().await;
        let applier = Arc::new(RecordingApplier::default());
        let firewall = Firewall::new(
            store.clone(),
            plan,
            applier.clone(),
            PortRange { from: 0, to: 0 },
            1,
        );
        firewall.reload().await.unwrap();
        firewall.reload().await.unwrap();
        assert_eq!(applier.0.lock().unwrap().len(), 2);
        store
            .register_user("blair", "blair@example.org", None, plan.range())
            .await
            .unwrap();
        firewall.reload().await.unwrap();
        assert_eq!(applier.0.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_flushed_table_is_restored_by_the_next_reload() {
        let (_dir, store, plan, _) = store_with_user().await;
        let applier = Arc::new(RecordingApplier::default());
        let firewall = Firewall::new(
            store,
            plan,
            applier.clone(),
            PortRange { from: 0, to: 0 },
            1,
        );
        firewall.reload().await.unwrap();
        // `nft flush ruleset` empties the kernel. The recorder stands for it.
        applier.0.lock().unwrap().clear();
        firewall.reload().await.unwrap();
        assert_eq!(applier.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn two_processes_do_not_leave_a_stale_table() {
        let (_dir, store, plan, user) = store_with_user().await;
        create_web_instance(&store, &user, Visibility::Public).await;
        let applier = Arc::new(RecordingApplier::default());
        let range = PortRange { from: 0, to: 0 };
        let serve = Firewall::new(store.clone(), plan, applier.clone(), range, 1);
        let sshd = Firewall::new(store.clone(), plan, applier.clone(), range, 1);
        serve.reload().await.unwrap();
        let public = applier.0.lock().unwrap().last().cloned().unwrap();
        store
            .set_visibility("uuid-1", Visibility::Off)
            .await
            .unwrap();
        sshd.reload().await.unwrap();
        assert_ne!(applier.0.lock().unwrap().last(), Some(&public));
        store
            .set_visibility("uuid-1", Visibility::Public)
            .await
            .unwrap();
        serve.reload().await.unwrap();
        assert_eq!(applier.0.lock().unwrap().last(), Some(&public));
    }

    #[tokio::test]
    async fn serve_and_sshd_render_the_same_table() {
        let (_dir, store, plan, user) = store_with_user().await;
        // A public VM is the one that makes the proxy range show in the text.
        create_web_instance(&store, &user, Visibility::Public).await;
        let mut cfg = bento_config::Config::default();
        cfg.listen.proxy_port_min = 20000;
        cfg.listen.proxy_port_max = 20999;
        let first = Arc::new(RecordingApplier::default());
        let second = Arc::new(RecordingApplier::default());
        Firewall::for_config(store.clone(), plan, &cfg, first.clone(), 1)
            .reload()
            .await
            .unwrap();
        Firewall::for_config(store, plan, &cfg, second.clone(), 1)
            .reload()
            .await
            .unwrap();
        let first = first.0.lock().unwrap().clone();
        assert_eq!(first, *second.0.lock().unwrap());
        assert!(first[0].contains("20000-20999"), "{}", first[0]);
    }
}
