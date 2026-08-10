package lifecycle

import (
	"context"
	"errors"
	"fmt"
	"os"
	"strings"
	"testing"

	"github.com/abbyfluoroethane/bento/internal/network"
	"github.com/abbyfluoroethane/bento/internal/types"
)

func TestNew(t *testing.T) {
	e := newEnv(t, nil, nil)
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "aa11")

	inst := e.create(t, owner, "web")

	if inst.DesiredState != types.DesiredRunning {
		t.Errorf("desired state = %s, want running", inst.DesiredState)
	}
	if inst.State != types.StateRunning {
		t.Errorf("observed state = %s, want running", inst.State)
	}
	if inst.Address != "10.77.0.2" {
		t.Errorf("address = %s, want 10.77.0.2 (first free after the .1 gateway)", inst.Address)
	}
	if want := network.MAC(inst.UUID); inst.MAC != want {
		t.Errorf("mac = %s, want %s", inst.MAC, want)
	}
	if inst.BaseChecksum != "aa11" {
		t.Errorf("base checksum = %s, want aa11 (recorded at create, SPEC 5.2)", inst.BaseChecksum)
	}
	if !inst.KSM {
		t.Error("ksm = false, want true by default (SPEC 5.4)")
	}
	if inst.Visibility != types.VisibilityOff {
		t.Errorf("visibility = %s, want off", inst.Visibility)
	}

	// The overlay was created from the current image version at the
	// UUID-derived path with the requested size.
	wantOverlay := fmt.Sprintf("aa11 %s 20", e.m.OverlayPath(inst.UUID))
	if len(e.images.calls) != 1 || e.images.calls[0] != wantOverlay {
		t.Errorf("overlay calls = %v, want [%s]", e.images.calls, wantOverlay)
	}

	// The seed carries the Bento-assigned network identity (SPEC 5.2, 6.2).
	seed, ok := e.iso.seeds[e.m.SeedISOPath(inst.UUID)]
	if !ok {
		t.Fatalf("no seed built at %s", e.m.SeedISOPath(inst.UUID))
	}
	// One fixed account name in every guest so the SSH frontend can
	// authenticate (SPEC 10 step 9).
	if seed.Hostname != "web" || seed.UserName != GuestUser {
		t.Errorf("seed hostname/user = %s/%s, want web/%s", seed.Hostname, seed.UserName, GuestUser)
	}
	if seed.AddressCIDR != "10.77.0.2/24" || seed.Gateway != "10.77.0.1" {
		t.Errorf("seed address/gateway = %s/%s, want 10.77.0.2/24 and 10.77.0.1", seed.AddressCIDR, seed.Gateway)
	}
	if seed.MAC != inst.MAC {
		t.Errorf("seed mac = %s, want %s", seed.MAC, inst.MAC)
	}
	if seed.InstanceID != inst.UUID {
		t.Errorf("seed instance-id = %s, want the uuid %s", seed.InstanceID, inst.UUID)
	}

	// The domain exists, runs, and its XML names the owner's network,
	// the overlay, and the seed ISO.
	dom := e.fake.Domain("web")
	if dom == nil {
		t.Fatal("domain web not created")
	}
	if dom.State != types.StateRunning {
		t.Errorf("domain state = %s, want running", dom.State)
	}
	for _, want := range []string{"bento-user-0", e.m.OverlayPath(inst.UUID), e.m.SeedISOPath(inst.UUID), inst.MAC} {
		if !strings.Contains(dom.XML, want) {
			t.Errorf("domain xml missing %q", want)
		}
	}

	// The row exists with the same values.
	stored, err := e.store.Instance(inst.UUID)
	if err != nil {
		t.Fatal(err)
	}
	if stored.State != types.StateRunning || stored.DesiredState != types.DesiredRunning {
		t.Errorf("stored states = %s/%s, want running/running", stored.State, stored.DesiredState)
	}
}

func TestNewAddressesAdvance(t *testing.T) {
	e := newEnv(t, nil, nil)
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "aa11")

	first := e.create(t, owner, "web")
	second := e.create(t, owner, "db")
	if first.Address != "10.77.0.2" || second.Address != "10.77.0.3" {
		t.Errorf("addresses = %s, %s; want 10.77.0.2 then 10.77.0.3", first.Address, second.Address)
	}
	if first.MAC == second.MAC {
		t.Error("two instances share a MAC")
	}
}

func TestNewValidation(t *testing.T) {
	tests := []struct {
		name    string
		mutate  func(*NewRequest)
		wantErr string
	}{
		{"empty name", func(r *NewRequest) { r.Name = "" }, "needs a name"},
		{"empty image", func(r *NewRequest) { r.ImageName = "" }, "needs an image"},
		{"zero vcpu", func(r *NewRequest) { r.VCPU = 0 }, "positive"},
		{"zero memory", func(r *NewRequest) { r.MemoryMiB = 0 }, "positive"},
		{"zero disk", func(r *NewRequest) { r.DiskGiB = 0 }, "positive"},
		{"unknown image", func(r *NewRequest) { r.ImageName = "arch" }, "arch"},
		{"bad subnet", func(r *NewRequest) { r.Owner.Subnet = "banana" }, "bad subnet"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			e := newEnv(t, nil, nil)
			owner := e.addUser(t, 1, "amber", 0)
			e.addImage("debian-13", "aa11")
			req := NewRequest{
				Name: "web", Owner: owner, HostID: 1,
				ImageName: "debian-13", VCPU: 2, MemoryMiB: 2048, DiskGiB: 20,
			}
			tt.mutate(&req)
			_, err := e.m.New(context.Background(), req)
			if err == nil || !strings.Contains(err.Error(), tt.wantErr) {
				t.Fatalf("New error = %v, want contains %q", err, tt.wantErr)
			}
			if len(e.store.mutations) != 0 {
				t.Errorf("store mutated on invalid request: %v", e.store.mutations)
			}
			if len(e.images.calls) != 0 || len(e.iso.seeds) != 0 {
				t.Error("overlay or iso work ran on invalid request")
			}
		})
	}
}

