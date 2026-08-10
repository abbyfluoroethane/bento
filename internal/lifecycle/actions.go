package lifecycle

import (
	"context"
	"errors"
	"fmt"

	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// Start starts a stopped instance and records desired state running
// (SPEC 11.1).
func (m *Manager) Start(ctx context.Context, uuid string) error {
	inst, err := m.store.Instance(uuid)
	if err != nil {
		return err
	}
	if err := m.hyp.Start(ctx, inst.Name); err != nil {
		return fmt.Errorf("lifecycle: start %s: %w", inst.Name, err)
	}
	if err := m.store.SetDesiredState(uuid, types.DesiredRunning); err != nil {
		return err
	}
	return m.store.SetObservedState(uuid, types.StateRunning)
}

// Stop stops an instance: ACPI shutdown request, a 60 second wait, then
// destroy only after the timeout. It reports which path the stop took
// (SPEC 11.1). The desired state is recorded before the wait begins, so a
// crash during the wait still restores the instance as stopped.
func (m *Manager) Stop(ctx context.Context, uuid string) (hypervisor.StopResult, error) {
	inst, err := m.store.Instance(uuid)
	if err != nil {
		return "", err
	}
	if err := m.store.SetDesiredState(uuid, types.DesiredStopped); err != nil {
		return "", err
	}
	result, err := m.hyp.Stop(ctx, inst.Name)
	if err != nil {
		return "", fmt.Errorf("lifecycle: stop %s: %w", inst.Name, err)
	}
	if err := m.store.SetObservedState(uuid, types.StateStopped); err != nil {
		return result, err
	}
	m.log.Info("stop: instance stopped", "instance", inst.Name, "path", string(result))
	return result, nil
}

// Restart reboots a running instance and records desired state running
// (SPEC 11.1).
func (m *Manager) Restart(ctx context.Context, uuid string) error {
	inst, err := m.store.Instance(uuid)
	if err != nil {
		return err
	}
	if err := m.hyp.Reboot(ctx, inst.Name); err != nil {
		return fmt.Errorf("lifecycle: restart %s: %w", inst.Name, err)
	}
	return m.store.SetDesiredState(uuid, types.DesiredRunning)
}

// Remove deletes an instance. Confirmation is the frontend's job; Remove
// does exactly the four ordered steps of SPEC 11.1:
//
//  1. Destroy and undefine the domain.
//  2. Delete the overlay file.
//  3. Delete every share for the UUID.
//  4. Insert the name into released_names.
//
// Steps 3 and 4 run in one store transaction with the row delete. Nothing
// here is time-based: Bento never deletes an instance on its own
// (SPEC section 3); this method runs only on a user command.
func (m *Manager) Remove(ctx context.Context, uuid string) error {
	inst, err := m.store.Instance(uuid)
	if err != nil {
		return err
	}
	// Step 1. A domain that is already gone (reconcile: row without
	// domain) does not block the delete of the row.
	if err := m.hyp.Remove(ctx, inst.Name); err != nil {
		if !errors.Is(err, hypervisor.ErrDomainNotFound) {
			return fmt.Errorf("lifecycle: rm %s: %w", inst.Name, err)
		}
		m.log.Warn("rm: domain already gone, removing the rest", "instance", inst.Name)
	}
	// Step 2. Also delete a seed ISO left by an instance that never
	// finished its first boot; the file holds the owner's public keys.
	if err := removeFile(m.OverlayPath(uuid)); err != nil {
		return fmt.Errorf("lifecycle: rm %s: delete overlay: %w", inst.Name, err)
	}
	if err := m.deleteISO(m.SeedISOPath(uuid)); err != nil {
		m.log.Warn("rm: seed iso not deleted", "instance", inst.Name, "error", err)
	}
	// Steps 3 and 4: the row delete cascades the shares and inserts the
	// released name, in one transaction.
	if _, err := m.store.DeleteInstance(uuid); err != nil {
		return fmt.Errorf("lifecycle: rm %s: %w", inst.Name, err)
	}
	m.log.Info("rm: instance removed", "instance", inst.Name, "uuid", uuid)
	return nil
}

// ResizeRequest is the full target shape of an instance. The CLI resolves
// any unchanged value to the current one before calling Resize.
type ResizeRequest struct {
	UUID      string
	VCPU      int
	MemoryMiB int64
	DiskGiB   int64
	Nested    bool
}

// ResizeResult reports what a resize did and what the user must do next.
type ResizeResult struct {
	// RestartRequired is true when memory, vCPU count, or the nested
	// setting changed. The XML edit takes effect at the next restart;
	// the caller warns the user before the change (SPEC 11.1).
	RestartRequired bool
	// DiskGrown is true when the overlay was resized. The guest sees
	// the new size after a restart (SPEC 11.1).
	DiskGrown bool
}

// Resize changes the shape of an instance (SPEC 11.1). Memory, vCPU, and
// nested changes edit the domain XML and need a restart; disk growth
// resizes the overlay. The store reruns the quota check. Disk shrink is
// rejected.
func (m *Manager) Resize(ctx context.Context, req ResizeRequest) (ResizeResult, error) {
	var res ResizeResult
	inst, err := m.store.Instance(req.UUID)
	if err != nil {
		return res, err
	}
	if req.VCPU <= 0 || req.MemoryMiB <= 0 || req.DiskGiB <= 0 {
		return res, errors.New("lifecycle: resize needs positive vcpu, memory, and disk")
	}
	if req.DiskGiB < inst.DiskGiB {
		return res, fmt.Errorf("%w: %s has %d GiB, requested %d GiB",
			ErrDiskShrink, inst.Name, inst.DiskGiB, req.DiskGiB)
	}
	if req.Nested && !inst.Nested {
		if err := m.checkNested(); err != nil {
			return res, err
		}
	}
	res.RestartRequired = req.VCPU != inst.VCPU ||
		req.MemoryMiB != inst.MemoryMiB ||
		req.Nested != inst.Nested
	res.DiskGrown = req.DiskGiB > inst.DiskGiB

	// The store reruns the four-limit quota check with the instance's
	// own use excluded (SPEC 6.1).
	if err := m.store.Resize(req.UUID, req.VCPU, req.MemoryMiB, req.DiskGiB, req.Nested); err != nil {
		return ResizeResult{}, err
	}

	if res.DiskGrown {
		if err := m.resizer.ResizeOverlay(ctx, m.OverlayPath(req.UUID), req.DiskGiB); err != nil {
			return ResizeResult{}, err
		}
	}

	if res.RestartRequired {
		inst.VCPU = req.VCPU
		inst.MemoryMiB = req.MemoryMiB
		inst.DiskGiB = req.DiskGiB
		inst.Nested = req.Nested
		if err := m.redefine(ctx, inst); err != nil {
			return ResizeResult{}, err
		}
	}
	m.log.Info("resize: instance resized", "instance", inst.Name,
		"vcpu", req.VCPU, "memory_mib", req.MemoryMiB, "disk_gib", req.DiskGiB,
		"nested", req.Nested, "restart_required", res.RestartRequired)
	return res, nil
}

// redefine replaces the persistent domain XML with one built from the
// current stored configuration. The seed ISO stays attached only while it
// still exists on disk (before the first boot). When the hypervisor cannot
// redefine, the change is recorded in the store and a warning says the
// definition will catch up at the next redefine.
func (m *Manager) redefine(ctx context.Context, inst types.Instance) error {
	definer, ok := m.hyp.(Definer)
	if !ok {
		m.log.Warn("redefine: hypervisor cannot redefine XML; the stored configuration applies at the next domain redefine",
			"instance", inst.Name)
		return nil
	}
	withISO := m.isoExists(m.SeedISOPath(inst.UUID))
	xml, err := m.domainXMLByUUID(inst, withISO)
	if err != nil {
		return err
	}
	if err := definer.Define(ctx, xml); err != nil {
		return fmt.Errorf("lifecycle: redefine %s: %w", inst.Name, err)
	}
	return nil
}
