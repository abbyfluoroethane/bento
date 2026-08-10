package store

import (
	"fmt"
	"net/netip"
	"path/filepath"
	"sync"
	"testing"
	"time"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// fakeClock is an injectable time source for the cooldown and expiry tests.
type fakeClock struct {
	mu sync.Mutex
	t  time.Time
}

func newFakeClock() *fakeClock {
	return &fakeClock{t: time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)}
}

func (c *fakeClock) Now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.t
}

func (c *fakeClock) Advance(d time.Duration) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.t = c.t.Add(d)
}

func newTestStore(t *testing.T, opts ...Option) *Store {
	t.Helper()
	s, err := Open(filepath.Join(t.TempDir(), "bento.db"), opts...)
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	t.Cleanup(func() { s.Close() })
	return s
}

var testRange = netip.MustParsePrefix("10.100.0.0/16")

// seedStore registers a user, a host, an image, and one image version, and
// returns the user and host for instance fixtures.
func seedStore(t *testing.T, s *Store) (types.User, types.Host) {
	t.Helper()
	user, err := s.RegisterUser("alice", "alice@example.org", "", testRange)
	if err != nil {
		t.Fatalf("RegisterUser: %v", err)
	}
	host, err := s.EnsureHost("host1", "qemu:///system")
	if err != nil {
		t.Fatalf("EnsureHost: %v", err)
	}
	if err := s.UpsertImage(types.Image{Name: "debian-13", URL: "https://example.org/d13.qcow2"}); err != nil {
		t.Fatalf("UpsertImage: %v", err)
	}
	err = s.AddImageVersion(types.ImageVersion{
		Checksum:  "sha256-aa",
		ImageName: "debian-13",
		Path:      "/var/lib/bento/images/sha256-aa.qcow2",
		Size:      1,
		FetchedAt: time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC),
	})
	if err != nil {
		t.Fatalf("AddImageVersion: %v", err)
	}
	return user, host
}

// testInstance builds a valid instance fixture. n keeps the uuid, address,
// and mac unique across fixtures in one test.
func testInstance(n int, name string, owner types.User, host types.Host) types.Instance {
	return types.Instance{
		UUID:         fmt.Sprintf("uuid-%03d", n),
		Name:         name,
		OwnerID:      owner.ID,
		HostID:       host.ID,
		ImageName:    "debian-13",
		BaseChecksum: "sha256-aa",
		State:        types.StateStopped,
		DesiredState: types.DesiredRunning,
		Address:      fmt.Sprintf("10.100.0.%d", n+2),
		MAC:          fmt.Sprintf("52:54:00:00:00:%02x", n+1),
		VCPU:         1,
		MemoryMiB:    512,
		DiskGiB:      10,
		KSM:          true,
		HTTPPort:     80,
		Visibility:   types.VisibilityOff,
	}
}
