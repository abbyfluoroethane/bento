//! How a user `/24` divides into runner slots (MULTI-NODE 7.1).
//!
//! A slot is a subprefix of every user `/24`, and slot ownership is
//! deployment-wide: the runner that owns slot 1 owns slot 1 of every
//! user. That makes the address-to-runner mapping arithmetic rather than
//! a per-instance route directory (MULTI-NODE 7.2 and 7.3).
//!
//! Guests are never told about slots. A guest keeps its `/24` prefix and
//! its `.1` gateway, so it treats the whole user network as on-link and
//! resolves a remote address by ARP. The owning runner answers that ARP
//! and routes the packet (MULTI-NODE 8.1). Subdivision therefore changes
//! no guest configuration and moves no address.

use std::net::Ipv4Addr;

use crate::subnet::{addr_to_u32, masked, require_slash_24, u32_to_addr};
use crate::{Ipv4Prefix, Result, invalid};

/// The narrowest and widest slot prefix Bento supports (MULTI-NODE 17).
pub const MIN_SLOT_BITS: u8 = 24;
pub const MAX_SLOT_BITS: u8 = 27;

/// The host octet of the gateway, which every runner carries on its copy
/// of the user bridge (MULTI-NODE 7.1).
const GATEWAY_HOST: u32 = 1;
/// The lowest and highest host octets a `/24` can ever assign.
const FIRST_HOST: u32 = 2;
const LAST_HOST: u32 = 254;

/// The deployment-wide slot division. `/24` is one slot, which is what
/// version 1 was, so a single-machine deployment keeps working unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slots {
    bits: u8,
}

impl Slots {
    /// Accepts `/24` through `/27`. A wider division would leave too few
    /// addresses per slot to be useful.
    pub fn new(bits: u8) -> Result<Self> {
        if !(MIN_SLOT_BITS..=MAX_SLOT_BITS).contains(&bits) {
            return Err(invalid(format!(
                "runner slot prefix /{bits} is outside /{MIN_SLOT_BITS}-/{MAX_SLOT_BITS}"
            )));
        }
        Ok(Self { bits })
    }

    pub fn bits(self) -> u8 {
        self.bits
    }

    /// How many slots each user `/24` divides into: 1, 2, 4, or 8.
    pub fn count(self) -> u32 {
        1 << (self.bits - MIN_SLOT_BITS)
    }

    /// How far a host octet shifts right to give its slot number.
    fn shift(self) -> u32 {
        u32::from(8 - (self.bits - MIN_SLOT_BITS))
    }

    /// The routed prefix of one slot inside a user `/24`, for example
    /// `10.100.1.128/25` for slot 1 of `10.100.1.0/24` at `/25`.
    pub fn prefix(self, subnet: Ipv4Prefix, slot: u32) -> Result<Ipv4Prefix> {
        require_slash_24(subnet)?;
        self.check_slot(slot)?;
        let base = addr_to_u32(masked(subnet).addr);
        Ok(Ipv4Prefix {
            addr: u32_to_addr(base + (slot << self.shift())),
            bits: self.bits,
        })
    }

    /// Which slot holds an address. This is how an address allocated
    /// before subdivision finds its slot: Bento keeps the address and
    /// assigns its instance to the one slot whose prefix contains it
    /// (MULTI-NODE 7.1).
    pub fn slot_of(self, subnet: Ipv4Prefix, address: Ipv4Addr) -> Result<u32> {
        require_slash_24(subnet)?;
        if !crate::subnet::contains(subnet, address) {
            return Err(invalid(format!(
                "address {address} is outside subnet {}",
                crate::subnet::format_prefix(subnet)
            )));
        }
        Ok(host_octet(address) >> self.shift())
    }

