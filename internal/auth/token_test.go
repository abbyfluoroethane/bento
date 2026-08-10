package auth

import (
	"errors"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func TestMintTokenStoresOnlyTheHash(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, _, tokens := newTestService(t, clock)

	plaintext, tok, err := svc.MintToken(7, time.Hour)
	if err != nil {
		t.Fatalf("MintToken: %v", err)
	}
	if !strings.HasPrefix(plaintext, TokenPrefix) {
		t.Errorf("plaintext %q lacks the %q prefix", plaintext, TokenPrefix)
	}
	if tok.UserID != 7 {
		t.Errorf("UserID = %d, want 7", tok.UserID)
	}
	if want := testEpoch.Add(time.Hour); !tok.ExpiresAt.Equal(want) {
		t.Errorf("ExpiresAt = %v, want %v", tok.ExpiresAt, want)
	}
	if len(tokens.created) != 1 {
		t.Fatalf("created %d rows, want 1", len(tokens.created))
	}
	stored := tokens.created[0]
	if stored.Hash == plaintext || strings.Contains(stored.Hash, plaintext) {
		t.Error("plaintext reached the store")
	}
	if stored.Hash != HashToken(plaintext) {
		t.Error("stored hash is not the SHA-256 of the plaintext")
	}
}

func TestMintTokenPlaintextsAreUnique(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, _, _ := newTestService(t, clock)
	seen := make(map[string]bool)
	for i := 0; i < 50; i++ {
		p, _, err := svc.MintToken(1, 0)
		if err != nil {
			t.Fatalf("MintToken: %v", err)
		}
		if seen[p] {
			t.Fatalf("duplicate plaintext %q", p)
		}
		seen[p] = true
	}
}

func TestAuthenticateToken(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, _, _ := newTestService(t, clock)

	expiring, _, err := svc.MintToken(7, time.Hour)
	if err != nil {
		t.Fatalf("MintToken: %v", err)
	}
	forever, _, err := svc.MintToken(8, 0)
	if err != nil {
		t.Fatalf("MintToken: %v", err)
	}

	if tok, err := svc.AuthenticateToken(expiring); err != nil || tok.UserID != 7 {
		t.Fatalf("AuthenticateToken(expiring) = (%+v, %v)", tok, err)
	}
	if _, err := svc.AuthenticateToken("bento_forged"); !errors.Is(err, ErrUnauthenticated) {
		t.Fatalf("unknown token: err = %v, want ErrUnauthenticated", err)
	}
	if _, err := svc.AuthenticateToken(""); !errors.Is(err, ErrUnauthenticated) {
		t.Fatalf("empty token: err = %v, want ErrUnauthenticated", err)
	}

	clock.Advance(time.Hour + time.Second)
	if _, err := svc.AuthenticateToken(expiring); !errors.Is(err, ErrTokenExpired) {
		t.Fatalf("expired token: err = %v, want ErrTokenExpired", err)
	}
	// A zero expires_at never expires.
	clock.Advance(1000 * time.Hour)
	if tok, err := svc.AuthenticateToken(forever); err != nil || tok.UserID != 8 {
		t.Fatalf("AuthenticateToken(forever) = (%+v, %v)", tok, err)
	}
}

func TestRevokeToken(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, _, _ := newTestService(t, clock)
	plaintext, tok, err := svc.MintToken(7, time.Hour)
	if err != nil {
		t.Fatalf("MintToken: %v", err)
	}
	if err := svc.RevokeToken(tok.ID); err != nil {
		t.Fatalf("RevokeToken: %v", err)
	}
	if _, err := svc.AuthenticateToken(plaintext); !errors.Is(err, ErrUnauthenticated) {
		t.Fatalf("revoked token: err = %v, want ErrUnauthenticated", err)
	}
}

func TestBearerToken(t *testing.T) {
	tests := []struct {
		name   string
		header string
		want   string
	}{
		{"no header", "", ""},
		{"bearer", "Bearer bento_abc", "bento_abc"},
		{"lowercase scheme", "bearer bento_abc", "bento_abc"},
		{"padded", "Bearer   bento_abc  ", "bento_abc"},
		{"basic scheme", "Basic dXNlcjpwYXNz", ""},
		{"scheme only", "Bearer", ""},
		{"scheme with space only", "Bearer ", ""},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			r := httptest.NewRequest("GET", "/", nil)
			if tt.header != "" {
				r.Header.Set("Authorization", tt.header)
			}
			if got := BearerToken(r); got != tt.want {
				t.Fatalf("BearerToken = %q, want %q", got, tt.want)
			}
		})
	}
}

func TestHashTokenIsStable(t *testing.T) {
	// Pinned so a refactor cannot silently orphan every stored token.
	const want = "6534dfb43d2797ced449eb69034d35aa68c7a93ae1580da72db3f7050a1acb79"
	if got := HashToken("bento_test-token"); got != want {
		t.Fatalf("HashToken changed: got %s, want %s", got, want)
	}
}
