use std::fmt::Write as _;
use std::net::IpAddr;

use crate::builder::Error;

/// The data for one NoCloud seed: hostname, one user account with the
/// owner's public keys, and the static network configuration that Bento
/// assigned (SPEC sections 5.2 and 6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    /// Becomes the NoCloud instance-id. Use the instance UUID.
    pub instance_id: String,
    /// The instance name (SPEC 5.2: host name = instance name).
    pub hostname: String,
    /// The one user account to create.
    pub user_name: String,
    /// The public keys of the owner, one per entry.
    pub authorized_keys: Vec<String>,
    /// The interface MAC address Bento assigned. The network
    /// configuration matches on it, so the guest interface name does not
    /// matter.
    pub mac: String,
    /// The static instance address with prefix length, for example
    /// `10.20.3.5/24`.
    pub address_cidr: String,
    /// The gateway address.
    pub gateway: String,
    /// The DNS server address.
    pub dns: String,
    /// Traditional cloud images install the guest agent on first boot.
    /// Bootc images must bake packages into the OCI image because `/usr` is
    /// read-only after deployment.
    pub install_guest_agent: bool,
}

impl Seed {
    /// Checks that every field is present and well formed. The renderers
    /// call it, so a malformed seed can never reach an ISO.
    pub fn validate(&self) -> Result<(), Error> {
        for (name, value) in [
            ("instance id", self.instance_id.as_str()),
            ("hostname", self.hostname.as_str()),
            ("user name", self.user_name.as_str()),
        ] {
            if value.is_empty() {
                return Err(Error::invalid(format!("{name} is empty")));
            }
            single_line(name, value)?;
        }

        if self.hostname.contains([' ', '\t']) {
            return Err(Error::invalid(format!(
                "hostname {:?} contains whitespace",
                self.hostname
            )));
        }
        if self.user_name.contains([' ', '\t']) {
            return Err(Error::invalid(format!(
                "user name {:?} contains whitespace",
                self.user_name
            )));
        }
        if self.authorized_keys.is_empty() {
            return Err(Error::invalid(
                "no authorized keys; the instance would be unreachable",
            ));
        }
        for key in &self.authorized_keys {
            if key.trim().is_empty() {
                return Err(Error::invalid("empty authorized key"));
            }
            single_line("authorized key", key)?;
        }

        parse_mac(&self.mac).map_err(|reason| {
            Error::invalid(format!("invalid MAC address {:?}: {reason}", self.mac))
        })?;
        parse_prefix(&self.address_cidr).map_err(|reason| {
            Error::invalid(format!(
                "invalid address {:?} (want address/prefix): {reason}",
                self.address_cidr
            ))
        })?;
        parse_addr(&self.gateway).map_err(|reason| {
            Error::invalid(format!("invalid gateway {:?}: {reason}", self.gateway))
        })?;
        parse_addr(&self.dns).map_err(|reason| {
            Error::invalid(format!("invalid DNS server {:?}: {reason}", self.dns))
        })?;
        Ok(())
    }

    /// Renders the NoCloud `meta-data` file.
    pub fn meta_data(&self) -> Result<String, Error> {
        self.validate()?;
        Ok(format!(
            "instance-id: {}\nlocal-hostname: {}\n",
            quote(&self.instance_id),
            quote(&self.hostname)
        ))
    }

    /// Renders the NoCloud `user-data` file. Per SPEC section 5.2 it
    /// sets the host name to the instance name, creates one user account,
    /// installs the public keys of the owner, and installs and starts
    /// `qemu-guest-agent`. The static network settings live in
    /// [`Seed::network_config`].
    pub fn user_data(&self) -> Result<String, Error> {
        self.validate()?;
        let mut rendered = String::from("#cloud-config\n");
        writeln!(rendered, "hostname: {}", quote(&self.hostname)).unwrap();
        rendered.push_str("users:\n");
        writeln!(rendered, "  - name: {}", quote(&self.user_name)).unwrap();
        rendered.push_str("    shell: /bin/bash\n");
        rendered.push_str("    lock_passwd: true\n");
        rendered.push_str("    sudo: \"ALL=(ALL) NOPASSWD:ALL\"\n");
        rendered.push_str("    ssh_authorized_keys:\n");
        for key in &self.authorized_keys {
            writeln!(rendered, "      - {}", quote(key.trim())).unwrap();
        }
        if self.install_guest_agent {
            rendered.push_str("package_update: true\n");
            rendered.push_str("packages:\n");
            rendered.push_str("  - qemu-guest-agent\n");
        }
        rendered.push_str("runcmd:\n");
        rendered.push_str("  - [systemctl, enable, --now, qemu-guest-agent]\n");
        Ok(rendered)
    }

