package network

import (
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"net/netip"
)

// Errors returned by the allocators.
var (
	// ErrSubnetsExhausted reports that every /24 in the private range is
	// assigned to a user.
	ErrSubnetsExhausted = errors.New("network: no free /24 subnet in the private range")
	// ErrAddressesExhausted reports that every usable address in a user
	// subnet is assigned to an instance.
	ErrAddressesExhausted = errors.New("network: no free address in the subnet")
)

// Host address layout inside a user /24 (SPEC 6.2). The .1 address is the
// host side of the bridge and the gateway of every instance. Instances
// receive .2 through .254.
const (
	gatewayHost   = 1
	firstInstance = 2
	lastInstance  = 254
)

// Plan carves the operator-configured private range into per-user /24
// subnets (SPEC 6.2). The mapping from subnet index to CIDR is
// deterministic: index 0 is the first /24 of the range, index 1 the
// second, and so on. The private range must be /24 or wider.
type Plan struct {
	prefix netip.Prefix
}

// NewPlan parses the private range, for example "10.77.0.0/16", and
// returns the subnet plan for it.
func NewPlan(privateRange string) (Plan, error) {
	prefix, err := netip.ParsePrefix(privateRange)
	if err != nil {
		return Plan{}, fmt.Errorf("network: private range: %w", err)
	}
	if !prefix.Addr().Is4() {
		return Plan{}, fmt.Errorf("network: private range %q is not IPv4", privateRange)
	}
	if prefix.Bits() > 24 {
		return Plan{}, fmt.Errorf("network: private range %q is narrower than /24", privateRange)
	}
	return Plan{prefix: prefix.Masked()}, nil
}

// Range returns the whole private range.
func (p Plan) Range() netip.Prefix { return p.prefix }

// Subnets returns how many user /24 subnets the range holds.
func (p Plan) Subnets() int { return 1 << (24 - p.prefix.Bits()) }

// Subnet returns the /24 for a user subnet index. The mapping is
// deterministic and never changes for a given range.
func (p Plan) Subnet(index int) (netip.Prefix, error) {
	if index < 0 || index >= p.Subnets() {
		return netip.Prefix{}, fmt.Errorf("network: subnet index %d out of range [0, %d)", index, p.Subnets())
	}
	base := addrToU32(p.prefix.Addr())
	return netip.PrefixFrom(u32ToAddr(base+uint32(index)<<8), 24), nil
}

// Index returns the subnet index of a user /24 inside the range. It is
// the inverse of Subnet.
func (p Plan) Index(subnet netip.Prefix) (int, error) {
	if subnet.Bits() != 24 || !subnet.Addr().Is4() {
		return 0, fmt.Errorf("network: %s is not an IPv4 /24", subnet)
	}
	masked := subnet.Masked()
	if !p.prefix.Contains(masked.Addr()) {
		return 0, fmt.Errorf("network: subnet %s is outside the private range %s", subnet, p.prefix)
	}
	return int((addrToU32(masked.Addr()) - addrToU32(p.prefix.Addr())) >> 8), nil
}

// SubnetStore is the view of the store that the subnet allocator needs:
// the /24 of every existing user (the users.subnet column).
type SubnetStore interface {
	UsedSubnets(ctx context.Context) ([]netip.Prefix, error)
}

// Allocate picks the lowest free subnet index and returns its /24. A
// subnet freed by a deleted user is reused. Allocate returns
// ErrSubnetsExhausted when every /24 is taken.
func (p Plan) Allocate(ctx context.Context, store SubnetStore) (netip.Prefix, error) {
	used, err := store.UsedSubnets(ctx)
	if err != nil {
		return netip.Prefix{}, fmt.Errorf("network: list used subnets: %w", err)
	}
	taken := make(map[int]bool, len(used))
	for _, s := range used {
		idx, err := p.Index(s)
		if err != nil {
			// A subnet outside the range cannot collide with this plan.
			continue
		}
		taken[idx] = true
	}
	for i := range p.Subnets() {
		if !taken[i] {
			return p.Subnet(i)
		}
	}
	return netip.Prefix{}, ErrSubnetsExhausted
}

