//! The complete desired network state of one machine (MULTI-NODE 8).
//!
//! Both machines render from this one type: the controller applies its
//! own copy directly, and sends each runner the copy for that machine.
//! Nothing here is a path, a command, or nftables text. A machine
//! receives facts about users, slots, and neighbours, and renders its own
//! bridges, routes, proxy ARP, and firewall from them (MULTI-NODE 11.2).
//!
//! Applying it is a full re-render rather than a set of increments. The
//! nftables table is already replaced whole in one transaction (SPEC 6.3)
//! and [`crate::converge`] brings routes to the desired set, so applying
//! the same state twice changes nothing the second time. That is what
//! lets a machine recover by re-applying instead of by replaying a
//! history it may have missed.

use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use crate::slot::Slots;
use crate::subnet::format_prefix;
use crate::{
    FirewallUser, Ipv4Prefix, PublishedInstance, Result, Route, Ruleset, UserNetwork, invalid,
};

/// What one machine should have on its network, in full.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineNetwork {
    /// The whole operator-configured private range. Guest traffic leaving
    /// it is masqueraded; traffic inside it is routed between machines
    /// with the guest source address preserved (MULTI-NODE 8.4).
    pub private_range: Ipv4Prefix,
    /// How each user `/24` divides into runner slots (MULTI-NODE 7.1).
    pub runner_prefix: u8,
    /// Every user network, with the instances this machine runs.
    ///
    /// A machine carries a bridge for every user, not only for the users
    /// it currently runs an instance for (MULTI-NODE 7.2). The gateway
    /// `.1` sits on every machine's copy, so a guest always has a local
    /// gateway.
    pub users: Vec<MachineUser>,
    /// Every slot this machine does not own, and where it lives.
    ///
    /// The route list is the product of this and [`Self::users`]: one
    /// route for each user `/24` and each remote slot (MULTI-NODE 8.2).
    pub remote_slots: Vec<RemoteSlot>,
    /// Frontend addresses on other machines that may reach an instance
    /// here. Empty on the machine that runs the frontend itself.
    pub frontends: Vec<Ipv4Addr>,
}

/// One user's network as one machine sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineUser {
    /// The user's subnet index inside the private range. The bridge name
    /// and the libvirt network name both derive from it, so neither
    /// carries user-controlled text.
    pub index: i64,
    pub subnet: Ipv4Prefix,
    /// Only the instances this machine runs. Another machine's instance
    /// is reached by a route, not by a firewall rule here.
    pub instances: Vec<PublishedInstance>,
}

/// One slot owned by another machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSlot {
    pub slot: u32,
    /// The address to send that slot's traffic to (MULTI-NODE 8.5).
    pub next_hop: Ipv4Addr,
}

impl MachineNetwork {
    /// The libvirt networks this machine should define.
    pub fn networks(&self) -> Result<Vec<UserNetwork>> {
        self.users
            .iter()
            .map(|user| {
                Ok(UserNetwork {
                    name: format!("bento-user-{}", user.index),
                    bridge: format!("bento{}", user.index),
                    subnet: user.subnet,
                })
            })
            .collect()
    }

    /// The bridges that need proxy ARP.
    ///
    /// Proxy ARP goes on the user bridges and nowhere else
    /// (MULTI-NODE 8.3). A guest asks for the MAC of an address it
    /// believes is on-link; the kernel answers only because a more
    /// specific route sends that address through the underlay. Enabling
    /// it host-wide would answer for addresses Bento does not route.
    pub fn proxy_arp_bridges(&self) -> Result<Vec<String>> {
        Ok(self.networks()?.into_iter().map(|net| net.bridge).collect())
    }

    /// One route for each user `/24` and each remote slot
    /// (MULTI-NODE 8.2).
    ///
    /// This machine installs no route for a slot it owns: the bridge's
    /// connected `/24` route already covers every local destination, and
    /// a more specific route through the underlay would send a local
    /// guest's traffic off the machine.
    pub fn routes(&self) -> Result<Vec<Route>> {
        let slots = Slots::new(self.runner_prefix)?;
        // A `/24` divides into one slot, so a slot route would name the
        // whole user network. This machine carries that network on its own
        // bridge, and the bridge's connected route has the same prefix, so
        // installing the slot route would replace it. The machine would
        // then send its own guests' traffic to another machine, and every
        // guest on it would be unreachable.
        //
        // A machine that owns no slot at `/24` has no guests of that user
        // either, because there is nowhere for them to live. It needs no
        // route: dividing the `/24` is what gives it somewhere, and that
        // is what makes the routes narrower than the bridge.
        if slots.count() == 1 {
            return Ok(Vec::new());
        }
        let mut routes = Vec::with_capacity(self.users.len() * self.remote_slots.len());
        for user in &self.users {
            for remote in &self.remote_slots {
                let destination = slots.prefix(user.subnet, remote.slot)?;
                // Belt and braces for the same failure: never install a
                // route as wide as the bridge that carries the network.
                if destination.bits <= user.subnet.bits {
                    return Err(invalid(format!(
                        "slot route {} is not narrower than the user network {}",
                        format_prefix(destination),
                        format_prefix(user.subnet)
                    )));
                }
                routes.push(Route {
                    destination,
                    next_hop: remote.next_hop,
                });
            }
        }
        Ok(routes)
    }

