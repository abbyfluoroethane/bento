package store

import (
	"errors"
	"fmt"
	"sync"
	"testing"
	"time"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// TestCreateInstanceQuotaLimits drives each of the four limits (SPEC 6.1)
// over the edge and checks the reported limit name.
func TestCreateInstanceQuotaLimits(t *testing.T) {
	quota := types.Quota{MaxInstances: 2, MaxVCPU: 4, MaxMemoryMiB: 4096, MaxDiskGiB: 50}
	base := func(n int, name string, owner types.User, host types.Host) types.Instance {
		inst := testInstance(n, name, owner, host)
		inst.VCPU = 1
		inst.MemoryMiB = 1024
		inst.DiskGiB = 10
		return inst
	}

	tests := []struct {
		name      string
		mutate    func(*types.Instance) // second instance tweak
		wantLimit string                // "" means the create succeeds
	}{
		{name: "fits", mutate: func(i *types.Instance) {}},
		{name: "vcpu", mutate: func(i *types.Instance) { i.VCPU = 4 }, wantLimit: "vcpu"},
		{name: "memory", mutate: func(i *types.Instance) { i.MemoryMiB = 4000 }, wantLimit: "memory"},
		{name: "disk", mutate: func(i *types.Instance) { i.DiskGiB = 41 }, wantLimit: "disk"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			s := newTestStore(t)
			owner, host := seedStore(t, s)
			quota.UserID = owner.ID
			if err := s.SetQuota(quota); err != nil {
				t.Fatal(err)
			}
			if err := s.CreateInstance(base(1, "first", owner, host), 0); err != nil {
				t.Fatalf("first create: %v", err)
			}
			second := base(2, "second", owner, host)
			tt.mutate(&second)
			err := s.CreateInstance(second, 0)
			if tt.wantLimit == "" {
				if err != nil {
					t.Fatalf("second create: %v", err)
				}
				return
			}
			var qErr *QuotaError
			if !errors.As(err, &qErr) {
				t.Fatalf("err = %v, want QuotaError", err)
			}
			if qErr.Limit != tt.wantLimit {
				t.Errorf("limit = %q, want %q", qErr.Limit, tt.wantLimit)
			}
		})
	}
}

func TestCreateInstanceCountLimit(t *testing.T) {
	s := newTestStore(t)
	owner, host := seedStore(t, s)
	if err := s.SetQuota(types.Quota{UserID: owner.ID, MaxInstances: 1,
		MaxVCPU: 100, MaxMemoryMiB: 1 << 20, MaxDiskGiB: 1 << 20}); err != nil {
		t.Fatal(err)
	}
	if err := s.CreateInstance(testInstance(1, "first", owner, host), 0); err != nil {
		t.Fatal(err)
	}
	err := s.CreateInstance(testInstance(2, "second", owner, host), 0)
	var qErr *QuotaError
	if !errors.As(err, &qErr) || qErr.Limit != "instances" {
		t.Errorf("err = %v, want QuotaError on instances", err)
	}
}

// TestCreateInstanceConcurrentQuota is the SPEC 6.1 race: many concurrent
// creates against a quota with room for one. Exactly one may pass, because
// the check and the insert share a transaction.
func TestCreateInstanceConcurrentQuota(t *testing.T) {
	s := newTestStore(t)
	owner, host := seedStore(t, s)
	if err := s.SetQuota(types.Quota{UserID: owner.ID, MaxInstances: 1,
		MaxVCPU: 100, MaxMemoryMiB: 1 << 20, MaxDiskGiB: 1 << 20}); err != nil {
		t.Fatal(err)
	}

	const workers = 8
	var (
		wg   sync.WaitGroup
		mu   sync.Mutex
		ok   int
		errs []error
	)
	for i := 0; i < workers; i++ {
		wg.Add(1)
		go func(n int) {
			defer wg.Done()
			err := s.CreateInstance(testInstance(n, fmt.Sprintf("racer-%d", n), owner, host), 0)
			mu.Lock()
			defer mu.Unlock()
			if err == nil {
				ok++
			} else {
				errs = append(errs, err)
			}
		}(i)
	}
	wg.Wait()

	if ok != 1 {
		t.Fatalf("%d concurrent creates passed the quota, want exactly 1", ok)
	}
	for _, err := range errs {
		var qErr *QuotaError
		if !errors.As(err, &qErr) {
			t.Errorf("loser got %v, want QuotaError", err)
		}
	}
	usage, err := s.UsageFor(owner.ID)
	if err != nil {
		t.Fatal(err)
	}
	if usage.Instances != 1 {
		t.Errorf("instances in table = %d, want 1", usage.Instances)
	}
}

