// Package lifecycle implements instance lifecycle actions, desired-state
// tracking, and host reboot restore (SPEC section 11). It is the control
// plane orchestration layer: it drives the hypervisor, the image store, the
// cloud-init builder, and the data layer, and owns the ordering and the
// unwind logic between them. It never touches the host directly; every host
// operation goes through an injected interface so unit tests run anywhere.
package lifecycle

import (
	"context"
	cryptorand "crypto/rand"
	"errors"
	"fmt"
	"log/slog"
	"net/netip"
	"path/filepath"
	"time"

	"github.com/abbyfluoroethane/bento/internal/cloudinit"
	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/network"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// Store is the consumer-side view of the data layer that lifecycle needs.
// *store.Store satisfies it.
type Store interface {
	// CreateInstance runs the name cooldown check, the quota check, and
	// the insert in one transaction (SPEC 6.1, 7.2).
	CreateInstance(inst types.Instance, nameCooldown time.Duration) error
	// DeleteInstance removes the row (shares cascade with it) and inserts
	// the name into released_names, in one transaction (SPEC 11.1 steps
	// 3 and 4).
	DeleteInstance(uuid string) (types.Instance, error)
	Instance(uuid string) (types.Instance, error)
	Instances() ([]types.Instance, error)
	// InstancesToRestore lists desired running and observed stopped
	// (SPEC 11.2).
	InstancesToRestore() ([]types.Instance, error)
	Image(name string) (types.Image, error)
	UserByID(id int64) (types.User, error)
	// RenameInstance changes the name and puts the old name into the
	// cooldown, in one transaction (SPEC 7.2, 7.3).
	RenameInstance(uuid, newName string, nameCooldown time.Duration) error
	// Resize reruns the quota check with the instance's own use excluded.
	Resize(uuid string, vcpu int, memoryMiB, diskGiB int64, nested bool) error
	SetDesiredState(uuid string, state types.DesiredState) error
	SetObservedState(uuid string, state types.State) error
	UpdateObservedStates(states map[string]types.State) error
}

// ImageStore is the consumer-side view of the image store. *images.Store
// satisfies it.
type ImageStore interface {
	// CreateOverlay creates the qcow2 overlay backed by the stored image
	// version and resizes it to the requested disk size (SPEC 5.2).
	CreateOverlay(ctx context.Context, checksum, overlayPath string, diskGiB int64) error
}

// ISOBuilder builds the NoCloud seed ISO. *cloudinit.Builder satisfies it.
type ISOBuilder interface {
	Build(ctx context.Context, seed cloudinit.Seed, isoPath string) error
}

// OverlayResizer grows an existing overlay. QemuImgResizer is the real
// implementation; tests inject a fake.
type OverlayResizer interface {
	ResizeOverlay(ctx context.Context, overlayPath string, diskGiB int64) error
}

// Definer is an optional hypervisor capability: replace the persistent XML
// of an existing domain without starting or stopping it (virDomainDefineXML
// on an already defined domain). The lifecycle layer uses it for the resize
// XML edit (SPEC 11.1) and for detaching the cloud-init CD-ROM after the
// first boot (SPEC 5.2). When the configured Hypervisor does not implement
// it, both operations log a warning and the definition catches up the next
// time the domain is redefined.
type Definer interface {
	Define(ctx context.Context, xml string) error
}

// AutostartClearer is an optional hypervisor capability: clear the libvirt
// autostart flag of a domain. Create already clears the flag at define time
// (SPEC 11.2); the restore clears it again on every domain when the
// hypervisor supports it, so a flag set out of band cannot race the restore.
type AutostartClearer interface {
	ClearAutostart(ctx context.Context, name string) error
}

// Errors returned by lifecycle actions.
var (
	// ErrNestedUnavailable rejects a new or resize with nested=true when
	// the host has nesting off (SPEC 5.5).
	ErrNestedUnavailable = errors.New("lifecycle: nested virtualization is off on this host")
	// ErrDiskShrink rejects a resize that shrinks the disk. Version 1
	// only grows the overlay (SPEC 11.1).
	ErrDiskShrink = errors.New("lifecycle: disk size cannot shrink")
	// ErrNoImageVersion rejects a new when the image has no fetched
	// version to back the overlay (SPEC 5.1).
	ErrNoImageVersion = errors.New("lifecycle: image has no fetched version; run fetch-images")
)

// Config wires a Manager. Hypervisor, Store, Images, ISO, Plan, and
// StorageDir are required; everything else has a default.
type Config struct {
	Hypervisor hypervisor.Hypervisor
	Store      Store
	Images     ImageStore
	ISO        ISOBuilder
	// Resizer grows overlays on disk resize. Default: QemuImgResizer{}.
	Resizer OverlayResizer
	// Plan is the operator private range carved into per-user /24s.
	Plan network.Plan
	// StorageDir holds the per-instance overlay and seed ISO files.
	StorageDir string
	// NameCooldown is the released-name cooldown (SPEC 7.2).
	// Default 24 hours.
	NameCooldown time.Duration
	// BatchSize is the host reboot restore batch size (SPEC 11.2).
	// Default 4.
	BatchSize int
	// DNS is written into the cloud-init network configuration.
	// Default network.DefaultDNS.
	DNS []netip.Addr
	// Logger receives progress and warnings. Default slog.Default().
	Logger *slog.Logger
	// NestedEnabled reports whether the host KVM module has nesting on
	// (SPEC 5.5). Default: hypervisor.NestedEnabled with the default
	// host probes.
	NestedEnabled func() (bool, string)
	// PollInterval is the observed-state poll interval (SPEC 12).
	// Default 30 seconds.
	PollInterval time.Duration
	// StartPollInterval and StartTimeout bound the per-instance wait for
	// running during the restore. Defaults 500ms and 5 minutes.
	StartPollInterval time.Duration
	StartTimeout      time.Duration
	// Sleep is the wait primitive, injectable for tests.
	// Default time.Sleep.
	Sleep func(time.Duration)
	// NewUUID mints instance UUIDs, injectable for tests.
	NewUUID func() string
	// Now is the clock, injectable for tests. Default time.Now.
	Now func() time.Time
	// DeleteISO removes a seed ISO file, injectable for tests.
	// Default cloudinit.Delete.
	DeleteISO func(isoPath string) error
	// ISOExists reports whether a seed ISO file is still on disk,
	// injectable for tests. Default: stat the path.
	ISOExists func(isoPath string) bool
}

// Manager orchestrates the instance lifecycle.
type Manager struct {
	hyp        hypervisor.Hypervisor
	store      Store
	images     ImageStore
	iso        ISOBuilder
	resizer    OverlayResizer
	plan       network.Plan
	storageDir string
	cooldown   time.Duration
	batchSize  int
	dns        []netip.Addr
	log        *slog.Logger
	nested     func() (bool, string)
	pollEvery  time.Duration
	startPoll  time.Duration
	startWait  time.Duration
	sleep      func(time.Duration)
	newUUID    func() string
	now        func() time.Time
	deleteISO  func(string) error
	isoExists  func(string) bool
}

// NewManager validates the configuration and returns a Manager.
func NewManager(cfg Config) (*Manager, error) {
	switch {
	case cfg.Hypervisor == nil:
		return nil, errors.New("lifecycle: config needs a Hypervisor")
	case cfg.Store == nil:
		return nil, errors.New("lifecycle: config needs a Store")
	case cfg.Images == nil:
		return nil, errors.New("lifecycle: config needs an ImageStore")
	case cfg.ISO == nil:
		return nil, errors.New("lifecycle: config needs an ISOBuilder")
	case cfg.StorageDir == "":
		return nil, errors.New("lifecycle: config needs a StorageDir")
	case !cfg.Plan.Range().IsValid():
		return nil, errors.New("lifecycle: config needs a network Plan")
	}
	m := &Manager{
		hyp:        cfg.Hypervisor,
		store:      cfg.Store,
		images:     cfg.Images,
		iso:        cfg.ISO,
		resizer:    cfg.Resizer,
		plan:       cfg.Plan,
		storageDir: cfg.StorageDir,
		cooldown:   cfg.NameCooldown,
		batchSize:  cfg.BatchSize,
		dns:        cfg.DNS,
		log:        cfg.Logger,
		nested:     cfg.NestedEnabled,
		pollEvery:  cfg.PollInterval,
		startPoll:  cfg.StartPollInterval,
		startWait:  cfg.StartTimeout,
		sleep:      cfg.Sleep,
		newUUID:    cfg.NewUUID,
		now:        cfg.Now,
		deleteISO:  cfg.DeleteISO,
		isoExists:  cfg.ISOExists,
	}
	if m.resizer == nil {
		m.resizer = QemuImgResizer{}
	}
	if m.cooldown <= 0 {
		m.cooldown = 24 * time.Hour
	}
	if m.batchSize <= 0 {
		m.batchSize = 4
	}
	if len(m.dns) == 0 {
		m.dns = network.DefaultDNS
	}
	if m.log == nil {
		m.log = slog.Default()
	}
	if m.nested == nil {
		m.nested = func() (bool, string) {
			return hypervisor.NestedEnabled(hypervisor.CheckConfig{}, hypervisor.DefaultCheckDeps())
		}
	}
	if m.pollEvery <= 0 {
		m.pollEvery = 30 * time.Second
	}
	if m.startPoll <= 0 {
		m.startPoll = 500 * time.Millisecond
	}
	if m.startWait <= 0 {
		m.startWait = 5 * time.Minute
	}
	if m.sleep == nil {
		m.sleep = time.Sleep
	}
	if m.newUUID == nil {
		m.newUUID = randomUUID
	}
	if m.now == nil {
		m.now = time.Now
	}
	if m.deleteISO == nil {
		m.deleteISO = cloudinit.Delete
	}
	if m.isoExists == nil {
		m.isoExists = fileExists
	}
	return m, nil
}

// OverlayPath returns the root volume path of an instance. The path derives
// from the UUID, the identifier, so a rename never moves a disk.
func (m *Manager) OverlayPath(uuid string) string {
	return filepath.Join(m.storageDir, uuid+".qcow2")
}

// SeedISOPath returns the cloud-init seed ISO path of an instance. The file
// exists only between creation and the first successful boot (SPEC 5.2).
func (m *Manager) SeedISOPath(uuid string) string {
	return filepath.Join(m.storageDir, uuid+"-seed.iso")
}

// checkNested rejects a request for nested virtualization when the host has
// nesting off, naming the module parameter (SPEC 5.5).
func (m *Manager) checkNested() error {
	enabled, detail := m.nested()
	if enabled {
		return nil
	}
	msg := "load the KVM module with kvm_intel.nested=1 (Intel) or kvm_amd.nested=1 (AMD)"
	if detail != "" {
		msg += ": " + detail
	}
	return fmt.Errorf("%w: %s", ErrNestedUnavailable, msg)
}

// userNetworkName resolves the libvirt network name of the subnet owner
// (SPEC 6.2).
func (m *Manager) userNetworkName(subnet netip.Prefix) (string, error) {
	index, err := m.plan.Index(subnet)
	if err != nil {
		return "", fmt.Errorf("lifecycle: subnet %s outside the private range: %w", subnet, err)
	}
	un, err := network.NewUserNetwork(m.plan, index)
	if err != nil {
		return "", err
	}
	return un.Name, nil
}

// domainXML builds the domain XML for an instance from its stored
// configuration. withISO attaches the seed ISO (before the first boot).
func (m *Manager) domainXML(inst types.Instance, owner types.User, withISO bool) (string, error) {
	subnet, err := netip.ParsePrefix(owner.Subnet)
	if err != nil {
		return "", fmt.Errorf("lifecycle: user %s has a bad subnet %q: %w", owner.Name, owner.Subnet, err)
	}
	netName, err := m.userNetworkName(subnet)
	if err != nil {
		return "", err
	}
	spec := hypervisor.DomainSpec{
		Name:      inst.Name,
		UUID:      inst.UUID,
		VCPU:      inst.VCPU,
		MemoryMiB: inst.MemoryMiB,
		DiskPath:  m.OverlayPath(inst.UUID),
		Network:   netName,
		MAC:       inst.MAC,
		Nested:    inst.Nested,
		KSM:       inst.KSM,
	}
	if withISO {
		spec.ISOPath = m.SeedISOPath(inst.UUID)
	}
	return hypervisor.DomainXML(spec)
}

// domainXMLByUUID is domainXML with the owner looked up from the store.
func (m *Manager) domainXMLByUUID(inst types.Instance, withISO bool) (string, error) {
	owner, err := m.store.UserByID(inst.OwnerID)
	if err != nil {
		return "", fmt.Errorf("lifecycle: owner of %s: %w", inst.Name, err)
	}
	return m.domainXML(inst, owner, withISO)
}

// addressView adapts the instance list to the allocator's AddressStore.
type addressView struct {
	store Store
}

// UsedAddresses lists the address of every instance inside the subnet.
func (v addressView) UsedAddresses(_ context.Context, subnet netip.Prefix) ([]netip.Addr, error) {
	insts, err := v.store.Instances()
	if err != nil {
		return nil, err
	}
	var used []netip.Addr
	for _, inst := range insts {
		addr, err := netip.ParseAddr(inst.Address)
		if err != nil {
			continue
		}
		if subnet.Contains(addr) {
			used = append(used, addr)
		}
	}
	return used, nil
}

// randomUUID returns a random version 4 UUID in the canonical text form.
func randomUUID() string {
	var b [16]byte
	if _, err := cryptorand.Read(b[:]); err != nil {
		panic(fmt.Sprintf("lifecycle: crypto/rand failed: %v", err))
	}
	b[6] = (b[6] & 0x0f) | 0x40
	b[8] = (b[8] & 0x3f) | 0x80
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:16])
}