    /// The firewall policy for this machine (SPEC 6.3, MULTI-NODE 8.4).
    pub fn ruleset(&self) -> Result<Ruleset> {
        let networks = self.networks()?;
        let users = networks
            .into_iter()
            .zip(&self.users)
            .map(|(network, user)| FirewallUser {
                network,
                instances: user.instances.clone(),
            })
            .collect();
        Ok(Ruleset {
            private_range: self.private_range,
            users,
            // A packet can arrive over the underlay only when another
            // machine runs a guest for one of these users, which is
            // exactly when this machine holds a route to a remote slot.
            underlay_peers: !self.remote_slots.is_empty(),
            frontends: self.frontends.clone(),
        })
    }

    /// Refuses a plan that could not be applied safely.
    ///
    /// A wrong next hop and a wrong slot both produce a black hole that
    /// looks like a healthy machine (MULTI-NODE 8.3), so the checks run
    /// before anything is applied rather than after.
    pub fn check(&self) -> Result<()> {
        let slots = Slots::new(self.runner_prefix)?;
        if self.private_range.bits > 24 {
            return Err(invalid(format!(
                "private range {} is narrower than /24",
                format_prefix(self.private_range)
            )));
        }
        for user in &self.users {
            crate::subnet::require_slash_24(user.subnet)?;
            if !crate::subnet::contains(self.private_range, user.subnet.addr) {
                return Err(invalid(format!(
                    "user subnet {} is outside the private range {}",
                    format_prefix(user.subnet),
                    format_prefix(self.private_range)
                )));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for remote in &self.remote_slots {
            if remote.slot >= slots.count() {
                return Err(invalid(format!(
                    "slot {} does not exist at /{}",
                    remote.slot, self.runner_prefix
                )));
            }
            if !seen.insert(remote.slot) {
                return Err(invalid(format!(
                    "slot {} has more than one next hop; \
                     two machines cannot own one slot",
                    remote.slot
                )));
            }
            // A next hop inside the private range would be reached
            // through the very routes being installed (MULTI-NODE 8.5).
            if crate::subnet::contains(self.private_range, remote.next_hop) {
                return Err(invalid(format!(
                    "next hop {} for slot {} is inside the private range {}",
                    remote.next_hop,
                    remote.slot,
                    format_prefix(self.private_range)
                )));
            }
        }
        for frontend in &self.frontends {
            if crate::subnet::contains(self.private_range, *frontend) {
                return Err(invalid(format!(
                    "frontend address {frontend} is inside the private range {}",
                    format_prefix(self.private_range)
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix(cidr: &str) -> Ipv4Prefix {
        bento_config::parse_prefix(cidr).unwrap()
    }

    /// Two users, this machine owning slot 0 and one neighbour owning
    /// slot 1 at `/25`. This is the two-machine shape.
    fn two_machines() -> MachineNetwork {
        MachineNetwork {
            private_range: prefix("10.100.0.0/16"),
            runner_prefix: 25,
            users: vec![
                MachineUser {
                    index: 0,
                    subnet: prefix("10.100.0.0/24"),
                    instances: vec![PublishedInstance {
                        address: Ipv4Addr::new(10, 100, 0, 2),
                        http_ports: vec![80],
                        port_ranges: Vec::new(),
                    }],
                },
                MachineUser {
                    index: 1,
                    subnet: prefix("10.100.1.0/24"),
                    instances: Vec::new(),
                },
            ],
            remote_slots: vec![RemoteSlot {
                slot: 1,
                next_hop: Ipv4Addr::new(10, 0, 0, 97),
            }],
            frontends: Vec::new(),
        }
    }

    #[test]
    fn one_route_per_user_and_remote_slot() {
        let routes = two_machines().routes().unwrap();
        let rendered: Vec<String> = routes
            .iter()
            .map(|route| {
                format!(
                    "{} via {}",
                    format_prefix(route.destination),
                    route.next_hop
                )
            })
            .collect();
        assert_eq!(
            rendered,
            [
                "10.100.0.128/25 via 10.0.0.97",
                "10.100.1.128/25 via 10.0.0.97",
            ]
        );
    }

    #[test]
    fn a_machine_installs_no_route_for_a_slot_it_owns() {
        // The bridge's connected /24 covers local destinations. A more
        // specific route through the underlay would send a local guest's
        // traffic off the machine and never bring it back.
        let routes = two_machines().routes().unwrap();
        for route in &routes {
            assert_ne!(
                format_prefix(route.destination),
                "10.100.0.0/25",
                "the machine routed its own slot away"
            );
        }
    }

    #[test]
    fn a_machine_never_routes_away_the_network_its_own_bridge_carries() {
        // At /24 there is one slot. A machine that does not own it would
        // otherwise install a route for the whole user network, and
        // `ip route replace` would take out the bridge's connected route
        // of the same prefix. Every guest on that machine would then be
        // unreachable, including from the machine itself.
        let mut plan = two_machines();
        plan.runner_prefix = 24;
        plan.remote_slots = vec![RemoteSlot {
            slot: 0,
            next_hop: Ipv4Addr::new(10, 0, 0, 97),
        }];

        assert!(
            plan.routes().unwrap().is_empty(),
            "a machine routed away the network its own bridge carries"
        );
    }

    #[test]
    fn every_installed_route_is_narrower_than_the_bridge() {
        for bits in 25..=27 {
            let mut plan = two_machines();
            plan.runner_prefix = bits;
            plan.remote_slots = vec![RemoteSlot {
                slot: 1,
                next_hop: Ipv4Addr::new(10, 0, 0, 97),
            }];
            for route in plan.routes().unwrap() {
                assert!(
                    route.destination.bits > 24,
                    "/{bits} installed {} which is as wide as the bridge",
                    format_prefix(route.destination)
                );
            }
        }
    }

    #[test]
    fn one_machine_alone_has_no_routes_and_no_underlay_policy() {
        let mut plan = two_machines();
        plan.remote_slots.clear();
        assert!(plan.routes().unwrap().is_empty());
        assert!(!plan.ruleset().unwrap().underlay_peers);
    }

    #[test]
    fn every_user_gets_a_bridge_even_with_no_instance_here() {
        // The gateway .1 lives on every machine's copy of a user bridge,
        // so a guest always has a local gateway (MULTI-NODE 7.1).
        let plan = two_machines();
        let networks = plan.networks().unwrap();
        assert_eq!(networks.len(), 2);
        assert_eq!(networks[1].bridge, "bento1");
        assert!(plan.users[1].instances.is_empty());
        assert_eq!(plan.proxy_arp_bridges().unwrap(), ["bento0", "bento1"]);
    }

    #[test]
    fn the_ruleset_describes_only_the_instances_on_this_machine() {
        let text = two_machines().ruleset().unwrap().render().unwrap();
        assert!(text.contains("ip daddr 10.100.0.2"), "{text}");
        // bento1 has no instance here, so it gets a bridge and a drop and
        // no accept.
        assert!(text.contains("oifname \"bento1\" drop"), "{text}");
        assert!(!text.contains("oifname \"bento1\" ip daddr"), "{text}");
    }

    #[test]
    fn check_refuses_a_plan_that_would_black_hole() {
        type Break = fn(&mut MachineNetwork);
        let cases: [(&str, Break); 5] = [
            ("next hop inside the guest range", |plan| {
                plan.remote_slots[0].next_hop = Ipv4Addr::new(10, 100, 4, 9)
            }),
            ("two machines owning one slot", |plan| {
                plan.remote_slots.push(RemoteSlot {
                    slot: 1,
                    next_hop: Ipv4Addr::new(10, 0, 0, 98),
                })
            }),
            ("a slot that does not exist", |plan| {
                plan.remote_slots[0].slot = 2
            }),
            ("a user subnet outside the range", |plan| {
                plan.users[0].subnet = prefix("192.168.9.0/24")
            }),
            ("a frontend inside the guest range", |plan| {
                plan.frontends.push(Ipv4Addr::new(10, 100, 1, 1))
            }),
        ];
        for (name, break_it) in cases {
            let mut plan = two_machines();
            break_it(&mut plan);
            assert!(plan.check().is_err(), "accepted {name}");
        }
        assert!(two_machines().check().is_ok());
    }

    #[test]
    fn a_plan_survives_the_wire_unchanged() {
        let plan = two_machines();
        let text = serde_json::to_string(&plan).unwrap();
        let back: MachineNetwork = serde_json::from_str(&text).unwrap();
        assert_eq!(plan, back);
    }
}
