use crate::subnet::{gateway, require_slash_24};
use crate::{Ipv4Prefix, Plan, Result, invalid};

/// Describes one per-user libvirt network (SPEC 6.2). Every instance of
/// the user attaches to this network's bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserNetwork {
    /// The libvirt network name, for example `bento-user-3`.
    pub name: String,
    /// The bridge device name, for example `bento3`. Linux limits
    /// interface names to 15 bytes.
    pub bridge: String,
    /// The user's `/24`.
    pub subnet: Ipv4Prefix,
}

impl UserNetwork {
    /// Returns the network for the user at a subnet index. Both names
    /// derive from the index, so they are deterministic and contain no
    /// user-controlled text.
    pub fn new(plan: Plan, index: isize) -> Result<Self> {
        Ok(Self {
            name: format!("bento-user-{index}"),
            bridge: format!("bento{index}"),
            subnet: plan.subnet(index)?,
        })
    }

    /// Renders the libvirt network definition. The forward mode is
    /// `open`: libvirt creates the bridge and installs no firewall rules,
    /// so Bento owns the whole policy (SPEC 6.2). There is no `dhcp`
    /// element; Bento assigns every address statically. The `ip` element
    /// puts the `.1` gateway address on the host side of the bridge.
    pub fn xml(&self) -> Result<String> {
        if self.name.is_empty() {
            return Err(invalid("user network has no name"));
        }
        if self.bridge.is_empty() || self.bridge.len() > 15 {
            return Err(invalid(format!(
                "bridge name {:?} is empty or longer than 15 bytes",
                self.bridge
            )));
        }
        require_slash_24(self.subnet)?;
        let name = escape_xml_text(&self.name)?;
        let bridge = escape_xml_attribute(&self.bridge)?;
        let address = gateway(self.subnet);
        Ok(format!(
            "<network>\n  <name>{name}</name>\n  <forward mode=\"open\"></forward>\n  \
             <bridge name=\"{bridge}\" stp=\"off\" delay=\"0\"></bridge>\n  \
             <ip address=\"{address}\" netmask=\"255.255.255.0\"></ip>\n</network>\n"
        ))
    }
}

fn escape_xml_text(value: &str) -> Result<String> {
    escape_xml(value, false)
}

fn escape_xml_attribute(value: &str) -> Result<String> {
    escape_xml(value, true)
}

// A fixed subset of the schema is emitted here. Every interpolated value
// is escaped so strings that reach XML are always treated as hostile
// (SPEC 4.2).
fn escape_xml(value: &str, attribute: bool) -> Result<String> {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' if attribute => escaped.push_str("&quot;"),
            '\'' if attribute => escaped.push_str("&apos;"),
            '\u{9}'
            | '\u{a}'
            | '\u{d}'
            | '\u{20}'..='\u{d7ff}'
            | '\u{e000}'..='\u{fffd}'
            | '\u{10000}'..='\u{10ffff}' => escaped.push(character),
            _ => {
                return Err(invalid(
                    "string contains a character that XML cannot represent",
                ));
            }
        }
    }
    Ok(escaped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subnet::format_prefix;

    fn prefix(cidr: &str) -> Ipv4Prefix {
        bento_config::parse_prefix(cidr).unwrap()
    }

    #[test]
    fn new_user_network() {
        let plan = Plan::new("10.77.0.0/16").unwrap();
        let network = UserNetwork::new(plan, 3).unwrap();
        assert_eq!(network.name, "bento-user-3");
        assert_eq!(network.bridge, "bento3");
        assert_eq!(format_prefix(network.subnet), "10.77.3.0/24");
        assert!(UserNetwork::new(plan, 256).is_err());
    }

    #[test]
    fn user_network_xml_golden() {
        let network = UserNetwork::new(Plan::new("10.77.0.0/16").unwrap(), 3).unwrap();
        let got = network.xml().unwrap();
        let want = include_str!("../testdata/user_network.xml");
        assert_eq!(got, want, "network XML mismatch");
    }

    #[test]
    fn user_network_xml_escapes() {
        // SPEC 4.2: treat every string that reaches XML as hostile.
        let network = UserNetwork {
            name: r#"evil"/><script>alert(1)</script>"#.to_string(),
            bridge: "bento3".to_string(),
            subnet: prefix("10.77.3.0/24"),
        };
        let got = network.xml().unwrap();
        assert!(
            !got.contains("<script>"),
            "XML contains unescaped markup:\n{got}"
        );
        assert!(
            got.contains("&lt;script&gt;"),
            "hostile name was not escaped:\n{got}"
        );
    }

    #[test]
    fn user_network_xml_rejects_bad_input() {
        let subnet = prefix("10.77.3.0/24");
        let cases = [
            UserNetwork {
                name: String::new(),
                bridge: "bento3".to_string(),
                subnet,
            },
            UserNetwork {
                name: "bento-user-3".to_string(),
                bridge: String::new(),
                subnet,
            },
            UserNetwork {
                name: "n".to_string(),
                bridge: "b".repeat(16),
                subnet,
            },
            UserNetwork {
                name: "n".to_string(),
                bridge: "bento3".to_string(),
                subnet: prefix("10.77.0.0/16"),
            },
        ];
        for network in cases {
            assert!(network.xml().is_err(), "XML accepted {network:?}");
        }
    }
}
