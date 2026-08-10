// Package cloudinit builds NoCloud cloud-init seed ISOs for instance
// first boot (SPEC section 5.2).
package cloudinit

import (
	"fmt"
	"net"
	"net/netip"
	"strings"
)

// Seed is the data for one NoCloud seed: hostname, one user account with
// the owner's public keys, and the static network configuration that
// Bento assigned (SPEC sections 5.2 and 6.2).
type Seed struct {
	// InstanceID becomes the NoCloud instance-id. Use the instance UUID.
	InstanceID string
	// Hostname is the instance name (SPEC 5.2: host name = instance name).
	Hostname string
	// UserName is the one user account to create.
	UserName string
	// AuthorizedKeys are the public keys of the owner, one per entry.
	AuthorizedKeys []string
	// MAC is the interface MAC address Bento assigned. The network
	// configuration matches on it, so the guest interface name does not
	// matter.
	MAC string
	// AddressCIDR is the static instance address with prefix length, for
	// example "10.20.3.5/24".
	AddressCIDR string
	// Gateway is the gateway address.
	Gateway string
	// DNS is the DNS server address.
	DNS string
}

// Validate checks that every field is present and well formed. The
// renderers call it, so a malformed seed can never reach an ISO.
func (s Seed) Validate() error {
	for name, v := range map[string]string{
		"instance id": s.InstanceID,
		"hostname":    s.Hostname,
		"user name":   s.UserName,
	} {
		if v == "" {
			return fmt.Errorf("cloudinit: %s is empty", name)
		}
		if err := singleLine(name, v); err != nil {
			return err
		}
	}
	if strings.ContainsAny(s.Hostname, " \t") {
		return fmt.Errorf("cloudinit: hostname %q contains whitespace", s.Hostname)
	}
	if strings.ContainsAny(s.UserName, " \t") {
		return fmt.Errorf("cloudinit: user name %q contains whitespace", s.UserName)
	}
	if len(s.AuthorizedKeys) == 0 {
		return fmt.Errorf("cloudinit: no authorized keys; the instance would be unreachable")
	}
	for _, k := range s.AuthorizedKeys {
		if strings.TrimSpace(k) == "" {
			return fmt.Errorf("cloudinit: empty authorized key")
		}
		if err := singleLine("authorized key", k); err != nil {
			return err
		}
	}
	if _, err := net.ParseMAC(s.MAC); err != nil {
		return fmt.Errorf("cloudinit: invalid MAC address %q: %w", s.MAC, err)
	}
	if _, err := netip.ParsePrefix(s.AddressCIDR); err != nil {
		return fmt.Errorf("cloudinit: invalid address %q (want address/prefix): %w", s.AddressCIDR, err)
	}
	if _, err := netip.ParseAddr(s.Gateway); err != nil {
		return fmt.Errorf("cloudinit: invalid gateway %q: %w", s.Gateway, err)
	}
	if _, err := netip.ParseAddr(s.DNS); err != nil {
		return fmt.Errorf("cloudinit: invalid DNS server %q: %w", s.DNS, err)
	}
	return nil
}

func singleLine(name, v string) error {
	for _, r := range v {
		if r < 0x20 || r == 0x7f {
			return fmt.Errorf("cloudinit: %s contains a control character", name)
		}
	}
	return nil
}

// quote renders a string as a double-quoted YAML scalar. Validate has
// already rejected control characters, so escaping backslash and quote is
// sufficient.
var yamlEscaper = strings.NewReplacer(`\`, `\\`, `"`, `\"`)

func quote(v string) string {
	return `"` + yamlEscaper.Replace(v) + `"`
}

// MetaData renders the NoCloud meta-data file.
func (s Seed) MetaData() (string, error) {
	if err := s.Validate(); err != nil {
		return "", err
	}
	var b strings.Builder
	fmt.Fprintf(&b, "instance-id: %s\n", quote(s.InstanceID))
	fmt.Fprintf(&b, "local-hostname: %s\n", quote(s.Hostname))
	return b.String(), nil
}

// UserData renders the NoCloud user-data file. Per SPEC section 5.2 it
// sets the host name to the instance name, creates one user account,
// installs the public keys of the owner, and installs and starts
// qemu-guest-agent. The static network settings live in NetworkConfig.
func (s Seed) UserData() (string, error) {
	if err := s.Validate(); err != nil {
		return "", err
	}
	var b strings.Builder
	b.WriteString("#cloud-config\n")
	fmt.Fprintf(&b, "hostname: %s\n", quote(s.Hostname))
	b.WriteString("users:\n")
	fmt.Fprintf(&b, "  - name: %s\n", quote(s.UserName))
	b.WriteString("    shell: /bin/bash\n")
	b.WriteString("    lock_passwd: true\n")
	b.WriteString("    sudo: \"ALL=(ALL) NOPASSWD:ALL\"\n")
	b.WriteString("    ssh_authorized_keys:\n")
	for _, k := range s.AuthorizedKeys {
		fmt.Fprintf(&b, "      - %s\n", quote(strings.TrimSpace(k)))
	}
	b.WriteString("package_update: true\n")
	b.WriteString("packages:\n")
	b.WriteString("  - qemu-guest-agent\n")
	b.WriteString("runcmd:\n")
	b.WriteString("  - [systemctl, enable, --now, qemu-guest-agent]\n")
	return b.String(), nil
}

// NetworkConfig renders the NoCloud network-config file (version 2). It
// sets the static address, the gateway, and the DNS server that Bento
// assigned (SPEC sections 5.2 and 6.2), matching the interface by the MAC
// address Bento chose.
func (s Seed) NetworkConfig() (string, error) {
	if err := s.Validate(); err != nil {
		return "", err
	}
	mac, err := net.ParseMAC(s.MAC)
	if err != nil {
		return "", fmt.Errorf("cloudinit: invalid MAC address %q: %w", s.MAC, err)
	}
	var b strings.Builder
	b.WriteString("version: 2\n")
	b.WriteString("ethernets:\n")
	b.WriteString("  primary:\n")
	b.WriteString("    match:\n")
	fmt.Fprintf(&b, "      macaddress: %s\n", quote(mac.String()))
	b.WriteString("    addresses:\n")
	fmt.Fprintf(&b, "      - %s\n", quote(s.AddressCIDR))
	b.WriteString("    routes:\n")
	b.WriteString("      - to: \"0.0.0.0/0\"\n")
	fmt.Fprintf(&b, "        via: %s\n", quote(s.Gateway))
	b.WriteString("    nameservers:\n")
	b.WriteString("      addresses:\n")
	fmt.Fprintf(&b, "        - %s\n", quote(s.DNS))
	return b.String(), nil
}
