package lifecycle

import (
	"context"
	"fmt"
	"time"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// PollOnce runs one observed-state poll (SPEC 12): list every domain,
// update the state column in one transaction, then finish the first boot of
// any instance that has reached running with its seed ISO still on disk
// (SPEC 5.2).
func (m *Manager) PollOnce(ctx context.Context) error {
	domains, err := m.hyp.List(ctx)
	if err != nil {
		return fmt.Errorf("lifecycle: poll: list domains: %w", err)
	}
	states := make(map[string]types.State, len(domains))
	for _, dom := range domains {
		states[dom.UUID] = dom.State
	}
	if err := m.store.UpdateObservedStates(states); err != nil {
		return fmt.Errorf("lifecycle: poll: record observed states: %w", err)
	}

	insts, err := m.store.Instances()
	if err != nil {
		return fmt.Errorf("lifecycle: poll: list instances: %w", err)
	}
	for _, inst := range insts {
		if states[inst.UUID] != types.StateRunning {
			continue
		}
		if err := m.FinishFirstBoot(ctx, inst); err != nil {
			m.log.Warn("poll: first boot cleanup failed; will retry next poll",
				"instance", inst.Name, "error", err)
		}
	}
	return nil
}

// RunPoller polls every PollInterval (default 30 seconds, SPEC 12) until
// the context ends. A poll misses a short transition; HandleEvent covers
// those from libvirt lifecycle events.
func (m *Manager) RunPoller(ctx context.Context) error {
	ticker := time.NewTicker(m.pollEvery)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-ticker.C:
			if err := m.PollOnce(ctx); err != nil {
				m.log.Warn("poll failed", "error", err)
			}
		}
	}
}

// HandleEvent applies one libvirt lifecycle event: the observed state of
// the domain changed (SPEC 12). The wiring layer subscribes to libvirt
// events and calls this with the domain UUID and the mapped state. An event
// for a domain with no row is ignored; the reconcile report covers it.
func (m *Manager) HandleEvent(ctx context.Context, uuid string, state types.State) error {
	inst, err := m.store.Instance(uuid)
	if err != nil {
		m.log.Warn("event: no row for domain, ignoring", "uuid", uuid, "state", string(state))
		return nil
	}
	if err := m.store.SetObservedState(uuid, state); err != nil {
		return err
	}
	if state == types.StateRunning {
		if err := m.FinishFirstBoot(ctx, inst); err != nil {
			m.log.Warn("event: first boot cleanup failed; will retry next poll",
				"instance", inst.Name, "error", err)
		}
	}
	return nil
}

// FinishFirstBoot detaches and deletes the seed ISO of an instance after
// its first successful boot (SPEC 5.2). The ISO holds the public keys of
// the owner and does not need to stay attached. An instance whose ISO is
// already gone is a no-op.
//
// The detach is a redefine without the CD-ROM: the live domain keeps the
// device until it stops, but the persistent definition no longer references
// the deleted file, so the next start cannot fail on a missing ISO. When
// the hypervisor cannot redefine, the ISO file is still deleted (the keys
// must not stay on disk) and a warning names the leftover device.
func (m *Manager) FinishFirstBoot(ctx context.Context, inst types.Instance) error {
	isoPath := m.SeedISOPath(inst.UUID)
	if !m.isoExists(isoPath) {
		return nil
	}
	if definer, ok := m.hyp.(Definer); ok {
		xml, err := m.domainXMLByUUID(inst, false)
		if err != nil {
			return err
		}
		if err := definer.Define(ctx, xml); err != nil {
			return fmt.Errorf("lifecycle: detach seed iso of %s: %w", inst.Name, err)
		}
	} else {
		m.log.Warn("first boot: hypervisor cannot redefine; CD-ROM stays in the definition until the next redefine",
			"instance", inst.Name)
	}
	if err := m.deleteISO(isoPath); err != nil {
		return fmt.Errorf("lifecycle: delete seed iso of %s: %w", inst.Name, err)
	}
	m.log.Info("first boot complete: seed iso removed", "instance", inst.Name)
	return nil
}
