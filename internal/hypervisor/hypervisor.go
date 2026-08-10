// Package hypervisor manages libvirt domains: XML generation,
// define/start/stop/undefine, and host requirement checks
// (SPEC sections 4.2, 5, 11).
package hypervisor

import (
	"context"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// StopResult reports which path a stop took (SPEC section 11.1: report
// whether the guest honored the ACPI request or was destroyed).
type StopResult string

// Stop paths.
const (
	// StopGraceful means the guest shut down after the ACPI request.
	StopGraceful StopResult = "graceful"
	// StopForced means the 60 second wait expired and the domain was
	// destroyed.
	StopForced StopResult = "forced"
	// StopNoop means the domain was already shut off.
	StopNoop StopResult = "already-stopped"
)

// DomainInfo is one libvirt domain with its observed state.
type DomainInfo struct {
	Name  string
	UUID  string
	State types.State
}

// Hypervisor is the consumer-facing surface of the libvirt layer. The
// lifecycle package drives it; tests use Fake. Every method maps to the
// action table in SPEC section 11.1.
type Hypervisor interface {
	// Create defines a domain from XML, clears the libvirt autostart
	// flag (SPEC 11.2: Bento restores, not libvirt), and starts it.
	Create(ctx context.Context, xml string) error
	// Start starts an already defined domain.
	Start(ctx context.Context, name string) error
	// Stop sends an ACPI shutdown request, waits up to 60 seconds for
	// the guest to power off, and destroys the domain only after the
	// timeout. It reports which path the stop took.
	Stop(ctx context.Context, name string) (StopResult, error)
	// Reboot asks the guest to reboot.
	Reboot(ctx context.Context, name string) error
	// Remove destroys the domain if it is running and undefines it.
	Remove(ctx context.Context, name string) error
	// List returns every domain known to libvirt with its observed
	// state, defined or running.
	List(ctx context.Context) ([]DomainInfo, error)
	// State returns the observed state of one domain.
	State(ctx context.Context, name string) (types.State, error)
}
