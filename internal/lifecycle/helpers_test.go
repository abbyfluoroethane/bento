package lifecycle

import (
	"bytes"
	"context"
	"encoding/xml"
	"fmt"
	"log/slog"
	"os"
	"sync"
	"testing"
	"time"

	"github.com/abbyfluoroethane/bento/internal/cloudinit"
	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/network"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// fakeStore is an in-memory Store that records every mutation.
type fakeStore struct {
	mu        sync.Mutex
	instances map[string]types.Instance
	order     []string
	users     map[int64]types.User
	images    map[string]types.Image
	mutations []string
	released  []string

	createErr error
	resizeErr error
	renameErr error
}

func newFakeStore() *fakeStore {
	return &fakeStore{
		instances: map[string]types.Instance{},
		users:     map[int64]types.User{},
		images:    map[string]types.Image{},
	}
}

func (s *fakeStore) mutate(what string) {
	s.mutations = append(s.mutations, what)
}

func (s *fakeStore) CreateInstance(inst types.Instance, _ time.Duration) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.mutate("create " + inst.Name)
	if s.createErr != nil {
		return s.createErr
	}
	if _, ok := s.instances[inst.UUID]; ok {
		return fmt.Errorf("fake store: uuid %s exists", inst.UUID)
	}
	s.instances[inst.UUID] = inst
	s.order = append(s.order, inst.UUID)
	return nil
}

func (s *fakeStore) DeleteInstance(uuid string) (types.Instance, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.mutate("delete " + uuid)
	inst, ok := s.instances[uuid]
	if !ok {
		return types.Instance{}, fmt.Errorf("fake store: no instance %s", uuid)
	}
	delete(s.instances, uuid)
	for i, u := range s.order {
		if u == uuid {
			s.order = append(s.order[:i], s.order[i+1:]...)
			break
		}
	}
	s.released = append(s.released, inst.Name)
	return inst, nil
}

func (s *fakeStore) Instance(uuid string) (types.Instance, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	inst, ok := s.instances[uuid]
	if !ok {
		return types.Instance{}, fmt.Errorf("fake store: no instance %s", uuid)
	}
	return inst, nil
}

func (s *fakeStore) Instances() ([]types.Instance, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]types.Instance, 0, len(s.order))
	for _, uuid := range s.order {
		out = append(out, s.instances[uuid])
	}
	return out, nil
}

func (s *fakeStore) InstancesToRestore() ([]types.Instance, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	var out []types.Instance
	for _, uuid := range s.order {
		inst := s.instances[uuid]
		if inst.DesiredState == types.DesiredRunning && inst.State == types.StateStopped {
			out = append(out, inst)
		}
	}
	return out, nil
}

func (s *fakeStore) Image(name string) (types.Image, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	img, ok := s.images[name]
	if !ok {
		return types.Image{}, fmt.Errorf("fake store: no image %s", name)
	}
	return img, nil
}

func (s *fakeStore) UserByID(id int64) (types.User, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	u, ok := s.users[id]
	if !ok {
		return types.User{}, fmt.Errorf("fake store: no user %d", id)
	}
	return u, nil
}

func (s *fakeStore) RenameInstance(uuid, newName string, _ time.Duration) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.mutate(fmt.Sprintf("rename %s %s", uuid, newName))
	if s.renameErr != nil {
		return s.renameErr
	}
	inst, ok := s.instances[uuid]
	if !ok {
		return fmt.Errorf("fake store: no instance %s", uuid)
	}
	for _, other := range s.instances {
		if other.UUID != uuid && other.Name == newName {
			return fmt.Errorf("fake store: name %s taken", newName)
		}
	}
	s.released = append(s.released, inst.Name)
	inst.Name = newName
	s.instances[uuid] = inst
	return nil
}

func (s *fakeStore) Resize(uuid string, vcpu int, memoryMiB, diskGiB int64, nested bool) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.mutate(fmt.Sprintf("resize %s %d %d %d %t", uuid, vcpu, memoryMiB, diskGiB, nested))
	if s.resizeErr != nil {
		return s.resizeErr
	}
	inst, ok := s.instances[uuid]
	if !ok {
		return fmt.Errorf("fake store: no instance %s", uuid)
	}
	inst.VCPU, inst.MemoryMiB, inst.DiskGiB, inst.Nested = vcpu, memoryMiB, diskGiB, nested
	s.instances[uuid] = inst
	return nil
}

func (s *fakeStore) SetDesiredState(uuid string, state types.DesiredState) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.mutate(fmt.Sprintf("desired %s %s", uuid, state))
	inst, ok := s.instances[uuid]
	if !ok {
		return fmt.Errorf("fake store: no instance %s", uuid)
	}
	inst.DesiredState = state
	s.instances[uuid] = inst
	return nil
}

func (s *fakeStore) SetObservedState(uuid string, state types.State) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.mutate(fmt.Sprintf("observed %s %s", uuid, state))
	inst, ok := s.instances[uuid]
	if !ok {
		return fmt.Errorf("fake store: no instance %s", uuid)
	}
	inst.State = state
	s.instances[uuid] = inst
	return nil
}

func (s *fakeStore) UpdateObservedStates(states map[string]types.State) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.mutate(fmt.Sprintf("observed-batch %d", len(states)))
	for uuid, state := range states {
		inst, ok := s.instances[uuid]
		if !ok {
			continue // skipped, like the real store
		}
		inst.State = state
		s.instances[uuid] = inst
	}
	return nil
}

// fakeImages records overlay creations and creates the file, so file
// deletion paths are observable.
type fakeImages struct {
	calls []string
	err   error
}

