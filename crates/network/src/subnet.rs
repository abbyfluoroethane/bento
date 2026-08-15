use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};

use async_trait::async_trait;

use crate::{DynError, Error, Ipv4Prefix, Result, invalid};

// Host address layout inside a user /24 (SPEC 6.2). The .1 address is the
// host side of the bridge and the gateway of every instance. Instances
// receive .2 through .254.
const GATEWAY_HOST: u32 = 1;
const FIRST_INSTANCE: u32 = 2;
const LAST_INSTANCE: u32 = 254;

/// The DNS servers written into cloud-init network configuration when
/// the operator does not override them. No resolver runs on the user
/// bridges, so instances use public resolvers.
pub const DEFAULT_DNS: [IpAddr; 2] = [
    IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
    IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)),
];

/// Carves the operator-configured private range into per-user `/24`
/// subnets (SPEC 6.2). The mapping from subnet index to CIDR is
/// deterministic: index 0 is the first `/24` of the range, index 1 the
/// second, and so on. The private range must be `/24` or wider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    prefix: Ipv4Prefix,
}

impl Plan {
    /// Parses the private range, for example `10.77.0.0/16`, and returns
    /// its subnet plan.
    pub fn new(private_range: &str) -> Result<Self> {
        let prefix = bento_config::parse_prefix(private_range)
            .map_err(|err| invalid(format!("private range: {err}")))?;
        if prefix.bits > 24 {
            return Err(invalid(format!(
                "private range {private_range:?} is narrower than /24"
            )));
        }
        Ok(Self {
            prefix: masked(prefix),
        })
    }

    /// Returns the whole private range.
    pub fn range(self) -> Ipv4Prefix {
        self.prefix
    }

    /// Returns how many user `/24` subnets the range holds.
    pub fn subnets(self) -> usize {
        1usize << (24 - self.prefix.bits)
    }

    /// Returns the `/24` for a user subnet index. The mapping is
    /// deterministic and never changes for a given range.
    pub fn subnet(self, index: isize) -> Result<Ipv4Prefix> {
        if index < 0 || index as usize >= self.subnets() {
            return Err(invalid(format!(
                "subnet index {index} out of range [0, {})",
                self.subnets()
            )));
        }
        let base = addr_to_u32(self.prefix.addr);
        Ok(Ipv4Prefix {
            addr: u32_to_addr(base + ((index as u32) << 8)),
            bits: 24,
        })
    }

    /// Returns the subnet index of a user `/24` inside the range. It is
    /// the inverse of [`Plan::subnet`].
    pub fn index(self, subnet: Ipv4Prefix) -> Result<usize> {
        if subnet.bits != 24 {
            return Err(invalid(format!(
                "{} is not an IPv4 /24",
                format_prefix(subnet)
            )));
        }
        let subnet = masked(subnet);
        if !contains(self.prefix, subnet.addr) {
            return Err(invalid(format!(
                "subnet {} is outside the private range {}",
                format_prefix(subnet),
                format_prefix(self.prefix)
            )));
        }
        Ok(((addr_to_u32(subnet.addr) - addr_to_u32(self.prefix.addr)) >> 8) as usize)
    }

    /// Picks the lowest free subnet index and returns its `/24`. A subnet
    /// freed by a deleted user is reused. Returns
    /// [`Error::SubnetsExhausted`] when every `/24` is taken.
    pub async fn allocate<S: SubnetStore + ?Sized>(self, store: &S) -> Result<Ipv4Prefix> {
        let used = store.used_subnets().await.map_err(Error::ListUsedSubnets)?;
        let taken: HashSet<usize> = used
            .into_iter()
            .filter_map(|subnet| self.index(subnet).ok())
            .collect();
        for index in 0..self.subnets() {
            if !taken.contains(&index) {
                return self.subnet(index as isize);
            }
        }
        Err(Error::SubnetsExhausted)
    }
}

/// The view of the store that the subnet allocator needs: the `/24` of
/// every existing user (the `users.subnet` column).
#[async_trait]
pub trait SubnetStore: Send + Sync {
    async fn used_subnets(&self) -> std::result::Result<Vec<Ipv4Prefix>, DynError>;
}

/// The view of the store that the address allocator needs: the address
/// of every instance inside one user subnet (the `instances.address`
/// column).
#[async_trait]
pub trait AddressStore: Send + Sync {
    async fn used_addresses(
        &self,
        subnet: Ipv4Prefix,
    ) -> std::result::Result<Vec<Ipv4Addr>, DynError>;
}

/// Picks the lowest free instance address in a user `/24`. Bento selects
/// the address at creation time; there is no DHCP (SPEC 6.2). The `.1`
/// address belongs to the host and is never returned. A freed address is
/// reused. Returns [`Error::AddressesExhausted`] when `.2` through `.254`
/// are all taken.
pub async fn allocate_address<S: AddressStore + ?Sized>(
    store: &S,
    subnet: Ipv4Prefix,
) -> Result<Ipv4Addr> {
    require_slash_24(subnet)?;
    let used: HashSet<Ipv4Addr> = store
        .used_addresses(subnet)
        .await
        .map_err(Error::ListUsedAddresses)?
        .into_iter()
        .collect();
    let base = addr_to_u32(masked(subnet).addr);
    for host in FIRST_INSTANCE..=LAST_INSTANCE {
        let address = u32_to_addr(base + host);
        if !used.contains(&address) {
            return Ok(address);
        }
    }
    Err(Error::AddressesExhausted)
}

