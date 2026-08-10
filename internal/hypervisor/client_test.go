package hypervisor

import (
	"context"
	"errors"
	"fmt"
	"testing"
	"time"

	"github.com/digitalocean/go-libvirt"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// apiDomain is one domain inside fakeAPI.
type apiDomain struct {
	dom       libvirt.Domain
	state     libvirt.DomainState
	autostart int32
	xml       string
}

// fakeAPI implements libvirtAPI in memory so Client logic tests run
// without a libvirtd.
type fakeAPI struct {
	domains map[string]*apiDomain
	calls   []string
	errOn   map[string]error
	// shutdownAfterPolls makes a domain reach shutoff after N calls to
	// DomainGetState following a DomainShutdown. Negative means never.
	shutdownAfterPolls int
	pollsSinceShutdown int
	shuttingDown       string
	// undefineFlags records the flags of every DomainUndefineFlags call.
	undefineFlags []libvirt.DomainUndefineFlagsValues
}

func newFakeAPI() *fakeAPI {
	return &fakeAPI{
		domains:            make(map[string]*apiDomain),
		errOn:              make(map[string]error),
		shutdownAfterPolls: -1,
	}
}

func (f *fakeAPI) add(name string, state libvirt.DomainState) {
	f.domains[name] = &apiDomain{
		dom:       libvirt.Domain{Name: name, UUID: libvirt.UUID{1}, ID: int32(len(f.domains) + 1)},
		state:     state,
		autostart: 1, // pre-set so tests can observe the clear
	}
}

func (f *fakeAPI) call(op string) error {
	f.calls = append(f.calls, op)
	return f.errOn[op]
}

func (f *fakeAPI) get(dom libvirt.Domain) (*apiDomain, error) {
	d, ok := f.domains[dom.Name]
	if !ok {
		return nil, fmt.Errorf("no such domain %q", dom.Name)
	}
	return d, nil
}

func (f *fakeAPI) DomainDefineXML(xmlStr string) (libvirt.Domain, error) {
	if err := f.call("define"); err != nil {
		return libvirt.Domain{}, err
	}
	name := "defined"
	f.domains[name] = &apiDomain{
		dom:       libvirt.Domain{Name: name},
		state:     libvirt.DomainShutoff,
		autostart: 1,
		xml:       xmlStr,
	}
	return f.domains[name].dom, nil
}

func (f *fakeAPI) DomainCreate(dom libvirt.Domain) error {
	if err := f.call("create"); err != nil {
		return err
	}
	d, err := f.get(dom)
	if err != nil {
		return err
	}
	d.state = libvirt.DomainRunning
	return nil
}

func (f *fakeAPI) DomainShutdown(dom libvirt.Domain) error {
	if err := f.call("shutdown"); err != nil {
		return err
	}
	if _, err := f.get(dom); err != nil {
		return err
	}
	f.shuttingDown = dom.Name
	f.pollsSinceShutdown = 0
	return nil
}

func (f *fakeAPI) DomainReboot(dom libvirt.Domain, _ libvirt.DomainRebootFlagValues) error {
	if err := f.call("reboot"); err != nil {
		return err
	}
	_, err := f.get(dom)
	return err
}

func (f *fakeAPI) DomainDestroy(dom libvirt.Domain) error {
	if err := f.call("destroy"); err != nil {
		return err
	}
	d, err := f.get(dom)
	if err != nil {
		return err
	}
	d.state = libvirt.DomainShutoff
	return nil
}

func (f *fakeAPI) DomainUndefineFlags(dom libvirt.Domain, flags libvirt.DomainUndefineFlagsValues) error {
	f.undefineFlags = append(f.undefineFlags, flags)
	if err := f.call("undefine"); err != nil {
		return err
	}
	if _, err := f.get(dom); err != nil {
		return err
	}
	// A real libvirtd refuses to undefine a domain that owns an NVRAM
	// file — and every Bento domain is UEFI (SPEC 5) — unless the call
	// carries the NVRAM or the keep-NVRAM flag.
	if flags&(libvirt.DomainUndefineNvram|libvirt.DomainUndefineKeepNvram) == 0 {
		return fmt.Errorf("cannot undefine domain %s: it owns an NVRAM file", dom.Name)
	}
	delete(f.domains, dom.Name)
	return nil
}

func (f *fakeAPI) DomainSetAutostart(dom libvirt.Domain, autostart int32) error {
	if err := f.call("autostart"); err != nil {
		return err
	}
	d, err := f.get(dom)
	if err != nil {
		return err
	}
	d.autostart = autostart
	return nil
}

func (f *fakeAPI) DomainLookupByName(name string) (libvirt.Domain, error) {
	if err := f.call("lookup"); err != nil {
		return libvirt.Domain{}, err
	}
	d, ok := f.domains[name]
	if !ok {
		return libvirt.Domain{}, fmt.Errorf("no such domain %q", name)
	}
	return d.dom, nil
}

func (f *fakeAPI) DomainGetState(dom libvirt.Domain, _ uint32) (int32, int32, error) {
	if err := f.call("state"); err != nil {
		return 0, 0, err
	}
	d, err := f.get(dom)
	if err != nil {
		return 0, 0, err
	}
	if f.shuttingDown == dom.Name && f.shutdownAfterPolls >= 0 {
		f.pollsSinceShutdown++
		if f.pollsSinceShutdown > f.shutdownAfterPolls {
			d.state = libvirt.DomainShutoff
		}
	}
	return int32(d.state), 0, nil
}

func (f *fakeAPI) ConnectListAllDomains(_ int32, _ libvirt.ConnectListAllDomainsFlags) ([]libvirt.Domain, uint32, error) {
	if err := f.call("list"); err != nil {
		return nil, 0, err
	}
	var out []libvirt.Domain
	for _, d := range f.domains {
		out = append(out, d.dom)
	}
	return out, uint32(len(out)), nil
}

// testClient wires a Client to a fakeAPI with an instant fake clock.
func testClient(api *fakeAPI) (*Client, *[]time.Duration) {
	c := newClient(api)
	var slept []time.Duration
	c.sleep = func(_ context.Context, d time.Duration) error {
		slept = append(slept, d)
		return nil
	}
	return c, &slept
}

func TestClientCreateClearsAutostart(t *testing.T) {
	api := newFakeAPI()
	c, _ := testClient(api)
	if err := c.Create(context.Background(), "<domain/>"); err != nil {
		t.Fatalf("Create: %v", err)
	}
	d := api.domains["defined"]
	if d == nil {
		t.Fatal("domain not defined")
	}
	if d.autostart != 0 {
		t.Error("Create must clear the autostart flag (SPEC 11.2)")
	}
	if d.state != libvirt.DomainRunning {
		t.Errorf("state = %v, want running", d.state)
	}
	want := []string{"define", "autostart", "create"}
	if len(api.calls) != len(want) {
		t.Fatalf("calls = %v, want %v", api.calls, want)
	}
	for i := range want {
		if api.calls[i] != want[i] {
			t.Fatalf("calls = %v, want %v", api.calls, want)
		}
	}
}

func TestClientCreateUndefinesOnStartFailure(t *testing.T) {
	api := newFakeAPI()
	api.errOn["create"] = errors.New("no memory")
	c, _ := testClient(api)
	if err := c.Create(context.Background(), "<domain/>"); err == nil {
		t.Fatal("Create should fail")
	}
	if _, ok := api.domains["defined"]; ok {
		t.Error("failed Create must undefine the domain again")
	}
}

func TestClientStopGraceful(t *testing.T) {
	api := newFakeAPI()
	api.add("web", libvirt.DomainRunning)
	api.shutdownAfterPolls = 3
	c, slept := testClient(api)

	res, err := c.Stop(context.Background(), "web")
	if err != nil {
		t.Fatalf("Stop: %v", err)
	}
	if res != StopGraceful {
		t.Errorf("result = %q, want %q", res, StopGraceful)
	}
	for _, call := range api.calls {
		if call == "destroy" {
			t.Error("graceful stop must not call destroy")
		}
	}
	if len(*slept) != 3 {
		t.Errorf("slept %d times, want 3", len(*slept))
	}
}

func TestClientStopForcedAfterTimeout(t *testing.T) {
	api := newFakeAPI()
	api.add("web", libvirt.DomainRunning)
	// Guest never honors the ACPI request.
	api.shutdownAfterPolls = -1
	c, slept := testClient(api)

	res, err := c.Stop(context.Background(), "web")
	if err != nil {
		t.Fatalf("Stop: %v", err)
	}
	if res != StopForced {
		t.Errorf("result = %q, want %q", res, StopForced)
	}
	if api.domains["web"].state != libvirt.DomainShutoff {
		t.Error("forced stop must destroy the domain")
	}
	// 60s at 500ms per poll = 120 sleeps before the destroy.
	wantSleeps := int(defaultStopTimeout / defaultPollInterval)
	if len(*slept) != wantSleeps {
		t.Errorf("slept %d times, want %d", len(*slept), wantSleeps)
	}
	var total time.Duration
	for _, d := range *slept {
		total += d
	}
	if total != defaultStopTimeout {
		t.Errorf("total wait = %v, want %v (SPEC 11.1: wait 60 seconds)", total, defaultStopTimeout)
	}
}

func TestClientStopAlreadyStopped(t *testing.T) {
	api := newFakeAPI()
	api.add("web", libvirt.DomainShutoff)
	c, _ := testClient(api)
	res, err := c.Stop(context.Background(), "web")
	if err != nil {
		t.Fatalf("Stop: %v", err)
	}
	if res != StopNoop {
		t.Errorf("result = %q, want %q", res, StopNoop)
	}
	for _, call := range api.calls {
		if call == "shutdown" || call == "destroy" {
			t.Errorf("stop of a stopped domain must not call %s", call)
		}
	}
}

func TestClientStopCanceledContext(t *testing.T) {
	api := newFakeAPI()
	api.add("web", libvirt.DomainRunning)
	c := newClient(api)
	c.sleep = sleepContext
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := c.Stop(ctx, "web"); !errors.Is(err, context.Canceled) {
		t.Errorf("err = %v, want context.Canceled", err)
	}
}

func TestClientRemove(t *testing.T) {
	tests := []struct {
		name        string
		state       libvirt.DomainState
		wantDestroy bool
	}{
		{"running domain", libvirt.DomainRunning, true},
		{"stopped domain", libvirt.DomainShutoff, false},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			api := newFakeAPI()
			api.add("web", tt.state)
			c, _ := testClient(api)
			if err := c.Remove(context.Background(), "web"); err != nil {
				t.Fatalf("Remove: %v", err)
			}
			if _, ok := api.domains["web"]; ok {
				t.Error("Remove must undefine the domain")
			}
			destroyed := false
			for _, call := range api.calls {
				if call == "destroy" {
					destroyed = true
				}
			}
			if destroyed != tt.wantDestroy {
				t.Errorf("destroy called = %v, want %v", destroyed, tt.wantDestroy)
			}
		})
	}
}

