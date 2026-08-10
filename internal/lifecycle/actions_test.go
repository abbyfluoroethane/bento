package lifecycle

import (
	"context"
	"errors"
	"fmt"
	"os"
	"strings"
	"testing"

	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// setupInstance creates one instance "web" owned by "amber" and returns it.
func setupInstance(t *testing.T, e *env) types.Instance {
	t.Helper()
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "aa11")
	return e.create(t, owner, "web")
}

func TestStopPaths(t *testing.T) {
	tests := []struct {
		name    string
		prepare func(*env, types.Instance)
		want    hypervisor.StopResult
	}{
		{"graceful", func(*env, types.Instance) {}, hypervisor.StopGraceful},
		{"forced after timeout", func(e *env, _ types.Instance) { e.fake.ForceStop = true }, hypervisor.StopForced},
		{"already stopped", func(e *env, inst types.Instance) {
			ctx := context.Background()
			if _, err := e.m.Stop(ctx, inst.UUID); err != nil {
				t.Fatal(err)
			}
		}, hypervisor.StopNoop},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			e := newEnv(t, nil, nil)
			inst := setupInstance(t, e)
			tt.prepare(e, inst)

			got, err := e.m.Stop(context.Background(), inst.UUID)
			if err != nil {
				t.Fatal(err)
			}
			if got != tt.want {
				t.Errorf("stop path = %s, want %s", got, tt.want)
			}
			stored, _ := e.store.Instance(inst.UUID)
			if stored.DesiredState != types.DesiredStopped {
				t.Errorf("desired = %s, want stopped (SPEC 11.1)", stored.DesiredState)
			}
			if stored.State != types.StateStopped {
				t.Errorf("observed = %s, want stopped", stored.State)
			}
		})
	}
}

func TestStopRecordsDesiredBeforeTheWait(t *testing.T) {
	// A crash during the 60 second ACPI wait must still restore the
	// instance as stopped: the desired state is written first.
	e := newEnv(t, nil, nil)
	inst := setupInstance(t, e)
	e.fake.Hook = func(op, _ string) error {
		if op == "stop" {
			return errors.New("libvirt lost")
		}
		return nil
	}

	_, err := e.m.Stop(context.Background(), inst.UUID)
	if err == nil {
		t.Fatal("stop succeeded despite the injected failure")
	}
	stored, _ := e.store.Instance(inst.UUID)
	if stored.DesiredState != types.DesiredStopped {
		t.Errorf("desired = %s, want stopped even when the stop call fails", stored.DesiredState)
	}
	if stored.State != types.StateRunning {
		t.Errorf("observed = %s, want running (libvirt still authoritative)", stored.State)
	}
}

func TestStartAfterStop(t *testing.T) {
	e := newEnv(t, nil, nil)
	inst := setupInstance(t, e)
	ctx := context.Background()
	if _, err := e.m.Stop(ctx, inst.UUID); err != nil {
		t.Fatal(err)
	}

	if err := e.m.Start(ctx, inst.UUID); err != nil {
		t.Fatal(err)
	}
	stored, _ := e.store.Instance(inst.UUID)
	if stored.DesiredState != types.DesiredRunning || stored.State != types.StateRunning {
		t.Errorf("states = %s/%s, want running/running", stored.State, stored.DesiredState)
	}
	if e.fake.Domain("web").State != types.StateRunning {
		t.Error("domain not running")
	}
}

func TestRestart(t *testing.T) {
	e := newEnv(t, nil, nil)
	inst := setupInstance(t, e)
	ctx := context.Background()

	if err := e.m.Restart(ctx, inst.UUID); err != nil {
		t.Fatal(err)
	}
	stored, _ := e.store.Instance(inst.UUID)
	if stored.DesiredState != types.DesiredRunning {
		t.Errorf("desired = %s, want running", stored.DesiredState)
	}
	var rebooted bool
	for _, call := range e.fake.Calls {
		if call == "reboot web" {
			rebooted = true
		}
	}
	if !rebooted {
		t.Errorf("calls = %v, want a reboot (SPEC 11.1: restart is virDomainReboot)", e.fake.Calls)
	}
}

