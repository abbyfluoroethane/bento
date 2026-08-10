package network

import (
	"context"
	"errors"
	"net/netip"
	"testing"
)

func TestNewPlan(t *testing.T) {
	tests := []struct {
		name    string
		cidr    string
		wantErr bool
		subnets int
	}{
		{name: "default /16", cidr: "10.77.0.0/16", subnets: 256},
		{name: "single /24", cidr: "192.168.7.0/24", subnets: 1},
		{name: "wide /8", cidr: "10.0.0.0/8", subnets: 65536},
		{name: "unmasked bits", cidr: "10.77.3.9/16", subnets: 256},
		{name: "narrower than /24", cidr: "10.77.0.0/25", wantErr: true},
		{name: "ipv6", cidr: "fd00::/64", wantErr: true},
		{name: "garbage", cidr: "not-a-cidr", wantErr: true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			p, err := NewPlan(tt.cidr)
			if tt.wantErr {
				if err == nil {
					t.Fatalf("NewPlan(%q) = %v, want error", tt.cidr, p)
				}
				return
			}
			if err != nil {
				t.Fatalf("NewPlan(%q): %v", tt.cidr, err)
			}
			if got := p.Subnets(); got != tt.subnets {
				t.Errorf("Subnets() = %d, want %d", got, tt.subnets)
			}
		})
	}
}

func TestPlanSubnetMapping(t *testing.T) {
	p, err := NewPlan("10.77.0.0/16")
	if err != nil {
		t.Fatal(err)
	}
	tests := []struct {
		index   int
		want    string
		wantErr bool
	}{
		{index: 0, want: "10.77.0.0/24"},
		{index: 1, want: "10.77.1.0/24"},
		{index: 42, want: "10.77.42.0/24"},
		{index: 255, want: "10.77.255.0/24"},
		{index: 256, wantErr: true},
		{index: -1, wantErr: true},
	}
	for _, tt := range tests {
		got, err := p.Subnet(tt.index)
		if tt.wantErr {
			if err == nil {
				t.Errorf("Subnet(%d) = %s, want error", tt.index, got)
			}
			continue
		}
		if err != nil {
			t.Errorf("Subnet(%d): %v", tt.index, err)
			continue
		}
		if got.String() != tt.want {
			t.Errorf("Subnet(%d) = %s, want %s", tt.index, got, tt.want)
		}
		// Index is the inverse of Subnet.
		back, err := p.Index(got)
		if err != nil {
			t.Errorf("Index(%s): %v", got, err)
			continue
		}
		if back != tt.index {
			t.Errorf("Index(%s) = %d, want %d", got, back, tt.index)
		}
	}
}

func TestPlanIndexRejectsOutsideRange(t *testing.T) {
	p, err := NewPlan("10.77.0.0/16")
	if err != nil {
		t.Fatal(err)
	}
	for _, cidr := range []string{"10.78.0.0/24", "192.168.1.0/24", "10.77.0.0/16"} {
		if _, err := p.Index(netip.MustParsePrefix(cidr)); err == nil {
			t.Errorf("Index(%s): want error", cidr)
		}
	}
}

type fakeSubnetStore struct {
	subnets []netip.Prefix
	err     error
}

func (f *fakeSubnetStore) UsedSubnets(context.Context) ([]netip.Prefix, error) {
	return f.subnets, f.err
}

func TestPlanAllocate(t *testing.T) {
	ctx := context.Background()
	p, err := NewPlan("10.77.0.0/22") // 4 subnets
	if err != nil {
		t.Fatal(err)
	}
	prefixes := func(cidrs ...string) []netip.Prefix {
		out := make([]netip.Prefix, len(cidrs))
		for i, c := range cidrs {
			out[i] = netip.MustParsePrefix(c)
		}
		return out
	}
	tests := []struct {
		name    string
		used    []netip.Prefix
		want    string
		wantErr error
	}{
		{name: "empty range", used: nil, want: "10.77.0.0/24"},
		{name: "lowest free", used: prefixes("10.77.0.0/24", "10.77.1.0/24"), want: "10.77.2.0/24"},
		{name: "reuse freed gap", used: prefixes("10.77.0.0/24", "10.77.2.0/24"), want: "10.77.1.0/24"},
		{name: "foreign subnet ignored", used: prefixes("192.168.0.0/24"), want: "10.77.0.0/24"},
		{
			name:    "exhausted",
			used:    prefixes("10.77.0.0/24", "10.77.1.0/24", "10.77.2.0/24", "10.77.3.0/24"),
			wantErr: ErrSubnetsExhausted,
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := p.Allocate(ctx, &fakeSubnetStore{subnets: tt.used})
			if tt.wantErr != nil {
				if !errors.Is(err, tt.wantErr) {
					t.Fatalf("Allocate() error = %v, want %v", err, tt.wantErr)
				}
				return
			}
			if err != nil {
				t.Fatalf("Allocate(): %v", err)
			}
			if got.String() != tt.want {
				t.Errorf("Allocate() = %s, want %s", got, tt.want)
			}
		})
	}
}