    /// The host octets a new allocation may use in one slot, as an
    /// inclusive range.
    ///
    /// The allocator excludes the `/24` network address, the `.1`
    /// gateway, the `/24` broadcast address, and every subprefix's first
    /// and last address (MULTI-NODE 7.1). Skipping the boundary
    /// addresses costs at most two addresses per slot and keeps routing
    /// and diagnostics from disagreeing about whether a boundary address
    /// is assignable.
    ///
    /// This bound applies only to a *new* allocation. An address that
    /// already exists is grandfathered and keeps its value.
    pub fn assignable(self, slot: u32) -> Result<(u32, u32)> {
        self.check_slot(slot)?;
        let width = 1u32 << self.shift();
        let first = slot * width;
        let last = first + width - 1;
        // `first + 1` skips the subprefix network address. For slot 0 that
        // lands on the gateway, so the floor of `FIRST_HOST` also covers it.
        let low = (first + 1).max(FIRST_HOST).max(GATEWAY_HOST + 1);
        // `last - 1` skips the subprefix broadcast address. For the last
        // slot that is also the `/24` broadcast address.
        let high = (last - 1).min(LAST_HOST);
        Ok((low, high))
    }

    /// How many addresses a slot can assign to new instances.
    pub fn capacity(self, slot: u32) -> Result<u32> {
        let (low, high) = self.assignable(slot)?;
        Ok(high + 1 - low)
    }

    fn check_slot(self, slot: u32) -> Result<()> {
        if slot >= self.count() {
            return Err(invalid(format!(
                "slot {slot} does not exist at /{}, which has {} slots",
                self.bits,
                self.count()
            )));
        }
        Ok(())
    }
}

