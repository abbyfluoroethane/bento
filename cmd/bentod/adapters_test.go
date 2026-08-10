package main

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"slices"
	"strings"
	"testing"

	"github.com/abbyfluoroethane/bento/internal/auth"
	"github.com/abbyfluoroethane/bento/internal/cli"
	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/lifecycle"
	"github.com/abbyfluoroethane/bento/internal/sshfront"
	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
)

func createInstance(t *testing.T, e *cmdEnv, b *cliBackend, owner types.User, name string) types.Instance {
	t.Helper()
	inst, err := b.Create(context.Background(), cli.CreateRequest{
		OwnerID:   owner.ID,
		Name:      name,
		Image:     "debian-13",
		VCPU:      2,
		MemoryMiB: 2048,
		DiskGiB:   20,
		KSM:       true,
	})
	if err != nil {
		t.Fatalf("Create(%s): %v", name, err)
	}
	return inst
}

func TestCLIBackendCreateSeedsFrontendKey(t *testing.T) {
	e := newCmdEnv(t)
	owner := e.addUser(t, "amber")
	e.addImage(t, "debian-13", "aa11")
	b := e.backendFor("ssh-ed25519 AAAAfrontend bento-frontend")

	inst := createInstance(t, e, b, owner, "web")

	seed, ok := e.iso.seeds[e.mgr.SeedISOPath(inst.UUID)]
	if !ok {
		t.Fatal("no seed built")
	}
	// The owner's keys and the frontend key both reach the guest
	// (SPEC 5.2, 10 step 9).
	if !slices.Contains(seed.AuthorizedKeys, testOwnerKey) {
		t.Errorf("seed keys %v missing the owner key", seed.AuthorizedKeys)
	}
	if !slices.Contains(seed.AuthorizedKeys, "ssh-ed25519 AAAAfrontend bento-frontend") {
		t.Errorf("seed keys %v missing the frontend key", seed.AuthorizedKeys)
	}
	// The guest account matches what the SSH frontend dials.
	if seed.UserName != sshfront.DefaultGuestUser {
		t.Errorf("seed user = %s, want %s", seed.UserName, sshfront.DefaultGuestUser)
	}
	if e.hyp.Domain("web") == nil {
		t.Error("domain web not created")
	}
}

func TestCLIBackendStopStartRestart(t *testing.T) {
	e := newCmdEnv(t)
	owner := e.addUser(t, "amber")
	e.addImage(t, "debian-13", "aa11")
	b := e.backendFor("")
	inst := createInstance(t, e, b, owner, "web")

	result, err := b.Stop(context.Background(), inst)
	if err != nil {
		t.Fatalf("Stop: %v", err)
	}
	if result != hypervisor.StopGraceful {
		t.Errorf("stop result = %s, want graceful", result)
	}
	row, _ := e.st.Instance(inst.UUID)
	if row.DesiredState != types.DesiredStopped || row.State != types.StateStopped {
		t.Errorf("states after stop = %s/%s, want stopped/stopped", row.DesiredState, row.State)
	}

	if err := b.Start(context.Background(), inst); err != nil {
		t.Fatalf("Start: %v", err)
	}
	row, _ = e.st.Instance(inst.UUID)
	if row.DesiredState != types.DesiredRunning {
		t.Errorf("desired after start = %s, want running", row.DesiredState)
	}

	if err := b.Restart(context.Background(), inst); err != nil {
		t.Fatalf("Restart: %v", err)
	}
}

func TestCLIBackendRenameMovesDomain(t *testing.T) {
	e := newCmdEnv(t)
	owner := e.addUser(t, "amber")
	e.addImage(t, "debian-13", "aa11")
	b := e.backendFor("")
	inst := createInstance(t, e, b, owner, "web")
	if _, err := b.Stop(context.Background(), inst); err != nil {
		t.Fatal(err)
	}

	if err := b.Rename(context.Background(), inst, "api"); err != nil {
		t.Fatalf("Rename: %v", err)
	}
	if _, err := e.st.InstanceByName("api"); err != nil {
		t.Errorf("row api after rename: %v", err)
	}
	if e.hyp.Domain("web") != nil || e.hyp.Domain("api") == nil {
		t.Error("domain did not move from web to api")
	}
	// The old name is in cooldown (SPEC 7.2).
	if _, err := e.st.ReleasedName("web"); err != nil {
		t.Errorf("released name web: %v", err)
	}
}

