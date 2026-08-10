package hypervisor

import (
	"context"
	"fmt"
	"time"

	"github.com/digitalocean/go-libvirt"
	"github.com/digitalocean/go-libvirt/socket/dialers"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// DefaultSocketPath is the libvirtd local socket for qemu:///system.
const DefaultSocketPath = "/var/run/libvirt/libvirt-sock"

// defaultStopTimeout is the wait between the ACPI shutdown request and
// the destroy (SPEC 11.1).
const defaultStopTimeout = 60 * time.Second

// defaultPollInterval is how often Stop re-reads the domain state while
// waiting for the guest to power off.
const defaultPollInterval = 500 * time.Millisecond

// undefineFlags is passed to every virDomainUndefineFlags call. Every
// Bento domain boots UEFI through OVMF (SPEC 5), so it owns an NVRAM
// file, and libvirt refuses a plain undefine of such a domain. The
// NVRAM flag removes the file with the definition, which the four-step
// `rm` of SPEC 11.1 requires.
const undefineFlags = libvirt.DomainUndefineNvram

// libvirtAPI is the slice of github.com/digitalocean/go-libvirt that
// Client uses. *libvirt.Libvirt satisfies it; tests substitute a fake.
type libvirtAPI interface {
	DomainDefineXML(xml string) (libvirt.Domain, error)
	DomainCreate(dom libvirt.Domain) error
	DomainShutdown(dom libvirt.Domain) error
	DomainReboot(dom libvirt.Domain, flags libvirt.DomainRebootFlagValues) error
	DomainDestroy(dom libvirt.Domain) error
	DomainUndefineFlags(dom libvirt.Domain, flags libvirt.DomainUndefineFlagsValues) error
	DomainSetAutostart(dom libvirt.Domain, autostart int32) error
	DomainLookupByName(name string) (libvirt.Domain, error)
	DomainGetState(dom libvirt.Domain, flags uint32) (int32, int32, error)
	ConnectListAllDomains(needResults int32, flags libvirt.ConnectListAllDomainsFlags) ([]libvirt.Domain, uint32, error)
}

// Client implements Hypervisor over the libvirt RPC protocol at
// qemu:///system (SPEC 4.1).
type Client struct {
	api          libvirtAPI
	conn         *libvirt.Libvirt // nil when constructed over a fake api
	stopTimeout  time.Duration
	pollInterval time.Duration
	// sleep is injectable so tests do not wait 60 real seconds.
	sleep func(ctx context.Context, d time.Duration) error
}

var _ Hypervisor = (*Client)(nil)

// Connect dials the libvirtd local unix socket and connects to
// qemu:///system. An empty socketPath uses DefaultSocketPath.
func Connect(socketPath string) (*Client, error) {
	if socketPath == "" {
		socketPath = DefaultSocketPath
	}
	conn := libvirt.NewWithDialer(dialers.NewLocal(dialers.WithSocket(socketPath)))
	if err := conn.ConnectToURI(libvirt.QEMUSystem); err != nil {
		return nil, fmt.Errorf("connect to libvirtd at %s: %w", socketPath, err)
	}
	c := newClient(conn)
	c.conn = conn
	return c, nil
}

// newClient builds a Client over any libvirtAPI. Tests use it with a
// fake API to exercise the operation logic without a libvirtd.
func newClient(api libvirtAPI) *Client {
	return &Client{
		api:          api,
		stopTimeout:  defaultStopTimeout,
		pollInterval: defaultPollInterval,
		sleep:        sleepContext,
	}
}

// Close disconnects from libvirtd.
func (c *Client) Close() error {
	if c.conn == nil {
		return nil
	}
	return c.conn.Disconnect()
}

func sleepContext(ctx context.Context, d time.Duration) error {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-t.C:
		return nil
	}
}

// Create maps the `new` action: DefineXML, clear the autostart flag
// (SPEC 11.2: the control plane restores state, not libvirt), then
// Create. A failed start undefines the domain again so a retry does not
// hit a stale definition.
func (c *Client) Create(_ context.Context, domXML string) error {
	dom, err := c.api.DomainDefineXML(domXML)
	if err != nil {
		return fmt.Errorf("define domain: %w", err)
	}
	if err := c.api.DomainSetAutostart(dom, 0); err != nil {
		_ = c.api.DomainUndefineFlags(dom, undefineFlags)
		return fmt.Errorf("clear autostart on %s: %w", dom.Name, err)
	}
	if err := c.api.DomainCreate(dom); err != nil {
		_ = c.api.DomainUndefineFlags(dom, undefineFlags)
		return fmt.Errorf("start domain %s: %w", dom.Name, err)
	}
	return nil
}

// Start maps the `start` action: virDomainCreate on a defined domain.
func (c *Client) Start(_ context.Context, name string) error {
	dom, err := c.lookup(name)
	if err != nil {
		return err
	}
	if err := c.api.DomainCreate(dom); err != nil {
		return fmt.Errorf("start domain %s: %w", name, err)
	}
	return nil
}

