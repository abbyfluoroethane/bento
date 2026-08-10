package lifecycle

import (
	"context"
	"errors"
	"fmt"

	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// ErrRenameNeedsStop rejects a rename of a running instance: the libvirt
// domain carries the instance name (SPEC 7), and a defined domain can
// only change its name through an undefine and redefine, which needs the
// domain off.
var ErrRenameNeedsStop = errors.New("lifecycle: stop the instance before renaming it")

// Rename changes the name of an instance. The old name enters the
// release cooldown; no alias and no redirect is created (SPEC 7.2, 7.3).
// The overlay and seed ISO paths derive from the UUID and never move;
// the libvirt domain is undefined under the old name and redefined under
// the new one, so the instance must be stopped.
func (m *Manager) Rename(ctx context.Context, uuid, newName string) error {
	inst, err := m.store.Instance(uuid)
	if err != nil {
		return err
	}
	if newName == inst.Name {
		return nil
	}
	definer, canDefine := m.hyp.(Definer)

	// Ask libvirt, not the possibly stale state column: destroying a
	// running domain here would be a surprise stop.
	oldName := inst.Name
	domainGone := false
	switch state, err := m.hyp.State(ctx, oldName); {
	case errors.Is(err, hypervisor.ErrDomainNotFound):
		domainGone = true // row without domain; reconcile reports it
	case err != nil:
		return fmt.Errorf("lifecycle: rename %s: %w", oldName, err)
	case state != types.StateStopped:
		return fmt.Errorf("%w: %s is %s", ErrRenameNeedsStop, oldName, state)
	}
	if !domainGone && !canDefine {
		return fmt.Errorf("lifecycle: rename %s: hypervisor cannot redefine domains", oldName)
	}

	// The store enforces the unique name and the cooldown of the new
	// name in one transaction (SPEC 7.2).
	if err := m.store.RenameInstance(uuid, newName, m.cooldown); err != nil {
		return err
	}
	if domainGone {
		return nil
	}

	inst.Name = newName
	withISO := m.isoExists(m.SeedISOPath(uuid))
	xml, err := m.domainXMLByUUID(inst, withISO)
	if err != nil {
		return m.unwindRename(inst, oldName, err)
	}
	if err := m.hyp.Remove(ctx, oldName); err != nil && !errors.Is(err, hypervisor.ErrDomainNotFound) {
		return m.unwindRename(inst, oldName, fmt.Errorf("undefine %s: %w", oldName, err))
	}
	if err := definer.Define(ctx, xml); err != nil {
		// The old definition is gone. Try to bring it back under the
		// old name before reverting the row.
		inst.Name = oldName
		if oldXML, xmlErr := m.domainXMLByUUID(inst, withISO); xmlErr == nil {
			if defErr := definer.Define(ctx, oldXML); defErr != nil {
				m.log.Error("rename: domain lost; redefine it by hand",
					"instance", oldName, "uuid", uuid, "error", defErr)
			}
		}
		inst.Name = newName
		return m.unwindRename(inst, oldName, fmt.Errorf("define %s: %w", newName, err))
	}
	m.log.Info("rename: instance renamed", "from", oldName, "to", newName, "uuid", uuid)
	return nil
}

// unwindRename reverts the row to the old name after a failed domain
// redefine. The owner retakes a name from their own cooldown at once
// (SPEC 7.2 rule 1), so the revert cannot be blocked.
func (m *Manager) unwindRename(inst types.Instance, oldName string, cause error) error {
	if err := m.store.RenameInstance(inst.UUID, oldName, m.cooldown); err != nil {
		m.log.Error("rename: revert to the old name failed",
			"instance", inst.Name, "old", oldName, "error", err)
	}
	return fmt.Errorf("lifecycle: rename %s to %s: %w", oldName, inst.Name, cause)
}
