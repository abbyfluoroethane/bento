//! The one Bento nftables table (SPEC 6.3), built from database state and
//! applied atomically. The control plane applies it at startup and every poll
//! tick; applying an unchanged ruleset is skipped.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;

use anyhow::Result;
use bento_network::{
    Applier, FirewallUser, Plan, PortRange, PublishedInstance, Ruleset, UserNetwork,
};
use bento_store::Store;
use bento_types::Visibility;
use tokio::sync::Mutex;

pub(crate) struct Firewall {
    store: Store,
    plan: Plan,
    applier: Arc<dyn Applier>,
    high_ports: PortRange,
    last: Mutex<String>,
}

impl Firewall {
    pub(crate) fn new(
        store: Store,
        plan: Plan,
        applier: Arc<dyn Applier>,
        high_ports: PortRange,
    ) -> Self {
        Self {
            store,
            plan,
            applier,
            high_ports,
            last: Mutex::new(String::new()),
        }
    }

    /// Rebuilds the ruleset and applies it when it changed since the last
    /// successful apply.
    pub(crate) async fn reload(&self) -> Result<()> {
        let mut last = self.last.lock().await;
        let ruleset = build_ruleset(&self.store, self.plan, self.high_ports).await?;
        let text = ruleset.render()?;
        if text == *last {
            return Ok(());
        }
        self.applier
            .apply_ruleset(&text)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        *last = text;
        tracing::info!(
            users = ruleset.users.len(),
            "firewall: nftables table reloaded"
        );
        Ok(())
    }
}

/// Derives SPEC 6.3 policy from users and instances. Every user contributes a
/// bridge. Port 22 is published for every instance so the SSH frontend can
/// reach it; private and public instances also publish their HTTP ports.
pub(crate) async fn build_ruleset(
    store: &Store,
    plan: Plan,
    mut high_ports: PortRange,
) -> Result<Ruleset> {
    if high_ports.from == 0 && high_ports.to == 0 {
        high_ports = PortRange {
            from: i32::from(bento_proxy::HIGH_PORT_MIN),
            to: i32::from(bento_proxy::HIGH_PORT_MAX),
        };
    }
    let users = store.users().await?;
    let instances = store.instances().await?;
    let mut by_owner: HashMap<i64, Vec<PublishedInstance>> = HashMap::new();
    for instance in instances {
        let Ok(address) = instance.address.parse::<Ipv4Addr>() else {
            continue;
        };
        let mut published = PublishedInstance {
            address,
            http_ports: Vec::new(),
            port_ranges: Vec::new(),
        };
        if matches!(
            instance.visibility,
            Visibility::Private | Visibility::Public
        ) {
            published
                .http_ports
                .push(i32::from(if instance.http_port == 0 {
                    80
                } else {
                    instance.http_port
                }));
            published.port_ranges.push(high_ports);
        }
        by_owner
            .entry(instance.owner_id)
            .or_default()
            .push(published);
    }
    let mut firewall_users = Vec::new();
    for user in users {
        let Ok(prefix) = bento_config::parse_prefix(&user.subnet) else {
            continue;
        };
        let Ok(index) = plan.index(prefix) else {
            continue;
        };
        firewall_users.push(FirewallUser {
            network: UserNetwork::new(plan, index as isize)?,
            instances: by_owner.remove(&user.id).unwrap_or_default(),
        });
    }
    Ok(Ruleset {
        private_range: plan.range(),
        users: firewall_users,
    })
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
            .ensure_host("testhost", "qemu:///system")
            .await
            .unwrap();
        (dir, store, plan, user)
    }

    #[tokio::test]
    async fn off_instance_keeps_ssh_only() {
        let (_dir, store, plan, user) = store_with_user().await;
        store
            .upsert_image(Image {
                name: "debian-13".into(),
                url: "https://example.test/image".into(),
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
                    visibility: Visibility::Off,
                    created_at: OffsetDateTime::now_utc(),
                    last_seen_at: None,
                },
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
        let text = build_ruleset(&store, plan, PortRange { from: 0, to: 0 })
            .await
            .unwrap()
            .render()
            .unwrap();
        assert!(text.contains("ip daddr 10.100.0.2 tcp dport { 22 } accept"));
        assert!(!text.contains("3000-9999"));
    }

    #[tokio::test]
    async fn reload_skips_unchanged() {
        let (_dir, store, plan, _) = store_with_user().await;
        let applier = Arc::new(RecordingApplier::default());
        let firewall = Firewall::new(
            store.clone(),
            plan,
            applier.clone(),
            PortRange { from: 0, to: 0 },
        );
        firewall.reload().await.unwrap();
        firewall.reload().await.unwrap();
        assert_eq!(applier.0.lock().unwrap().len(), 1);
        store
            .register_user("blair", "blair@example.org", None, plan.range())
            .await
            .unwrap();
        firewall.reload().await.unwrap();
        assert_eq!(applier.0.lock().unwrap().len(), 2);
    }
}