// Stop maps the `stop` action: ACPI shutdown request, wait up to the
// stop timeout for the guest to power off, destroy only after the
// timeout (SPEC 11.1). The return value reports which path was taken.
func (c *Client) Stop(ctx context.Context, name string) (StopResult, error) {
	dom, err := c.lookup(name)
	if err != nil {
		return "", err
	}
	state, err := c.state(dom)
	if err != nil {
		return "", err
	}
	if state == types.StateStopped {
		return StopNoop, nil
	}
	if err := c.api.DomainShutdown(dom); err != nil {
		return "", fmt.Errorf("shutdown domain %s: %w", name, err)
	}
	var elapsed time.Duration
	for {
		state, err := c.state(dom)
		if err != nil {
			return "", err
		}
		if state == types.StateStopped {
			return StopGraceful, nil
		}
		if elapsed >= c.stopTimeout {
			break
		}
		if err := c.sleep(ctx, c.pollInterval); err != nil {
			return "", err
		}
		elapsed += c.pollInterval
	}
	if err := c.api.DomainDestroy(dom); err != nil {
		return "", fmt.Errorf("destroy domain %s after shutdown timeout: %w", name, err)
	}
	return StopForced, nil
}

// Reboot maps the `restart` action.
func (c *Client) Reboot(_ context.Context, name string) error {
	dom, err := c.lookup(name)
	if err != nil {
		return err
	}
	if err := c.api.DomainReboot(dom, 0); err != nil {
		return fmt.Errorf("reboot domain %s: %w", name, err)
	}
	return nil
}

// Remove maps the domain half of the `rm` action: destroy if running,
// then undefine with the NVRAM flag, so the UEFI variable store goes
// with the domain and the undefine cannot fail on it (SPEC 11.1, 5).
func (c *Client) Remove(_ context.Context, name string) error {
	dom, err := c.lookup(name)
	if err != nil {
		return err
	}
	state, err := c.state(dom)
	if err != nil {
		return err
	}
	if state != types.StateStopped {
		if err := c.api.DomainDestroy(dom); err != nil {
			return fmt.Errorf("destroy domain %s: %w", name, err)
		}
	}
	if err := c.api.DomainUndefineFlags(dom, undefineFlags); err != nil {
		return fmt.Errorf("undefine domain %s: %w", name, err)
	}
	return nil
}

// List returns every domain, defined or running, with its observed
// state (SPEC 11.2 step 1 reads this at startup).
func (c *Client) List(_ context.Context) ([]DomainInfo, error) {
	doms, _, err := c.api.ConnectListAllDomains(1, 0)
	if err != nil {
		return nil, fmt.Errorf("list domains: %w", err)
	}
	infos := make([]DomainInfo, 0, len(doms))
	for _, dom := range doms {
		state, err := c.state(dom)
		if err != nil {
			return nil, err
		}
		infos = append(infos, DomainInfo{
			Name:  dom.Name,
			UUID:  formatUUID(dom.UUID),
			State: state,
		})
	}
	return infos, nil
}

// State returns the observed state of one domain.
func (c *Client) State(_ context.Context, name string) (types.State, error) {
	dom, err := c.lookup(name)
	if err != nil {
		return "", err
	}
	return c.state(dom)
}

func (c *Client) lookup(name string) (libvirt.Domain, error) {
	dom, err := c.api.DomainLookupByName(name)
	if err != nil {
		if isLibvirtError(err, libvirt.ErrNoDomain) {
			return libvirt.Domain{}, fmt.Errorf("lookup domain %s: %w", name, ErrDomainNotFound)
		}
		return libvirt.Domain{}, fmt.Errorf("lookup domain %s: %w", name, err)
	}
	return dom, nil
}

func (c *Client) state(dom libvirt.Domain) (types.State, error) {
	raw, _, err := c.api.DomainGetState(dom, 0)
	if err != nil {
		return "", fmt.Errorf("get state of domain %s: %w", dom.Name, err)
	}
	return stateFromLibvirt(libvirt.DomainState(raw)), nil
}

// stateFromLibvirt collapses the eight libvirt states into the two
// observed values of SPEC 11.1. A domain that is active in any form
// counts as running; only a domain with no live QEMU process counts as
// stopped. The third observed value, starting, is Bento-derived during
// the host reboot restore and never comes from libvirt.
func stateFromLibvirt(s libvirt.DomainState) types.State {
	switch s {
	case libvirt.DomainRunning, libvirt.DomainBlocked, libvirt.DomainPaused,
		libvirt.DomainShutdown, libvirt.DomainPmsuspended:
		return types.StateRunning
	default:
		// DomainNostate, DomainShutoff, DomainCrashed.
		return types.StateStopped
	}
}

// formatUUID renders a libvirt 16-byte UUID in the canonical
// 8-4-4-4-12 form.
func formatUUID(u libvirt.UUID) string {
	return fmt.Sprintf("%x-%x-%x-%x-%x", u[0:4], u[4:6], u[6:8], u[8:10], u[10:16])
}