/// Returns the host side of the bridge for a user `/24`. This is the
/// `.1` address, the gateway of every instance in the subnet.
pub fn gateway(subnet: Ipv4Prefix) -> Ipv4Addr {
    u32_to_addr(addr_to_u32(masked(subnet).addr) + GATEWAY_HOST)
}

/// Values that cloud-init writes into the guest network configuration
/// for one instance (SPEC sections 5.2 and 6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestNetwork {
    /// The instance address with the `/24` prefix length.
    pub address: Ipv4Prefix,
    /// The `.1` address of the user subnet.
    pub gateway: Ipv4Addr,
    /// The resolver list for the guest.
    pub dns: Vec<IpAddr>,
}

impl GuestNetwork {
    /// Returns the guest network values for an instance address inside a
    /// user `/24`. `None` selects [`DEFAULT_DNS`]; `Some(&[])` keeps an
    /// explicitly empty resolver list.
    pub fn new(subnet: Ipv4Prefix, address: Ipv4Addr, dns: Option<&[IpAddr]>) -> Result<Self> {
        require_slash_24(subnet)?;
        if !contains(subnet, address) {
            return Err(invalid(format!(
                "address {address} is outside subnet {}",
                format_prefix(subnet)
            )));
        }
        Ok(Self {
            address: Ipv4Prefix {
                addr: address,
                bits: 24,
            },
            gateway: gateway(subnet),
            dns: dns.unwrap_or(&DEFAULT_DNS).to_vec(),
        })
    }
}

pub(crate) fn require_slash_24(prefix: Ipv4Prefix) -> Result<()> {
    if prefix.bits != 24 {
        return Err(invalid(format!(
            "{} is not an IPv4 /24",
            format_prefix(prefix)
        )));
    }
    Ok(())
}

pub(crate) fn contains(prefix: Ipv4Prefix, address: Ipv4Addr) -> bool {
    prefix.bits <= 32
        && (addr_to_u32(address) & prefix_mask(prefix.bits))
            == (addr_to_u32(prefix.addr) & prefix_mask(prefix.bits))
}

pub(crate) fn masked(prefix: Ipv4Prefix) -> Ipv4Prefix {
    Ipv4Prefix {
        addr: u32_to_addr(addr_to_u32(prefix.addr) & prefix_mask(prefix.bits)),
        bits: prefix.bits,
    }
}

pub(crate) fn format_prefix(prefix: Ipv4Prefix) -> String {
    format!("{}/{}", prefix.addr, prefix.bits)
}

fn prefix_mask(bits: u8) -> u32 {
    match bits {
        0 => 0,
        1..=32 => u32::MAX << (32 - bits),
        _ => 0,
    }
}

fn addr_to_u32(address: Ipv4Addr) -> u32 {
    u32::from(address)
}