// TestClientUndefineCarriesNvramFlag pins the UEFI requirement: every
// Bento domain owns an NVRAM file (SPEC 5), so every undefine — the rm
// path and the failed-create unwind — must pass the NVRAM flag, or a
// real libvirtd refuses it and the four rm steps of SPEC 11.1 never
// complete.
func TestClientUndefineCarriesNvramFlag(t *testing.T) {
	t.Run("remove", func(t *testing.T) {
		api := newFakeAPI()
		api.add("web", libvirt.DomainRunning)
		c, _ := testClient(api)
		if err := c.Remove(context.Background(), "web"); err != nil {
			t.Fatalf("Remove: %v", err)
		}
		if len(api.undefineFlags) != 1 {
			t.Fatalf("undefine calls = %d, want 1", len(api.undefineFlags))
		}
		if api.undefineFlags[0]&libvirt.DomainUndefineNvram == 0 {
			t.Errorf("undefine flags = %v, want the NVRAM flag set", api.undefineFlags[0])
		}
	})
	t.Run("failed create unwind", func(t *testing.T) {
		api := newFakeAPI()
		api.errOn["create"] = errors.New("no memory")
		c, _ := testClient(api)
		if err := c.Create(context.Background(), "<domain/>"); err == nil {
			t.Fatal("Create should fail")
		}
		if len(api.undefineFlags) != 1 {
			t.Fatalf("undefine calls = %d, want 1", len(api.undefineFlags))
		}
		if api.undefineFlags[0]&libvirt.DomainUndefineNvram == 0 {
			t.Errorf("unwind undefine flags = %v, want the NVRAM flag set", api.undefineFlags[0])
		}
	})
}

