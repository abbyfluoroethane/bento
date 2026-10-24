use std::collections::{BTreeSet, HashSet};
use std::fmt::Write;
use std::net::Ipv4Addr;

use crate::slot::Slots;
use crate::subnet::{contains, format_prefix, masked, require_slash_24};
use crate::{Ipv4Prefix, Result, UserNetwork, invalid};

/// An inclusive TCP port range the host may reach on an instance, such
/// as the 3000-9999 proxy range of SPEC 9.1.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct PortRange {
    pub from: i32,
    pub to: i32,
}

/// One instance as the firewall sees it: its address and the HTTP ports
/// the host may reach. Port 22 is always reachable from the host and
/// need not be listed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublishedInstance {
    pub address: Ipv4Addr,
    pub http_ports: Vec<i32>,
    pub port_ranges: Vec<PortRange>,
}

/// One user's network and instances as the firewall sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirewallUser {
    pub network: UserNetwork,
    pub instances: Vec<PublishedInstance>,
}

/// The complete input for the one Bento nftables table (SPEC 6.3).
/// Bento owns every rule in that table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ruleset {
    /// The whole operator-configured private range. Egress leaving this
    /// range is masqueraded.
    pub private_range: Ipv4Prefix,
    /// Every user with a network. Order does not matter to the policy;
    /// [`Ruleset::render`] sorts by bridge name so output is stable.
    pub users: Vec<FirewallUser>,
    /// Whether another machine runs guests for these users.
    ///
    /// When it does, a guest packet can reach this machine over the
    /// underlay rather than from a local bridge, and the policy must
    /// admit it (MULTI-NODE 8.4). Leaving this false on a single-machine
    /// deployment keeps exactly the version-1 policy, so nothing that
    /// arrives from outside a user bridge is ever forwarded to a guest.
    pub underlay_peers: bool,
    /// Frontend addresses on other machines that may reach a local
    /// instance's published ports (MULTI-NODE 8.4).
    ///
    /// A frontend on this machine needs no entry: it reaches a local
    /// guest through the `output` chain. This list exists for the
    /// machines that do not run the frontend, whose guests the frontend
    /// reaches over the underlay.
    pub frontends: Vec<Ipv4Addr>,
    /// How each user `/24` divides into runner slots (MULTI-NODE 7.1).
    pub runner_prefix: u8,
    /// The slots this machine does not own. Every other slot is local.
    /// The anti-spoofing and underlay rules read slot ownership from
    /// this list (MULTI-NODE 9).
    pub remote_slots: Vec<u32>,
    /// Addresses a guest must never reach: every runner endpoint and
    /// underlay address and the controller (MULTI-NODE 11.1). They sit
    /// outside the private range, so the egress accept alone would let a
    /// guest through to them.
    pub management: Vec<Ipv4Addr>,
}