func TestRemove(t *testing.T) {
	e := newEnv(t, nil, nil)
	inst := setupInstance(t, e)
	ctx := context.Background()
	overlay := e.m.OverlayPath(inst.UUID)
	if _, err := os.Stat(overlay); err != nil {
		t.Fatalf("overlay missing before rm: %v", err)
	}

	if err := e.m.Remove(ctx, inst.UUID); err != nil {
		t.Fatal(err)
	}

	// Step 1: domain destroyed and undefined.
	if e.fake.Domain("web") != nil {
		t.Error("domain survived rm")
	}
	// Step 2: overlay file deleted.
	if _, err := os.Stat(overlay); err == nil {
		t.Error("overlay file survived rm")
	}
	// Steps 3+4: row gone (shares cascade with it) and name released.
	if insts, _ := e.store.Instances(); len(insts) != 0 {
		t.Errorf("row survived rm: %v", insts)
	}
	if len(e.store.released) != 1 || e.store.released[0] != "web" {
		t.Errorf("released names = %v, want [web]", e.store.released)
	}
	// The delete call carries the UUID, never the name (SPEC 7.2).
	var deleted bool
	for _, mut := range e.store.mutations {
		if mut == "delete "+inst.UUID {
			deleted = true
		}
	}
	if !deleted {
		t.Errorf("mutations = %v, want delete by uuid", e.store.mutations)
	}
}

func TestRemoveToleratesMissingDomain(t *testing.T) {
	// A row without a domain (reconcile finding) can still be removed.
	e := newEnv(t, nil, nil)
	inst := setupInstance(t, e)
	ctx := context.Background()
	if err := e.fake.Remove(ctx, "web"); err != nil { // domain vanishes out of band
		t.Fatal(err)
	}

	if err := e.m.Remove(ctx, inst.UUID); err != nil {
		t.Fatalf("rm with missing domain: %v", err)
	}
	if insts, _ := e.store.Instances(); len(insts) != 0 {
		t.Error("row survived rm")
	}
}

func TestRemoveIsOnlyEverUserDriven(t *testing.T) {
	// SPEC section 3: Bento never deletes an instance on its own. The
	// passage of time plus every background path (poll, event, restore,
	// reconcile) must not delete anything.
	e := newEnv(t, nil, nil)
	inst := setupInstance(t, e)
	ctx := context.Background()

	if err := e.m.PollOnce(ctx); err != nil {
		t.Fatal(err)
	}
	if err := e.m.HandleEvent(ctx, inst.UUID, types.StateStopped); err != nil {
		t.Fatal(err)
	}
	if err := e.m.Restore(ctx); err != nil {
		t.Fatal(err)
	}
	if _, err := e.m.Reconcile(ctx); err != nil {
		t.Fatal(err)
	}

	if insts, _ := e.store.Instances(); len(insts) != 1 {
		t.Fatal("a background path deleted the instance")
	}
	for _, mut := range e.store.mutations {
		if strings.HasPrefix(mut, "delete ") {
			t.Errorf("a background path deleted a row: %v", e.store.mutations)
		}
	}
	for _, call := range e.fake.Calls {
		if strings.HasPrefix(call, "remove ") {
			t.Errorf("a background path removed a domain: %v", e.fake.Calls)
		}
	}
}

func TestResizeDiskGrowth(t *testing.T) {
	e := newEnv(t, nil, nil)
	inst := setupInstance(t, e) // 2 vcpu, 2048 MiB, 20 GiB

	res, err := e.m.Resize(context.Background(), ResizeRequest{
		UUID: inst.UUID, VCPU: 2, MemoryMiB: 2048, DiskGiB: 30, Nested: false,
	})
	if err != nil {
		t.Fatal(err)
	}
	if res.RestartRequired {
		t.Error("disk-only growth flagged restart required")
	}
	if !res.DiskGrown {
		t.Error("DiskGrown = false")
	}
	want := fmt.Sprintf("%s %d", e.m.OverlayPath(inst.UUID), 30)
	if len(e.resizer.calls) != 1 || e.resizer.calls[0] != want {
		t.Errorf("resizer calls = %v, want [%s]", e.resizer.calls, want)
	}
	stored, _ := e.store.Instance(inst.UUID)
	if stored.DiskGiB != 30 {
		t.Errorf("stored disk = %d, want 30", stored.DiskGiB)
	}
}

