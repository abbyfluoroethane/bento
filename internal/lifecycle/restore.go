package lifecycle

import (
	"context"
	"fmt"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// Restore brings the host back to the last recorded desired state after a
// reboot (SPEC 11.2):
//
//  1. Read the observed state of every domain from libvirt.
//  2. Select every instance with desired running and observed stopped.
//  3. Start those instances in batches (operator setting, default 4).
//  4. Wait for each batch to reach running before the next batch.
//
// An instance a user left running comes back running; one a user stopped
// stays stopped. Progress goes to the logger; a user who connects during
// the restore sees "starting", not an error.
func (m *Manager) Restore(ctx context.Context) error {
	domains, err := m.hyp.List(ctx)
	if err != nil {
		return fmt.Errorf("lifecycle: restore: list domains: %w", err)
	}
	// One component decides who starts, and it is this one: clear the
	// libvirt autostart flag again on every domain when the hypervisor
	// supports it. Create already clears it at define time.
	if clearer, ok := m.hyp.(AutostartClearer); ok {
		for _, dom := range domains {
			if err := clearer.ClearAutostart(ctx, dom.Name); err != nil {
				m.log.Warn("restore: autostart not cleared", "domain", dom.Name, "error", err)
			}
		}
	}
	states := make(map[string]types.State, len(domains))
	for _, dom := range domains {
		states[dom.UUID] = dom.State
	}
	if err := m.store.UpdateObservedStates(states); err != nil {
		return fmt.Errorf("lifecycle: restore: record observed states: %w", err)
	}

	insts, err := m.store.InstancesToRestore()
	if err != nil {
		return fmt.Errorf("lifecycle: restore: %w", err)
	}
	if len(insts) == 0 {
		m.log.Info("restore: nothing to start")
		return nil
	}
	batches := (len(insts) + m.batchSize - 1) / m.batchSize
	m.log.Info("restore: starting instances", "instances", len(insts), "batches", batches, "batch_size", m.batchSize)

	for i := 0; i < len(insts); i += m.batchSize {
		batch := insts[i:min(i+m.batchSize, len(insts))]
		m.log.Info("restore: batch starting", "batch", i/m.batchSize+1, "of", batches, "instances", len(batch))

		started := make([]types.Instance, 0, len(batch))
		for _, inst := range batch {
			if err := m.store.SetObservedState(inst.UUID, types.StateStarting); err != nil {
				m.log.Warn("restore: starting state not recorded", "instance", inst.Name, "error", err)
			}
			if err := m.hyp.Start(ctx, inst.Name); err != nil {
				m.log.Warn("restore: instance did not start", "instance", inst.Name, "error", err)
				m.syncObserved(ctx, inst)
				continue
			}
			started = append(started, inst)
		}
		for _, inst := range started {
			if err := m.waitRunning(ctx, inst.Name); err != nil {
				m.log.Warn("restore: instance did not reach running", "instance", inst.Name, "error", err)
				m.syncObserved(ctx, inst)
				continue
			}
			if err := m.store.SetObservedState(inst.UUID, types.StateRunning); err != nil {
				m.log.Warn("restore: running state not recorded", "instance", inst.Name, "error", err)
			}
			m.log.Info("restore: instance running", "instance", inst.Name)
		}
		m.log.Info("restore: batch done", "batch", i/m.batchSize+1, "of", batches)
	}
	m.log.Info("restore: complete")
	return nil
}

// waitRunning polls the observed state of one domain until it is running,
// up to the configured start timeout.
func (m *Manager) waitRunning(ctx context.Context, name string) error {
	attempts := int(m.startWait / m.startPoll)
	if attempts < 1 {
		attempts = 1
	}
	var last types.State
	for i := 0; i < attempts; i++ {
		if err := ctx.Err(); err != nil {
			return err
		}
		state, err := m.hyp.State(ctx, name)
		if err != nil {
			return err
		}
		last = state
		if state == types.StateRunning {
			return nil
		}
		if i < attempts-1 {
			m.sleep(m.startPoll)
		}
	}
	return fmt.Errorf("lifecycle: %s not running after %s (last state %s)", name, m.startWait, last)
}

// syncObserved records what libvirt reports for one instance, so a failed
// start never leaves "starting" stuck in the store.
func (m *Manager) syncObserved(ctx context.Context, inst types.Instance) {
	state, err := m.hyp.State(ctx, inst.Name)
	if err != nil {
		state = types.StateStopped
	}
	if err := m.store.SetObservedState(inst.UUID, state); err != nil {
		m.log.Warn("observed state not recorded", "instance", inst.Name, "error", err)
	}
}