impl Ruleset {
    /// Generates the full nftables ruleset text. The text deletes and
    /// redefines the whole table in one nft transaction, so applying it
    /// via `nft -f -` is an atomic full-table reload: a partial rule
    /// update never leaves a period with the wrong policy (SPEC 6.3).
    ///
    /// The table implements the five rules of SPEC 6.3, plus guard rules
    /// that keep a guest off the host and off the management network and
    /// tie each guest to its own address (SPEC 6.3, MULTI-NODE 9):
    ///
    /// 1. Host to instance on port 22 and the published HTTP ports.
    /// 2. Instance egress to the internet.
    /// 3. Drop between the bridges of two different users.
    /// 4. Permit within a user's bridge.
    /// 5. Masquerade instance egress behind the host address.
    ///
    /// Rule 4 is a decision, not an oversight. A user who runs a database
    /// instance and a web instance expects the two to communicate. The
    /// isolation boundary in version 1 is the user, not the instance.
    ///
    /// The inter-user drop (rule 3) precedes the egress accept (rule 2):
    /// the egress accept matches any traffic entering from a user bridge,
    /// so cross-bridge traffic must be dropped before it.
    pub fn render(&self) -> Result<String> {
        if self.private_range.bits > 32 {
            return Err(invalid(format!(
                "ruleset private range {} is not IPv4",
                format_prefix(self.private_range)
            )));
        }
        let slots = Slots::new(self.runner_prefix)?;
        for slot in &self.remote_slots {
            if *slot >= slots.count() {
                return Err(invalid(format!(
                    "remote slot {slot} does not exist at /{}",
                    self.runner_prefix
                )));
            }
        }
        let remote: BTreeSet<u32> = self.remote_slots.iter().copied().collect();
        let management: BTreeSet<Ipv4Addr> = self.management.iter().copied().collect();
        let mut users = self.users.clone();
        users.sort_by(|left, right| left.network.bridge.cmp(&right.network.bridge));

        let mut seen = HashSet::with_capacity(users.len());
        for user in &users {
            if !safe_interface_name(&user.network.bridge) {
                return Err(invalid(format!(
                    "unsafe bridge name {:?}",
                    user.network.bridge
                )));
            }
            if !seen.insert(user.network.bridge.as_str()) {
                return Err(invalid(format!(
                    "duplicate bridge name {:?}",
                    user.network.bridge
                )));
            }
            require_slash_24(user.network.subnet)?;
            for instance in &user.instances {
                if !contains(user.network.subnet, instance.address) {
                    return Err(invalid(format!(
                        "instance address {} is outside subnet {}",
                        instance.address,
                        format_prefix(user.network.subnet)
                    )));
                }
                for port in &instance.http_ports {
                    if !(1..=65_535).contains(port) {
                        return Err(invalid(format!(
                            "invalid port {port} for instance {}",
                            instance.address
                        )));
                    }
                }
                for range in &instance.port_ranges {
                    if range.from < 1 || range.to > 65_535 || range.from > range.to {
                        return Err(invalid(format!(
                            "invalid port range {}-{} for instance {}",
                            range.from, range.to, instance.address
                        )));
                    }
                }
            }
        }

        let mut text = String::new();
        text.push_str(
            "# Bento nftables table. Bento owns every rule here; do not edit by hand (SPEC 6.3).\n",
        );
        text.push_str(
            "# The whole table is deleted and redefined in one transaction on every change.\n",
        );
        text.push_str("add table inet bento\n");
        text.push_str("delete table inet bento\n");
        text.push_str("table inet bento {\n");

        text.push_str("\tchain output {\n");
        text.push_str("\t\ttype filter hook output priority filter; policy accept;\n");
        text.push_str("\t\t# rule 1: host to instance on port 22 and the published HTTP ports\n");
        for user in &users {
            for instance in &user.instances {
                writeln!(
                    text,
                    "\t\toifname {:?} ip daddr {} tcp dport {{ {} }} accept",
                    user.network.bridge,
                    instance.address,
                    port_set(&instance.http_ports, &instance.port_ranges)
                )
                .expect("writing to a String cannot fail");
            }
            writeln!(text, "\t\toifname {:?} drop", user.network.bridge)
                .expect("writing to a String cannot fail");
        }
        text.push_str("\t}\n");

        // A guest has no DHCP and no resolver on the host (SPEC 6.2), so it
        // needs almost nothing from the host's own addresses. ICMP is kept,
        // for ping of the gateway and path-MTU discovery, and TCP 80 and 443
        // for public ingress (SPEC 9.2). Everything else
        // a guest sends to the host, such as the runner port or any
        // service on the LAN address, is dropped (MULTI-NODE 11.1).
        text.push_str("\tchain input {\n");
        text.push_str("\t\ttype filter hook input priority filter; policy accept;\n");
        text.push_str("\t\tct state established,related accept\n");
        text.push_str("\t\t# rule 8: a guest cannot reach the host's own addresses\n");
        for user in &users {
            writeln!(
                text,
                "\t\tiifname {:?} icmp type {{ echo-request, destination-unreachable, time-exceeded }} accept",
                user.network.bridge
            )
            .expect("writing to a String cannot fail");
            // Public ingress is public anyway, and a VM that calls its own
            // public URL reaches the host through hairpin NAT. The proxy
            // enforces visibility per instance (SPEC 9.2).
            writeln!(
                text,
                "\t\tiifname {:?} tcp dport {{ 80, 443 }} accept",
                user.network.bridge
            )
            .expect("writing to a String cannot fail");
            writeln!(text, "\t\tiifname {:?} drop", user.network.bridge)
                .expect("writing to a String cannot fail");
        }
        text.push_str("\t}\n");

        text.push_str("\tchain forward {\n");
        text.push_str("\t\ttype filter hook forward priority filter; policy drop;\n");
        text.push_str("\t\tct state established,related accept\n");
        // Anti-spoofing: a bridge carries only the addresses its machine
        // owns. This precedes every accept, so no later rule can pass a
        // claimed source from another user or another machine's slot
        // (MULTI-NODE 9).
        text.push_str("\t\t# rule 9: a guest uses only its own source address\n");
        for user in &users {
            let local = local_prefixes(&slots, user.network.subnet, &remote)?;
            writeln!(
                text,
                "\t\tiifname {:?} ip saddr != {} drop",
                user.network.bridge,
                prefix_set(&local)
            )
            .expect("writing to a String cannot fail");
        }
        if !management.is_empty() {
            text.push_str("\t\t# rule 10: a guest cannot reach a management address\n");
            for user in &users {
                writeln!(
                    text,
                    "\t\tiifname {:?} ip daddr {} drop",
                    user.network.bridge,
                    addr_set(&management)
                )
                .expect("writing to a String cannot fail");
            }
        }
        text.push_str("\t\t# rule 3: drop traffic between the bridges of two different users\n");
        for user in &users {
            for other in &users {
                if user.network.bridge == other.network.bridge {
                    continue;
                }
                writeln!(
                    text,
                    "\t\tiifname {:?} oifname {:?} drop",
                    user.network.bridge, other.network.bridge
                )
                .expect("writing to a String cannot fail");
            }
        }
        if self.underlay_peers {
            // A guest of this user may run on another machine. Its packets
            // arrive over the underlay rather than from a user bridge, so
            // the bridge-to-bridge rules below never see them
            // (MULTI-NODE 8.4). The source must lie in a slot another
            // machine owns, the destination in a slot this machine owns,
            // and both in the same user network, which is what stops the
            // underlay carrying traffic between two different users. The
            // source address is the guest's own; no machine rewrites it.
            //
            // No setting names the underlay interface, so the match is on
            // what the packet is not: it did not come from a user bridge.
            // A bridge is already held to its own addresses by rule 9.
            text.push_str(
                "\t\t# rule 6: same user, different machines, arriving over the underlay\n",
            );
            let bridge_set = users
                .iter()
                .map(|user| format!("{:?}", user.network.bridge))
                .collect::<Vec<_>>()
                .join(", ");
            for user in &users {
                let local = local_prefixes(&slots, user.network.subnet, &remote)?;
                let far = remote_prefixes(&slots, user.network.subnet, &remote)?;
                if local.is_empty() || far.is_empty() {
                    continue;
                }
                writeln!(
                    text,
                    "\t\tiifname != {{ {bridge_set} }} ip saddr {} ip daddr {} oifname {:?} accept",
                    prefix_set(&far),
                    prefix_set(&local),
                    user.network.bridge,
                )
                .expect("writing to a String cannot fail");
            }
        }
        if !self.frontends.is_empty() {
            // The frontend runs on one machine and the guest may run on
            // another. Reaching it is then a forwarded packet, not one
            // this machine originates, so the `output` rule above does not
            // cover it (MULTI-NODE 8.4). Only the named frontend
            // addresses and only the published ports are permitted.
            text.push_str("\t\t# rule 7: a frontend on another machine reaching an instance\n");
            for user in &users {
                for instance in &user.instances {
                    for frontend in &self.frontends {
                        writeln!(
                            text,
                            "\t\tip saddr {frontend} oifname {:?} ip daddr {} tcp dport {{ {} }} accept",
                            user.network.bridge,
                            instance.address,
                            port_set(&instance.http_ports, &instance.port_ranges)
                        )
                        .expect("writing to a String cannot fail");
                    }
                }
            }
        }
        text.push_str("\t\t# rule 4: permit traffic within a user's bridge\n");
        for user in &users {
            writeln!(
                text,
                "\t\tiifname {:?} oifname {:?} accept",
                user.network.bridge, user.network.bridge
            )
            .expect("writing to a String cannot fail");
        }
        if self.underlay_peers {
            // The slot route sends a packet for a guest on another machine
            // out through the underlay. Only the user's own network may
            // take that path (MULTI-NODE 9, predicate 2).
            text.push_str(
                "\t\t# rule 2b: same user, different machines, leaving over the underlay\n",
            );
            for user in &users {
                writeln!(
                    text,
                    "\t\tiifname {:?} ip daddr {} accept",
                    user.network.bridge,
                    format_prefix(user.network.subnet)
                )
                .expect("writing to a String cannot fail");
            }
        }
        // Egress leaves the private range. Nothing inside it is reachable
        // except through rules 4, 2b, and 6 (MULTI-NODE 9, predicate 5).
        text.push_str("\t\t# rule 2: permit instance egress to the internet\n");
        for user in &users {
            writeln!(
                text,
                "\t\tiifname {:?} ip daddr != {} accept",
                user.network.bridge,
                format_prefix(masked(self.private_range))
            )
            .expect("writing to a String cannot fail");
        }
        text.push_str("\t}\n");

        text.push_str("\tchain postrouting {\n");
        text.push_str("\t\ttype nat hook postrouting priority srcnat; policy accept;\n");
        text.push_str("\t\t# rule 5: masquerade instance egress behind the host address\n");
        for user in &users {
            writeln!(
                text,
                "\t\tip saddr {} ip daddr != {} masquerade",
                format_prefix(user.network.subnet),
                format_prefix(masked(self.private_range))
            )
            .expect("writing to a String cannot fail");
        }
        text.push_str("\t}\n");
        text.push_str("}\n");
        Ok(text)
    }
}