fn u32_to_addr(address: u32) -> Ipv4Addr {
    Ipv4Addr::from(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeSubnetStore {
        subnets: Vec<Ipv4Prefix>,
    }

    #[async_trait]
    impl SubnetStore for FakeSubnetStore {
        async fn used_subnets(&self) -> std::result::Result<Vec<Ipv4Prefix>, DynError> {
            Ok(self.subnets.clone())
        }
    }

    #[derive(Default)]
    struct FakeAddressStore {
        addresses: Vec<Ipv4Addr>,
    }

    #[async_trait]
    impl AddressStore for FakeAddressStore {
        async fn used_addresses(
            &self,
            _subnet: Ipv4Prefix,
        ) -> std::result::Result<Vec<Ipv4Addr>, DynError> {
            Ok(self.addresses.clone())
        }
    }

    fn prefix(cidr: &str) -> Ipv4Prefix {
        bento_config::parse_prefix(cidr).unwrap()
    }

    #[test]
    fn new_plan() {
        let cases = [
            ("default /16", "10.77.0.0/16", false, 256),
            ("single /24", "192.168.7.0/24", false, 1),
            ("wide /8", "10.0.0.0/8", false, 65_536),
            ("unmasked bits", "10.77.3.9/16", false, 256),
            ("narrower than /24", "10.77.0.0/25", true, 0),
            ("ipv6", "fd00::/64", true, 0),
            ("garbage", "not-a-cidr", true, 0),
        ];
        for (name, cidr, want_error, subnets) in cases {
            let result = Plan::new(cidr);
            assert_eq!(result.is_err(), want_error, "case {name}: {result:?}");
            if let Ok(plan) = result {
                assert_eq!(plan.subnets(), subnets, "case {name}");
            }
        }
    }

    #[test]
    fn plan_subnet_mapping() {
        let plan = Plan::new("10.77.0.0/16").unwrap();
        let cases = [
            (0, Some("10.77.0.0/24")),
            (1, Some("10.77.1.0/24")),
            (42, Some("10.77.42.0/24")),
            (255, Some("10.77.255.0/24")),
            (256, None),
            (-1, None),
        ];
        for (index, want) in cases {
            let result = plan.subnet(index);
            let Some(want) = want else {
                assert!(result.is_err(), "subnet({index}) = {result:?}");
                continue;
            };
            let subnet = result.unwrap();
            assert_eq!(format_prefix(subnet), want);
            assert_eq!(plan.index(subnet).unwrap(), index as usize);
        }
    }

    #[test]
    fn plan_index_rejects_outside_range() {
        let plan = Plan::new("10.77.0.0/16").unwrap();
        for cidr in ["10.78.0.0/24", "192.168.1.0/24", "10.77.0.0/16"] {
            assert!(plan.index(prefix(cidr)).is_err(), "index({cidr}) succeeded");
        }
    }

    #[tokio::test]
    async fn plan_allocate() {
        let plan = Plan::new("10.77.0.0/22").unwrap();
        let cases: [(&str, &[&str], Option<&str>, bool); 5] = [
            ("empty range", &[], Some("10.77.0.0/24"), false),
            (
                "lowest free",
                &["10.77.0.0/24", "10.77.1.0/24"],
                Some("10.77.2.0/24"),
                false,
            ),
            (
                "reuse freed gap",
                &["10.77.0.0/24", "10.77.2.0/24"],
                Some("10.77.1.0/24"),
                false,
            ),
            (
                "foreign subnet ignored",
                &["192.168.0.0/24"],
                Some("10.77.0.0/24"),
                false,
            ),
            (
                "exhausted",
                &[
                    "10.77.0.0/24",
                    "10.77.1.0/24",
                    "10.77.2.0/24",
                    "10.77.3.0/24",
                ],
                None,
                true,
            ),
        ];
        for (name, used, want, exhausted) in cases {
            let store = FakeSubnetStore {
                subnets: used.iter().map(|cidr| prefix(cidr)).collect(),
            };
            let result = plan.allocate(&store).await;
            if exhausted {
                assert!(
                    matches!(result, Err(Error::SubnetsExhausted)),
                    "case {name}"
                );
            } else {
                assert_eq!(format_prefix(result.unwrap()), want.unwrap(), "case {name}");
            }
        }
    }

    #[tokio::test]
    async fn allocate_address_cases() {
        let subnet = prefix("10.77.3.0/24");
        let full: Vec<_> = (2..=254)
            .map(|host| Ipv4Addr::new(10, 77, 3, host))
            .collect();
        let cases: [(&str, Vec<Ipv4Addr>, Option<Ipv4Addr>, bool); 5] = [
            (
                "first address is .2 not .1",
                vec![],
                Some(Ipv4Addr::new(10, 77, 3, 2)),
                false,
            ),
            (
                "skips used",
                vec![Ipv4Addr::new(10, 77, 3, 2), Ipv4Addr::new(10, 77, 3, 3)],
                Some(Ipv4Addr::new(10, 77, 3, 4)),
                false,
            ),
            (
                "reuses freed gap",
                vec![Ipv4Addr::new(10, 77, 3, 2), Ipv4Addr::new(10, 77, 3, 4)],
                Some(Ipv4Addr::new(10, 77, 3, 3)),
                false,
            ),
            (
                "all but last",
                full[..full.len() - 1].to_vec(),
                Some(Ipv4Addr::new(10, 77, 3, 254)),
                false,
            ),
            ("exhausted", full, None, true),
        ];
        for (name, used, want, exhausted) in cases {
            let result = allocate_address(&FakeAddressStore { addresses: used }, subnet).await;
            if exhausted {
                assert!(
                    matches!(result, Err(Error::AddressesExhausted)),
                    "case {name}"
                );
            } else {
                assert_eq!(result.unwrap(), want.unwrap(), "case {name}");
            }
        }
    }

    #[tokio::test]
    async fn allocate_address_rejects_non_slash_24() {
        assert!(
            allocate_address(&FakeAddressStore::default(), prefix("10.77.0.0/16"))
                .await
                .is_err()
        );
    }

    #[test]
    fn gateway_address() {
        assert_eq!(gateway(prefix("10.77.3.0/24")), Ipv4Addr::new(10, 77, 3, 1));
    }

    #[test]
    fn new_guest_network() {
        let subnet = prefix("10.77.3.0/24");
        let network = GuestNetwork::new(subnet, Ipv4Addr::new(10, 77, 3, 2), None).unwrap();
        assert_eq!(format_prefix(network.address), "10.77.3.2/24");
        assert_eq!(network.gateway, Ipv4Addr::new(10, 77, 3, 1));
        assert!(!network.dns.is_empty());
        assert!(GuestNetwork::new(subnet, Ipv4Addr::new(10, 77, 4, 2), None).is_err());
    }
}
