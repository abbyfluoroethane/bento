use sha2::{Digest, Sha256};

/// Returns the MAC address for an instance. Bento assigns the MAC;
/// libvirt never generates it (SPEC sections 5 and 6.2). The address is
/// derived deterministically from the instance UUID, the identity of an
/// instance (SPEC 7.2), so it is stable across restarts and domain
/// redefines. A fixed MAC keeps the interface name stable in the guest.
///
/// The address is in the locally administered unicast range: the local
/// bit of the first octet is set and the multicast bit is clear.
pub fn mac(instance_uuid: &str) -> String {
    let digest = Sha256::digest(format!("bento-mac:{instance_uuid}").as_bytes());
    let mut bytes: [u8; 6] = digest[..6]
        .try_into()
        .expect("SHA-256 digest has six bytes");
    bytes[0] = bytes[0] & !0x01 | 0x02; // unicast, locally administered
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_format() {
        let identities = [
            "8f9c3a1e-2b4d-4e6f-8a9b-0c1d2e3f4a5b",
            "00000000-0000-0000-0000-000000000000",
            "another-identity",
        ];
        for identity in identities {
            let address = mac(identity);
            let octets: Vec<_> = address.split(':').collect();
            assert_eq!(octets.len(), 6, "mac({identity:?}) = {address:?}");
            assert!(
                octets.iter().all(|octet| {
                    octet.len() == 2
                        && octet
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                }),
                "mac({identity:?}) = {address:?}, not colon-separated lowercase hex"
            );
            let first = u8::from_str_radix(octets[0], 16).unwrap();
            assert_eq!(first & 0x01, 0, "multicast bit set in {address}");
            assert_ne!(first & 0x02, 0, "local bit clear in {address}");
        }
    }

    #[test]
    fn mac_deterministic() {
        const IDENTITY: &str = "8f9c3a1e-2b4d-4e6f-8a9b-0c1d2e3f4a5b";
        assert_eq!(mac(IDENTITY), mac(IDENTITY));
        assert_ne!(mac(IDENTITY), mac("a different uuid"));
    }

    #[test]
    fn mac_golden() {
        // Pin the derivation: a change here silently changes the MAC of
        // every existing instance and breaks guest interface-name
        // stability.
        const IDENTITY: &str = "8f9c3a1e-2b4d-4e6f-8a9b-0c1d2e3f4a5b";
        assert_eq!(mac(IDENTITY), "ba:c9:e6:f9:7c:2b");
    }
}
