package store

import (
	"errors"
	"testing"
	"time"

	"github.com/abbyfluoroethane/bento/internal/types"
)

func TestSSHKeyFingerprintLookup(t *testing.T) {
	s := newTestStore(t)
	user, _ := seedStore(t, s)

	id, err := s.AddSSHKey(user.ID, "ssh-ed25519 AAAA... alice@laptop", "SHA256:abcdef", "laptop")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := s.AddSSHKey(user.ID, "ssh-ed25519 BBBB... alice@desk", "SHA256:ghijkl", "desk"); err != nil {
		t.Fatal(err)
	}

	key, err := s.SSHKeyByFingerprint("SHA256:abcdef")
	if err != nil {
		t.Fatal(err)
	}
	if key.ID != id || key.UserID != user.ID || key.Comment != "laptop" {
		t.Errorf("SSHKeyByFingerprint = %+v", key)
	}
	if _, err := s.SSHKeyByFingerprint("SHA256:missing"); !errors.Is(err, ErrNotFound) {
		t.Errorf("missing fingerprint err = %v, want ErrNotFound", err)
	}

	keys, err := s.SSHKeysForUser(user.ID)
	if err != nil {
		t.Fatal(err)
	}
	if len(keys) != 2 {
		t.Errorf("SSHKeysForUser = %d keys, want 2", len(keys))
	}

	if err := s.DeleteSSHKey(user.ID+1, id); !errors.Is(err, ErrNotFound) {
		t.Errorf("delete with wrong user = %v, want ErrNotFound", err)
	}
	if err := s.DeleteSSHKey(user.ID, id); err != nil {
		t.Fatal(err)
	}
	if _, err := s.SSHKeyByFingerprint("SHA256:abcdef"); !errors.Is(err, ErrNotFound) {
		t.Errorf("key still present after delete: %v", err)
	}
}

func TestTokens(t *testing.T) {
	clock := newFakeClock()
	s := newTestStore(t, WithClock(clock.Now))
	user, _ := seedStore(t, s)

	expiry := clock.Now().Add(time.Hour)
	id, err := s.CreateToken(user.ID, "hash-of-secret", expiry)
	if err != nil {
		t.Fatal(err)
	}

	token, err := s.TokenByHash("hash-of-secret")
	if err != nil {
		t.Fatal(err)
	}
	if token.ID != id || token.UserID != user.ID || !token.ExpiresAt.Equal(expiry) {
		t.Errorf("TokenByHash = %+v", token)
	}

	if _, err := s.TokenByHash("unknown"); !errors.Is(err, ErrNotFound) {
		t.Errorf("unknown hash err = %v, want ErrNotFound", err)
	}

	clock.Advance(2 * time.Hour)
	if _, err := s.TokenByHash("hash-of-secret"); !errors.Is(err, ErrTokenExpired) {
		t.Errorf("expired token err = %v, want ErrTokenExpired", err)
	}

	if err := s.DeleteToken(user.ID, id); err != nil {
		t.Fatal(err)
	}
	if _, err := s.TokenByHash("hash-of-secret"); !errors.Is(err, ErrNotFound) {
		t.Errorf("token survives delete: %v", err)
	}
}

func TestEnsureHostIdempotent(t *testing.T) {
	s := newTestStore(t)
	first, err := s.EnsureHost("host1", "qemu:///system")
	if err != nil {
		t.Fatal(err)
	}
	second, err := s.EnsureHost("host1", "qemu+ssh://root@host1/system")
	if err != nil {
		t.Fatal(err)
	}
	if second.ID != first.ID {
		t.Errorf("EnsureHost minted a new id: %d then %d", first.ID, second.ID)
	}
	if second.LibvirtURI != "qemu+ssh://root@host1/system" {
		t.Errorf("uri not updated: %s", second.LibvirtURI)
	}
}

