package lifecycle

import (
	"context"
	"fmt"
	"strings"
	"testing"
	"time"

	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/network"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// seedRestore seeds n instances that look like a host after a reboot:
// domains defined but shut off, rows with desired running and a stale
// observed running (the pre-reboot value).
func seedRestore(t *testing.T, e *env, n int) []types.Instance {
	t.Helper()
	e.addUser(t, 1, "amber", 0)
	insts := make([]types.Instance, 0, n)
	for i := 0; i < n; i++ {
		uuid := fmt.Sprintf("%08d-aaaa-4aaa-8aaa-aaaaaaaaaaaa", i+1)
		name := fmt.Sprintf("vm%02d", i+1)
		inst := types.Instance{
			UUID: uuid, Name: name, OwnerID: 1, HostID: 1,
			ImageName: "debian-13", BaseChecksum: "aa11",
			State:        types.StateRunning, // stale pre-reboot value
			DesiredState: types.DesiredRunning,
			Address:      fmt.Sprintf("10.77.0.%d", i+2),
			MAC:          network.MAC(uuid),
			VCPU:         1, MemoryMiB: 1024, DiskGiB: 10, KSM: true,
			Visibility: types.VisibilityOff,
		}
		if err := e.store.CreateInstance(inst, 0); err != nil {
			t.Fatal(err)
		}
		e.fake.SetDomain(hypervisor.FakeDomain{Name: name, UUID: uuid, State: types.StateStopped})
		insts = append(insts, inst)
	}
	e.store.mutations = nil
	return insts
}

// startBatches groups the "start" calls of the fake by the "list" and
// "state" calls around them: every run of consecutive starts is one batch.
func startBatches(calls []string) [][]string {
	var batches [][]string
	var current []string
	for _, call := range calls {
		if strings.HasPrefix(call, "start ") {
			current = append(current, strings.TrimPrefix(call, "start "))
			continue
		}
		if len(current) > 0 {
			batches = append(batches, current)
			current = nil
		}
	}
	if len(current) > 0 {
		batches = append(batches, current)
	}
	return batches
}

func TestRestoreBatches(t *testing.T) {
	tests := []struct {
		name      string
		instances int
		batchSize int
		want      [][]string
	}{
		{
			name: "six instances default batch of four", instances: 6, batchSize: 0,
			want: [][]string{{"vm01", "vm02", "vm03", "vm04"}, {"vm05", "vm06"}},
		},
		{
			name: "exact multiple", instances: 4, batchSize: 2,
			want: [][]string{{"vm01", "vm02"}, {"vm03", "vm04"}},
		},
		{
			name: "batch larger than the fleet", instances: 2, batchSize: 10,
			want: [][]string{{"vm01", "vm02"}},
		},
		{
			name: "batch of one", instances: 3, batchSize: 1,
			want: [][]string{{"vm01"}, {"vm02"}, {"vm03"}},
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			e := newEnv(t, nil, func(c *Config) { c.BatchSize = tt.batchSize })
			insts := seedRestore(t, e, tt.instances)

			if err := e.m.Restore(context.Background()); err != nil {
				t.Fatal(err)
			}

			got := startBatches(e.fake.Calls)
			if len(got) != len(tt.want) {
				t.Fatalf("batches = %v, want %v", got, tt.want)
			}
			for i := range tt.want {
				if strings.Join(got[i], ",") != strings.Join(tt.want[i], ",") {
					t.Errorf("batch %d = %v, want %v", i+1, got[i], tt.want[i])
				}
			}
			// Every instance ends observed running, and each passed
			// through starting first (SPEC 11.2: a user who connects
			// during the restore sees starting).
			for _, inst := range insts {
				stored, _ := e.store.Instance(inst.UUID)
				if stored.State != types.StateRunning {
					t.Errorf("%s observed = %s, want running", inst.Name, stored.State)
				}
				var sawStarting bool
				for _, mut := range e.store.mutations {
					if mut == fmt.Sprintf("observed %s starting", inst.UUID) {
						sawStarting = true
					}
				}
				if !sawStarting {
					t.Errorf("%s never showed starting", inst.Name)
				}
			}
		})
	}
}

func TestRestoreOnlyStartsDesiredRunningObservedStopped(t *testing.T) {
	e := newEnv(t, nil, nil)
	insts := seedRestore(t, e, 3)
	// vm02 was stopped by its user before the reboot: desired stopped.
	if err := e.store.SetDesiredState(insts[1].UUID, types.DesiredStopped); err != nil {
		t.Fatal(err)
	}
	// vm03's domain survived and already runs.
	e.fake.SetDomain(hypervisor.FakeDomain{Name: "vm03", UUID: insts[2].UUID, State: types.StateRunning})

	if err := e.m.Restore(context.Background()); err != nil {
		t.Fatal(err)
	}

	batches := startBatches(e.fake.Calls)
	if len(batches) != 1 || strings.Join(batches[0], ",") != "vm01" {
		t.Errorf("started %v, want only vm01: stopped stays stopped, running needs nothing", batches)
	}
	stored, _ := e.store.Instance(insts[1].UUID)
	if stored.State != types.StateStopped || stored.DesiredState != types.DesiredStopped {
		t.Errorf("vm02 = %s/%s, want stopped/stopped (an instance a user stopped stays stopped)",
			stored.State, stored.DesiredState)
	}
}