func (f *fakeImages) CreateOverlay(_ context.Context, checksum, overlayPath string, diskGiB int64) error {
	f.calls = append(f.calls, fmt.Sprintf("%s %s %d", checksum, overlayPath, diskGiB))
	if f.err != nil {
		return f.err
	}
	return os.WriteFile(overlayPath, []byte("overlay"), 0o600)
}

// fakeISO records built seeds in memory; no file is written.
type fakeISO struct {
	seeds map[string]cloudinit.Seed // by iso path
	err   error
}

func newFakeISO() *fakeISO { return &fakeISO{seeds: map[string]cloudinit.Seed{}} }

func (f *fakeISO) Build(_ context.Context, seed cloudinit.Seed, isoPath string) error {
	if f.err != nil {
		return f.err
	}
	f.seeds[isoPath] = seed
	return nil
}

func (f *fakeISO) exists(isoPath string) bool {
	_, ok := f.seeds[isoPath]
	return ok
}

func (f *fakeISO) delete(isoPath string) error {
	delete(f.seeds, isoPath)
	return nil
}

// fakeResizer records overlay resizes.
type fakeResizer struct {
	calls []string
	err   error
}

func (f *fakeResizer) ResizeOverlay(_ context.Context, overlayPath string, diskGiB int64) error {
	f.calls = append(f.calls, fmt.Sprintf("%s %d", overlayPath, diskGiB))
	return f.err
}

// definerFake extends the hypervisor fake with the Definer capability.
type definerFake struct {
	*hypervisor.Fake
	defines []string
}

func (d *definerFake) Define(_ context.Context, domXML string) error {
	d.defines = append(d.defines, domXML)
	var parsed struct {
		Name string `xml:"name"`
	}
	if err := xml.Unmarshal([]byte(domXML), &parsed); err != nil {
		return err
	}
	if dom := d.Domain(parsed.Name); dom != nil {
		updated := *dom
		updated.XML = domXML
		d.SetDomain(updated)
	}
	return nil
}

// autostartFake extends the hypervisor fake with the AutostartClearer
// capability.
type autostartFake struct {
	*hypervisor.Fake
	cleared []string
}

func (a *autostartFake) ClearAutostart(_ context.Context, name string) error {
	a.cleared = append(a.cleared, name)
	return nil
}

// env bundles a Manager with all its fakes.
type env struct {
	m       *Manager
	hyp     hypervisor.Hypervisor
	fake    *hypervisor.Fake
	store   *fakeStore
	images  *fakeImages
	iso     *fakeISO
	resizer *fakeResizer
	log     *bytes.Buffer
	sleeps  []time.Duration
	uuids   []string
}

// newEnv builds a Manager over fakes. hyp may wrap e.fake with extra
// capabilities; nil means use the plain fake.
func newEnv(t *testing.T, wrap func(*hypervisor.Fake) hypervisor.Hypervisor, tweak func(*Config)) *env {
	t.Helper()
	plan, err := network.NewPlan("10.77.0.0/16")
	if err != nil {
		t.Fatal(err)
	}
	e := &env{
		fake:    &hypervisor.Fake{},
		store:   newFakeStore(),
		images:  &fakeImages{},
		iso:     newFakeISO(),
		resizer: &fakeResizer{},
		log:     &bytes.Buffer{},
	}
	e.hyp = e.fake
	if wrap != nil {
		e.hyp = wrap(e.fake)
	}
	next := 0
	cfg := Config{
		Hypervisor: e.hyp,
		Store:      e.store,
		Images:     e.images,
		ISO:        e.iso,
		Resizer:    e.resizer,
		Plan:       plan,
		StorageDir: t.TempDir(),
		Logger:     slog.New(slog.NewTextHandler(e.log, nil)),
		NestedEnabled: func() (bool, string) {
			return false, "kvm_intel nested is N"
		},
		Sleep: func(d time.Duration) { e.sleeps = append(e.sleeps, d) },
		NewUUID: func() string {
			next++
			return fmt.Sprintf("%08d-1111-4111-8111-111111111111", next)
		},
		Now:       func() time.Time { return time.Date(2026, 8, 10, 12, 0, 0, 0, time.UTC) },
		DeleteISO: e.iso.delete,
		ISOExists: e.iso.exists,
	}
	if tweak != nil {
		tweak(&cfg)
	}
	m, err := NewManager(cfg)
	if err != nil {
		t.Fatal(err)
	}
	e.m = m
	e.uuids = nil
	return e
}

// addUser seeds a user with the nth /24 of the test range.
func (e *env) addUser(t *testing.T, id int64, name string, subnetIndex int) types.User {
	t.Helper()
	u := types.User{
		ID:     id,
		Name:   name,
		Subnet: fmt.Sprintf("10.77.%d.0/24", subnetIndex),
	}
	e.store.users[id] = u
	return u
}

// addImage seeds an allowlist image with a current version.
func (e *env) addImage(name, checksum string) types.Image {
	img := types.Image{Name: name, URL: "https://example.test/" + name, CurrentChecksum: checksum}
	e.store.images[name] = img
	return img
}

// create runs New for a standard request and fails the test on error.
func (e *env) create(t *testing.T, owner types.User, name string) types.Instance {
	t.Helper()
	inst, err := e.m.New(context.Background(), NewRequest{
		Name:      name,
		Owner:     owner,
		HostID:    1,
		SSHKeys:   []string{"ssh-ed25519 AAAA test@key"},
		ImageName: "debian-13",
		VCPU:      2,
		MemoryMiB: 2048,
		DiskGiB:   20,
	})
	if err != nil {
		t.Fatalf("New(%s): %v", name, err)
	}
	return inst
}
