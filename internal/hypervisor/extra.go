package hypervisor

// Optional capabilities of Client beyond the Hypervisor interface:
// redefining an existing domain, clearing the autostart flag, and
// managing per-user libvirt networks (SPEC 6.2). The lifecycle package
// discovers the first two by type assertion; cmd/bentod discovers the
// network methods the same way, so fakes without them still satisfy
// Hypervisor.

import (
	"context"
	"errors"
	"fmt"

	"github.com/digitalocean/go-libvirt"
)

// networkAPI is the slice of go-libvirt used for per-user networks.
// *libvirt.Libvirt satisfies it; the domain-only test fakes do not.
type networkAPI interface {
	NetworkLookupByName(name string) (libvirt.Network, error)
	NetworkDefineXML(xml string) (libvirt.Network, error)
	NetworkCreate(net libvirt.Network) error
	NetworkIsActive(net libvirt.Network) (int32, error)
	NetworkSetAutostart(net libvirt.Network, autostart int32) error
}

// Define replaces the persistent XML of a domain without starting or
// stopping it (virDomainDefineXML on an existing definition). It
// implements the lifecycle package's Definer capability: resize XML
// edits, the first-boot CD-ROM detach, and rename all go through it.
func (c *Client) Define(_ context.Context, domXML string) error {
	if _, err := c.api.DomainDefineXML(domXML); err != nil {
		return fmt.Errorf("define domain: %w", err)
	}
	return nil
}

// ClearAutostart clears the libvirt autostart flag of a domain
// (SPEC 11.2: the control plane restores state, not libvirt). It
// implements the lifecycle package's AutostartClearer capability.
func (c *Client) ClearAutostart(_ context.Context, name string) error {
	dom, err := c.lookup(name)
	if err != nil {
		return err
	}
	if err := c.api.DomainSetAutostart(dom, 0); err != nil {
		return fmt.Errorf("clear autostart on %s: %w", name, err)
	}
	return nil
}

// EnsureNetwork defines the named libvirt network from netXML when it
// does not exist and starts it when it is not active (SPEC 6.2: one
// network per user, created at registration and re-ensured at control
// plane startup). Autostart stays on so the bridge and its gateway
// address survive a libvirtd restart; domains never autostart
// (SPEC 11.2), so no start decision moves to libvirt.
func (c *Client) EnsureNetwork(_ context.Context, name, netXML string) error {
	napi, ok := c.api.(networkAPI)
	if !ok {
		return errors.New("hypervisor: this connection cannot manage networks")
	}
	net, err := napi.NetworkLookupByName(name)
	switch {
	case err == nil:
	case isLibvirtError(err, libvirt.ErrNoNetwork):
		if net, err = napi.NetworkDefineXML(netXML); err != nil {
			return fmt.Errorf("define network %s: %w", name, err)
		}
	default:
		return fmt.Errorf("lookup network %s: %w", name, err)
	}
	if err := napi.NetworkSetAutostart(net, 1); err != nil {
		return fmt.Errorf("set autostart on network %s: %w", name, err)
	}
	active, err := napi.NetworkIsActive(net)
	if err != nil {
		return fmt.Errorf("check network %s: %w", name, err)
	}
	if active == 0 {
		if err := napi.NetworkCreate(net); err != nil {
			return fmt.Errorf("start network %s: %w", name, err)
		}
	}
	return nil
}

// isLibvirtError reports whether err is a libvirt RPC error with the
// given code.
func isLibvirtError(err error, code libvirt.ErrorNumber) bool {
	var le libvirt.Error
	return errors.As(err, &le) && le.Code == uint32(code)
}
