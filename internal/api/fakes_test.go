package api

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"os"

	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// fakeStore is an in-memory Store for handler tests.
type fakeStore struct {
	users     map[int64]types.User
	quotas    map[int64]types.Quota
	instances map[string]types.Instance
	shares    map[string][]types.Share
	keys      map[int64][]types.SSHKey
	images    []types.Image
	nextKeyID int64
	dumpBytes []byte
	err       error // when set, every method fails with it
}

func newFakeStore() *fakeStore {
	return &fakeStore{
		users:     map[int64]types.User{},
		quotas:    map[int64]types.Quota{},
		instances: map[string]types.Instance{},
		shares:    map[string][]types.Share{},
		keys:      map[int64][]types.SSHKey{},
		dumpBytes: []byte("SQLite format 3\x00fake"),
	}
}

func (f *fakeStore) UserByID(id int64) (types.User, error) {
	if f.err != nil {
		return types.User{}, f.err
	}
	u, ok := f.users[id]
	if !ok {
		return types.User{}, store.ErrNotFound
	}
	return u, nil
}

func (f *fakeStore) UserByName(name string) (types.User, error) {
	if f.err != nil {
		return types.User{}, f.err
	}
	for _, u := range f.users {
		if u.Name == name {
			return u, nil
		}
	}
	return types.User{}, store.ErrNotFound
}

func (f *fakeStore) QuotaFor(userID int64) (types.Quota, error) {
	if f.err != nil {
		return types.Quota{}, f.err
	}
	q, ok := f.quotas[userID]
	if !ok {
		return types.Quota{}, store.ErrNotFound
	}
	return q, nil
}

func (f *fakeStore) UsageFor(userID int64) (store.Usage, error) {
	if f.err != nil {
		return store.Usage{}, f.err
	}
	var u store.Usage
	for _, inst := range f.instances {
		if inst.OwnerID == userID {
			u.Instances++
			u.VCPU += int64(inst.VCPU)
			u.MemoryMiB += inst.MemoryMiB
			u.DiskGiB += inst.DiskGiB
		}
	}
	return u, nil
}

func (f *fakeStore) Instance(uuid string) (types.Instance, error) {
	if f.err != nil {
		return types.Instance{}, f.err
	}
	inst, ok := f.instances[uuid]
	if !ok {
		return types.Instance{}, store.ErrNotFound
	}
	return inst, nil
}

func (f *fakeStore) InstancesByOwner(ownerID int64) ([]types.Instance, error) {
	if f.err != nil {
		return nil, f.err
	}
	var out []types.Instance
	for _, inst := range f.instances {
		if inst.OwnerID == ownerID {
			out = append(out, inst)
		}
	}
	return out, nil
}

func (f *fakeStore) InstancesSharedWith(userID int64) ([]types.Instance, error) {
	if f.err != nil {
		return nil, f.err
	}
	var out []types.Instance
	for uuid, shares := range f.shares {
		for _, sh := range shares {
			if sh.UserID == userID {
				out = append(out, f.instances[uuid])
			}
		}
	}
	return out, nil
}

func (f *fakeStore) Instances() ([]types.Instance, error) {
	if f.err != nil {
		return nil, f.err
	}
	var out []types.Instance
	for _, inst := range f.instances {
		out = append(out, inst)
	}
	return out, nil
}

func (f *fakeStore) SetVisibility(uuid string, v types.Visibility) error {
	if f.err != nil {
		return f.err
	}
	inst, ok := f.instances[uuid]
	if !ok {
		return store.ErrNotFound
	}
	inst.Visibility = v
	f.instances[uuid] = inst
	return nil
}

func (f *fakeStore) AddShare(uuid string, userID int64) error {
	if f.err != nil {
		return f.err
	}
	f.shares[uuid] = append(f.shares[uuid], types.Share{InstanceUUID: uuid, UserID: userID})
	return nil
}

func (f *fakeStore) RemoveShare(uuid string, userID int64) error {
	if f.err != nil {
		return f.err
	}
	kept := f.shares[uuid][:0]
	for _, sh := range f.shares[uuid] {
		if sh.UserID != userID {
			kept = append(kept, sh)
		}
	}
	f.shares[uuid] = kept
	return nil
}

func (f *fakeStore) SharesFor(uuid string) ([]types.Share, error) {
	if f.err != nil {
		return nil, f.err
	}
	return f.shares[uuid], nil
}

func (f *fakeStore) Images() ([]types.Image, error) {
	if f.err != nil {
		return nil, f.err
	}
	return f.images, nil
}