func TestCLIBackendResizeFillsUnchangedFields(t *testing.T) {
	e := newCmdEnv(t)
	owner := e.addUser(t, "amber")
	e.addImage(t, "debian-13", "aa11")
	b := e.backendFor("")
	inst := createInstance(t, e, b, owner, "web")

	mem := int64(4096)
	if err := b.Resize(context.Background(), inst, cli.ResizeRequest{MemoryMiB: &mem}); err != nil {
		t.Fatalf("Resize: %v", err)
	}
	row, _ := e.st.Instance(inst.UUID)
	if row.MemoryMiB != 4096 {
		t.Errorf("memory = %d, want 4096", row.MemoryMiB)
	}
	if row.VCPU != 2 || row.DiskGiB != 20 || row.Nested {
		t.Errorf("unchanged fields moved: %+v", row)
	}
}

func TestCLIBackendCopyAndRemove(t *testing.T) {
	e := newCmdEnv(t)
	owner := e.addUser(t, "amber")
	e.addImage(t, "debian-13", "aa11")
	b := e.backendFor("")
	src := createInstance(t, e, b, owner, "web")
	if _, err := b.Stop(context.Background(), src); err != nil {
		t.Fatal(err)
	}

	clone, err := b.Copy(context.Background(), src, cli.CreateRequest{
		OwnerID: owner.ID, Name: "web2", Image: "debian-13",
		VCPU: 2, MemoryMiB: 2048, DiskGiB: 20, KSM: true,
	})
	if err != nil {
		t.Fatalf("Copy: %v", err)
	}
	if clone.BaseChecksum != "aa11" {
		t.Errorf("clone checksum = %s, want aa11", clone.BaseChecksum)
	}

	if err := b.Remove(context.Background(), clone); err != nil {
		t.Fatalf("Remove: %v", err)
	}
	if _, err := e.st.Instance(clone.UUID); !errors.Is(err, store.ErrNotFound) {
		t.Errorf("clone row after rm = %v, want ErrNotFound", err)
	}
	if _, err := e.st.ReleasedName("web2"); err != nil {
		t.Errorf("released name web2: %v", err)
	}
}

func TestCLIBackendConsoleUnavailable(t *testing.T) {
	e := newCmdEnv(t)
	b := e.backendFor("")
	err := b.Console(context.Background(), types.Instance{Name: "web"}, nil)
	if err == nil || !strings.Contains(err.Error(), "console") {
		t.Errorf("Console = %v, want a clear unavailability error", err)
	}
}

func TestRegistrar(t *testing.T) {
	e := newCmdEnv(t)
	ensured := map[string]string{}
	r := &registrar{
		st:   e.st,
		plan: e.plan,
		networks: ensureFunc(func(_ context.Context, name, xml string) error {
			ensured[name] = xml
			return nil
		}),
		log: discardLogger(),
	}
	u, err := r.Register(context.Background(), sshfront.Registration{
		Name:        "amber",
		Email:       "amber@example.org",
		PublicKey:   testOwnerKey,
		Fingerprint: "SHA256:fp",
		Comment:     "owner@laptop",
	})
	if err != nil {
		t.Fatalf("Register: %v", err)
	}
	if u.Subnet != "10.100.0.0/24" {
		t.Errorf("subnet = %s, want the first /24", u.Subnet)
	}
	keys, err := e.st.SSHKeysForUser(u.ID)
	if err != nil || len(keys) != 1 {
		t.Fatalf("keys = %v, %v; want the registered key", keys, err)
	}
	xml, ok := ensured["bento-user-0"]
	if !ok {
		t.Fatalf("networks ensured = %v, want bento-user-0", ensured)
	}
	if !strings.Contains(xml, "bento0") {
		t.Errorf("network XML misses the bridge name:\n%s", xml)
	}
}

