package hypervisor

import (
	"context"
	"encoding/xml"
	"fmt"
	"sort"
	"sync"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// FakeDomain is one domain held by Fake.
type FakeDomain struct {
	Name  string
	UUID  string
	XML   string
	State types.State
	// Autostart mirrors the libvirt flag. Create always clears it
	// (SPEC 11.2), so it stays false unless a test sets it.
	Autostart bool
}

// Fake is an in-memory Hypervisor for tests. It is exported so the
// lifecycle package can reuse it. The zero value is ready to use.
type Fake struct {
	mu      sync.Mutex
	domains map[string]*FakeDomain

	// Calls records every operation as "op name" ("create bento-web").
	Calls []string
	// Hook, when set, runs before every operation. A non-nil return
	// aborts the operation with that error. Tests use it to inject
	// failures.
	Hook func(op, name string) error
	// ForceStop makes Stop report StopForced, as if the guest ignored
	// the ACPI request and the destroy path ran. Default false:
	// Stop reports StopGraceful.
	ForceStop bool
}

var _ Hypervisor = (*Fake)(nil)

// ErrDomainNotFound is returned by Fake for operations on an unknown
// domain name.
var ErrDomainNotFound = fmt.Errorf("hypervisor: domain not found")

// ErrDomainExists is returned by Fake.Create when the name is taken.
var ErrDomainExists = fmt.Errorf("hypervisor: domain already exists")

// fakeDomainXML is the subset of the domain XML Fake reads back.
type fakeDomainXML struct {
	Name string `xml:"name"`
	UUID string `xml:"uuid"`
}

func (f *Fake) begin(op, name string) error {
	f.Calls = append(f.Calls, op+" "+name)
	if f.Hook != nil {
		return f.Hook(op, name)
	}
	return nil
}

// Create parses the name and UUID out of the XML, which also verifies
// that the caller produced well-formed XML.
func (f *Fake) Create(_ context.Context, domXML string) error {
	var parsed fakeDomainXML
	if err := xml.Unmarshal([]byte(domXML), &parsed); err != nil {
		return fmt.Errorf("fake create: bad domain xml: %w", err)
	}
	if parsed.Name == "" || parsed.UUID == "" {
		return fmt.Errorf("fake create: domain xml missing name or uuid")
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	if err := f.begin("create", parsed.Name); err != nil {
		return err
	}
	if f.domains == nil {
		f.domains = make(map[string]*FakeDomain)
	}
	if _, ok := f.domains[parsed.Name]; ok {
		return fmt.Errorf("%w: %s", ErrDomainExists, parsed.Name)
	}
	f.domains[parsed.Name] = &FakeDomain{
		Name:  parsed.Name,
		UUID:  parsed.UUID,
		XML:   domXML,
		State: types.StateRunning,
	}
	return nil
}

func (f *Fake) Start(_ context.Context, name string) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	if err := f.begin("start", name); err != nil {
		return err
	}
	dom, ok := f.domains[name]
	if !ok {
		return fmt.Errorf("%w: %s", ErrDomainNotFound, name)
	}
	dom.State = types.StateRunning
	return nil
}

func (f *Fake) Stop(_ context.Context, name string) (StopResult, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if err := f.begin("stop", name); err != nil {
		return "", err
	}
	dom, ok := f.domains[name]
	if !ok {
		return "", fmt.Errorf("%w: %s", ErrDomainNotFound, name)
	}
	if dom.State == types.StateStopped {
		return StopNoop, nil
	}
	dom.State = types.StateStopped
	if f.ForceStop {
		return StopForced, nil
	}
	return StopGraceful, nil
}

func (f *Fake) Reboot(_ context.Context, name string) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	if err := f.begin("reboot", name); err != nil {
		return err
	}
	dom, ok := f.domains[name]
	if !ok {
		return fmt.Errorf("%w: %s", ErrDomainNotFound, name)
	}
	if dom.State != types.StateRunning {
		return fmt.Errorf("fake reboot: domain %s is not running", name)
	}
	return nil
}

func (f *Fake) Remove(_ context.Context, name string) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	if err := f.begin("remove", name); err != nil {
		return err
	}
	if _, ok := f.domains[name]; !ok {
		return fmt.Errorf("%w: %s", ErrDomainNotFound, name)
	}
	delete(f.domains, name)
	return nil
}

func (f *Fake) List(_ context.Context) ([]DomainInfo, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if err := f.begin("list", ""); err != nil {
		return nil, err
	}
	infos := make([]DomainInfo, 0, len(f.domains))
	for _, dom := range f.domains {
		infos = append(infos, DomainInfo{Name: dom.Name, UUID: dom.UUID, State: dom.State})
	}
	sort.Slice(infos, func(i, j int) bool { return infos[i].Name < infos[j].Name })
	return infos, nil
}

func (f *Fake) State(_ context.Context, name string) (types.State, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if err := f.begin("state", name); err != nil {
		return "", err
	}
	dom, ok := f.domains[name]
	if !ok {
		return "", fmt.Errorf("%w: %s", ErrDomainNotFound, name)
	}
	return dom.State, nil
}

// Domain returns the stored domain, or nil if it does not exist. Tests
// use it to inspect XML and state.
func (f *Fake) Domain(name string) *FakeDomain {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.domains[name]
}

// SetDomain seeds a domain directly, for tests that need a preexisting
// state (for example the host reboot restore in SPEC 11.2).
func (f *Fake) SetDomain(dom FakeDomain) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.domains == nil {
		f.domains = make(map[string]*FakeDomain)
	}
	f.domains[dom.Name] = &dom
}