// slowStartFake delays the observed running state: a started domain stays
// "stopped" for pollsUntilRunning State calls, like a real guest that takes
// time to boot. A non-nil slowNames limits the delay to those domains.
type slowStartFake struct {
	*hypervisor.Fake
	pollsUntilRunning int
	slowNames         map[string]bool
	remaining         map[string]int
}

func (s *slowStartFake) Start(ctx context.Context, name string) error {
	if err := s.Fake.Start(ctx, name); err != nil {
		return err
	}
	if s.slowNames != nil && !s.slowNames[name] {
		return nil
	}
	if s.remaining == nil {
		s.remaining = map[string]int{}
	}
	s.remaining[name] = s.pollsUntilRunning
	return nil
}

func (s *slowStartFake) State(ctx context.Context, name string) (types.State, error) {
	state, err := s.Fake.State(ctx, name)
	if err != nil {
		return state, err
	}
	if left, ok := s.remaining[name]; ok && left > 0 {
		s.remaining[name] = left - 1
		return types.StateStopped, nil
	}
	return state, nil
}

func TestRestoreWaitsForEachBatch(t *testing.T) {
	var slow *slowStartFake
	e := newEnv(t, func(f *hypervisor.Fake) hypervisor.Hypervisor {
		slow = &slowStartFake{Fake: f, pollsUntilRunning: 3}
		return slow
	}, func(c *Config) {
		c.BatchSize = 2
		c.StartPollInterval = 500 * time.Millisecond
		c.StartTimeout = time.Minute
	})
	seedRestore(t, e, 4)

	if err := e.m.Restore(context.Background()); err != nil {
		t.Fatal(err)
	}

	// The wait polled: each of the 4 instances needed 3 polls, so the
	// injected sleep ran (3 polls -> at least 2 sleeps each) and never
	// a real one.
	if len(e.sleeps) < 8 {
		t.Errorf("sleeps = %d, want the poll loop to have waited", len(e.sleeps))
	}
	for _, d := range e.sleeps {
		if d != 500*time.Millisecond {
			t.Errorf("sleep = %v, want the configured poll interval", d)
		}
	}
	// The second batch starts only after the first is running: by the
	// time "start vm03" happens, vm01 and vm02 already report running.
	calls := e.fake.Calls
	idx := func(call string) int {
		for i, c := range calls {
			if c == call {
				return i
			}
		}
		return -1
	}
	lastFirstBatchStateCheck := -1
	for i, c := range calls {
		if (c == "state vm01" || c == "state vm02") && i > lastFirstBatchStateCheck {
			lastFirstBatchStateCheck = i
		}
	}
	if start3 := idx("start vm03"); start3 < lastFirstBatchStateCheck {
		// The last state check of batch one must come before batch
		// two starts... it does not: fail with the order.
		t.Errorf("vm03 started at call %d before batch one finished waiting (last batch-one state check at %d): %v",
			start3, lastFirstBatchStateCheck, calls)
	}
}

func TestRestoreTimeoutMovesOn(t *testing.T) {
	// vm01 never reaches running; the restore logs it, records what
	// libvirt reports, and still starts the next batch.
	e := newEnv(t, func(f *hypervisor.Fake) hypervisor.Hypervisor {
		// Only vm01 is slow; vm02 boots at once.
		return &slowStartFake{Fake: f, pollsUntilRunning: 1 << 30, slowNames: map[string]bool{"vm01": true}}
	}, func(c *Config) {
		c.BatchSize = 1
		c.StartPollInterval = time.Second
		c.StartTimeout = 3 * time.Second
	})
	insts := seedRestore(t, e, 2)

	if err := e.m.Restore(context.Background()); err != nil {
		t.Fatal(err)
	}

	batches := startBatches(e.fake.Calls)
	if len(batches) != 2 {
		t.Fatalf("batches = %v, want vm01 then vm02 despite the timeout", batches)
	}
	if !strings.Contains(e.log.String(), "did not reach running") {
		t.Error("timeout not logged")
	}
	stored, _ := e.store.Instance(insts[0].UUID)
	if stored.State == types.StateStarting {
		t.Error("vm01 stuck in starting after the timeout")
	}
}

func TestRestoreClearsAutostart(t *testing.T) {
	var af *autostartFake
	e := newEnv(t, func(f *hypervisor.Fake) hypervisor.Hypervisor {
		af = &autostartFake{Fake: f}
		return af
	}, nil)
	seedRestore(t, e, 2)

	if err := e.m.Restore(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(af.cleared) != 2 {
		t.Errorf("autostart cleared on %v, want every domain (SPEC 11.2)", af.cleared)
	}
}

func TestRestoreLogsProgress(t *testing.T) {
	e := newEnv(t, nil, func(c *Config) { c.BatchSize = 2 })
	seedRestore(t, e, 3)

	if err := e.m.Restore(context.Background()); err != nil {
		t.Fatal(err)
	}
	log := e.log.String()
	for _, want := range []string{"batch starting", "batch done", "restore: complete", "batches=2"} {
		if !strings.Contains(log, want) {
			t.Errorf("log missing %q:\n%s", want, log)
		}
	}
}

func TestRestoreNothingToDo(t *testing.T) {
	e := newEnv(t, nil, nil)
	if err := e.m.Restore(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(e.log.String(), "nothing to start") {
		t.Error("empty restore not logged")
	}
}