func TestClientListAndState(t *testing.T) {
	api := newFakeAPI()
	api.add("running-vm", libvirt.DomainRunning)
	api.add("stopped-vm", libvirt.DomainShutoff)
	c, _ := testClient(api)

	infos, err := c.List(context.Background())
	if err != nil {
		t.Fatalf("List: %v", err)
	}
	if len(infos) != 2 {
		t.Fatalf("len = %d, want 2", len(infos))
	}
	byName := map[string]types.State{}
	for _, info := range infos {
		byName[info.Name] = info.State
		if info.UUID == "" {
			t.Errorf("%s: empty UUID", info.Name)
		}
	}
	if byName["running-vm"] != types.StateRunning {
		t.Errorf("running-vm state = %q", byName["running-vm"])
	}
	if byName["stopped-vm"] != types.StateStopped {
		t.Errorf("stopped-vm state = %q", byName["stopped-vm"])
	}

	st, err := c.State(context.Background(), "running-vm")
	if err != nil {
		t.Fatalf("State: %v", err)
	}
	if st != types.StateRunning {
		t.Errorf("state = %q, want running", st)
	}
	if _, err := c.State(context.Background(), "missing"); err == nil {
		t.Error("State of a missing domain should fail")
	}
}

func TestStateFromLibvirt(t *testing.T) {
	tests := []struct {
		in   libvirt.DomainState
		want types.State
	}{
		{libvirt.DomainRunning, types.StateRunning},
		{libvirt.DomainBlocked, types.StateRunning},
		{libvirt.DomainPaused, types.StateRunning},
		{libvirt.DomainShutdown, types.StateRunning},
		{libvirt.DomainPmsuspended, types.StateRunning},
		{libvirt.DomainShutoff, types.StateStopped},
		{libvirt.DomainCrashed, types.StateStopped},
		{libvirt.DomainNostate, types.StateStopped},
	}
	for _, tt := range tests {
		if got := stateFromLibvirt(tt.in); got != tt.want {
			t.Errorf("stateFromLibvirt(%d) = %q, want %q", tt.in, got, tt.want)
		}
	}
}

func TestFormatUUID(t *testing.T) {
	u := libvirt.UUID{0x6d, 0x1e, 0x0f, 0x1c, 0x9a, 0x3b, 0x4f, 0x6e, 0x8a, 0x2d, 0x3c, 0x5b, 0x7e, 0x9f, 0x1a, 0x2b}
	want := "6d1e0f1c-9a3b-4f6e-8a2d-3c5b7e9f1a2b"
	if got := formatUUID(u); got != want {
		t.Errorf("formatUUID = %q, want %q", got, want)
	}
}