func TestResizeShrinkRejected(t *testing.T) {
	e := newEnv(t, nil, nil)
	inst := setupInstance(t, e)

	_, err := e.m.Resize(context.Background(), ResizeRequest{
		UUID: inst.UUID, VCPU: 2, MemoryMiB: 2048, DiskGiB: 10, Nested: false,
	})
	if !errors.Is(err, ErrDiskShrink) {
		t.Fatalf("error = %v, want ErrDiskShrink", err)
	}
	if len(e.resizer.calls) != 0 {
		t.Error("overlay resized despite the rejection")
	}
}

func TestResizeMemoryEditsXMLAndRequiresRestart(t *testing.T) {
	var df *definerFake
	e := newEnv(t, func(f *hypervisor.Fake) hypervisor.Hypervisor {
		df = &definerFake{Fake: f}
		return df
	}, nil)
	inst := setupInstance(t, e)

	res, err := e.m.Resize(context.Background(), ResizeRequest{
		UUID: inst.UUID, VCPU: 4, MemoryMiB: 4096, DiskGiB: 20, Nested: false,
	})
	if err != nil {
		t.Fatal(err)
	}
	if !res.RestartRequired {
		t.Error("vcpu/memory change did not require a restart (SPEC 11.1)")
	}
	if res.DiskGrown {
		t.Error("DiskGrown = true for an unchanged disk")
	}
	if len(df.defines) != 1 {
		t.Fatalf("defines = %d, want 1 (the XML edit)", len(df.defines))
	}
	for _, want := range []string{">4096<", ">4<"} {
		if !strings.Contains(df.defines[0], want) {
			t.Errorf("redefined xml missing %q", want)
		}
	}
	// The seed ISO has not been detached yet, so the redefined XML keeps
	// the CD-ROM.
	if !strings.Contains(df.defines[0], e.m.SeedISOPath(inst.UUID)) {
		t.Error("redefined xml dropped the seed iso before the first boot")
	}
	stored, _ := e.store.Instance(inst.UUID)
	if stored.VCPU != 4 || stored.MemoryMiB != 4096 {
		t.Errorf("stored shape = %d vcpu / %d MiB, want 4 / 4096", stored.VCPU, stored.MemoryMiB)
	}
}

func TestResizeNestedRejectedWhenHostOff(t *testing.T) {
	e := newEnv(t, nil, nil) // nested check reports off
	inst := setupInstance(t, e)

	_, err := e.m.Resize(context.Background(), ResizeRequest{
		UUID: inst.UUID, VCPU: 2, MemoryMiB: 2048, DiskGiB: 20, Nested: true,
	})
	if !errors.Is(err, ErrNestedUnavailable) {
		t.Fatalf("error = %v, want ErrNestedUnavailable", err)
	}
	if !strings.Contains(err.Error(), "kvm_intel.nested=1") {
		t.Errorf("error %q does not give the module parameter (SPEC 5.5)", err)
	}
	stored, _ := e.store.Instance(inst.UUID)
	if stored.Nested {
		t.Error("nested recorded despite the rejection")
	}
}

func TestResizeQuotaFailureChangesNothing(t *testing.T) {
	e := newEnv(t, nil, nil)
	inst := setupInstance(t, e)
	e.store.resizeErr = errors.New("quota exceeded")

	_, err := e.m.Resize(context.Background(), ResizeRequest{
		UUID: inst.UUID, VCPU: 8, MemoryMiB: 8192, DiskGiB: 40, Nested: false,
	})
	if err == nil || !strings.Contains(err.Error(), "quota exceeded") {
		t.Fatalf("error = %v, want the quota error", err)
	}
	if len(e.resizer.calls) != 0 {
		t.Error("overlay resized after the quota check failed")
	}
}