func TestSharesAndAccess(t *testing.T) {
	s := newTestStore(t)
	owner, host := seedStore(t, s)
	friend, err := s.RegisterUser("bob", "bob@example.org", "", testRange)
	if err != nil {
		t.Fatal(err)
	}
	stranger, err := s.RegisterUser("carol", "carol@example.org", "", testRange)
	if err != nil {
		t.Fatal(err)
	}

	inst := testInstance(1, "web", owner, host)
	if err := s.CreateInstance(inst, 0); err != nil {
		t.Fatal(err)
	}
	if err := s.AddShare(inst.UUID, friend.ID); err != nil {
		t.Fatal(err)
	}
	if err := s.AddShare(inst.UUID, friend.ID); err != nil {
		t.Errorf("duplicate share not a no-op: %v", err)
	}

	tests := []struct {
		name string
		user int64
		want bool
	}{
		{"owner", owner.ID, true},
		{"shared user", friend.ID, true},
		{"stranger", stranger.ID, false},
	}
	for _, tt := range tests {
		got, err := s.HasAccess(inst.UUID, tt.user)
		if err != nil {
			t.Fatal(err)
		}
		if got != tt.want {
			t.Errorf("HasAccess(%s) = %v, want %v", tt.name, got, tt.want)
		}
	}

	shared, err := s.InstancesSharedWith(friend.ID)
	if err != nil {
		t.Fatal(err)
	}
	if len(shared) != 1 || shared[0].UUID != inst.UUID {
		t.Errorf("InstancesSharedWith = %+v", shared)
	}

	if err := s.RemoveShare(inst.UUID, friend.ID); err != nil {
		t.Fatal(err)
	}
	if err := s.RemoveShare(inst.UUID, friend.ID); !errors.Is(err, ErrNotFound) {
		t.Errorf("second remove = %v, want ErrNotFound", err)
	}
}

func TestImagesAndUnusedVersions(t *testing.T) {
	s := newTestStore(t)
	owner, host := seedStore(t, s) // adds debian-13 with version sha256-aa

	older := types.ImageVersion{
		Checksum:  "sha256-old",
		ImageName: "debian-13",
		Path:      "/var/lib/bento/images/sha256-old.qcow2",
		Size:      2,
		FetchedAt: time.Date(2025, 12, 1, 0, 0, 0, 0, time.UTC),
	}
	if err := s.AddImageVersion(older); err != nil {
		t.Fatal(err)
	}
	if err := s.SetCurrentChecksum("debian-13", "sha256-aa"); err != nil {
		t.Fatal(err)
	}

	img, err := s.Image("debian-13")
	if err != nil {
		t.Fatal(err)
	}
	if img.CurrentChecksum != "sha256-aa" {
		t.Errorf("current checksum = %q, want sha256-aa", img.CurrentChecksum)
	}

	// sha256-old is unused: not current, no instance built from it.
	unused, err := s.UnusedImageVersions()
	if err != nil {
		t.Fatal(err)
	}
	if len(unused) != 1 || unused[0].Checksum != "sha256-old" {
		t.Fatalf("UnusedImageVersions = %+v, want only sha256-old", unused)
	}

	// An instance built from sha256-old pins it (SPEC 5.1).
	inst := testInstance(1, "web", owner, host)
	inst.BaseChecksum = "sha256-old"
	if err := s.CreateInstance(inst, 0); err != nil {
		t.Fatal(err)
	}
	unused, err = s.UnusedImageVersions()
	if err != nil {
		t.Fatal(err)
	}
	if len(unused) != 0 {
		t.Errorf("UnusedImageVersions with instance on sha256-old = %+v, want none", unused)
	}

	versions, err := s.ImageVersions("debian-13")
	if err != nil {
		t.Fatal(err)
	}
	if len(versions) != 2 || versions[0].Checksum != "sha256-aa" {
		t.Errorf("ImageVersions = %+v, want newest (sha256-aa) first", versions)
	}
}

func TestDeleteTokenByID(t *testing.T) {
	s := newTestStore(t)
	user, err := s.RegisterUser("amber", "amber@example.org", "", testRange)
	if err != nil {
		t.Fatal(err)
	}
	id, err := s.CreateToken(user.ID, "hash-by-id", time.Time{})
	if err != nil {
		t.Fatal(err)
	}
	if err := s.DeleteTokenByID(id); err != nil {
		t.Fatalf("DeleteTokenByID: %v", err)
	}
	if _, err := s.TokenByHash("hash-by-id"); !errors.Is(err, ErrNotFound) {
		t.Errorf("TokenByHash after delete = %v, want ErrNotFound", err)
	}
	if err := s.DeleteTokenByID(id); !errors.Is(err, ErrNotFound) {
		t.Errorf("second DeleteTokenByID = %v, want ErrNotFound", err)
	}
}
