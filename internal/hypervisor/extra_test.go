package hypervisor

import (
	"context"
	"errors"
	"testing"

	"github.com/digitalocean/go-libvirt"
)

// netFakeAPI extends fakeAPI with the network slice of go-libvirt.
type netFakeAPI struct {
	*fakeAPI
	networks  map[string]*fakeNetwork
	netErrOn  map[string]error
	netCalls  []string
	defineXML string
}

type fakeNetwork struct {
	net       libvirt.Network
	active    int32
	autostart int32
}

func newNetFakeAPI() *netFakeAPI {
	return &netFakeAPI{
		fakeAPI:  newFakeAPI(),
		networks: make(map[string]*fakeNetwork),
		netErrOn: make(map[string]error),
	}
}

func (f *netFakeAPI) netCall(op string) error {
	f.netCalls = append(f.netCalls, op)
	return f.netErrOn[op]
}

func (f *netFakeAPI) NetworkLookupByName(name string) (libvirt.Network, error) {
	if err := f.netCall("lookup"); err != nil {
		return libvirt.Network{}, err
	}
	n, ok := f.networks[name]
	if !ok {
		return libvirt.Network{}, libvirt.Error{Code: uint32(libvirt.ErrNoNetwork), Message: "network not found"}
	}
	return n.net, nil
}

func (f *netFakeAPI) NetworkDefineXML(xml string) (libvirt.Network, error) {
	if err := f.netCall("define"); err != nil {
		return libvirt.Network{}, err
	}
	f.defineXML = xml
	n := &fakeNetwork{net: libvirt.Network{Name: "defined"}}
	f.networks["defined"] = n
	return n.net, nil
}

func (f *netFakeAPI) NetworkCreate(net libvirt.Network) error {
	if err := f.netCall("start"); err != nil {
		return err
	}
	if n, ok := f.networks[net.Name]; ok {
		n.active = 1
	}
	return nil
}

func (f *netFakeAPI) NetworkIsActive(net libvirt.Network) (int32, error) {
	if err := f.netCall("isactive"); err != nil {
		return 0, err
	}
	if n, ok := f.networks[net.Name]; ok {
		return n.active, nil
	}
	return 0, nil
}

func (f *netFakeAPI) NetworkSetAutostart(net libvirt.Network, autostart int32) error {
	if err := f.netCall("autostart"); err != nil {
		return err
	}
	if n, ok := f.networks[net.Name]; ok {
		n.autostart = autostart
	}
	return nil
}

func TestClientDefine(t *testing.T) {
	api := newFakeAPI()
	c := newClient(api)
	if err := c.Define(context.Background(), "<domain/>"); err != nil {
		t.Fatalf("Define: %v", err)
	}
	if api.domains["defined"].xml != "<domain/>" {
		t.Errorf("defined XML = %q", api.domains["defined"].xml)
	}
	api.errOn["define"] = errors.New("boom")
	if err := c.Define(context.Background(), "<domain/>"); err == nil {
		t.Error("Define with failing define: want error")
	}
}

func TestClientClearAutostart(t *testing.T) {
	api := newFakeAPI()
	api.add("web", libvirt.DomainShutoff)
	c := newClient(api)
	if err := c.ClearAutostart(context.Background(), "web"); err != nil {
		t.Fatalf("ClearAutostart: %v", err)
	}
	if got := api.domains["web"].autostart; got != 0 {
		t.Errorf("autostart = %d, want 0", got)
	}
}

func TestClientEnsureNetwork(t *testing.T) {
	tests := []struct {
		name      string
		setup     func(*netFakeAPI)
		wantCalls []string
		wantErr   bool
	}{
		{
			name:      "missing network is defined started and autostarted",
			setup:     func(*netFakeAPI) {},
			wantCalls: []string{"lookup", "define", "autostart", "isactive", "start"},
		},
		{
			name: "existing inactive network is started",
			setup: func(f *netFakeAPI) {
				f.networks["bento-user-1"] = &fakeNetwork{net: libvirt.Network{Name: "bento-user-1"}}
			},
			wantCalls: []string{"lookup", "autostart", "isactive", "start"},
		},
		{
			name: "existing active network is left alone",
			setup: func(f *netFakeAPI) {
				f.networks["bento-user-1"] = &fakeNetwork{net: libvirt.Network{Name: "bento-user-1"}, active: 1}
			},
			wantCalls: []string{"lookup", "autostart", "isactive"},
		},
		{
			name: "lookup failure other than not-found is fatal",
			setup: func(f *netFakeAPI) {
				f.netErrOn["lookup"] = errors.New("connection lost")
			},
			wantCalls: []string{"lookup"},
			wantErr:   true,
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			api := newNetFakeAPI()
			tt.setup(api)
			c := newClient(api)
			err := c.EnsureNetwork(context.Background(), "bento-user-1", "<network/>")
			if (err != nil) != tt.wantErr {
				t.Fatalf("EnsureNetwork error = %v, wantErr %v", err, tt.wantErr)
			}
			if len(api.netCalls) != len(tt.wantCalls) {
				t.Fatalf("calls = %v, want %v", api.netCalls, tt.wantCalls)
			}
			for i := range tt.wantCalls {
				if api.netCalls[i] != tt.wantCalls[i] {
					t.Fatalf("calls = %v, want %v", api.netCalls, tt.wantCalls)
				}
			}
		})
	}
}

func TestClientEnsureNetworkWithoutNetworkAPI(t *testing.T) {
	c := newClient(newFakeAPI())
	if err := c.EnsureNetwork(context.Background(), "n", "<network/>"); err == nil {
		t.Error("want error from a connection without network support")
	}
}

func TestLookupMapsNoDomain(t *testing.T) {
	api := newFakeAPI()
	api.errOn["lookup"] = libvirt.Error{Code: uint32(libvirt.ErrNoDomain), Message: "no domain"}
	c := newClient(api)
	_, err := c.State(context.Background(), "ghost")
	if !errors.Is(err, ErrDomainNotFound) {
		t.Errorf("State on missing domain = %v, want ErrDomainNotFound", err)
	}
}