// AddressStore is the view of the store that the address allocator needs:
// the address of every instance inside one user subnet (the
// instances.address column).
type AddressStore interface {
	UsedAddresses(ctx context.Context, subnet netip.Prefix) ([]netip.Addr, error)
}

// AllocateAddress picks the lowest free instance address in a user /24.
// Bento selects the address at creation time; there is no DHCP (SPEC
// 6.2). The .1 address belongs to the host and is never returned. An
// address freed by a deleted instance is reused. AllocateAddress returns
// ErrAddressesExhausted when .2 through .254 are all taken.
func AllocateAddress(ctx context.Context, store AddressStore, subnet netip.Prefix) (netip.Addr, error) {
	if subnet.Bits() != 24 || !subnet.Addr().Is4() {
		return netip.Addr{}, fmt.Errorf("network: %s is not an IPv4 /24", subnet)
	}
	used, err := store.UsedAddresses(ctx, subnet)
	if err != nil {
		return netip.Addr{}, fmt.Errorf("network: list used addresses: %w", err)
	}
	taken := make(map[netip.Addr]bool, len(used))
	for _, a := range used {
		taken[a] = true
	}
	base := addrToU32(subnet.Masked().Addr())
	for host := firstInstance; host <= lastInstance; host++ {
		addr := u32ToAddr(base + uint32(host))
		if !taken[addr] {
			return addr, nil
		}
	}
	return netip.Addr{}, ErrAddressesExhausted
}

// Gateway returns the host side of the bridge for a user /24. This is
// the .1 address, the gateway of every instance in the subnet.
func Gateway(subnet netip.Prefix) netip.Addr {
	return u32ToAddr(addrToU32(subnet.Masked().Addr()) + gatewayHost)
}

// DefaultDNS is the DNS servers written into the cloud-init
// network-config when the operator does not override them. No resolver
// runs on the user bridges, so instances use public resolvers.
var DefaultDNS = []netip.Addr{
	netip.MustParseAddr("1.1.1.1"),
	netip.MustParseAddr("9.9.9.9"),
}

// GuestNetwork holds the values that cloud-init writes into the guest
// network-config for one instance (SPEC 5.2, 6.2).
type GuestNetwork struct {
	// Address is the instance address with the /24 prefix length,
	// for example 10.77.3.2/24.
	Address netip.Prefix
	// Gateway is the .1 address of the user subnet.
	Gateway netip.Addr
	// DNS is the resolver list for the guest.
	DNS []netip.Addr
}

// NewGuestNetwork returns the guest network values for an instance
// address inside a user /24. A nil dns slice selects DefaultDNS.
func NewGuestNetwork(subnet netip.Prefix, addr netip.Addr, dns []netip.Addr) (GuestNetwork, error) {
	if subnet.Bits() != 24 || !subnet.Addr().Is4() {
		return GuestNetwork{}, fmt.Errorf("network: %s is not an IPv4 /24", subnet)
	}
	if !subnet.Contains(addr) {
		return GuestNetwork{}, fmt.Errorf("network: address %s is outside subnet %s", addr, subnet)
	}
	if dns == nil {
		dns = DefaultDNS
	}
	return GuestNetwork{
		Address: netip.PrefixFrom(addr, 24),
		Gateway: Gateway(subnet),
		DNS:     dns,
	}, nil
}

func addrToU32(a netip.Addr) uint32 {
	b := a.As4()
	return binary.BigEndian.Uint32(b[:])
}

func u32ToAddr(v uint32) netip.Addr {
	var b [4]byte
	binary.BigEndian.PutUint32(b[:], v)
	return netip.AddrFrom4(b)
}
