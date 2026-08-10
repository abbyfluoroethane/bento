package hypervisor

import (
	"context"
	"errors"
	"testing"

	"github.com/abbyfluoroethane/bento/internal/types"
)

func fakeWithDomain(t *testing.T) *Fake {
	t.Helper()
	f := &Fake{}
	xml, err := DomainXML(baseSpec())
	if err != nil {
		t.Fatalf("DomainXML: %v", err)
	}
	if err := f.Create(context.Background(), xml); err != nil {
		t.Fatalf("Create: %v", err)
	}
	return f
}

func TestFakeLifecycle(t *testing.T) {
	ctx := context.Background()
	f := fakeWithDomain(t)

	dom := f.Domain("bento-web")
	if dom == nil {
		t.Fatal("domain not stored")
	}
	if dom.UUID != baseSpec().UUID {
		t.Errorf("uuid = %q, want %q", dom.UUID, baseSpec().UUID)
	}
	if dom.Autostart {
		t.Error("created domain must not have autostart set")
	}
	if st, _ := f.State(ctx, "bento-web"); st != types.StateRunning {
		t.Errorf("state after create = %q, want running", st)
	}

	if res, err := f.Stop(ctx, "bento-web"); err != nil || res != StopGraceful {
		t.Errorf("Stop = (%q, %v), want graceful", res, err)
	}
	if st, _ := f.State(ctx, "bento-web"); st != types.StateStopped {
		t.Errorf("state after stop = %q, want stopped", st)
	}
	if res, err := f.Stop(ctx, "bento-web"); err != nil || res != StopNoop {
		t.Errorf("second Stop = (%q, %v), want noop", res, err)
	}

	if err := f.Start(ctx, "bento-web"); err != nil {
		t.Fatalf("Start: %v", err)
	}
	if err := f.Reboot(ctx, "bento-web"); err != nil {
		t.Fatalf("Reboot: %v", err)
	}

	if err := f.Remove(ctx, "bento-web"); err != nil {
		t.Fatalf("Remove: %v", err)
	}
	if _, err := f.State(ctx, "bento-web"); !errors.Is(err, ErrDomainNotFound) {
		t.Errorf("State after remove = %v, want ErrDomainNotFound", err)
	}
}

func TestFakeCreateRejectsDuplicateAndBadXML(t *testing.T) {
	ctx := context.Background()
	f := fakeWithDomain(t)
	xml, _ := DomainXML(baseSpec())
	if err := f.Create(ctx, xml); !errors.Is(err, ErrDomainExists) {
		t.Errorf("duplicate create = %v, want ErrDomainExists", err)
	}
	if err := (&Fake{}).Create(ctx, "not xml <"); err == nil {
		t.Error("bad XML must be rejected")
	}
	if err := (&Fake{}).Create(ctx, "<domain><name>x</name></domain>"); err == nil {
		t.Error("XML without uuid must be rejected")
	}
}

func TestFakeUnknownDomainErrors(t *testing.T) {
	ctx := context.Background()
	f := &Fake{}
	if err := f.Start(ctx, "ghost"); !errors.Is(err, ErrDomainNotFound) {
		t.Errorf("Start = %v", err)
	}
	if _, err := f.Stop(ctx, "ghost"); !errors.Is(err, ErrDomainNotFound) {
		t.Errorf("Stop = %v", err)
	}
	if err := f.Reboot(ctx, "ghost"); !errors.Is(err, ErrDomainNotFound) {
		t.Errorf("Reboot = %v", err)
	}
	if err := f.Remove(ctx, "ghost"); !errors.Is(err, ErrDomainNotFound) {
		t.Errorf("Remove = %v", err)
	}
}

func TestFakeListSortedAndSeeded(t *testing.T) {
	ctx := context.Background()
	f := &Fake{}
	f.SetDomain(FakeDomain{Name: "b-vm", UUID: "u2", State: types.StateStopped})
	f.SetDomain(FakeDomain{Name: "a-vm", UUID: "u1", State: types.StateRunning})
	infos, err := f.List(ctx)
	if err != nil {
		t.Fatalf("List: %v", err)
	}
	if len(infos) != 2 || infos[0].Name != "a-vm" || infos[1].Name != "b-vm" {
		t.Fatalf("List = %+v, want sorted a-vm then b-vm", infos)
	}
	if infos[0].State != types.StateRunning || infos[1].State != types.StateStopped {
		t.Errorf("states = %+v", infos)
	}
}

func TestFakeHookAndForceStop(t *testing.T) {
	ctx := context.Background()
	f := fakeWithDomain(t)
	boom := errors.New("boom")
	f.Hook = func(op, name string) error {
		if op == "start" && name == "bento-web" {
			return boom
		}
		return nil
	}
	if err := f.Start(ctx, "bento-web"); !errors.Is(err, boom) {
		t.Errorf("hooked Start = %v, want boom", err)
	}

	f.Hook = nil
	f.ForceStop = true
	if res, err := f.Stop(ctx, "bento-web"); err != nil || res != StopForced {
		t.Errorf("Stop = (%q, %v), want forced", res, err)
	}

	if len(f.Calls) == 0 || f.Calls[0] != "create bento-web" {
		t.Errorf("Calls = %v, want create bento-web first", f.Calls)
	}
}