func (f *fakeStore) AddSSHKey(userID int64, publicKey, fingerprint, comment string) (int64, error) {
	if f.err != nil {
		return 0, f.err
	}
	f.nextKeyID++
	f.keys[userID] = append(f.keys[userID], types.SSHKey{
		ID: f.nextKeyID, UserID: userID,
		PublicKey: publicKey, Fingerprint: fingerprint, Comment: comment,
	})
	return f.nextKeyID, nil
}

func (f *fakeStore) SSHKeysForUser(userID int64) ([]types.SSHKey, error) {
	if f.err != nil {
		return nil, f.err
	}
	return f.keys[userID], nil
}

func (f *fakeStore) DeleteSSHKey(userID, keyID int64) error {
	if f.err != nil {
		return f.err
	}
	kept := f.keys[userID][:0]
	found := false
	for _, k := range f.keys[userID] {
		if k.ID == keyID {
			found = true
			continue
		}
		kept = append(kept, k)
	}
	f.keys[userID] = kept
	if !found {
		return store.ErrNotFound
	}
	return nil
}

func (f *fakeStore) DumpDB(destPath string) error {
	if f.err != nil {
		return f.err
	}
	return os.WriteFile(destPath, f.dumpBytes, 0o600)
}

// fakeLifecycle records every call and mutates the fake store the way the
// real lifecycle mutates the real one.
type fakeLifecycle struct {
	st    *fakeStore
	calls []string
	err   error // when set, every method fails with it
}

func (f *fakeLifecycle) record(format string, args ...any) {
	f.calls = append(f.calls, fmt.Sprintf(format, args...))
}

func (f *fakeLifecycle) Create(_ context.Context, owner types.User, spec CreateSpec) (types.Instance, error) {
	f.record("create %s", spec.Name)
	if f.err != nil {
		return types.Instance{}, f.err
	}
	inst := types.Instance{
		UUID: "uuid-" + spec.Name, Name: spec.Name, OwnerID: owner.ID,
		ImageName: spec.Image, State: types.StateStarting,
		DesiredState: types.DesiredRunning,
		VCPU:         spec.VCPU, MemoryMiB: spec.MemoryMiB, DiskGiB: spec.DiskGiB,
		Nested: spec.Nested, KSM: spec.KSM,
		HTTPPort: 80, Visibility: types.VisibilityOff,
	}
	f.st.instances[inst.UUID] = inst
	return inst, nil
}

func (f *fakeLifecycle) Delete(_ context.Context, uuid string) error {
	f.record("delete %s", uuid)
	if f.err != nil {
		return f.err
	}
	delete(f.st.instances, uuid)
	delete(f.st.shares, uuid)
	return nil
}

func (f *fakeLifecycle) Start(_ context.Context, uuid string) error {
	f.record("start %s", uuid)
	return f.err
}

func (f *fakeLifecycle) Stop(_ context.Context, uuid string) error {
	f.record("stop %s", uuid)
	return f.err
}

func (f *fakeLifecycle) Restart(_ context.Context, uuid string) error {
	f.record("restart %s", uuid)
	return f.err
}

func (f *fakeLifecycle) Rename(_ context.Context, uuid, newName string) error {
	f.record("rename %s %s", uuid, newName)
	if f.err != nil {
		return f.err
	}
	inst := f.st.instances[uuid]
	inst.Name = newName
	f.st.instances[uuid] = inst
	return nil
}

func (f *fakeLifecycle) Resize(_ context.Context, uuid string, spec ResizeSpec) error {
	f.record("resize %s vcpu=%d mem=%d disk=%d nested=%t",
		uuid, spec.VCPU, spec.MemoryMiB, spec.DiskGiB, spec.Nested)
	if f.err != nil {
		return f.err
	}
	inst := f.st.instances[uuid]
	inst.VCPU, inst.MemoryMiB, inst.DiskGiB, inst.Nested =
		spec.VCPU, spec.MemoryMiB, spec.DiskGiB, spec.Nested
	f.st.instances[uuid] = inst
	return nil
}

func (f *fakeLifecycle) SetHTTPPort(_ context.Context, uuid string, port int) error {
	f.record("port %s %d", uuid, port)
	if f.err != nil {
		return f.err
	}
	inst := f.st.instances[uuid]
	inst.HTTPPort = port
	f.st.instances[uuid] = inst
	return nil
}

func (f *fakeLifecycle) SetVisibility(_ context.Context, uuid string, v types.Visibility) error {
	f.record("visibility %s %s", uuid, v)
	if f.err != nil {
		return f.err
	}
	return f.st.SetVisibility(uuid, v)
}

// fakeAuth authenticates every request as a fixed user, or rejects all
// requests when the user is nil.
type fakeAuth struct {
	user *types.User
}

func (f *fakeAuth) UserFromRequest(*http.Request) (types.User, error) {
	if f.user == nil {
		return types.User{}, errors.New("no session")
	}
	return *f.user, nil
}
