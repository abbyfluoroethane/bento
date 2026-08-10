package main

// Test env: a real store and a real lifecycle manager over an in-memory
// hypervisor and fake host tools, which is exactly what the adapters
// glue together in production.

import (
	"context"
	"encoding/xml"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/abbyfluoroethane/bento/internal/cloudinit"
	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/lifecycle"
	"github.com/abbyfluoroethane/bento/internal/network"
	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
)

const testOwnerKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFoo owner@laptop"

// defineFake adds the Definer capability the rename path needs.
type defineFake struct {
	*hypervisor.Fake
}

func (d *defineFake) Define(_ context.Context, domXML string) error {
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

// fakeImages creates overlay files without qemu-img.
type fakeImages struct{}

func (fakeImages) CreateOverlay(_ context.Context, _ string, overlayPath string, _ int64) error {
	return os.WriteFile(overlayPath, []byte("overlay"), 0o600)
}

// fakeISO records seeds without xorriso.
type fakeISO struct {
	seeds map[string]cloudinit.Seed
}

func (f *fakeISO) Build(_ context.Context, seed cloudinit.Seed, isoPath string) error {
	f.seeds[isoPath] = seed
	return nil
}

type cmdEnv struct {
	st   *store.Store
	plan network.Plan
	hyp  *defineFake
	mgr  *lifecycle.Manager
	iso  *fakeISO
	host types.Host
}

func newCmdEnv(t *testing.T) *cmdEnv {
	t.Helper()
	dir := t.TempDir()
	st, err := store.Open(filepath.Join(dir, "bento.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { st.Close() })
	plan, err := network.NewPlan("10.100.0.0/16")
	if err != nil {
		t.Fatal(err)
	}
	host, err := st.EnsureHost("testhost", "qemu:///system")
	if err != nil {
		t.Fatal(err)
	}
	e := &cmdEnv{
		st:   st,
		plan: plan,
		hyp:  &defineFake{Fake: &hypervisor.Fake{}},
		iso:  &fakeISO{seeds: map[string]cloudinit.Seed{}},
		host: host,
	}
	mgr, err := lifecycle.NewManager(lifecycle.Config{
		Hypervisor:    e.hyp,
		Store:         st,
		Images:        fakeImages{},
		ISO:           e.iso,
		Plan:          plan,
		StorageDir:    dir,
		Logger:        slog.New(slog.NewTextHandler(io.Discard, nil)),
		NestedEnabled: func() (bool, string) { return true, "" },
		DeleteISO:     func(string) error { return nil },
		ISOExists:     func(string) bool { return false },
	})
	if err != nil {
		t.Fatal(err)
	}
	e.mgr = mgr
	return e
}

// addUser registers a user with one SSH key.
func (e *cmdEnv) addUser(t *testing.T, name string) types.User {
	t.Helper()
	u, err := e.st.RegisterUser(name, name+"@example.org", "oidc-"+name, e.plan.Range())
	if err != nil {
		t.Fatal(err)
	}
	if _, err := e.st.AddSSHKey(u.ID, testOwnerKey, "SHA256:fp-"+name, "owner@laptop"); err != nil {
		t.Fatal(err)
	}
	return u
}

// addImage seeds an allowlist image with one fetched version.
func (e *cmdEnv) addImage(t *testing.T, name, checksum string) {
	t.Helper()
	if err := e.st.UpsertImage(types.Image{Name: name, URL: "https://example.test/" + name}); err != nil {
		t.Fatal(err)
	}
	err := e.st.AddImageVersion(types.ImageVersion{
		Checksum:  checksum,
		ImageName: name,
		Path:      "/var/lib/bento/images/sha256-" + checksum + ".qcow2",
		Size:      1,
		FetchedAt: time.Now().UTC(),
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := e.st.SetCurrentChecksum(name, checksum); err != nil {
		t.Fatal(err)
	}
}

// backendFor returns a CLI backend with the frontend key of the tests.
func (e *cmdEnv) backendFor(frontendKey string) *cliBackend {
	return &cliBackend{backend{m: e.mgr, st: e.st, hostID: e.host.ID, frontendKey: frontendKey}}
}