// Interface and chain names must be safe to interpolate into nft syntax.
// nft has no general escaping, so reject instead (the SPEC 4.2 rationale
// applies here too).
fn safe_interface_name(name: &str) -> bool {
    (1..=15).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

// The prefixes of a user network this machine owns. With no remote slot
// that is the whole network.
fn local_prefixes(
    slots: &Slots,
    subnet: Ipv4Prefix,
    remote: &BTreeSet<u32>,
) -> Result<Vec<Ipv4Prefix>> {
    if remote.is_empty() {
        return Ok(vec![masked(subnet)]);
    }
    (0..slots.count())
        .filter(|slot| !remote.contains(slot))
        .map(|slot| slots.prefix(subnet, slot))
        .collect()
}

// The prefixes of a user network that other machines own.
fn remote_prefixes(
    slots: &Slots,
    subnet: Ipv4Prefix,
    remote: &BTreeSet<u32>,
) -> Result<Vec<Ipv4Prefix>> {
    remote
        .iter()
        .map(|slot| slots.prefix(subnet, *slot))
        .collect()
}

// One prefix renders bare; several render as an nft anonymous set.
fn prefix_set(prefixes: &[Ipv4Prefix]) -> String {
    let parts: Vec<String> = prefixes
        .iter()
        .map(|prefix| format_prefix(*prefix))
        .collect();
    if let [one] = parts.as_slice() {
        return one.clone();
    }
    format!("{{ {} }}", parts.join(", "))
}

fn addr_set(addresses: &BTreeSet<Ipv4Addr>) -> String {
    let parts: Vec<String> = addresses.iter().map(ToString::to_string).collect();
    format!("{{ {} }}", parts.join(", "))
}

// Renders the host-reachable ports of an instance as an nft set body:
// port 22 plus the published HTTP ports and ranges, deduplicated and
// sorted for stable output.
fn port_set(http_ports: &[i32], ranges: &[PortRange]) -> String {
    let mut ports = BTreeSet::from([22]);
    ports.extend(http_ports.iter().copied());
    let mut parts: Vec<String> = ports.into_iter().map(|port| port.to_string()).collect();
    let sorted_ranges: BTreeSet<PortRange> = ranges.iter().copied().collect();
    parts.extend(
        sorted_ranges
            .into_iter()
            .map(|range| format!("{}-{}", range.from, range.to)),
    );
    parts.join(", ")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn two_user_ruleset() -> Ruleset {
        Ruleset {
            private_range: bento_config::parse_prefix("10.77.0.0/16").unwrap(),
            underlay_peers: false,
            frontends: Vec::new(),
            runner_prefix: 24,
            remote_slots: Vec::new(),
            management: Vec::new(),
            users: vec![
                FirewallUser {
                    network: UserNetwork {
                        name: "bento-user-2".to_string(),
                        bridge: "bento2".to_string(),
                        subnet: bento_config::parse_prefix("10.77.2.0/24").unwrap(),
                    },
                    instances: vec![PublishedInstance {
                        address: Ipv4Addr::new(10, 77, 2, 2),
                        http_ports: vec![3000],
                        port_ranges: vec![],
                    }],
                },
                FirewallUser {
                    network: UserNetwork {
                        name: "bento-user-1".to_string(),
                        bridge: "bento1".to_string(),
                        subnet: bento_config::parse_prefix("10.77.1.0/24").unwrap(),
                    },
                    instances: vec![
                        PublishedInstance {
                            address: Ipv4Addr::new(10, 77, 1, 2),
                            http_ports: vec![80],
                            port_ranges: vec![PortRange {
                                from: 3000,
                                to: 9999,
                            }],
                        },
                        PublishedInstance {
                            address: Ipv4Addr::new(10, 77, 1, 3),
                            http_ports: vec![8080, 80, 80],
                            port_ranges: vec![],
                        },
                    ],
                },
            ],
        }
    }

    /// The same two users, but a second machine also runs guests for
    /// them and the frontend lives elsewhere (MULTI-NODE 8.4).
    fn multi_node_ruleset() -> Ruleset {
        Ruleset {
            underlay_peers: true,
            frontends: vec![Ipv4Addr::new(10, 0, 0, 188)],
            runner_prefix: 25,
            remote_slots: vec![1],
            management: vec![Ipv4Addr::new(10, 0, 0, 10), Ipv4Addr::new(10, 0, 0, 21)],
            ..two_user_ruleset()
        }
    }

    #[test]
    fn one_machine_alone_keeps_the_version_one_policy() {
        // Nothing arriving from outside a user bridge may reach a guest
        // when no other machine runs one.
        let text = two_user_ruleset().render().unwrap();
        assert!(!text.contains("rule 6"), "{text}");
        assert!(!text.contains("rule 7"), "{text}");
        assert!(text.contains("type filter hook forward priority filter; policy drop;"));
    }

    #[test]
    fn the_underlay_carries_one_users_traffic_and_not_another_users() {
        let text = multi_node_ruleset().render().unwrap();

        // Rule 6 holds both addresses to the same user network, so the
        // underlay cannot carry traffic between two users.
        assert!(
            text.contains(
                "ip saddr 10.77.2.128/25 ip daddr 10.77.2.0/25 oifname \"bento2\" accept"
            ),
            "{text}"
        );
        assert!(
            !text.contains("ip saddr 10.77.2.128/25 ip daddr 10.77.1."),
            "the underlay would carry traffic between two users:\n{text}"
        );

        // The cross-bridge drops still precede every accept.
        let drop_at = text
            .find("iifname \"bento1\" oifname \"bento2\" drop")
            .unwrap();
        let accept_at = text.find("ip saddr 10.77.2.128/25 ip daddr").unwrap();
        assert!(drop_at < accept_at, "an accept precedes the drops:\n{text}");
    }

    #[test]
    fn a_remote_frontend_reaches_only_published_ports() {
        let text = multi_node_ruleset().render().unwrap();

        // The frontend runs on another machine, so reaching a guest here
        // is a forwarded packet and the output chain never sees it.
        assert!(
            text.contains(
                "ip saddr 10.0.0.188 oifname \"bento2\" ip daddr 10.77.2.2 \
                 tcp dport { 22, 3000 } accept"
            ) || text.contains("ip saddr 10.0.0.188 oifname \"bento2\" ip daddr 10.77.2.2"),
            "{text}"
        );
        // No unrestricted accept for the frontend address.
        assert!(
            !text.contains("ip saddr 10.0.0.188 accept"),
            "the frontend may reach any port:\n{text}"
        );
    }

    #[test]
    fn guest_to_guest_across_machines_is_not_masqueraded() {
        // The destination is inside the private range, so rule 5 does not
        // match it and the guest source address survives the hop
        // (MULTI-NODE 8.4).
        let text = multi_node_ruleset().render().unwrap();
        assert!(
            text.contains("ip saddr 10.77.2.0/24 ip daddr != 10.77.0.0/16 masquerade"),
            "{text}"
        );
    }

    /// Returns the text of one chain, so a test cannot match a rule in
    /// the wrong chain.
    fn chain<'a>(text: &'a str, name: &str) -> &'a str {
        let start = text
            .find(&format!("\tchain {name} {{"))
            .unwrap_or_else(|| panic!("no {name} chain:\n{text}"));
        let rest = &text[start..];
        &rest[..rest.find("\n\t}\n").unwrap()]
    }

    #[test]
    fn a_guest_cannot_reach_the_host_itself() {
        // Without an input chain a guest reaches every service the host
        // listens on, including the unauthenticated runner port
        // (MULTI-NODE 11.1).
        let text = two_user_ruleset().render().unwrap();
        let input = chain(&text, "input");
        assert!(input.contains("hook input"), "{text}");
        assert!(
            input.contains("ct state established,related accept"),
            "{text}"
        );
        for bridge in ["bento1", "bento2"] {
            let drop = format!("iifname \"{bridge}\" drop");
            let drop_at = input
                .find(&drop)
                .unwrap_or_else(|| panic!("no drop for {bridge}:\n{text}"));
            // A guest keeps ICMP and public ingress, ahead of the drop.
            let icmp_at = input.find(&format!("iifname \"{bridge}\" icmp")).unwrap();
            assert!(icmp_at < drop_at, "{text}");
            let web = format!("iifname \"{bridge}\" tcp dport {{ 80, 443 }} accept");
            let web_at = input
                .find(&web)
                .unwrap_or_else(|| panic!("no web accept for {bridge}:\n{text}"));
            assert!(web_at < drop_at, "{text}");
            // No other TCP accept: the runner port and ssh stay closed.
            let tcp_accepts = input
                .lines()
                .filter(|l| l.contains(&format!("iifname \"{bridge}\" tcp")))
                .count();
            assert_eq!(tcp_accepts, 1, "{text}");
            assert!(!input.contains("10443"), "{text}");
            assert!(!input.contains("22"), "{text}");
            assert!(
                !input.contains(&format!("iifname \"{bridge}\" accept")),
                "{text}"
            );
        }
    }

    #[test]
    fn a_guest_cannot_forward_to_a_management_address() {
        let text = multi_node_ruleset().render().unwrap();
        let forward = chain(&text, "forward");
        for bridge in ["bento1", "bento2"] {
            let drop = format!("iifname \"{bridge}\" ip daddr {{ 10.0.0.10, 10.0.0.21 }} drop");
            let drop_at = forward
                .find(&drop)
                .unwrap_or_else(|| panic!("no management drop for {bridge}:\n{text}"));
            // No accept for this bridge precedes the drop.
            let earlier = &forward[..drop_at];
            assert!(
                !earlier
                    .lines()
                    .any(|line| line.contains(&format!("iifname \"{bridge}\""))
                        && line.ends_with("accept")),
                "an accept for {bridge} precedes the management drop:\n{text}"
            );
        }
    }

    #[test]
    fn egress_leaves_the_private_range_only() {
        // The bare `iifname accept` also passed every address inside the
        // private range and on the LAN (MULTI-NODE 9, predicate 5).
        let text = two_user_ruleset().render().unwrap();
        assert!(
            text.contains("iifname \"bento1\" ip daddr != 10.77.0.0/16 accept"),
            "{text}"
        );
        assert!(!text.contains("iifname \"bento1\" accept"), "{text}");
    }

    #[test]
    fn a_bridge_carries_only_its_own_source_addresses() {
        // Rule 2 accepted any source from a bridge. The source check
        // stops a guest claiming another user's address (MULTI-NODE 9).
        let text = two_user_ruleset().render().unwrap();
        let forward = chain(&text, "forward");
        for (bridge, subnet) in [("bento1", "10.77.1.0/24"), ("bento2", "10.77.2.0/24")] {
            let check = format!("iifname \"{bridge}\" ip saddr != {subnet} drop");
            let check_at = forward
                .find(&check)
                .unwrap_or_else(|| panic!("no source check for {bridge}:\n{text}"));
            let earlier = &forward[..check_at];
            assert!(
                !earlier
                    .lines()
                    .any(|line| line.contains(&format!("iifname \"{bridge}\""))
                        && line.ends_with("accept")),
                "an accept for {bridge} precedes the source check:\n{text}"
            );
        }
    }

    #[test]
    fn a_machine_with_remote_slots_vouches_only_for_its_own_slot() {
        // Slot 1 (10.77.1.128/25) is another machine's, so a guest here
        // cannot claim an address in it.
        let text = multi_node_ruleset().render().unwrap();
        assert!(
            text.contains("iifname \"bento1\" ip saddr != 10.77.1.0/25 drop"),
            "{text}"
        );
    }

    #[test]
    fn underlay_ingress_names_the_remote_source_and_local_destination_slots() {
        let text = multi_node_ruleset().render().unwrap();
        let rule = "iifname != { \"bento1\", \"bento2\" } ip saddr 10.77.1.128/25 \
                    ip daddr 10.77.1.0/25 oifname \"bento1\" accept";
        assert!(
            text.contains(rule),
            "rule 6 lacks an ingress match:\n{text}"
        );
        // Every underlay accept names an ingress.
        for line in forward_lines(&text, "ip saddr 10.77.") {
            if !line.contains("tcp dport") {
                assert!(line.contains("iifname"), "no ingress match: {line}");
            }
        }
        // The local slot is never a valid source from off the machine.
        assert!(
            !text.contains("ip saddr 10.77.1.0/24 ip daddr 10.77.1.0/24"),
            "{text}"
        );
    }

    fn forward_lines<'a>(text: &'a str, needle: &str) -> Vec<&'a str> {
        chain(text, "forward")
            .lines()
            .filter(|line| line.contains(needle) && line.ends_with("accept"))
            .collect()
    }

    #[test]
    fn cross_runner_egress_is_limited_to_the_users_own_network() {
        let text = multi_node_ruleset().render().unwrap();
        assert!(
            text.contains("iifname \"bento1\" ip daddr 10.77.1.0/24 accept"),
            "{text}"
        );
        assert!(
            !text.contains("iifname \"bento1\" ip daddr 10.77.2.0/24 accept"),
            "{text}"
        );
    }

    #[test]
    fn ruleset_render_rejects_a_remote_slot_that_does_not_exist() {
        let mut rules = multi_node_ruleset();
        rules.remote_slots = vec![2];
        assert!(rules.render().is_err());
    }

    #[test]
    fn ruleset_render_golden() {
        let got = two_user_ruleset().render().unwrap();
        let want = include_str!("../testdata/two_users.nft");
        assert_eq!(got, want, "ruleset mismatch");
    }

    #[test]
    fn ruleset_inter_user_drop_precedes_egress_accept() {
        let got = two_user_ruleset().render().unwrap();
        let positions = [
            (
                "drop 1->2",
                got.find(r#"iifname "bento1" oifname "bento2" drop"#),
            ),
            (
                "drop 2->1",
                got.find(r#"iifname "bento2" oifname "bento1" drop"#),
            ),
            (
                "egress 1",
                got.find(r#"iifname "bento1" ip daddr != 10.77.0.0/16 accept"#),
            ),
            (
                "egress 2",
                got.find(r#"iifname "bento2" ip daddr != 10.77.0.0/16 accept"#),
            ),
        ];
        for (name, position) in &positions {
            assert!(position.is_some(), "ruleset is missing {name}:\n{got}");
        }
        let drop = positions[0].1.unwrap();
        let drop_back = positions[1].1.unwrap();
        let egress1 = positions[2].1.unwrap();
        let egress2 = positions[3].1.unwrap();
        assert!(drop < egress1 && drop < egress2 && drop_back < egress1 && drop_back < egress2);
    }

    #[test]
    fn ruleset_render_contents() {
        let got = two_user_ruleset().render().unwrap();
        let expected = [
            "add table inet bento\n",
            "delete table inet bento\n",
            "table inet bento {\n",
            r#"oifname "bento1" ip daddr 10.77.1.2 tcp dport { 22, 80, 3000-9999 } accept"#,
            r#"oifname "bento1" ip daddr 10.77.1.3 tcp dport { 22, 80, 8080 } accept"#,
            r#"oifname "bento2" ip daddr 10.77.2.2 tcp dport { 22, 3000 } accept"#,
            r#"iifname "bento1" oifname "bento1" accept"#,
            r#"iifname "bento2" oifname "bento2" accept"#,
            "ip saddr 10.77.1.0/24 ip daddr != 10.77.0.0/16 masquerade",
            "ip saddr 10.77.2.0/24 ip daddr != 10.77.0.0/16 masquerade",
        ];
        for wanted in expected {
            assert!(
                got.contains(wanted),
                "ruleset is missing {wanted:?}:\n{got}"
            );
        }
        assert_eq!(got.matches("table inet bento").count(), 3);
    }

    #[test]
    fn ruleset_render_stable_order() {
        let original = two_user_ruleset().render().unwrap();
        let mut reordered = two_user_ruleset();
        reordered.users.swap(0, 1);
        assert_eq!(original, reordered.render().unwrap());
    }

    #[test]
    fn ruleset_render_rejects_bad_input() {
        let mutations: &[fn(&mut Ruleset)] = &[
            |rules| rules.users[0].network.bridge = r#"br"; flush ruleset; #"#.to_string(),
            |rules| rules.users[0].network.bridge = rules.users[1].network.bridge.clone(),
            |rules| rules.users[0].instances[0].address = Ipv4Addr::new(10, 99, 0, 2),
            |rules| rules.users[0].instances[0].http_ports = vec![70_000],
            |rules| rules.users[0].instances[0].http_ports = vec![0],
            |rules| {
                rules.users[0].instances[0].port_ranges = vec![PortRange {
                    from: 9999,
                    to: 3000,
                }];
            },
            |rules| {
                rules.users[0].instances[0].port_ranges = vec![PortRange {
                    from: 3000,
                    to: 70_000,
                }];
            },
            |rules| {
                rules.users[0].instances[0].port_ranges = vec![PortRange { from: 0, to: 100 }];
            },
            |rules| rules.private_range.bits = 33,
            |rules| {
                rules.users[0].network.subnet = bento_config::parse_prefix("10.77.0.0/16").unwrap();
            },
        ];
        for mutate in mutations {
            let mut ruleset = two_user_ruleset();
            mutate(&mut ruleset);
            assert!(ruleset.render().is_err(), "render accepted {ruleset:?}");
        }
    }
}
