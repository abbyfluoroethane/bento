package cli

import (
	"context"
	"io"
	"strings"
	"testing"
	"time"

	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// fakeStore implements the cli.Store interface in memory.
type fakeStore struct {
	users     map[int64]types.User
	quota     *types.Quota
	usage     store.Usage
	instances []types.Instance
	shared    []types.Instance // shared with the invoking user
	access    map[string][]int64
	shares    map[string][]types.Share
	keys      []types.SSHKey
	images    []types.Image

	err error // when set, every store call fails with it

	addedShares [][2]any // uuid, userID
	removed     [][2]any
	addedKeys   []types.SSHKey
	deletedKeys []int64
}

func (f *fakeStore) UserByID(id int64) (types.User, error) {
	if f.err != nil {
		return types.User{}, f.err
	}
	if u, ok := f.users[id]; ok {
		return u, nil
	}
	return types.User{}, store.ErrNotFound
}

func (f *fakeStore) UserByName(name string) (types.User, error) {
	for _, u := range f.users {
		if u.Name == name {
			return u, nil
		}
	}
	return types.User{}, store.ErrNotFound
}

func (f *fakeStore) QuotaFor(int64) (types.Quota, error) {
	if f.quota == nil {
		return types.Quota{}, store.ErrNotFound
	}
	return *f.quota, nil
}

func (f *fakeStore) UsageFor(int64) (store.Usage, error) { return f.usage, f.err }

func (f *fakeStore) InstanceByName(name string) (types.Instance, error) {
	for _, inst := range f.instances {
		if inst.Name == name {
			return inst, nil
		}
	}
	return types.Instance{}, store.ErrNotFound
}

func (f *fakeStore) InstancesByOwner(ownerID int64) ([]types.Instance, error) {
	var out []types.Instance
	for _, inst := range f.instances {
		if inst.OwnerID == ownerID {
			out = append(out, inst)
		}
	}
	return out, nil
}

func (f *fakeStore) InstancesSharedWith(int64) ([]types.Instance, error) { return f.shared, nil }
func (f *fakeStore) Instances() ([]types.Instance, error)                { return f.instances, nil }

func (f *fakeStore) HasAccess(uuid string, userID int64) (bool, error) {
	for _, inst := range f.instances {
		if inst.UUID == uuid && inst.OwnerID == userID {
			return true, nil
		}
	}
	for _, id := range f.access[uuid] {
		if id == userID {
			return true, nil
		}
	}
	return false, nil
}

func (f *fakeStore) AddShare(uuid string, userID int64) error {
	f.addedShares = append(f.addedShares, [2]any{uuid, userID})
	return f.err
}

func (f *fakeStore) RemoveShare(uuid string, userID int64) error {
	for _, sh := range f.shares[uuid] {
		if sh.UserID == userID {
			f.removed = append(f.removed, [2]any{uuid, userID})
			return nil
		}
	}
	return store.ErrNotFound
}

func (f *fakeStore) SharesFor(uuid string) ([]types.Share, error) { return f.shares[uuid], nil }

func (f *fakeStore) AddSSHKey(userID int64, publicKey, fingerprint, comment string) (int64, error) {
	if f.err != nil {
		return 0, f.err
	}
	f.addedKeys = append(f.addedKeys, types.SSHKey{
		UserID: userID, PublicKey: publicKey, Fingerprint: fingerprint, Comment: comment,
	})
	return int64(len(f.addedKeys)), nil
}

func (f *fakeStore) SSHKeysForUser(int64) ([]types.SSHKey, error) { return f.keys, nil }

func (f *fakeStore) DeleteSSHKey(_, keyID int64) error {
	for _, k := range f.keys {
		if k.ID == keyID {
			f.deletedKeys = append(f.deletedKeys, keyID)
			return nil
		}
	}
	return store.ErrNotFound
}

func (f *fakeStore) Images() ([]types.Image, error) { return f.images, nil }

// fakeLifecycle records lifecycle calls and returns configured results.
type fakeLifecycle struct {
	err        error // returned by every action when set
	stopResult hypervisor.StopResult

	created   []CreateRequest
	started   []string
	stopped   []string
	restarted []string
	removed   []string
	renamed   [][2]string
	copied    []CreateRequest
	resized   []ResizeRequest
	consoled  []string

	setPortUUID string
	setPort     int
	setVisUUID  string
	setVis      types.Visibility
}

func (f *fakeLifecycle) Create(_ context.Context, req CreateRequest) (types.Instance, error) {
	if f.err != nil {
		return types.Instance{}, f.err
	}
	f.created = append(f.created, req)
	return types.Instance{
		UUID: "uuid-" + req.Name, Name: req.Name, OwnerID: req.OwnerID,
		ImageName: req.Image, VCPU: req.VCPU, MemoryMiB: req.MemoryMiB,
		DiskGiB: req.DiskGiB, Address: "10.100.0.2",
	}, nil
}

func (f *fakeLifecycle) Start(_ context.Context, inst types.Instance) error {
	f.started = append(f.started, inst.Name)
	return f.err
}

func (f *fakeLifecycle) Stop(_ context.Context, inst types.Instance) (hypervisor.StopResult, error) {
	if f.err != nil {
		return "", f.err
	}
	f.stopped = append(f.stopped, inst.Name)
	return f.stopResult, nil
}

func (f *fakeLifecycle) Restart(_ context.Context, inst types.Instance) error {
	f.restarted = append(f.restarted, inst.Name)
	return f.err
}

func (f *fakeLifecycle) Remove(_ context.Context, inst types.Instance) error {
	if f.err != nil {
		return f.err
	}
	f.removed = append(f.removed, inst.Name)
	return nil
}

func (f *fakeLifecycle) Rename(_ context.Context, inst types.Instance, newName string) error {
	if f.err != nil {
		return f.err
	}
	f.renamed = append(f.renamed, [2]string{inst.Name, newName})
	return nil
}

func (f *fakeLifecycle) Copy(_ context.Context, _ types.Instance, req CreateRequest) (types.Instance, error) {
	if f.err != nil {
		return types.Instance{}, f.err
	}
	f.copied = append(f.copied, req)
	return types.Instance{Name: req.Name, Address: "10.100.0.3"}, nil
}

func (f *fakeLifecycle) Resize(_ context.Context, _ types.Instance, req ResizeRequest) error {
	if f.err != nil {
		return f.err
	}
	f.resized = append(f.resized, req)
	return nil
}

func (f *fakeLifecycle) Console(_ context.Context, inst types.Instance, _ io.ReadWriter) error {
	f.consoled = append(f.consoled, inst.Name)
	return f.err
}

func (f *fakeLifecycle) SetHTTPPort(_ context.Context, inst types.Instance, port int) error {
	if f.err != nil {
		return f.err
	}
	f.setPortUUID, f.setPort = inst.UUID, port
	return nil
}

func (f *fakeLifecycle) SetVisibility(_ context.Context, inst types.Instance, v types.Visibility) error {
	if f.err != nil {
		return f.err
	}
	f.setVisUUID, f.setVis = inst.UUID, v
	return nil
}

// Test fixture: user alice (id 1) owns "web" (running) and "db"
// (stopped); user bob (id 2) owns "theirs" and shares it with alice.
var (
	alice = types.User{ID: 1, Name: "alice", Email: "alice@example.com", Subnet: "10.100.0.0/24"}
	bob   = types.User{ID: 2, Name: "bob", Email: "bob@example.com", Subnet: "10.100.1.0/24"}
)

func testTime() time.Time { return time.Date(2026, 8, 10, 12, 0, 0, 0, time.UTC) }

func newFixture() (*fakeStore, *fakeLifecycle, *CLI) {
	st := &fakeStore{
		users: map[int64]types.User{1: alice, 2: bob},
		instances: []types.Instance{
			{
				UUID: "uuid-web", Name: "web", OwnerID: 1, ImageName: "debian-13",
				BaseChecksum: "aaa", State: types.StateRunning, Address: "10.100.0.2",
				VCPU: 2, MemoryMiB: 2048, DiskGiB: 20, KSM: true,
				Visibility: types.VisibilityOff,
				LastSeenAt: testTime().Add(-3 * time.Hour),
			},
			{
				UUID: "uuid-db", Name: "db", OwnerID: 1, ImageName: "debian-13",
				BaseChecksum: "bbb", State: types.StateStopped, Address: "10.100.0.3",
				VCPU: 4, MemoryMiB: 4096, DiskGiB: 40, KSM: true,
				Visibility: types.VisibilityPublic,
			},
			{
				UUID: "uuid-theirs", Name: "theirs", OwnerID: 2, ImageName: "debian-13",
				BaseChecksum: "aaa", State: types.StateStopped, Address: "10.100.1.2",
				VCPU: 1, MemoryMiB: 1024, DiskGiB: 10, KSM: true,
				Visibility: types.VisibilityOff,
			},
		},
		access: map[string][]int64{"uuid-theirs": {1}},
		quota:  &types.Quota{UserID: 1, MaxInstances: 4, MaxVCPU: 8, MaxMemoryMiB: 8192, MaxDiskGiB: 100},
		usage:  store.Usage{Instances: 2, VCPU: 6, MemoryMiB: 6144, DiskGiB: 60},
	}
	lc := &fakeLifecycle{stopResult: hypervisor.StopGraceful}
	c := New(st, lc, Options{
		Domain:       "bento.example.org",
		DefaultImage: "debian-13",
		NameCooldown: 24 * time.Hour,
		Now:          testTime,
	})
	return st, lc, c
}

// run executes one command for user and returns exit code, stdout, and
// stderr.
func run(t *testing.T, c *CLI, user types.User, stdin string, args ...string) (int, string, string) {
	t.Helper()
	var out, errOut strings.Builder
	code := c.Run(context.Background(), user, args, strings.NewReader(stdin), &out, &errOut)
	return code, out.String(), errOut.String()
}