    /// Renders the NoCloud `network-config` file (version 2). It sets the
    /// static address, gateway, and DNS server that Bento assigned (SPEC
    /// sections 5.2 and 6.2), matching the interface by the MAC address
    /// Bento chose.
    pub fn network_config(&self) -> Result<String, Error> {
        self.validate()?;
        let mac = parse_mac(&self.mac).map_err(|reason| {
            Error::invalid(format!("invalid MAC address {:?}: {reason}", self.mac))
        })?;
        let mut rendered = String::from("version: 2\n");
        rendered.push_str("ethernets:\n");
        rendered.push_str("  primary:\n");
        rendered.push_str("    match:\n");
        writeln!(rendered, "      macaddress: {}", quote(&mac)).unwrap();
        rendered.push_str("    addresses:\n");
        writeln!(rendered, "      - {}", quote(&self.address_cidr)).unwrap();
        rendered.push_str("    routes:\n");
        rendered.push_str("      - to: \"0.0.0.0/0\"\n");
        writeln!(rendered, "        via: {}", quote(&self.gateway)).unwrap();
        rendered.push_str("    nameservers:\n");
        rendered.push_str("      addresses:\n");
        writeln!(rendered, "        - {}", quote(&self.dns)).unwrap();
        Ok(rendered)
    }
}

fn single_line(name: &str, value: &str) -> Result<(), Error> {
    if value
        .chars()
        .any(|character| character < ' ' || character == '\u{7f}')
    {
        return Err(Error::invalid(format!(
            "{name} contains a control character"
        )));
    }
    Ok(())
}

/// Renders a string as a double-quoted YAML scalar. Validation has
/// already rejected control characters, so escaping backslash and quote
/// is sufficient.
fn quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

fn parse_prefix(value: &str) -> Result<(), &'static str> {
    let (address, bits) = value.rsplit_once('/').ok_or("missing prefix length")?;
    let address: IpAddr = address.parse().map_err(|_| "invalid IP address")?;
    if bits.is_empty()
        || (bits.len() > 1 && !matches!(bits.as_bytes()[0], b'1'..=b'9'))
        || !bits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("invalid prefix length");
    }
    let bits: u16 = bits.parse().map_err(|_| "invalid prefix length")?;
    let max_bits = if address.is_ipv4() { 32 } else { 128 };
    if bits > max_bits {
        return Err("prefix length out of range");
    }
    Ok(())
}

fn parse_addr(value: &str) -> Result<(), &'static str> {
    if value.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    let (address, zone) = value.split_once('%').ok_or("invalid IP address")?;
    if zone.is_empty() || address.parse::<std::net::Ipv6Addr>().is_err() {
        return Err("invalid IP address");
    }
    Ok(())
}

// Accept the IEEE 802 and InfiniBand forms understood by the host
// networking stack and return the canonical lower-case, colon-separated
// representation.
fn parse_mac(value: &str) -> Result<String, &'static str> {
    let bytes = if value.as_bytes().get(4) == Some(&b'.') {
        let groups: Vec<_> = value.split('.').collect();
        if !matches!(groups.len(), 3 | 4 | 10) || groups.iter().any(|group| group.len() != 4) {
            return Err("invalid format");
        }
        let joined = groups.concat();
        parse_hex_pairs(&joined)?
    } else if matches!(value.as_bytes().get(2), Some(b':' | b'-')) {
        let separator = char::from(value.as_bytes()[2]);
        let groups: Vec<_> = value.split(separator).collect();
        if !matches!(groups.len(), 6 | 8 | 20) || groups.iter().any(|group| group.len() != 2) {
            return Err("invalid format");
        }
        let joined = groups.concat();
        parse_hex_pairs(&joined)?
    } else {
        if !matches!(value.len(), 12 | 16 | 40) {
            return Err("invalid format");
        }
        parse_hex_pairs(value)?
    };

    Ok(bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":"))
}

