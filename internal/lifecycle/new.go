package lifecycle

import (
	"context"
	"errors"
	"fmt"
	"net/netip"

	"github.com/abbyfluoroethane/bento/internal/cloudinit"
	"github.com/abbyfluoroethane/bento/internal/network"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// GuestUser is the one account cloud-init creates in every instance
// (SPEC 5.2). The name is the same everywhere so the SSH frontend can
// authenticate to any guest (SPEC 10 step 9); it must stay equal to
// sshfront.DefaultGuestUser.
const GuestUser = "bento"

// NewRequest carries everything the `new` command resolved: the owner row,
// the owner's public keys, and the requested shape of the instance.
type NewRequest struct {
	Name  string
	Owner types.User
	// HostID is the one host in version 1 (SPEC 12, 17).
	HostID int64
	// SSHKeys are the owner's public keys, installed by cloud-init
	// (SPEC 5.2).
	SSHKeys   []string
	ImageName string
	VCPU      int
	MemoryMiB int64
	DiskGiB   int64
	// Nested requests nested virtualization (SPEC 5.5). Rejected when
	// the host has nesting off.
	Nested bool
	// DisableKSM opts the instance out of same-page merging with
	// <nosharepages/> (SPEC 5.4). The zero value keeps KSM on, the
	// default.
	DisableKSM bool
	// HTTPPort is the default HTTP port the proxy targets (SPEC 9.1).
	// Zero means not set yet.
	HTTPPort int
}

// New creates an instance: quota-checked row insert, address and MAC
// assignment, overlay creation, cloud-init seed ISO, and domain
// define+start, in that order (SPEC 5.2, 6.1, 11.1). The desired state is
// running. A failure after the row insert unwinds every partial step, so a
// retried `new` starts clean.
func (m *Manager) New(ctx context.Context, req NewRequest) (types.Instance, error) {
	var zero types.Instance
	switch {
	case req.Name == "":
		return zero, errors.New("lifecycle: new needs a name")
	case req.ImageName == "":
		return zero, errors.New("lifecycle: new needs an image")
	case req.VCPU <= 0 || req.MemoryMiB <= 0 || req.DiskGiB <= 0:
		return zero, errors.New("lifecycle: new needs positive vcpu, memory, and disk")
	}
	if req.Nested {
		if err := m.checkNested(); err != nil {
			return zero, err
		}
	}

	img, err := m.store.Image(req.ImageName)
	if err != nil {
		return zero, fmt.Errorf("lifecycle: image %q: %w", req.ImageName, err)
	}
	if img.CurrentChecksum == "" {
		return zero, fmt.Errorf("%w: %s", ErrNoImageVersion, req.ImageName)
	}

	subnet, err := netip.ParsePrefix(req.Owner.Subnet)
	if err != nil {
		return zero, fmt.Errorf("lifecycle: user %s has a bad subnet %q: %w", req.Owner.Name, req.Owner.Subnet, err)
	}
	addr, err := network.AllocateAddress(ctx, addressView{m.store}, subnet)
	if err != nil {
		return zero, err
	}

	uuid := m.newUUID()
	inst := types.Instance{
		UUID:         uuid,
		Name:         req.Name,
		OwnerID:      req.Owner.ID,
		HostID:       req.HostID,
		ImageName:    req.ImageName,
		BaseChecksum: img.CurrentChecksum,
		State:        types.StateStopped,
		DesiredState: types.DesiredRunning,
		Address:      addr.String(),
		MAC:          network.MAC(uuid),
		VCPU:         req.VCPU,
		MemoryMiB:    req.MemoryMiB,
		DiskGiB:      req.DiskGiB,
		Nested:       req.Nested,
		KSM:          !req.DisableKSM,
		HTTPPort:     req.HTTPPort,
		Visibility:   types.VisibilityOff,
		CreatedAt:    m.now().UTC(),
	}

	// The store runs the name cooldown check, the four-limit quota
	// check, and the insert in one transaction (SPEC 6.1, 7.2).
	if err := m.store.CreateInstance(inst, m.cooldown); err != nil {
		return zero, err
	}

	overlayPath := m.OverlayPath(uuid)
	if err := m.images.CreateOverlay(ctx, img.CurrentChecksum, overlayPath, req.DiskGiB); err != nil {
		return zero, m.unwindNew(inst, err, false, false)
	}

	guest, err := network.NewGuestNetwork(subnet, addr, m.dns)
	if err != nil {
		return zero, m.unwindNew(inst, err, true, false)
	}
	seed := cloudinit.Seed{
		InstanceID:     uuid,
		Hostname:       req.Name,
		UserName:       GuestUser,
		AuthorizedKeys: req.SSHKeys,
		MAC:            inst.MAC,
		AddressCIDR:    guest.Address.String(),
		Gateway:        guest.Gateway.String(),
		DNS:            guest.DNS[0].String(),
	}
	if err := m.iso.Build(ctx, seed, m.SeedISOPath(uuid)); err != nil {
		return zero, m.unwindNew(inst, err, true, false)
	}

	xml, err := m.domainXML(inst, req.Owner, true)
	if err != nil {
		return zero, m.unwindNew(inst, err, true, true)
	}
	// Create defines the domain, clears the libvirt autostart flag
	// (SPEC 11.2: the control plane restores, not libvirt), and starts it.
	if err := m.hyp.Create(ctx, xml); err != nil {
		return zero, m.unwindNew(inst, err, true, true)
	}

	inst.State = types.StateRunning
	if err := m.store.SetObservedState(uuid, types.StateRunning); err != nil {
		m.log.Warn("new: observed state not recorded; the poller will catch up",
			"instance", req.Name, "error", err)
	}
	m.log.Info("new: instance created",
		"instance", req.Name, "uuid", uuid, "address", inst.Address, "image", req.ImageName)
	return inst, nil
}

// unwindNew rolls back the partial work of a failed New: the seed ISO, the
// overlay file, and the row, in reverse creation order. The released name
// re-enters the cooldown, and the owner may retake it at once (SPEC 7.2
// rule 1), so a retried `new` with the same name works.
func (m *Manager) unwindNew(inst types.Instance, cause error, overlay, iso bool) error {
	errs := []error{cause}
	if iso {
		if err := m.deleteISO(m.SeedISOPath(inst.UUID)); err != nil {
			errs = append(errs, fmt.Errorf("unwind seed iso: %w", err))
		}
	}
	if overlay {
		if err := removeFile(m.OverlayPath(inst.UUID)); err != nil {
			errs = append(errs, fmt.Errorf("unwind overlay: %w", err))
		}
	}
	if _, err := m.store.DeleteInstance(inst.UUID); err != nil {
		errs = append(errs, fmt.Errorf("unwind instance row: %w", err))
	}
	m.log.Warn("new: failed, partial work unwound", "instance", inst.Name, "error", cause)
	return fmt.Errorf("lifecycle: new %s: %w", inst.Name, errors.Join(errs...))
}
