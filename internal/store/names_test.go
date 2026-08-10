package store

import (
	"errors"
	"testing"
	"time"
)

const cooldown = 24 * time.Hour

// TestClaimName covers the three cooldown rules of SPEC 7.2 for a name
// that alice released by deleting her instance.
func TestClaimName(t *testing.T) {
	tests := []struct {
		name          string
		advance       time.Duration // clock movement after the release
		claimant      string        // "owner" or "other"
		wantCooldown  bool
		wantRemaining time.Duration
	}{
		{
			name:     "rule 1: previous owner retakes at once",
			advance:  0,
			claimant: "owner",
		},
		{
			name:          "rule 2: another user inside the cooldown is refused",
			advance:       1 * time.Hour,
			claimant:      "other",
			wantCooldown:  true,
			wantRemaining: 23 * time.Hour,
		},
		{
			name:     "rule 3: another user after the cooldown succeeds",
			advance:  cooldown,
			claimant: "other",
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			clock := newFakeClock()
			s := newTestStore(t, WithClock(clock.Now))
			owner, host := seedStore(t, s)
			other, err := s.RegisterUser("bob", "bob@example.org", "", testRange)
			if err != nil {
				t.Fatal(err)
			}

			if err := s.CreateInstance(testInstance(1, "web", owner, host), cooldown); err != nil {
				t.Fatal(err)
			}
			if _, err := s.DeleteInstance("uuid-001"); err != nil {
				t.Fatal(err)
			}

			clock.Advance(tt.advance)
			claimant := owner.ID
			if tt.claimant == "other" {
				claimant = other.ID
			}

			err = s.ClaimName("web", claimant, cooldown)
			if !tt.wantCooldown {
				if err != nil {
					t.Fatalf("ClaimName = %v, want nil", err)
				}
				return
			}
			var cdErr *NameCooldownError
			if !errors.As(err, &cdErr) {
				t.Fatalf("ClaimName = %v, want NameCooldownError", err)
			}
			if cdErr.Name != "web" {
				t.Errorf("error name = %q, want %q", cdErr.Name, "web")
			}
			if cdErr.Remaining != tt.wantRemaining {
				t.Errorf("remaining = %s, want %s", cdErr.Remaining, tt.wantRemaining)
			}
		})
	}
}

// TestClaimNameLiveInstance checks that a name held by a live instance is
// never claimable, by anyone.
func TestClaimNameLiveInstance(t *testing.T) {
	s := newTestStore(t)
	owner, host := seedStore(t, s)
	if err := s.CreateInstance(testInstance(1, "web", owner, host), cooldown); err != nil {
		t.Fatal(err)
	}
	if err := s.ClaimName("web", owner.ID, cooldown); !errors.Is(err, ErrNameTaken) {
		t.Errorf("ClaimName on live name = %v, want ErrNameTaken", err)
	}
}

// TestReleasedNameRowKeptAfterExpiry checks the SPEC 12 rule: rows in
// released_names stay after the cooldown expires and after a successful
// claim by another user; only the timestamp comparison gates the claim.
func TestReleasedNameRowKeptAfterExpiry(t *testing.T) {
	clock := newFakeClock()
	s := newTestStore(t, WithClock(clock.Now))
	owner, host := seedStore(t, s)
	other, err := s.RegisterUser("bob", "bob@example.org", "", testRange)
	if err != nil {
		t.Fatal(err)
	}

	if err := s.CreateInstance(testInstance(1, "web", owner, host), cooldown); err != nil {
		t.Fatal(err)
	}
	if _, err := s.DeleteInstance("uuid-001"); err != nil {
		t.Fatal(err)
	}
	releasedAt := clock.Now()

	clock.Advance(cooldown + time.Hour)
	if err := s.ClaimName("web", other.ID, cooldown); err != nil {
		t.Fatalf("claim after expiry: %v", err)
	}

	record, err := s.ReleasedName("web")
	if err != nil {
		t.Fatalf("released row is gone after claim: %v", err)
	}
	if record.PreviousOwnerID != owner.ID {
		t.Errorf("previous owner = %d, want %d", record.PreviousOwnerID, owner.ID)
	}
	if !record.ReleasedAt.Equal(releasedAt) {
		t.Errorf("released at = %s, want %s", record.ReleasedAt, releasedAt)
	}
}

