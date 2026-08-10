package network

import (
	"fmt"
	"net/netip"
	"regexp"
	"sort"
	"strings"
)

// PortRange is an inclusive TCP port range the host may reach on an
// instance, such as the 3000-9999 proxy range of SPEC 9.1.
type PortRange struct {
	From, To int
}

// PublishedInstance is one instance as the firewall sees it: its address
// and the HTTP ports the host may reach (the default HTTP port and any
// extra published ports or ranges). Port 22 is always reachable from
// the host and need not be listed.
type PublishedInstance struct {
	Address    netip.Addr
	HTTPPorts  []int
	PortRanges []PortRange
}

// FirewallUser is one user's network and instances as the firewall sees
// them.
type FirewallUser struct {
	Network   UserNetwork
	Instances []PublishedInstance
}

// Ruleset is the complete input for the one Bento nftables table (SPEC
// 6.3). Bento owns every rule in that table.
type Ruleset struct {
	// PrivateRange is the whole operator-configured private range. Egress
	// leaving this range is masqueraded.
	PrivateRange netip.Prefix
	// Users lists every user with a network. Order does not matter to
	// the policy; Render sorts by bridge name so output is stable.
	Users []FirewallUser
}

// Interface and chain names must be safe to interpolate into nft
// syntax. nft has no general escaping, so reject instead (SPEC 4.2
// rationale applies here too).
var safeIfaceName = regexp.MustCompile(`^[A-Za-z0-9_.-]{1,15}$`)

