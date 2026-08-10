package lifecycle

import (
	"context"
	"errors"
	"os"
	"testing"

	"github.com/abbyfluoroethane/bento/internal/types"
)

func copyRequest(owner types.User, name string) NewRequest {
	return NewRequest{
		Name:      name,
		Owner:     owner,
		HostID:    1,
		SSHKeys:   []string{"ssh-ed25519 AAAA test@key"},
		VCPU:      2,
		MemoryMiB: 2048,
		DiskGiB:   20,
	}
}

func TestCopy(t *testing.T) {
	e := newEnv(t, nil, nil)
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "aa11")
	src := e.create(t, owner, "web")
	if _, err := e.m.Stop(context.Background(), src.UUID); err != nil {
		t.Fatal(err)
	}
	// Age the source: the allowlist moved on, the copy must not.
	e.addImage("debian-13", "bb22")

	clone, err := e.m.Copy(context.Background(), src.UUID, copyRequest(owner, "web2"))
	if err != nil {
		t.Fatalf("Copy: %v", err)
	}
	if clone.UUID == src.UUID {
		t.Error("clone shares the source UUID")
	}
	if clone.BaseChecksum != "aa11" {
		t.Errorf("clone base checksum = %s, want the source's aa11, not the current bb22", clone.BaseChecksum)
	}
	if clone.Address == src.Address || clone.MAC == src.MAC {
		t.Errorf("clone address/mac = %s/%s, must differ from source %s/%s",
			clone.Address, clone.MAC, src.Address, src.MAC)
	}
	// The overlay is a file copy, not a fresh overlay from the image
	// store.
	if len(e.images.calls) != 1 {
		t.Errorf("image store calls = %v, want only the source's create", e.images.calls)
	}
	data, err := os.ReadFile(e.m.OverlayPath(clone.UUID))
	if err != nil {
		t.Fatalf("clone overlay: %v", err)
	}
	if string(data) != "overlay" {
		t.Errorf("clone overlay content = %q, want the copied bytes", data)
	}
	// A fresh seed with the clone's identity makes cloud-init rerun.
	seed, ok := e.iso.seeds[e.m.SeedISOPath(clone.UUID)]
	if !ok {
		t.Fatal("no seed built for the clone")
	}
	if seed.InstanceID != clone.UUID || seed.Hostname != "web2" || seed.MAC != clone.MAC {
		t.Errorf("clone seed = %+v, want the clone identity", seed)
	}
	if clone.State != types.StateRunning || clone.DesiredState != types.DesiredRunning {
		t.Errorf("clone states = %s/%s, want running/running", clone.State, clone.DesiredState)
	}
}

func TestCopyRunningSourceRefused(t *testing.T) {
	e := newEnv(t, nil, nil)
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "aa11")
	src := e.create(t, owner, "web") // running

	_, err := e.m.Copy(context.Background(), src.UUID, copyRequest(owner, "web2"))
	if !errors.Is(err, ErrCopySourceRunning) {
		t.Fatalf("Copy of running source = %v, want ErrCopySourceRunning", err)
	}
}

func TestCopyDiskShrinkRefused(t *testing.T) {
	e := newEnv(t, nil, nil)
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "aa11")
	src := e.create(t, owner, "web")
	if _, err := e.m.Stop(context.Background(), src.UUID); err != nil {
		t.Fatal(err)
	}
	req := copyRequest(owner, "web2")
	req.DiskGiB = 10 // source has 20
	if _, err := e.m.Copy(context.Background(), src.UUID, req); !errors.Is(err, ErrDiskShrink) {
		t.Fatalf("Copy with smaller disk = %v, want ErrDiskShrink", err)
	}
}

func TestCopyGrowsDisk(t *testing.T) {
	e := newEnv(t, nil, nil)
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "aa11")
	src := e.create(t, owner, "web")
	if _, err := e.m.Stop(context.Background(), src.UUID); err != nil {
		t.Fatal(err)
	}
	req := copyRequest(owner, "web2")
	req.DiskGiB = 40
	clone, err := e.m.Copy(context.Background(), src.UUID, req)
	if err != nil {
		t.Fatalf("Copy: %v", err)
	}
	want := e.m.OverlayPath(clone.UUID) + " 40"
	if len(e.resizer.calls) != 1 || e.resizer.calls[0] != want {
		t.Errorf("resizer calls = %v, want [%s]", e.resizer.calls, want)
	}
}

func TestCopyUnwindOnCreateFailure(t *testing.T) {
	e := newEnv(t, nil, nil)
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "aa11")
	src := e.create(t, owner, "web")
	if _, err := e.m.Stop(context.Background(), src.UUID); err != nil {
		t.Fatal(err)
	}
	e.fake.Hook = func(op, name string) error {
		if op == "create" && name == "web2" {
			return errors.New("define failed")
		}
		return nil
	}
	_, err := e.m.Copy(context.Background(), src.UUID, copyRequest(owner, "web2"))
	if err == nil {
		t.Fatal("Copy with failing create: want error")
	}
	// The row, the overlay copy, and the seed are all gone.
	insts, _ := e.store.Instances()
	if len(insts) != 1 {
		t.Errorf("instances = %d, want only the source", len(insts))
	}
	entries, err := os.ReadDir(e.m.storageDir)
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range entries {
		if entry.Name() != src.UUID+".qcow2" {
			t.Errorf("leftover file %s after unwind", entry.Name())
		}
	}
}