func TestCreateInstanceNoQuotaRowIsUnlimited(t *testing.T) {
	s := newTestStore(t)
	owner, host := seedStore(t, s)
	for i := 0; i < 3; i++ {
		if err := s.CreateInstance(testInstance(i, fmt.Sprintf("inst-%d", i), owner, host), 0); err != nil {
			t.Fatalf("create %d without quota row: %v", i, err)
		}
	}
}

func TestInstanceRoundTrip(t *testing.T) {
	clock := newFakeClock()
	s := newTestStore(t, WithClock(clock.Now))
	owner, host := seedStore(t, s)

	want := testInstance(1, "web", owner, host)
	want.Nested = true
	want.KSM = false
	want.HTTPPort = 3000
	want.Visibility = types.VisibilityPublic
	if err := s.CreateInstance(want, 0); err != nil {
		t.Fatal(err)
	}

	for _, lookup := range []struct {
		name string
		get  func() (types.Instance, error)
	}{
		{"by uuid", func() (types.Instance, error) { return s.Instance(want.UUID) }},
		{"by name", func() (types.Instance, error) { return s.InstanceByName("web") }},
	} {
		t.Run(lookup.name, func(t *testing.T) {
			got, err := lookup.get()
			if err != nil {
				t.Fatal(err)
			}
			want.CreatedAt = clock.Now()
			if got != want {
				t.Errorf("got %+v\nwant %+v", got, want)
			}
		})
	}
}

func TestDeleteInstanceReleasesNameAndShares(t *testing.T) {
	s := newTestStore(t)
	owner, host := seedStore(t, s)
	other, err := s.RegisterUser("bob", "bob@example.org", "", testRange)
	if err != nil {
		t.Fatal(err)
	}

	inst := testInstance(1, "web", owner, host)
	if err := s.CreateInstance(inst, 0); err != nil {
		t.Fatal(err)
	}
	if err := s.AddShare(inst.UUID, other.ID); err != nil {
		t.Fatal(err)
	}

	deleted, err := s.DeleteInstance(inst.UUID)
	if err != nil {
		t.Fatal(err)
	}
	if deleted.UUID != inst.UUID || deleted.Name != "web" {
		t.Errorf("deleted = %+v, want the web instance", deleted)
	}

	if _, err := s.Instance(inst.UUID); !errors.Is(err, ErrNotFound) {
		t.Errorf("instance still present: %v", err)
	}
	if shares, _ := s.SharesFor(inst.UUID); len(shares) != 0 {
		t.Errorf("shares survive the delete: %+v (SPEC 7.2)", shares)
	}
	record, err := s.ReleasedName("web")
	if err != nil {
		t.Fatalf("name was not released: %v", err)
	}
	if record.PreviousOwnerID != owner.ID {
		t.Errorf("previous owner = %d, want %d", record.PreviousOwnerID, owner.ID)
	}

	if _, err := s.DeleteInstance("no-such-uuid"); !errors.Is(err, ErrNotFound) {
		t.Errorf("delete of missing uuid = %v, want ErrNotFound", err)
	}
}

func TestObservedStateBatchAndRestoreList(t *testing.T) {
	s := newTestStore(t)
	owner, host := seedStore(t, s)

	// a: left running, host reports stopped -> restore it.
	// b: user stopped it, host reports stopped -> leave it.
	// c: left running, host reports running -> leave it.
	for i, name := range []string{"a", "b", "c"} {
		if err := s.CreateInstance(testInstance(i, name, owner, host), 0); err != nil {
			t.Fatal(err)
		}
	}
	if err := s.SetDesiredState("uuid-001", types.DesiredStopped); err != nil {
		t.Fatal(err)
	}
	if err := s.UpdateObservedStates(map[string]types.State{
		"uuid-000": types.StateStopped,
		"uuid-001": types.StateStopped,
		"uuid-002": types.StateRunning,
		"unknown":  types.StateRunning, // not in the table; skipped
	}); err != nil {
		t.Fatal(err)
	}

	restore, err := s.InstancesToRestore()
	if err != nil {
		t.Fatal(err)
	}
	if len(restore) != 1 || restore[0].UUID != "uuid-000" {
		t.Errorf("InstancesToRestore = %+v, want only uuid-000", restore)
	}

	c, err := s.Instance("uuid-002")
	if err != nil {
		t.Fatal(err)
	}
	if c.State != types.StateRunning {
		t.Errorf("uuid-002 state = %s, want running", c.State)
	}
}