type fakeAddressStore struct {
	addrs []netip.Addr
	err   error
}

func (f *fakeAddressStore) UsedAddresses(context.Context, netip.Prefix) ([]netip.Addr, error) {
	return f.addrs, f.err
}

func TestAllocateAddress(t *testing.T) {
	ctx := context.Background()
	subnet := netip.MustParsePrefix("10.77.3.0/24")
	addrs := func(ss ...string) []netip.Addr {
		out := make([]netip.Addr, len(ss))
		for i, s := range ss {
			out[i] = netip.MustParseAddr(s)
		}
		return out
	}
	full := make([]netip.Addr, 0, 253)
	for host := 2; host <= 254; host++ {
		full = append(full, netip.AddrFrom4([4]byte{10, 77, 3, byte(host)}))
	}
	tests := []struct {
		name    string
		used    []netip.Addr
		want    string
		wantErr error
	}{
		{name: "first address is .2 not .1", used: nil, want: "10.77.3.2"},
		{name: "skips used", used: addrs("10.77.3.2", "10.77.3.3"), want: "10.77.3.4"},
		{name: "reuses freed gap", used: addrs("10.77.3.2", "10.77.3.4"), want: "10.77.3.3"},
		{name: "all but last", used: full[:len(full)-1], want: "10.77.3.254"},
		{name: "exhausted", used: full, wantErr: ErrAddressesExhausted},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := AllocateAddress(ctx, &fakeAddressStore{addrs: tt.used}, subnet)
			if tt.wantErr != nil {
				if !errors.Is(err, tt.wantErr) {
					t.Fatalf("AllocateAddress() error = %v, want %v", err, tt.wantErr)
				}
				return
			}
			if err != nil {
				t.Fatalf("AllocateAddress(): %v", err)
			}
			if got.String() != tt.want {
				t.Errorf("AllocateAddress() = %s, want %s", got, tt.want)
			}
		})
	}
}

func TestAllocateAddressRejectsNonSlash24(t *testing.T) {
	ctx := context.Background()
	if _, err := AllocateAddress(ctx, &fakeAddressStore{}, netip.MustParsePrefix("10.77.0.0/16")); err == nil {
		t.Error("AllocateAddress with /16: want error")
	}
}

func TestGateway(t *testing.T) {
	got := Gateway(netip.MustParsePrefix("10.77.3.0/24"))
	if got.String() != "10.77.3.1" {
		t.Errorf("Gateway() = %s, want 10.77.3.1", got)
	}
}

func TestNewGuestNetwork(t *testing.T) {
	subnet := netip.MustParsePrefix("10.77.3.0/24")
	gn, err := NewGuestNetwork(subnet, netip.MustParseAddr("10.77.3.2"), nil)
	if err != nil {
		t.Fatal(err)
	}
	if gn.Address.String() != "10.77.3.2/24" {
		t.Errorf("Address = %s, want 10.77.3.2/24", gn.Address)
	}
	if gn.Gateway.String() != "10.77.3.1" {
		t.Errorf("Gateway = %s, want 10.77.3.1", gn.Gateway)
	}
	if len(gn.DNS) == 0 {
		t.Error("DNS is empty, want DefaultDNS")
	}
	if _, err := NewGuestNetwork(subnet, netip.MustParseAddr("10.77.4.2"), nil); err == nil {
		t.Error("address outside subnet: want error")
	}
}
