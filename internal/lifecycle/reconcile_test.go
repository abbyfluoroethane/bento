package lifecycle

import (
	"context"
	"testing"

	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/types"
)

func TestReconcile(t *testing.T) {
	e := newEnv(t, nil, nil)
	inst := setupInstance(t, e) // web: domain and row agree

	// A domain with no row: someone defined it by hand.
	e.fake.SetDomain(hypervisor.FakeDomain{
		Name: "rogue", UUID: "deadbeef-0000-4000-8000-000000000000", State: types.StateRunning,
	})
	// A row with no domain: the domain vanished after a crash.
	orphan := types.Instance{
		UUID: "0badc0de-0000-4000-8000-000000000000", Name: "ghost", OwnerID: 1, HostID: 1,
		ImageName: "debian-13", State: types.StateStopped, DesiredState: types.DesiredStopped,
		Address: "10.77.0.9", VCPU: 1, MemoryMiB: 512, DiskGiB: 5,
	}
	if err := e.store.CreateInstance(orphan, 0); err != nil {
		t.Fatal(err)
	}
	e.store.mutations = nil
	e.fake.Calls = nil

	report, err := e.m.Reconcile(context.Background())
	if err != nil {
		t.Fatal(err)
	}

	if report.Empty() {
		t.Fatal("report empty, want two findings")
	}
	if len(report.DomainsWithoutRows) != 1 || report.DomainsWithoutRows[0].Name != "rogue" {
		t.Errorf("domains without rows = %v, want [rogue]", report.DomainsWithoutRows)
	}
	if len(report.RowsWithoutDomains) != 1 || report.RowsWithoutDomains[0].Name != "ghost" {
		t.Errorf("rows without domains = %v, want [ghost]", report.RowsWithoutDomains)
	}

	// The matched pair appears in neither list.
	for _, dom := range report.DomainsWithoutRows {
		if dom.UUID == inst.UUID {
			t.Error("matched domain reported as unmatched")
		}
	}
}

func TestReconcileReportsAndNeverMutates(t *testing.T) {
	// SPEC 6.1: the reconcile command reports and never deletes. It must
	// not write to the store or touch a domain at all.
	e := newEnv(t, nil, nil)
	setupInstance(t, e)
	e.fake.SetDomain(hypervisor.FakeDomain{
		Name: "rogue", UUID: "deadbeef-0000-4000-8000-000000000000", State: types.StateRunning,
	})
	e.store.mutations = nil
	e.fake.Calls = nil

	if _, err := e.m.Reconcile(context.Background()); err != nil {
		t.Fatal(err)
	}

	if len(e.store.mutations) != 0 {
		t.Errorf("reconcile wrote to the store: %v", e.store.mutations)
	}
	for _, call := range e.fake.Calls {
		if call != "list " {
			t.Errorf("reconcile did more than list domains: %v", e.fake.Calls)
		}
	}
	// The rogue domain and every real domain survive.
	if e.fake.Domain("rogue") == nil || e.fake.Domain("web") == nil {
		t.Error("reconcile deleted a domain")
	}
}

func TestReconcileEmpty(t *testing.T) {
	e := newEnv(t, nil, nil)
	setupInstance(t, e)

	report, err := e.m.Reconcile(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if !report.Empty() {
		t.Errorf("report = %+v, want empty when libvirt and the database agree", report)
	}
}