func TestListingsAndSetters(t *testing.T) {
	s := newTestStore(t)
	owner, host := seedStore(t, s)
	other, err := s.RegisterUser("bob", "bob@example.org", "", testRange)
	if err != nil {
		t.Fatal(err)
	}

	mine := testInstance(1, "mine", owner, host)
	theirs := testInstance(2, "theirs", other, host)
	for _, inst := range []types.Instance{mine, theirs} {
		if err := s.CreateInstance(inst, 0); err != nil {
			t.Fatal(err)
		}
	}

	all, err := s.Instances()
	if err != nil {
		t.Fatal(err)
	}
	if len(all) != 2 {
		t.Errorf("Instances() = %d rows, want 2", len(all))
	}
	byOwner, err := s.InstancesByOwner(owner.ID)
	if err != nil {
		t.Fatal(err)
	}
	if len(byOwner) != 1 || byOwner[0].UUID != mine.UUID {
		t.Errorf("InstancesByOwner = %+v, want only %s", byOwner, mine.UUID)
	}

	if err := s.SetVisibility(mine.UUID, types.VisibilityPrivate); err != nil {
		t.Fatal(err)
	}
	if err := s.SetHTTPPort(mine.UUID, 8080); err != nil {
		t.Fatal(err)
	}
	got, err := s.Instance(mine.UUID)
	if err != nil {
		t.Fatal(err)
	}
	if got.Visibility != types.VisibilityPrivate || got.HTTPPort != 8080 {
		t.Errorf("after setters: visibility=%s port=%d", got.Visibility, got.HTTPPort)
	}

	if err := s.SetVisibility("no-such", types.VisibilityOff); !errors.Is(err, ErrNotFound) {
		t.Errorf("setter on missing uuid = %v, want ErrNotFound", err)
	}
}

func TestTouchLastSeen(t *testing.T) {
	clock := newFakeClock()
	s := newTestStore(t, WithClock(clock.Now))
	owner, host := seedStore(t, s)
	inst := testInstance(1, "web", owner, host)
	if err := s.CreateInstance(inst, 0); err != nil {
		t.Fatal(err)
	}

	got, err := s.Instance(inst.UUID)
	if err != nil {
		t.Fatal(err)
	}
	if !got.LastSeenAt.IsZero() {
		t.Errorf("last seen before any touch = %s, want zero", got.LastSeenAt)
	}

	clock.Advance(90 * time.Minute)
	if err := s.TouchLastSeen(inst.UUID); err != nil {
		t.Fatal(err)
	}
	got, err = s.Instance(inst.UUID)
	if err != nil {
		t.Fatal(err)
	}
	if !got.LastSeenAt.Equal(clock.Now()) {
		t.Errorf("last seen = %s, want %s", got.LastSeenAt, clock.Now())
	}
}

func TestResizeQuota(t *testing.T) {
	s := newTestStore(t)
	owner, host := seedStore(t, s)
	if err := s.SetQuota(types.Quota{UserID: owner.ID, MaxInstances: 2,
		MaxVCPU: 4, MaxMemoryMiB: 4096, MaxDiskGiB: 40}); err != nil {
		t.Fatal(err)
	}
	inst := testInstance(1, "web", owner, host)
	inst.VCPU = 2
	inst.MemoryMiB = 2048
	inst.DiskGiB = 20
	if err := s.CreateInstance(inst, 0); err != nil {
		t.Fatal(err)
	}

	// Growing within the quota works: the instance's own use is excluded.
	if err := s.Resize(inst.UUID, 4, 4096, 40, true); err != nil {
		t.Fatalf("resize to the limit: %v", err)
	}
	got, err := s.Instance(inst.UUID)
	if err != nil {
		t.Fatal(err)
	}
	if got.VCPU != 4 || got.MemoryMiB != 4096 || got.DiskGiB != 40 || !got.Nested {
		t.Errorf("after resize: %+v", got)
	}

	// Growing past the quota fails.
	err = s.Resize(inst.UUID, 5, 4096, 40, true)
	var qErr *QuotaError
	if !errors.As(err, &qErr) || qErr.Limit != "vcpu" {
		t.Errorf("resize past quota = %v, want QuotaError on vcpu", err)
	}
}
