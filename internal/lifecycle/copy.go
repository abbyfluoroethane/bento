package lifecycle

import (
	"context"
	"errors"
	"fmt"
	"net/netip"

	"github.com/abbyfluoroethane/bento/internal/cloudinit"
	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/network"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// ErrCopySourceRunning rejects a cp whose source is not stopped
// (SPEC 15): copying a live overlay produces a torn disk image.
var ErrCopySourceRunning = errors.New("lifecycle: the cp source must be stopped")

// Copy clones a stopped instance into a new one (SPEC 15 cp). The clone
// keeps the exact image version of the source (base_checksum), gets a
// byte-for-byte copy of the source overlay, and receives its own
// identity — UUID, name, address, MAC, and a fresh cloud-init seed whose
// new instance-id makes cloud-init rerun and apply the new hostname and
// network on first boot. The requested shape may grow the disk but never
// shrink it.
func (m *Manager) Copy(ctx context.Context, srcUUID string, req NewRequest) (types.Instance, error) {
	var zero types.Instance
	src, err := m.store.Instance(srcUUID)
	if err != nil {
		return zero, err
	}
	switch {
	case req.Name == "":
		return zero, errors.New("lifecycle: cp needs a target name")
	case req.VCPU <= 0 || req.MemoryMiB <= 0 || req.DiskGiB <= 0:
		return zero, errors.New("lifecycle: cp needs positive vcpu, memory, and disk")
	case req.DiskGiB < src.DiskGiB:
		return zero, fmt.Errorf("%w: %s has %d GiB, requested %d GiB",
			ErrDiskShrink, src.Name, src.DiskGiB, req.DiskGiB)
	}
	if req.Nested {
		if err := m.checkNested(); err != nil {
			return zero, err
		}
	}
	// Ask libvirt, not the state column: a poll can lag a start by up
	// to its interval, and copying a live disk must never happen.
	switch state, err := m.hyp.State(ctx, src.Name); {
	case errors.Is(err, hypervisor.ErrDomainNotFound):
		// Row without domain: the disk cannot be written, safe to copy.
	case err != nil:
		return zero, fmt.Errorf("lifecycle: cp %s: %w", src.Name, err)
	case state != types.StateStopped:
		return zero, fmt.Errorf("%w: %s is %s", ErrCopySourceRunning, src.Name, state)
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
		ImageName:    src.ImageName,
		BaseChecksum: src.BaseChecksum, // the exact version the source booted from
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
	if err := m.store.CreateInstance(inst, m.cooldown); err != nil {
		return zero, err
	}

	// A plain file copy preserves the qcow2 backing-file reference; the
	// backing image lives at a content-addressed path that never moves
	// (SPEC 5.1). The row above already carries base_checksum, so the
	// image GC cannot delete the backing version mid-copy.
	if err := copyFile(m.OverlayPath(src.UUID), m.OverlayPath(uuid)); err != nil {
		return zero, m.unwindNew(inst, err, false, false)
	}
	if req.DiskGiB > src.DiskGiB {
		if err := m.resizer.ResizeOverlay(ctx, m.OverlayPath(uuid), req.DiskGiB); err != nil {
			return zero, m.unwindNew(inst, err, true, false)
		}
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
	if err := m.hyp.Create(ctx, xml); err != nil {
		return zero, m.unwindNew(inst, err, true, true)
	}

	inst.State = types.StateRunning
	if err := m.store.SetObservedState(uuid, types.StateRunning); err != nil {
		m.log.Warn("cp: observed state not recorded; the poller will catch up",
			"instance", req.Name, "error", err)
	}
	m.log.Info("cp: instance copied",
		"source", src.Name, "instance", req.Name, "uuid", uuid, "address", inst.Address)
	return inst, nil
}
