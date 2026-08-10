package store

import (
	"path/filepath"
	"strings"
	"testing"
)

// TestDumpDBRoundTrip writes a snapshot with VACUUM INTO (SPEC 12.1) and
// reopens it as a full store: schema, rows, and indexes must all be there.
func TestDumpDBRoundTrip(t *testing.T) {
	s := newTestStore(t)
	owner, host := seedStore(t, s)
	inst := testInstance(1, "web", owner, host)
	if err := s.CreateInstance(inst, 0); err != nil {
		t.Fatal(err)
	}
	if err := s.TouchLastSeen(inst.UUID); err != nil {
		t.Fatal(err)
	}

	dest := filepath.Join(t.TempDir(), "backup.db")
	if err := s.DumpDB(dest); err != nil {
		t.Fatalf("DumpDB: %v", err)
	}

	// The dump keeps working even if the source moves on afterwards.
	if _, err := s.DeleteInstance(inst.UUID); err != nil {
		t.Fatal(err)
	}

	restored, err := Open(dest)
	if err != nil {
		t.Fatalf("open dump: %v", err)
	}
	defer restored.Close()

	user, err := restored.UserByName("alice")
	if err != nil {
		t.Fatalf("user missing from dump: %v", err)
	}
	if user.ID != owner.ID || user.Subnet != owner.Subnet {
		t.Errorf("restored user = %+v, want %+v", user, owner)
	}
	got, err := restored.Instance(inst.UUID)
	if err != nil {
		t.Fatalf("instance missing from dump: %v", err)
	}
	if got.Name != "web" || got.Address != inst.Address || got.LastSeenAt.IsZero() {
		t.Errorf("restored instance = %+v", got)
	}
}

// TestDumpDBRefusesExistingDestination checks that a dump never clobbers
// or appends to a file that is already there.
func TestDumpDBRefusesExistingDestination(t *testing.T) {
	s := newTestStore(t)
	dest := filepath.Join(t.TempDir(), "backup.db")
	if err := s.DumpDB(dest); err != nil {
		t.Fatal(err)
	}
	err := s.DumpDB(dest)
	if err == nil {
		t.Fatal("second dump to the same path succeeded, want an error")
	}
	if !strings.Contains(err.Error(), "exists") {
		t.Errorf("err = %v, want a mention of the existing file", err)
	}
}