func TestNewNestedRejectedWhenHostOff(t *testing.T) {
	// SPEC 5.5: reject a new with nested=true when the host has nesting
	// off, and give the module parameter in the error.
	e := newEnv(t, nil, nil) // newEnv's nested check reports off
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "aa11")

	_, err := e.m.New(context.Background(), NewRequest{
		Name: "lab", Owner: owner, HostID: 1,
		ImageName: "debian-13", VCPU: 2, MemoryMiB: 2048, DiskGiB: 20,
		Nested: true,
	})
	if !errors.Is(err, ErrNestedUnavailable) {
		t.Fatalf("error = %v, want ErrNestedUnavailable", err)
	}
	for _, param := range []string{"kvm_intel.nested=1", "kvm_amd.nested=1"} {
		if !strings.Contains(err.Error(), param) {
			t.Errorf("error %q does not name %s", err, param)
		}
	}
	if len(e.store.mutations) != 0 {
		t.Errorf("store mutated: %v", e.store.mutations)
	}
}

func TestNewNestedAllowedWhenHostOn(t *testing.T) {
	e := newEnv(t, nil, func(c *Config) {
		c.NestedEnabled = func() (bool, string) { return true, "" }
	})
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "aa11")

	inst, err := e.m.New(context.Background(), NewRequest{
		Name: "lab", Owner: owner, HostID: 1,
		ImageName: "debian-13", VCPU: 2, MemoryMiB: 2048, DiskGiB: 20,
		Nested: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	if !inst.Nested {
		t.Error("instance not marked nested")
	}
	if !strings.Contains(e.fake.Domain("lab").XML, "host-passthrough") {
		t.Error("domain xml lacks host-passthrough for a nested instance (SPEC 5.5)")
	}
}

func TestNewNoImageVersion(t *testing.T) {
	e := newEnv(t, nil, nil)
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "") // allowlisted, never fetched

	_, err := e.m.New(context.Background(), NewRequest{
		Name: "web", Owner: owner, HostID: 1,
		ImageName: "debian-13", VCPU: 2, MemoryMiB: 2048, DiskGiB: 20,
	})
	if !errors.Is(err, ErrNoImageVersion) {
		t.Fatalf("error = %v, want ErrNoImageVersion", err)
	}
}

func TestNewQuotaErrorStopsEverything(t *testing.T) {
	e := newEnv(t, nil, nil)
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "aa11")
	e.store.createErr = errors.New("quota exceeded")

	_, err := e.m.New(context.Background(), NewRequest{
		Name: "web", Owner: owner, HostID: 1,
		ImageName: "debian-13", VCPU: 2, MemoryMiB: 2048, DiskGiB: 20,
	})
	if err == nil || !strings.Contains(err.Error(), "quota exceeded") {
		t.Fatalf("error = %v, want the store's quota error", err)
	}
	if len(e.images.calls) != 0 || len(e.iso.seeds) != 0 || len(e.fake.Calls) != 0 {
		t.Error("work ran after the quota check failed")
	}
}

func TestNewUnwind(t *testing.T) {
	tests := []struct {
		name     string
		sabotage func(*env)
	}{
		{"overlay fails", func(e *env) { e.images.err = errors.New("qemu-img exploded") }},
		{"iso fails", func(e *env) { e.iso.err = errors.New("xorriso exploded") }},
		{"domain fails", func(e *env) {
			e.fake.Hook = func(op, _ string) error {
				if op == "create" {
					return errors.New("libvirt exploded")
				}
				return nil
			}
		}},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			e := newEnv(t, nil, nil)
			owner := e.addUser(t, 1, "amber", 0)
			e.addImage("debian-13", "aa11")
			tt.sabotage(e)

			_, err := e.m.New(context.Background(), NewRequest{
				Name: "web", Owner: owner, HostID: 1,
				ImageName: "debian-13", VCPU: 2, MemoryMiB: 2048, DiskGiB: 20,
			})
			if err == nil || !strings.Contains(err.Error(), "exploded") {
				t.Fatalf("error = %v, want the injected failure", err)
			}

			// The row is gone and the name released, so a retried
			// `new` starts clean (owner retakes at once, SPEC 7.2).
			if insts, _ := e.store.Instances(); len(insts) != 0 {
				t.Errorf("instance row survived the unwind: %v", insts)
			}
			if len(e.store.released) != 1 || e.store.released[0] != "web" {
				t.Errorf("released names = %v, want [web]", e.store.released)
			}
			// No overlay file, no seed, no domain left behind.
			uuid := "00000001-1111-4111-8111-111111111111"
			if _, err := os.Stat(e.m.OverlayPath(uuid)); err == nil {
				t.Error("overlay file survived the unwind")
			}
			if len(e.iso.seeds) != 0 {
				t.Error("seed iso survived the unwind")
			}
			if e.fake.Domain("web") != nil {
				t.Error("domain survived the unwind")
			}

			// A retry with the same name succeeds.
			e.images.err = nil
			e.iso.err = nil
			e.fake.Hook = nil
			if _, err := e.m.New(context.Background(), NewRequest{
				Name: "web", Owner: owner, HostID: 1,
				ImageName: "debian-13", VCPU: 2, MemoryMiB: 2048, DiskGiB: 20,
			}); err != nil {
				t.Fatalf("retry after unwind: %v", err)
			}
		})
	}
}
