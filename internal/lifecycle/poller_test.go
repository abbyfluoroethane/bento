package lifecycle

import (
	"context"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/types"
)

func TestPollOnceUpdatesObservedState(t *testing.T) {
	e := newEnv(t, nil, nil)
	inst := setupInstance(t, e)

	// The guest shuts itself down: libvirt sees stopped, the row still
	// says running.
	if _, err := e.fake.Stop(context.Background(), "web"); err != nil {
		t.Fatal(err)
	}
	if err := e.m.PollOnce(context.Background()); err != nil {
		t.Fatal(err)
	}

	stored, _ := e.store.Instance(inst.UUID)
	if stored.State != types.StateStopped {
		t.Errorf("observed = %s, want stopped after the poll", stored.State)
	}
	if stored.DesiredState != types.DesiredRunning {
		t.Errorf("desired = %s, the poll must never touch the desired state", stored.DesiredState)
	}
}

func TestPollOnceFinishesFirstBoot(t *testing.T) {
	var df *definerFake
	e := newEnv(t, func(f *hypervisor.Fake) hypervisor.Hypervisor {
		df = &definerFake{Fake: f}
		return df
	}, nil)
	inst := setupInstance(t, e)
	isoPath := e.m.SeedISOPath(inst.UUID)
	if !e.iso.exists(isoPath) {
		t.Fatal("seed iso missing after create")
	}

	// The instance runs: the first poll detaches and deletes the seed.
	if err := e.m.PollOnce(context.Background()); err != nil {
		t.Fatal(err)
	}
	if e.iso.exists(isoPath) {
		t.Error("seed iso survived the first successful boot (SPEC 5.2)")
	}
	if len(df.defines) != 1 {
		t.Fatalf("defines = %d, want 1 (detach via redefine)", len(df.defines))
	}
	if strings.Contains(df.defines[0], isoPath) {
		t.Error("redefined xml still references the seed iso")
	}
	if !strings.Contains(df.defines[0], e.m.OverlayPath(inst.UUID)) {
		t.Error("redefined xml lost the root disk")
	}

	// The second poll is a no-op: no second define, no error.
	if err := e.m.PollOnce(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(df.defines) != 1 {
		t.Errorf("defines = %d after second poll, want still 1", len(df.defines))
	}
}

func TestPollOnceSkipsFirstBootWhileStopped(t *testing.T) {
	e := newEnv(t, nil, nil)
	inst := setupInstance(t, e)
	if _, err := e.m.Stop(context.Background(), inst.UUID); err != nil {
		t.Fatal(err)
	}

	if err := e.m.PollOnce(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !e.iso.exists(e.m.SeedISOPath(inst.UUID)) {
		t.Error("seed iso deleted before the first successful boot")
	}
}

func TestHandleEvent(t *testing.T) {
	e := newEnv(t, nil, nil)
	inst := setupInstance(t, e)

	// A lifecycle event arrives between polls: the guest stopped.
	if err := e.m.HandleEvent(context.Background(), inst.UUID, types.StateStopped); err != nil {
		t.Fatal(err)
	}
	stored, _ := e.store.Instance(inst.UUID)
	if stored.State != types.StateStopped {
		t.Errorf("observed = %s, want stopped after the event", stored.State)
	}

	// An event for an unknown domain is ignored, not an error: the
	// reconcile report covers it.
	if err := e.m.HandleEvent(context.Background(), "unknown-uuid", types.StateRunning); err != nil {
		t.Fatalf("event for unknown domain: %v", err)
	}
}

func TestHandleEventRunningFinishesFirstBoot(t *testing.T) {
	var df *definerFake
	e := newEnv(t, func(f *hypervisor.Fake) hypervisor.Hypervisor {
		df = &definerFake{Fake: f}
		return df
	}, nil)
	inst := setupInstance(t, e)

	if err := e.m.HandleEvent(context.Background(), inst.UUID, types.StateRunning); err != nil {
		t.Fatal(err)
	}
	if e.iso.exists(e.m.SeedISOPath(inst.UUID)) {
		t.Error("seed iso survived the running event")
	}
	if len(df.defines) != 1 {
		t.Errorf("defines = %d, want 1", len(df.defines))
	}
}

func TestFirstBootWithoutDefinerStillDeletesISO(t *testing.T) {
	// The plain hypervisor cannot redefine; the keys must still leave
	// the disk, with a warning about the leftover CD-ROM device.
	e := newEnv(t, nil, nil)
	inst := setupInstance(t, e)

	if err := e.m.PollOnce(context.Background()); err != nil {
		t.Fatal(err)
	}
	if e.iso.exists(e.m.SeedISOPath(inst.UUID)) {
		t.Error("seed iso survived (the owner's keys stayed on disk)")
	}
	if !strings.Contains(e.log.String(), "cannot redefine") {
		t.Error("missing warning about the hypervisor that cannot redefine")
	}
}

// countingFake counts List calls, safely across goroutines.
type countingFake struct {
	*hypervisor.Fake
	mu    sync.Mutex
	lists int
}

func (c *countingFake) List(ctx context.Context) ([]hypervisor.DomainInfo, error) {
	c.mu.Lock()
	c.lists++
	c.mu.Unlock()
	return c.Fake.List(ctx)
}

func (c *countingFake) listCount() int {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.lists
}

func TestRunPollerPollsOnTheInterval(t *testing.T) {
	var counting *countingFake
	e := newEnv(t, func(f *hypervisor.Fake) hypervisor.Hypervisor {
		counting = &countingFake{Fake: f}
		return counting
	}, func(c *Config) { c.PollInterval = 5 * time.Millisecond })
	setupInstance(t, e)

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() { done <- e.m.RunPoller(ctx) }()

	deadline := time.After(2 * time.Second)
	for counting.listCount() < 3 {
		select {
		case <-deadline:
			t.Fatal("poller never polled repeatedly")
		default:
			time.Sleep(5 * time.Millisecond)
		}
	}
	cancel()
	if err := <-done; err != context.Canceled {
		t.Errorf("RunPoller returned %v, want context.Canceled", err)
	}
}