func TestAuthAdaptersRoundTrip(t *testing.T) {
	e := newCmdEnv(t)
	owner := e.addUser(t, "amber")
	svc := auth.New("bento.example.org", authUsers{e.st}, e.st, authTokens{e.st})
	an := &authenticator{svc: svc, st: e.st}

	// No credentials: unauthenticated.
	req := httptest.NewRequest("GET", "https://bento.example.org/api/whoami", nil)
	if _, err := an.UserFromRequest(req); !errors.Is(err, auth.ErrUnauthenticated) {
		t.Errorf("no credentials = %v, want ErrUnauthenticated", err)
	}

	// A minted token authenticates and resolves the user.
	plaintext, _, err := svc.MintToken(owner.ID, 0)
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Authorization", "Bearer "+plaintext)
	u, err := an.UserFromRequest(req)
	if err != nil {
		t.Fatalf("token auth: %v", err)
	}
	if u.ID != owner.ID {
		t.Errorf("user = %d, want %d", u.ID, owner.ID)
	}
}

func TestProxySourceHidesMissingNames(t *testing.T) {
	e := newCmdEnv(t)
	src := proxySource{e.st}
	_, ok, err := src.InstanceByName(context.Background(), "ghost")
	if err != nil || ok {
		t.Errorf("missing name = ok %v err %v, want ok=false err=nil", ok, err)
	}
}

func TestAPIBackendSetHTTPPortReloadsFirewall(t *testing.T) {
	e := newCmdEnv(t)
	owner := e.addUser(t, "amber")
	e.addImage(t, "debian-13", "aa11")
	cliB := e.backendFor("")
	inst := createInstance(t, e, cliB, owner, "web")
	if err := e.st.SetVisibility(inst.UUID, types.VisibilityPublic); err != nil {
		t.Fatal(err)
	}

	applier := &recordingApplier{}
	fw := &firewall{st: e.st, plan: e.plan, applier: applier, log: discardLogger()}
	be := cliB.backend
	be.firewall = fw
	b := &apiBackend{backend: be}

	if err := b.SetHTTPPort(context.Background(), inst.UUID, 8080); err != nil {
		t.Fatalf("SetHTTPPort: %v", err)
	}
	row, _ := e.st.Instance(inst.UUID)
	if row.HTTPPort != 8080 {
		t.Errorf("http port = %d, want 8080", row.HTTPPort)
	}
	if len(applier.applied) != 1 || !strings.Contains(applier.applied[0], "8080") {
		t.Errorf("firewall applies = %d, want 1 naming port 8080", len(applier.applied))
	}
}

// TestAPIBackendSetVisibilityReloadsFirewall pins SPEC 6.3: the
// nftables table is reloaded on every change, and a visibility change
// alters the published ports.
func TestAPIBackendSetVisibilityReloadsFirewall(t *testing.T) {
	e := newCmdEnv(t)
	owner := e.addUser(t, "amber")
	e.addImage(t, "debian-13", "aa11")
	cliB := e.backendFor("")
	inst := createInstance(t, e, cliB, owner, "web")

	applier := &recordingApplier{}
	fw := &firewall{st: e.st, plan: e.plan, applier: applier, log: discardLogger()}
	be := cliB.backend
	be.firewall = fw
	b := &apiBackend{backend: be}

	if err := b.SetVisibility(context.Background(), inst.UUID, types.VisibilityPublic); err != nil {
		t.Fatalf("SetVisibility: %v", err)
	}
	row, _ := e.st.Instance(inst.UUID)
	if row.Visibility != types.VisibilityPublic {
		t.Errorf("visibility = %q, want public", row.Visibility)
	}
	if len(applier.applied) != 1 || !strings.Contains(applier.applied[0], "3000-9999") {
		t.Errorf("firewall applies = %d, want 1 publishing the proxy range", len(applier.applied))
	}
}