fn host_octet(address: Ipv4Addr) -> u32 {
    addr_to_u32(address) & 0xff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix(cidr: &str) -> Ipv4Prefix {
        bento_config::parse_prefix(cidr).unwrap()
    }

    #[test]
    fn new_rejects_a_prefix_outside_24_to_27() {
        for bits in [0, 8, 16, 23, 28, 32] {
            assert!(Slots::new(bits).is_err(), "/{bits} was accepted");
        }
        for bits in 24..=27 {
            assert!(Slots::new(bits).is_ok(), "/{bits} was refused");
        }
    }

    #[test]
    fn count_doubles_with_each_bit() {
        for (bits, want) in [(24, 1), (25, 2), (26, 4), (27, 8)] {
            assert_eq!(Slots::new(bits).unwrap().count(), want, "/{bits}");
        }
    }

    #[test]
    fn slot_prefixes_match_the_spec_table() {
        // The four /26 slots of MULTI-NODE 7.1.
        let slots = Slots::new(26).unwrap();
        let subnet = prefix("10.100.7.0/24");
        let want = [
            "10.100.7.0/26",
            "10.100.7.64/26",
            "10.100.7.128/26",
            "10.100.7.192/26",
        ];
        for (slot, want) in want.into_iter().enumerate() {
            let got = slots.prefix(subnet, slot as u32).unwrap();
            assert_eq!(crate::subnet::format_prefix(got), want, "slot {slot}");
        }
        assert!(slots.prefix(subnet, 4).is_err());
    }

    #[test]
    fn a_slash_24_is_one_slot_covering_the_whole_subnet() {
        let slots = Slots::new(24).unwrap();
        let subnet = prefix("10.100.1.0/24");
        assert_eq!(
            crate::subnet::format_prefix(slots.prefix(subnet, 0).unwrap()),
            "10.100.1.0/24"
        );
        assert_eq!(slots.assignable(0).unwrap(), (2, 254));
        assert_eq!(slots.capacity(0).unwrap(), 253);
    }

    #[test]
    fn assignable_ranges_match_the_spec_capacities() {
        // MULTI-NODE 7.1 states the minimum capacity per slot.
        let cases = [(24u8, 253u32), (25, 125), (26, 61), (27, 29)];
        for (bits, minimum) in cases {
            let slots = Slots::new(bits).unwrap();
            for slot in 0..slots.count() {
                let capacity = slots.capacity(slot).unwrap();
                assert!(
                    capacity >= minimum,
                    "/{bits} slot {slot} holds {capacity}, below the stated {minimum}"
                );
            }
        }
    }

    #[test]
    fn assignable_excludes_every_boundary_address() {
        let slots = Slots::new(25).unwrap();
        assert_eq!(slots.assignable(0).unwrap(), (2, 126));
        assert_eq!(slots.assignable(1).unwrap(), (129, 254));

        // .0 and .1 and .127 and .128 and .255 are all excluded from a
        // new allocation at /25.
        for slot in 0..slots.count() {
            let (low, high) = slots.assignable(slot).unwrap();
            for excluded in [0, 1, 127, 128, 255] {
                assert!(
                    excluded < low || excluded > high,
                    "/25 slot {slot} would assign .{excluded}"
                );
            }
        }
    }

    #[test]
    fn slot_of_finds_the_slot_holding_an_address() {
        let subnet = prefix("10.100.1.0/24");
        let cases = [
            (24u8, 5u8, 0u32),
            (24, 200, 0),
            (25, 5, 0),
            (25, 126, 0),
            (25, 127, 0),
            (25, 128, 1),
            (25, 200, 1),
            (26, 63, 0),
            (26, 64, 1),
            (26, 191, 2),
            (26, 192, 3),
            (27, 31, 0),
            (27, 32, 1),
            (27, 224, 7),
        ];
        for (bits, host, want) in cases {
            let slots = Slots::new(bits).unwrap();
            let address = Ipv4Addr::new(10, 100, 1, host);
            assert_eq!(
                slots.slot_of(subnet, address).unwrap(),
                want,
                "/{bits} {address}"
            );
        }
    }

    #[test]
    fn slot_of_grandfathers_a_boundary_address() {
        // MULTI-NODE 7.1: an address allocated before subdivision keeps
        // its value, including .63, .64, .127, and .128. It belongs to
        // the one slot whose prefix contains it, even though a new
        // allocation would never choose it.
        let subnet = prefix("10.100.1.0/24");
        let slots = Slots::new(26).unwrap();
        for (host, want) in [(63u8, 0u32), (64, 1), (127, 1), (128, 2)] {
            let address = Ipv4Addr::new(10, 100, 1, host);
            let slot = slots.slot_of(subnet, address).unwrap();
            assert_eq!(slot, want, "{address}");
            let (low, high) = slots.assignable(slot).unwrap();
            let host = u32::from(host);
            assert!(
                host < low || host > high,
                "{address} should not be newly assignable"
            );
        }
    }

    #[test]
    fn slot_of_rejects_an_address_outside_the_subnet() {
        let slots = Slots::new(25).unwrap();
        assert!(
            slots
                .slot_of(prefix("10.100.1.0/24"), Ipv4Addr::new(10, 100, 2, 5))
                .is_err()
        );
    }

    #[test]
    fn every_address_maps_into_exactly_one_slot_range() {
        // No gap and no overlap: each host octet either belongs to the
        // assignable range of the slot that holds it, or is one of the
        // excluded boundary addresses.
        for bits in 24..=27 {
            let slots = Slots::new(bits).unwrap();
            let subnet = prefix("10.100.1.0/24");
            for host in 0..=255u32 {
                let address = u32_to_addr(addr_to_u32(subnet.addr) + host);
                let slot = slots.slot_of(subnet, address).unwrap();
                let (low, high) = slots.assignable(slot).unwrap();
                let assignable = host >= low && host <= high;
                let width = 1u32 << slots.shift();
                let boundary = host % width == 0 || host % width == width - 1;
                let reserved = host == 0 || host == GATEWAY_HOST || host == 255;
                assert_eq!(
                    assignable,
                    !(boundary || reserved),
                    "/{bits} .{host} assignable={assignable}"
                );
            }
        }
    }
}
