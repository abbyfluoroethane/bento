use std::collections::{BTreeSet, HashSet};
use std::fmt::Write;
use std::net::Ipv4Addr;

use crate::subnet::{contains, format_prefix, masked, require_slash_24};
use crate::{Ipv4Prefix, Result, UserNetwork, invalid};

/// An inclusive TCP port range the host may reach on an instance, such
/// as the 3000-9999 proxy range of SPEC 9.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PortRange {
    pub from: i32,
    pub to: i32,
}

/// One instance as the firewall sees it: its address and the HTTP ports
/// the host may reach. Port 22 is always reachable from the host and
/// need not be listed.
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

impl Ruleset {
    /// Generates the full nftables ruleset text. The text deletes and
    /// redefines the whole table in one nft transaction, so applying it
    /// via `nft -f -` is an atomic full-table reload: a partial rule
    /// update never leaves a period with the wrong policy (SPEC 6.3).
    ///
    /// The table implements exactly the five rules of SPEC 6.3:
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

        text.push_str("\tchain forward {\n");
        text.push_str("\t\ttype filter hook forward priority filter; policy drop;\n");
        text.push_str("\t\tct state established,related accept\n");
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
        text.push_str("\t\t# rule 4: permit traffic within a user's bridge\n");
        for user in &users {
            writeln!(
                text,
                "\t\tiifname {:?} oifname {:?} accept",
                user.network.bridge, user.network.bridge
            )
            .expect("writing to a String cannot fail");
        }
        text.push_str("\t\t# rule 2: permit instance egress to the internet\n");
        for user in &users {
            writeln!(text, "\t\tiifname {:?} accept", user.network.bridge)
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
            ("egress 1", got.find(r#"iifname "bento1" accept"#)),
            ("egress 2", got.find(r#"iifname "bento2" accept"#)),
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