// TestCLIBackendChangesReloadFirewall pins the same SPEC 6.3 rule for
// the SSH CLI process: port, visibility, create, and remove all reload
// the table instead of waiting for the 30-second convergence tick.
func TestCLIBackendChangesReloadFirewall(t *testing.T) {
	e := newCmdEnv(t)
	owner := e.addUser(t, "amber")
	e.addImage(t, "debian-13", "aa11")
	applier := &recordingApplier{}
	fw := &firewall{st: e.st, plan: e.plan, applier: applier, log: discardLogger()}
	b := e.backendFor("")
	b.firewall = fw
	ctx := context.Background()

	inst := createInstance(t, e, b, owner, "web")
	if len(applier.applied) != 1 {
		t.Fatalf("applies after create = %d, want 1", len(applier.applied))
	}
	if err := b.SetHTTPPort(ctx, inst, 8080); err != nil {
		t.Fatalf("SetHTTPPort: %v", err)
	}
	if err := b.SetVisibility(ctx, inst, types.VisibilityPublic); err != nil {
		t.Fatalf("SetVisibility: %v", err)
	}
	// The port was stored before the visibility flip, so only the
	// visibility change publishes it.
	if n := len(applier.applied); n != 2 || !strings.Contains(applier.applied[1], "8080") {
		t.Fatalf("applies after port+visibility = %d, want 2 with the new port published", n)
	}
	if err := b.Remove(ctx, inst); err != nil {
		t.Fatalf("Remove: %v", err)
	}
	if n := len(applier.applied); n != 3 {
		t.Fatalf("applies after remove = %d, want 3", n)
	}
	if strings.Contains(applier.applied[2], inst.Address) {
		t.Errorf("removed instance %s still published:\n%s", inst.Address, applier.applied[2])
	}
}

// TestAccessHandler pins the control plane half of the proxy's
// per-request authorization (SPEC 13): the owner is granted, another
// user's valid credential is forbidden, and no credential is
// unauthenticated.
func TestAccessHandler(t *testing.T) {
	e := newCmdEnv(t)
	owner := e.addUser(t, "amber")
	other := e.addUser(t, "blair")
	shared := e.addUser(t, "carol")
	e.addImage(t, "debian-13", "aa11")
	inst := createInstance(t, e, e.backendFor(""), owner, "web")
	if err := e.st.AddShare(inst.UUID, shared.ID); err != nil {
		t.Fatal(err)
	}

	svc := auth.New("bento.example.org", authUsers{e.st}, e.st, authTokens{e.st})
	mux := http.NewServeMux()
	mux.Handle("GET /access/{uuid}", accessHandler(svc))

	token := func(userID int64) string {
		plaintext, _, err := svc.MintToken(userID, 0)
		if err != nil {
			t.Fatal(err)
		}
		return plaintext
	}
	tests := []struct {
		name       string
		uuid       string
		token      string
		wantStatus int
	}{
		{"owner granted", inst.UUID, token(owner.ID), http.StatusNoContent},
		{"share granted", inst.UUID, token(shared.ID), http.StatusNoContent},
		{"other user forbidden", inst.UUID, token(other.ID), http.StatusForbidden},
		{"no credential unauthenticated", inst.UUID, "", http.StatusUnauthorized},
		{"unknown instance forbidden", "no-such-uuid", token(owner.ID), http.StatusForbidden},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			req := httptest.NewRequest("GET", "/access/"+tt.uuid, nil)
			if tt.token != "" {
				req.Header.Set("Authorization", "Bearer "+tt.token)
			}
			w := httptest.NewRecorder()
			mux.ServeHTTP(w, req)
			if w.Code != tt.wantStatus {
				t.Errorf("status = %d, want %d", w.Code, tt.wantStatus)
			}
		})
	}
}

// ensureFunc adapts a function to networkEnsurer.
type ensureFunc func(ctx context.Context, name, xml string) error

func (f ensureFunc) EnsureNetwork(ctx context.Context, name, xml string) error {
	return f(ctx, name, xml)
}

// Compile-time wiring checks: the adapters and the concrete types slot
// into every consumer-side interface.
var (
	_ cli.Store                  = (*store.Store)(nil)
	_ sshfront.KeyStore          = (*store.Store)(nil)
	_ lifecycle.Store            = (*store.Store)(nil)
	_ auth.AccessStore           = (*store.Store)(nil)
	_ networkEnsurer             = (*hypervisor.Client)(nil)
	_ lifecycle.Definer          = (*hypervisor.Client)(nil)
	_ lifecycle.AutostartClearer = (*hypervisor.Client)(nil)
)