// Render generates the full nftables ruleset text. The text deletes and
// redefines the whole table in one nft transaction, so applying it via
// `nft -f -` is an atomic full-table reload: a partial rule update never
// leaves a period with the wrong policy (SPEC 6.3).
//
// The table implements exactly the five rules of SPEC 6.3:
//
//  1. Host to instance on port 22 and the published HTTP ports.
//  2. Instance egress to the internet.
//  3. Drop between the bridges of two different users.
//  4. Permit within a user's bridge.
//  5. Masquerade instance egress behind the host address.
//
// The inter-user drop (rule 3) precedes the egress accept (rule 2):
// the egress accept matches any traffic entering from a user bridge, so
// cross-bridge traffic must be dropped before it.
func (r Ruleset) Render() (string, error) {
	if !r.PrivateRange.IsValid() || !r.PrivateRange.Addr().Is4() {
		return "", fmt.Errorf("network: ruleset private range %s is not IPv4", r.PrivateRange)
	}
	users := make([]FirewallUser, len(r.Users))
	copy(users, r.Users)
	sort.Slice(users, func(i, j int) bool { return users[i].Network.Bridge < users[j].Network.Bridge })

	seen := make(map[string]bool, len(users))
	for _, u := range users {
		if !safeIfaceName.MatchString(u.Network.Bridge) {
			return "", fmt.Errorf("network: unsafe bridge name %q", u.Network.Bridge)
		}
		if seen[u.Network.Bridge] {
			return "", fmt.Errorf("network: duplicate bridge name %q", u.Network.Bridge)
		}
		seen[u.Network.Bridge] = true
		if u.Network.Subnet.Bits() != 24 || !u.Network.Subnet.Addr().Is4() {
			return "", fmt.Errorf("network: %s is not an IPv4 /24", u.Network.Subnet)
		}
		for _, inst := range u.Instances {
			if !u.Network.Subnet.Contains(inst.Address) {
				return "", fmt.Errorf("network: instance address %s is outside subnet %s", inst.Address, u.Network.Subnet)
			}
			for _, p := range inst.HTTPPorts {
				if p < 1 || p > 65535 {
					return "", fmt.Errorf("network: invalid port %d for instance %s", p, inst.Address)
				}
			}
			for _, pr := range inst.PortRanges {
				if pr.From < 1 || pr.To > 65535 || pr.From > pr.To {
					return "", fmt.Errorf("network: invalid port range %d-%d for instance %s", pr.From, pr.To, inst.Address)
				}
			}
		}
	}

	var b strings.Builder
	b.WriteString("# Bento nftables table. Bento owns every rule here; do not edit by hand (SPEC 6.3).\n")
	b.WriteString("# The whole table is deleted and redefined in one transaction on every change.\n")
	b.WriteString("add table inet bento\n")
	b.WriteString("delete table inet bento\n")
	b.WriteString("table inet bento {\n")

	b.WriteString("\tchain output {\n")
	b.WriteString("\t\ttype filter hook output priority filter; policy accept;\n")
	b.WriteString("\t\t# rule 1: host to instance on port 22 and the published HTTP ports\n")
	for _, u := range users {
		for _, inst := range u.Instances {
			fmt.Fprintf(&b, "\t\toifname %q ip daddr %s tcp dport { %s } accept\n",
				u.Network.Bridge, inst.Address, portSet(inst.HTTPPorts, inst.PortRanges))
		}
		fmt.Fprintf(&b, "\t\toifname %q drop\n", u.Network.Bridge)
	}
	b.WriteString("\t}\n")

	b.WriteString("\tchain forward {\n")
	b.WriteString("\t\ttype filter hook forward priority filter; policy drop;\n")
	b.WriteString("\t\tct state established,related accept\n")
	b.WriteString("\t\t# rule 3: drop traffic between the bridges of two different users\n")
	for _, u := range users {
		for _, other := range users {
			if u.Network.Bridge == other.Network.Bridge {
				continue
			}
			fmt.Fprintf(&b, "\t\tiifname %q oifname %q drop\n", u.Network.Bridge, other.Network.Bridge)
		}
	}
	b.WriteString("\t\t# rule 4: permit traffic within a user's bridge\n")
	for _, u := range users {
		fmt.Fprintf(&b, "\t\tiifname %q oifname %q accept\n", u.Network.Bridge, u.Network.Bridge)
	}
	b.WriteString("\t\t# rule 2: permit instance egress to the internet\n")
	for _, u := range users {
		fmt.Fprintf(&b, "\t\tiifname %q accept\n", u.Network.Bridge)
	}
	b.WriteString("\t}\n")

	b.WriteString("\tchain postrouting {\n")
	b.WriteString("\t\ttype nat hook postrouting priority srcnat; policy accept;\n")
	b.WriteString("\t\t# rule 5: masquerade instance egress behind the host address\n")
	for _, u := range users {
		fmt.Fprintf(&b, "\t\tip saddr %s ip daddr != %s masquerade\n",
			u.Network.Subnet, r.PrivateRange.Masked())
	}
	b.WriteString("\t}\n")
	b.WriteString("}\n")
	return b.String(), nil
}

// portSet renders the host-reachable ports of an instance as an nft set
// body: port 22 plus the published HTTP ports and ranges, deduplicated
// and sorted for stable output.
func portSet(httpPorts []int, ranges []PortRange) string {
	set := map[int]bool{22: true}
	for _, p := range httpPorts {
		set[p] = true
	}
	ports := make([]int, 0, len(set))
	for p := range set {
		ports = append(ports, p)
	}
	sort.Ints(ports)
	parts := make([]string, 0, len(ports)+len(ranges))
	for _, p := range ports {
		parts = append(parts, fmt.Sprint(p))
	}
	sorted := make([]PortRange, len(ranges))
	copy(sorted, ranges)
	sort.Slice(sorted, func(i, j int) bool {
		if sorted[i].From != sorted[j].From {
			return sorted[i].From < sorted[j].From
		}
		return sorted[i].To < sorted[j].To
	})
	for i, pr := range sorted {
		if i > 0 && pr == sorted[i-1] {
			continue
		}
		parts = append(parts, fmt.Sprintf("%d-%d", pr.From, pr.To))
	}
	return strings.Join(parts, ", ")
}
