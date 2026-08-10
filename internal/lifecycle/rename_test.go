package lifecycle

import (
	"context"
	"encoding/xml"
	"errors"
	"strings"
	"testing"

	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// defineHyp extends the fake with a Define that inserts or replaces the
// stopped domain parsed from the XML, like virDomainDefineXML.
type defineHyp struct {
	*hypervisor.Fake
	defines   []string
	defineErr error
}

func (d *defineHyp) Define(_ context.Context, domXML string) error {
	d.defines = append(d.defines, domXML)
	if d.defineErr != nil {
		return d.defineErr
	}
	var parsed struct {
		Name string `xml:"name"`
		UUID string `xml:"uuid"`
	}
	if err := xml.Unmarshal([]byte(domXML), &parsed); err != nil {
		return err
	}
	d.SetDomain(hypervisor.FakeDomain{
		Name: parsed.Name, UUID: parsed.UUID, XML: domXML, State: types.StateStopped,
	})
	return nil
}

func renameEnv(t *testing.T) (*env, *defineHyp, types.Instance) {
	t.Helper()
	var dh *defineHyp
	e := newEnv(t, func(f *hypervisor.Fake) hypervisor.Hypervisor {
		dh = &defineHyp{Fake: f}
		return dh
	}, nil)
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "aa11")
	inst := e.create(t, owner, "web")
	if _, err := e.m.Stop(context.Background(), inst.UUID); err != nil {
		t.Fatal(err)
	}
	return e, dh, inst
}

func TestRenameStoppedInstance(t *testing.T) {
	e, dh, inst := renameEnv(t)

	if err := e.m.Rename(context.Background(), inst.UUID, "api"); err != nil {
		t.Fatalf("Rename: %v", err)
	}
	got, err := e.store.Instance(inst.UUID)
	if err != nil {
		t.Fatal(err)
	}
	if got.Name != "api" {
		t.Errorf("row name = %s, want api", got.Name)
	}
	// The old name entered the cooldown (SPEC 7.2).
	if len(e.store.released) == 0 || e.store.released[len(e.store.released)-1] != "web" {
		t.Errorf("released names = %v, want web released", e.store.released)
	}
	// The domain moved to the new name.
	if e.fake.Domain("web") != nil {
		t.Error("domain web still defined after rename")
	}
	dom := e.fake.Domain("api")
	if dom == nil {
		t.Fatal("domain api not defined after rename")
	}
	if dom.UUID != inst.UUID {
		t.Errorf("domain uuid = %s, want %s (rename keeps the UUID)", dom.UUID, inst.UUID)
	}
	// The redefined XML keeps the UUID-derived disk path: a rename
	// never moves files.
	if !strings.Contains(dom.XML, inst.UUID+".qcow2") {
		t.Errorf("domain XML lost the UUID-derived disk path:\n%s", dom.XML)
	}
	if len(dh.defines) != 1 {
		t.Errorf("defines = %d, want 1", len(dh.defines))
	}
}

func TestRenameRunningInstanceRefused(t *testing.T) {
	var dh *defineHyp
	e := newEnv(t, func(f *hypervisor.Fake) hypervisor.Hypervisor {
		dh = &defineHyp{Fake: f}
		return dh
	}, nil)
	_ = dh
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "aa11")
	inst := e.create(t, owner, "web") // running after New

	err := e.m.Rename(context.Background(), inst.UUID, "api")
	if !errors.Is(err, ErrRenameNeedsStop) {
		t.Fatalf("Rename running = %v, want ErrRenameNeedsStop", err)
	}
	got, _ := e.store.Instance(inst.UUID)
	if got.Name != "web" {
		t.Errorf("row name = %s, want web unchanged", got.Name)
	}
}

func TestRenameSameNameIsNoop(t *testing.T) {
	e, dh, inst := renameEnv(t)
	if err := e.m.Rename(context.Background(), inst.UUID, "web"); err != nil {
		t.Fatalf("Rename to same name: %v", err)
	}
	if len(dh.defines) != 0 {
		t.Errorf("defines = %d, want 0 for a same-name rename", len(dh.defines))
	}
}

func TestRenameRowWithoutDomain(t *testing.T) {
	e, dh, inst := renameEnv(t)
	// Simulate a domain lost out of band (reconcile territory).
	if err := e.fake.Remove(context.Background(), "web"); err != nil {
		t.Fatal(err)
	}
	if err := e.m.Rename(context.Background(), inst.UUID, "api"); err != nil {
		t.Fatalf("Rename without domain: %v", err)
	}
	got, _ := e.store.Instance(inst.UUID)
	if got.Name != "api" {
		t.Errorf("row name = %s, want api", got.Name)
	}
	if len(dh.defines) != 0 {
		t.Errorf("defines = %d, want 0 when no domain exists", len(dh.defines))
	}
}

func TestRenameWithoutDefinerRefused(t *testing.T) {
	e := newEnv(t, nil, nil) // plain fake: no Define capability
	owner := e.addUser(t, 1, "amber", 0)
	e.addImage("debian-13", "aa11")
	inst := e.create(t, owner, "web")
	if _, err := e.m.Stop(context.Background(), inst.UUID); err != nil {
		t.Fatal(err)
	}
	if err := e.m.Rename(context.Background(), inst.UUID, "api"); err == nil {
		t.Fatal("Rename without Definer: want error")
	}
	got, _ := e.store.Instance(inst.UUID)
	if got.Name != "web" {
		t.Errorf("row name = %s, want web unchanged", got.Name)
	}
}

func TestRenameDefineFailureRevertsRow(t *testing.T) {
	e, dh, inst := renameEnv(t)
	dh.defineErr = errors.New("libvirt exploded")

	err := e.m.Rename(context.Background(), inst.UUID, "api")
	if err == nil {
		t.Fatal("Rename with failing define: want error")
	}
	got, _ := e.store.Instance(inst.UUID)
	if got.Name != "web" {
		t.Errorf("row name = %s, want web reverted", got.Name)
	}
}

func TestRenameStoreFailureLeavesDomain(t *testing.T) {
	e, dh, inst := renameEnv(t)
	e.store.renameErr = errors.New("name taken")

	if err := e.m.Rename(context.Background(), inst.UUID, "api"); err == nil {
		t.Fatal("Rename with failing store: want error")
	}
	if e.fake.Domain("web") == nil {
		t.Error("domain web gone although the store refused the rename")
	}
	if len(dh.defines) != 0 {
		t.Errorf("defines = %d, want 0", len(dh.defines))
	}
}
