//! What each machine's network should be (MULTI-NODE 8).
//!
//! One function answers that for every machine, including this one. The
//! controller applies its own answer directly and sends each runner the
//! answer for that machine, so both sides render from the same code. Two
//! code paths would drift, and a drift here is two machines overwriting
//! each other's firewall every convergence tick.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use anyhow::Result;
use bento_network::{MachineNetwork, MachineUser, Plan, PortRange, PublishedInstance, RemoteSlot};
use bento_store::Store;
use bento_types::{SlotState, Visibility};

/// Builds the desired network state of one machine.
///
/// `host_id` is the machine being described. `controller_host_id` is the
/// machine running the frontends, which is permitted to reach a guest's
/// published ports from off-machine (MULTI-NODE 8.4).
pub(crate) async fn machine_network(
    store: &Store,
    plan: Plan,
    high_ports: PortRange,
    host_id: i64,
    controller_host_id: i64,
) -> Result<MachineNetwork> {
    let deployment = store.deployment().await?;
    let hosts = store.hosts().await?;
    let by_id: HashMap<i64, &bento_types::Host> =
        hosts.iter().map(|host| (host.id, host)).collect();

    // A slot this machine does not own needs a route to the machine that
    // does (MULTI-NODE 8.2). A slot whose owner has no underlay address
    // is skipped rather than guessed: an invented next hop black-holes
    // the traffic and looks like a healthy machine.
    let mut remote_slots = Vec::new();
    for slot in store.slots().await? {
        if slot.owner_host_id == host_id {
            continue;
        }
        let Some(owner) = by_id.get(&slot.owner_host_id) else {
            continue;
        };
        let next_hop = owner
            .underlay
            .as_deref()
            .and_then(|text| text.parse::<Ipv4Addr>().ok());
        let Some(next_hop) = next_hop else {
            tracing::warn!(
                slot = slot.slot,
                owner = %owner.name,
                "slot has no route: its machine has no underlay address"
            );
            continue;
        };
        // A slot being moved has no single owner, so no machine may route
        // it yet (MULTI-NODE 17).
        if slot.state == SlotState::Moving {
            tracing::warn!(slot = slot.slot, "slot is moving, so it gets no route");
            continue;
        }
        remote_slots.push(RemoteSlot {
            slot: slot.slot as u32,
            next_hop,
        });
    }

    // Only the instances this machine runs. Another machine's instance is
    // reached by a route, and its policy is enforced where it runs.
    let mut by_owner: HashMap<i64, Vec<PublishedInstance>> = HashMap::new();
    for instance in store.instances().await? {
        if instance.host_id != host_id {
            continue;
        }
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

    // Every user gets a bridge on every machine, not only the users with
    // an instance here. The gateway `.1` then exists wherever a guest of
    // that user is placed later (MULTI-NODE 7.2).
    let mut users = Vec::new();
    for user in store.users().await? {
        let Ok(subnet) = bento_config::parse_prefix(&user.subnet) else {
            tracing::warn!(user = %user.name, subnet = %user.subnet, "user subnet skipped");
            continue;
        };
        let Ok(index) = plan.index(subnet) else {
            tracing::warn!(user = %user.name, subnet = %user.subnet, "user subnet outside range");
            continue;
        };
        users.push(MachineUser {
            index: index as i64,
            subnet,
            instances: by_owner.remove(&user.id).unwrap_or_default(),
        });
    }

    // The machine running the frontends reaches its own guests through
    // the output chain, so it needs no entry for itself.
    let mut frontends = Vec::new();
    if host_id != controller_host_id
        && let Some(controller) = by_id.get(&controller_host_id)
        && let Some(address) = controller
            .underlay
            .as_deref()
            .and_then(|text| text.parse::<Ipv4Addr>().ok())
    {
        frontends.push(address);
    }

    // Both lists are sorted so the same database state always produces
    // the same description. The controller digests this state to decide
    // whether a machine needs telling again, and an unstable order would
    // make every tick look like a change and re-apply the network.
    users.sort_by_key(|user| user.index);
    remote_slots.sort_by_key(|remote| remote.slot);

    Ok(MachineNetwork {
        private_range: plan.range(),
        runner_prefix: deployment.runner_prefix,
        users,
        remote_slots,
        frontends,
    })
}

#[cfg(test)]
mod tests {
    use bento_network::Plan;
    use bento_store::HostSeen;

    use super::*;

    const KONATA: &str = "ebb80f403ef641deaa486417f2b6992a";
    const TSUKASA: &str = "167eeb6836c44115aa084e7780e4328c";
    const HIGH_PORTS: PortRange = PortRange {
        from: 3000,
        to: 9999,
    };

    fn plan() -> Plan {
        Plan::new("10.100.0.0/16").unwrap()
    }

    /// Two machines, two users, and one `/25` slot each. This is the
    /// shape of a two-machine deployment.
    async fn fleet() -> (tempfile::TempDir, Store, i64, i64) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("bento.db"))
            .await
            .unwrap();
        // The first machine takes slot 0 by itself (MULTI-NODE 7.2).
        let konata = store
            .ensure_host(KONATA, "konata", "qemu:///system")
            .await
            .unwrap();
        let tsukasa = store
            .register_runner("tsukasa", "http://10.0.0.97:10443", "10.0.0.97")
            .await
            .unwrap();
        store
            .observe_host(
                tsukasa.id,
                HostSeen {
                    machine_id: Some(TSUKASA.into()),
                    ..HostSeen::default()
                },
            )
            .await
            .unwrap();
        // konata needs an underlay too, so the other machine can route
        // back to it.
        store
            .register_runner("konata", "http://10.0.0.188:10443", "10.0.0.188")
            .await
            .unwrap();
        store.set_runner_prefix(25).await.unwrap();
        store.claim_slot(1, tsukasa.id).await.unwrap();
        for name in ["riley", "abby"] {
            store
                .register_user(name, format!("{name}@example.org"), None, plan().range())
                .await
                .unwrap();
        }
        (directory, store, konata.id, tsukasa.id)
    }

    #[tokio::test]
    async fn each_machine_routes_only_the_slots_it_does_not_own() {
        let (_dir, store, konata, tsukasa) = fleet().await;

        let here = machine_network(&store, plan(), HIGH_PORTS, konata, konata)
            .await
            .unwrap();
        assert_eq!(here.runner_prefix, 25);
        assert_eq!(here.remote_slots.len(), 1);
        assert_eq!(here.remote_slots[0].slot, 1);
        assert_eq!(
            here.remote_slots[0].next_hop.to_string(),
            "10.0.0.97",
            "konata must send slot 1 to tsukasa"
        );

        let there = machine_network(&store, plan(), HIGH_PORTS, tsukasa, konata)
            .await
            .unwrap();
        assert_eq!(there.remote_slots.len(), 1);
        assert_eq!(there.remote_slots[0].slot, 0);
        assert_eq!(there.remote_slots[0].next_hop.to_string(), "10.0.0.188");

        // Two users, so two routes on each machine.
        assert_eq!(here.routes().unwrap().len(), 2);
        assert_eq!(there.routes().unwrap().len(), 2);
        here.check().unwrap();
        there.check().unwrap();
    }

    #[tokio::test]
    async fn only_the_machine_without_the_frontend_permits_it() {
        let (_dir, store, konata, tsukasa) = fleet().await;

        // The frontend runs on konata and reaches a local guest through
        // the output chain, so konata permits nothing extra.
        let here = machine_network(&store, plan(), HIGH_PORTS, konata, konata)
            .await
            .unwrap();
        assert!(here.frontends.is_empty());

        // tsukasa's guests are reached over the underlay, so it permits
        // konata's address (MULTI-NODE 8.4).
        let there = machine_network(&store, plan(), HIGH_PORTS, tsukasa, konata)
            .await
            .unwrap();
        assert_eq!(
            there
                .frontends
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["10.0.0.188"]
        );
    }

    #[tokio::test]
    async fn every_user_gets_a_bridge_on_every_machine() {
        // A machine carries a bridge for a user it runs nothing for, so
        // the gateway exists before an instance is placed there
        // (MULTI-NODE 7.2).
        let (_dir, store, _konata, tsukasa) = fleet().await;
        let there = machine_network(&store, plan(), HIGH_PORTS, tsukasa, tsukasa)
            .await
            .unwrap();
        assert_eq!(there.users.len(), 2);
        assert!(there.users.iter().all(|user| user.instances.is_empty()));
        assert_eq!(
            there.proxy_arp_bridges().unwrap(),
            ["bento0", "bento1"],
            "proxy ARP belongs on the user bridges"
        );
    }

    #[tokio::test]
    async fn a_machine_with_no_underlay_gets_no_route_rather_than_a_guess() {
        // An invented next hop black-holes the traffic and still looks
        // like a healthy machine (MULTI-NODE 8.3), so a slot whose owner
        // has no address is left unrouted and logged.
        let (_dir, store, konata, tsukasa) = fleet().await;
        store
            .register_runner("tsukasa", "http://10.0.0.97:10443", "")
            .await
            .unwrap();

        let here = machine_network(&store, plan(), HIGH_PORTS, konata, konata)
            .await
            .unwrap();
        assert!(
            here.remote_slots.is_empty(),
            "a slot was routed to a machine with no address"
        );
        assert!(here.routes().unwrap().is_empty());
        // With no remote slot, the machine keeps the single-machine
        // policy and admits nothing from the underlay.
        assert!(!here.ruleset().unwrap().underlay_peers);
        let _ = tsukasa;
    }

    #[tokio::test]
    async fn the_same_state_produces_the_same_description() {
        // The controller digests this to decide whether a machine needs
        // telling again. An unstable order would re-apply every tick.
        let (_dir, store, konata, _tsukasa) = fleet().await;
        let first = machine_network(&store, plan(), HIGH_PORTS, konata, konata)
            .await
            .unwrap();
        for _ in 0..5 {
            let again = machine_network(&store, plan(), HIGH_PORTS, konata, konata)
                .await
                .unwrap();
            assert_eq!(
                serde_json::to_string(&first).unwrap(),
                serde_json::to_string(&again).unwrap()
            );
        }
    }

    #[tokio::test]
    async fn a_single_machine_deployment_is_unchanged() {
        // The version-1 shape: one machine, one /24-wide slot, no routes,
        // and no underlay policy.
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("bento.db"))
            .await
            .unwrap();
        let konata = store
            .ensure_host(KONATA, "konata", "qemu:///system")
            .await
            .unwrap();
        store
            .register_user("riley", "riley@example.org", None, plan().range())
            .await
            .unwrap();

        let here = machine_network(&store, plan(), HIGH_PORTS, konata.id, konata.id)
            .await
            .unwrap();
        assert_eq!(here.runner_prefix, 24);
        assert!(here.remote_slots.is_empty());
        assert!(here.frontends.is_empty());
        assert!(here.routes().unwrap().is_empty());
        let ruleset = here.ruleset().unwrap();
        assert!(!ruleset.underlay_peers);
        assert!(ruleset.render().unwrap().contains("policy drop;"));
    }
}