// TestRenameReleasesOldName checks SPEC 7.3: a rename frees the old name
// into the cooldown and the cooldown blocks other users from the old name.
func TestRenameReleasesOldName(t *testing.T) {
	clock := newFakeClock()
	s := newTestStore(t, WithClock(clock.Now))
	owner, host := seedStore(t, s)
	other, err := s.RegisterUser("bob", "bob@example.org", "", testRange)
	if err != nil {
		t.Fatal(err)
	}

	if err := s.CreateInstance(testInstance(1, "old-name", owner, host), cooldown); err != nil {
		t.Fatal(err)
	}
	if err := s.RenameInstance("uuid-001", "new-name", cooldown); err != nil {
		t.Fatal(err)
	}

	inst, err := s.Instance("uuid-001")
	if err != nil {
		t.Fatal(err)
	}
	if inst.Name != "new-name" {
		t.Errorf("name after rename = %q, want %q", inst.Name, "new-name")
	}
	if _, err := s.InstanceByName("old-name"); !errors.Is(err, ErrNotFound) {
		t.Errorf("old name still resolves: %v", err)
	}

	// The old name is cooling down for bob but free for alice.
	var cdErr *NameCooldownError
	if err := s.ClaimName("old-name", other.ID, cooldown); !errors.As(err, &cdErr) {
		t.Errorf("other user claim of old name = %v, want NameCooldownError", err)
	}
	if err := s.ClaimName("old-name", owner.ID, cooldown); err != nil {
		t.Errorf("owner claim of old name = %v, want nil", err)
	}

	// Renaming onto a name another user released and is cooling down fails.
	inst2 := testInstance(2, "bob-web", other, host)
	if err := s.CreateInstance(inst2, cooldown); err != nil {
		t.Fatal(err)
	}
	if _, err := s.DeleteInstance(inst2.UUID); err != nil {
		t.Fatal(err)
	}
	if err := s.RenameInstance("uuid-001", "bob-web", cooldown); !errors.As(err, &cdErr) {
		t.Errorf("rename onto cooling name = %v, want NameCooldownError", err)
	}
}

// TestCreateInstanceRespectsCooldown checks that a create that names a
// released name of another user fails with the remaining time (SPEC 15).
func TestCreateInstanceRespectsCooldown(t *testing.T) {
	clock := newFakeClock()
	s := newTestStore(t, WithClock(clock.Now))
	owner, host := seedStore(t, s)
	other, err := s.RegisterUser("bob", "bob@example.org", "", testRange)
	if err != nil {
		t.Fatal(err)
	}

	if err := s.CreateInstance(testInstance(1, "web", owner, host), cooldown); err != nil {
		t.Fatal(err)
	}
	if _, err := s.DeleteInstance("uuid-001"); err != nil {
		t.Fatal(err)
	}
	clock.Advance(30 * time.Minute)

	inst := testInstance(2, "web", other, host)
	err = s.CreateInstance(inst, cooldown)
	var cdErr *NameCooldownError
	if !errors.As(err, &cdErr) {
		t.Fatalf("CreateInstance = %v, want NameCooldownError", err)
	}
	if want := cooldown - 30*time.Minute; cdErr.Remaining != want {
		t.Errorf("remaining = %s, want %s", cdErr.Remaining, want)
	}

	// The failed create must not have inserted a row.
	if _, err := s.Instance(inst.UUID); !errors.Is(err, ErrNotFound) {
		t.Errorf("instance row exists after refused create: %v", err)
	}

	// The previous owner recreates immediately (the rebuild case).
	if err := s.CreateInstance(testInstance(3, "web", owner, host), cooldown); err != nil {
		t.Errorf("owner recreate = %v, want nil", err)
	}
}
