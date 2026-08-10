package store

import (
	"errors"
	"net/netip"
	"testing"

	"github.com/abbyfluoroethane/bento/internal/types"
)

func TestRegisterUserAllocatesSequentialSubnets(t *testing.T) {
	s := newTestStore(t)

	want := []string{"10.100.0.0/24", "10.100.1.0/24", "10.100.2.0/24"}
	for i, name := range []string{"alice", "bob", "carol"} {
		user, err := s.RegisterUser(name, name+"@example.org", "", testRange)
		if err != nil {
			t.Fatalf("RegisterUser(%s): %v", name, err)
		}
		if user.Subnet != want[i] {
			t.Errorf("user %s subnet = %s, want %s", name, user.Subnet, want[i])
		}
	}
}

func TestRegisterUserReusesFreedSubnet(t *testing.T) {
	s := newTestStore(t)
	for _, name := range []string{"alice", "bob", "carol"} {
		if _, err := s.RegisterUser(name, name+"@example.org", "", testRange); err != nil {
			t.Fatal(err)
		}
	}
	// Free the middle /24; the next registration takes the lowest gap.
	if _, err := s.db.Exec(`DELETE FROM users WHERE name = 'bob'`); err != nil {
		t.Fatal(err)
	}
	user, err := s.RegisterUser("dave", "dave@example.org", "", testRange)
	if err != nil {
		t.Fatal(err)
	}
	if user.Subnet != "10.100.1.0/24" {
		t.Errorf("subnet = %s, want the freed 10.100.1.0/24", user.Subnet)
	}
}

func TestRegisterUserExhaustsRange(t *testing.T) {
	s := newTestStore(t)
	tiny := netip.MustParsePrefix("10.200.0.0/23") // room for two /24s
	for _, name := range []string{"alice", "bob"} {
		if _, err := s.RegisterUser(name, name+"@example.org", "", tiny); err != nil {
			t.Fatalf("RegisterUser(%s): %v", name, err)
		}
	}
	if _, err := s.RegisterUser("carol", "carol@example.org", "", tiny); !errors.Is(err, ErrSubnetsExhausted) {
		t.Errorf("err = %v, want ErrSubnetsExhausted", err)
	}
}

func TestUserLookups(t *testing.T) {
	s := newTestStore(t)
	created, err := s.RegisterUser("alice", "alice@example.org", "oidc-alice", testRange)
	if err != nil {
		t.Fatal(err)
	}

	tests := []struct {
		name   string
		lookup func() (types.User, error)
	}{
		{"by id", func() (types.User, error) { return s.UserByID(created.ID) }},
		{"by name", func() (types.User, error) { return s.UserByName("alice") }},
		{"by oidc subject", func() (types.User, error) { return s.UserByOIDCSubject("oidc-alice") }},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			user, err := tt.lookup()
			if err != nil {
				t.Fatal(err)
			}
			if user.ID != created.ID || user.Name != "alice" ||
				user.Email != "alice@example.org" || user.OIDCSubject != "oidc-alice" ||
				user.Subnet != created.Subnet {
				t.Errorf("got %+v, want %+v", user, created)
			}
		})
	}

	if _, err := s.UserByName("nobody"); !errors.Is(err, ErrNotFound) {
		t.Errorf("UserByName(nobody) err = %v, want ErrNotFound", err)
	}
}

func TestQuotaRoundTripAndUsage(t *testing.T) {
	s := newTestStore(t)
	user, host := seedStore(t, s)

	if _, err := s.QuotaFor(user.ID); !errors.Is(err, ErrNotFound) {
		t.Errorf("QuotaFor before set err = %v, want ErrNotFound", err)
	}

	quota := types.Quota{UserID: user.ID, MaxInstances: 5, MaxVCPU: 8, MaxMemoryMiB: 8192, MaxDiskGiB: 100}
	if err := s.SetQuota(quota); err != nil {
		t.Fatal(err)
	}
	got, err := s.QuotaFor(user.ID)
	if err != nil {
		t.Fatal(err)
	}
	if got != quota {
		t.Errorf("QuotaFor = %+v, want %+v", got, quota)
	}

	// SetQuota replaces.
	quota.MaxInstances = 7
	if err := s.SetQuota(quota); err != nil {
		t.Fatal(err)
	}
	if got, _ := s.QuotaFor(user.ID); got.MaxInstances != 7 {
		t.Errorf("MaxInstances after replace = %d, want 7", got.MaxInstances)
	}

	for i := 0; i < 2; i++ {
		inst := testInstance(i, "web"+string(rune('a'+i)), user, host)
		inst.VCPU = 2
		inst.MemoryMiB = 1024
		inst.DiskGiB = 20
		if err := s.CreateInstance(inst, 0); err != nil {
			t.Fatal(err)
		}
	}
	usage, err := s.UsageFor(user.ID)
	if err != nil {
		t.Fatal(err)
	}
	want := Usage{Instances: 2, VCPU: 4, MemoryMiB: 2048, DiskGiB: 40}
	if usage != want {
		t.Errorf("UsageFor = %+v, want %+v", usage, want)
	}
}

func TestUsersListsAllOrderedByName(t *testing.T) {
	s := newTestStore(t)
	for _, name := range []string{"carol", "alice", "bob"} {
		if _, err := s.RegisterUser(name, name+"@example.org", "sub-"+name, testRange); err != nil {
			t.Fatal(err)
		}
	}
	users, err := s.Users()
	if err != nil {
		t.Fatal(err)
	}
	want := []string{"alice", "bob", "carol"}
	if len(users) != len(want) {
		t.Fatalf("len(users) = %d, want %d", len(users), len(want))
	}
	for i, u := range users {
		if u.Name != want[i] {
			t.Errorf("users[%d].Name = %s, want %s", i, u.Name, want[i])
		}
		if u.Subnet == "" || u.OIDCSubject != "sub-"+u.Name {
			t.Errorf("users[%d] = %+v: subnet or oidc subject not loaded", i, u)
		}
	}
}