fn parse_hex_pairs(value: &str) -> Result<Vec<u8>, &'static str> {
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return Err("invalid length");
    }
    pairs
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(|_| "invalid hexadecimal digit")?;
            u8::from_str_radix(pair, 16).map_err(|_| "invalid hexadecimal digit")
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn test_seed() -> Seed {
        Seed {
            instance_id: "6f1d2c3a-9b8e-4f5a-a1b2-c3d4e5f60789".to_string(),
            hostname: "web-1".to_string(),
            user_name: "alice".to_string(),
            authorized_keys: vec![
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIB6C5rzYtZQoYXsQ2N4YFJmXW4L0Yw1v9uW3o2n8m4Qq alice@laptop".to_string(),
                "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCk7example alice@desktop".to_string(),
            ],
            mac: "52:54:00:aa:bb:cc".to_string(),
            address_cidr: "10.20.3.5/24".to_string(),
            gateway: "10.20.3.1".to_string(),
            dns: "10.20.3.1".to_string(),
            install_guest_agent: true,
        }
    }

    fn golden(name: &str) -> String {
        std::fs::read_to_string(format!("{}/testdata/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn render_golden() {
        let seed = test_seed();
        let cases = [
            ("meta-data", seed.meta_data().unwrap(), "meta-data.golden"),
            ("user-data", seed.user_data().unwrap(), "user-data.golden"),
            (
                "network-config",
                seed.network_config().unwrap(),
                "network-config.golden",
            ),
        ];
        for (name, rendered, golden_file) in cases {
            assert_eq!(rendered, golden(golden_file), "{name} mismatch");
        }
    }

    #[test]
    fn user_data_starts_with_cloud_config_header() {
        let rendered = test_seed().user_data().unwrap();
        assert!(rendered.starts_with("#cloud-config\n"));
    }

    #[test]
    fn validate() {
        type ValidationCase = (&'static str, fn(&mut Seed), Option<&'static str>);
        let cases: &[ValidationCase] = &[
            ("valid", |_| {}, None),
            (
                "empty instance id",
                |s| s.instance_id.clear(),
                Some("instance id"),
            ),
            ("empty hostname", |s| s.hostname.clear(), Some("hostname")),
            (
                "hostname with space",
                |s| s.hostname = "web 1".into(),
                Some("whitespace"),
            ),
            (
                "hostname with newline",
                |s| s.hostname = "web\n1".into(),
                Some("control character"),
            ),
            ("empty user", |s| s.user_name.clear(), Some("user name")),
            (
                "user with space",
                |s| s.user_name = "a b".into(),
                Some("whitespace"),
            ),
            (
                "no keys",
                |s| s.authorized_keys.clear(),
                Some("no authorized keys"),
            ),
            (
                "blank key",
                |s| s.authorized_keys = vec!["  ".into()],
                Some("empty authorized key"),
            ),
            (
                "key with newline injection",
                |s| s.authorized_keys = vec!["ssh-ed25519 AAAA x\nusers:".into()],
                Some("control character"),
            ),
            ("bad mac", |s| s.mac = "not-a-mac".into(), Some("MAC")),
            (
                "address without prefix",
                |s| s.address_cidr = "10.20.3.5".into(),
                Some("address"),
            ),
            (
                "bad gateway",
                |s| s.gateway = "10.20.3.999".into(),
                Some("gateway"),
            ),
            ("bad dns", |s| s.dns.clear(), Some("DNS")),
        ];

        for (name, mutate, expected) in cases {
            let mut seed = test_seed();
            mutate(&mut seed);
            let result = seed.validate();
            match expected {
                None => assert!(result.is_ok(), "{name}: {result:?}"),
                Some(expected) => {
                    let error = result.expect_err(name);
                    assert!(
                        error.to_string().contains(expected),
                        "error {error:?} does not mention {expected:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn quote_escapes() {
        for (input, expected) in [
            ("plain", r#""plain""#),
            (r#"has "quotes""#, r#""has \"quotes\"""#),
            (r#"back\slash"#, r#""back\\slash""#),
        ] {
            assert_eq!(quote(input), expected, "quote({input:?})");
        }
    }
}
