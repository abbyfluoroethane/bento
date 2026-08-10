package network

import (
	"encoding/xml"
	"fmt"
	"net/netip"
)

// UserNetwork describes one per-user libvirt network (SPEC 6.2). Every
// instance of the user attaches to this network's bridge.
type UserNetwork struct {
	// Name is the libvirt network name, for example "bento-user-3".
	Name string
	// Bridge is the bridge device name, for example "bento3". Linux
	// limits interface names to 15 bytes.
	Bridge string
	// Subnet is the user's /24.
	Subnet netip.Prefix
}

// NewUserNetwork returns the network for the user at a subnet index.
// Both names derive from the index, so they are deterministic and
// contain no user-controlled text.
func NewUserNetwork(p Plan, index int) (UserNetwork, error) {
	subnet, err := p.Subnet(index)
	if err != nil {
		return UserNetwork{}, err
	}
	return UserNetwork{
		Name:   fmt.Sprintf("bento-user-%d", index),
		Bridge: fmt.Sprintf("bento%d", index),
		Subnet: subnet,
	}, nil
}

// libvirt network XML elements. A fixed subset of the schema, marshaled
// with encoding/xml so every interpolated value is escaped (SPEC 4.2).
type networkXML struct {
	XMLName xml.Name   `xml:"network"`
	Name    string     `xml:"name"`
	Forward forwardXML `xml:"forward"`
	Bridge  bridgeXML  `xml:"bridge"`
	IP      ipXML      `xml:"ip"`
}

type forwardXML struct {
	Mode string `xml:"mode,attr"`
}

type bridgeXML struct {
	Name  string `xml:"name,attr"`
	STP   string `xml:"stp,attr"`
	Delay string `xml:"delay,attr"`
}

type ipXML struct {
	Address string `xml:"address,attr"`
	Netmask string `xml:"netmask,attr"`
}

// XML renders the libvirt network definition. The forward mode is
// "open": libvirt creates the bridge and installs no firewall rules, so
// Bento owns the whole policy (SPEC 6.2). There is no <dhcp> element;
// Bento assigns every address statically. The <ip> element puts the .1
// gateway address on the host side of the bridge.
func (n UserNetwork) XML() (string, error) {
	if n.Name == "" {
		return "", fmt.Errorf("network: user network has no name")
	}
	if n.Bridge == "" || len(n.Bridge) > 15 {
		return "", fmt.Errorf("network: bridge name %q is empty or longer than 15 bytes", n.Bridge)
	}
	if n.Subnet.Bits() != 24 || !n.Subnet.Addr().Is4() {
		return "", fmt.Errorf("network: %s is not an IPv4 /24", n.Subnet)
	}
	doc := networkXML{
		Name:    n.Name,
		Forward: forwardXML{Mode: "open"},
		Bridge:  bridgeXML{Name: n.Bridge, STP: "off", Delay: "0"},
		IP: ipXML{
			Address: Gateway(n.Subnet).String(),
			Netmask: "255.255.255.0",
		},
	}
	out, err := xml.MarshalIndent(doc, "", "  ")
	if err != nil {
		return "", fmt.Errorf("network: marshal network XML: %w", err)
	}
	return string(out) + "\n", nil
}
