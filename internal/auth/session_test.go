package auth

import (
	"testing"
	"time"

	"github.com/abbyfluoroethane/bento/internal/types"
)

var testEpoch = time.Date(2026, 8, 10, 12, 0, 0, 0, time.UTC)

// newTestService builds a Service on fakes. Callers mutate the returned
// fakes to shape each case.
func newTestService(t *testing.T, clock *fakeClock, opts ...Option) (*Service, *fakeUserStore, *fakeAccessStore, *fakeTokenStore) {
	t.Helper()
	users := &fakeUserStore{users: map[string]types.User{}}
	access := &fakeAccessStore{}
	tokens := newFakeTokenStore()
	opts = append([]Option{WithClock(clock.Now)}, opts...)
	svc := New("bento.example.org", users, access, tokens, opts...)
	return svc, users, access, tokens
}

func TestMemorySessionStore(t *testing.T) {
	st := NewMemorySessionStore()
	s := Session{ID: "abc", UserID: 7, ExpiresAt: testEpoch.Add(time.Hour)}
	if err := st.Put(s); err != nil {
		t.Fatalf("Put: %v", err)
	}
	got, ok := st.Get("abc")
	if !ok || got != s {
		t.Fatalf("Get = %+v, %v; want %+v, true", got, ok, s)
	}
	if _, ok := st.Get("missing"); ok {
		t.Fatal("Get(missing) reported ok")
	}
	st.Delete("abc")
	if _, ok := st.Get("abc"); ok {
		t.Fatal("session survived Delete")
	}
	st.Delete("abc") // deleting again is a no-op
}

func TestMemorySessionStoreDeleteExpired(t *testing.T) {
	st := NewMemorySessionStore()
	live := Session{ID: "live", ExpiresAt: testEpoch.Add(time.Hour)}
	dead := Session{ID: "dead", ExpiresAt: testEpoch.Add(-time.Hour)}
	edge := Session{ID: "edge", ExpiresAt: testEpoch} // expires exactly now
	for _, s := range []Session{live, dead, edge} {
		if err := st.Put(s); err != nil {
			t.Fatalf("Put(%s): %v", s.ID, err)
		}
	}
	st.DeleteExpired(testEpoch)
	if _, ok := st.Get("live"); !ok {
		t.Error("live session was swept")
	}
	if _, ok := st.Get("dead"); ok {
		t.Error("dead session survived sweep")
	}
	if _, ok := st.Get("edge"); ok {
		t.Error("session expiring exactly now survived sweep")
	}
}

func TestSessionExpiry(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, access, _ := newTestService(t, clock, WithSessionTTL(time.Hour))
	access.grant("uuid-1", 7)

	sess, err := svc.newSession(7)
	if err != nil {
		t.Fatalf("newSession: %v", err)
	}
	if want := testEpoch.Add(time.Hour); !sess.ExpiresAt.Equal(want) {
		t.Fatalf("ExpiresAt = %v, want %v", sess.ExpiresAt, want)
	}

	if _, err := svc.Authorize(sess.ID, "uuid-1"); err != nil {
		t.Fatalf("Authorize before expiry: %v", err)
	}
	clock.Advance(time.Hour + time.Second)
	if _, err := svc.Authorize(sess.ID, "uuid-1"); err != ErrUnauthenticated {
		t.Fatalf("Authorize after expiry = %v, want ErrUnauthenticated", err)
	}
	// The expired session was also removed from the store.
	if _, ok := svc.sessions.Get(sess.ID); ok {
		t.Error("expired session still in store after lookup")
	}
}

func TestSessionIDsAreOpaqueAndUnique(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, _, _ := newTestService(t, clock)
	seen := make(map[string]bool)
	for i := 0; i < 100; i++ {
		sess, err := svc.newSession(1)
		if err != nil {
			t.Fatalf("newSession: %v", err)
		}
		if len(sess.ID) < 40 {
			t.Fatalf("session ID %q is too short to be a 256-bit value", sess.ID)
		}
		if seen[sess.ID] {
			t.Fatalf("duplicate session ID %q", sess.ID)
		}
		seen[sess.ID] = true
	}
}

func TestSessionCookieAttributes(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, _, _ := newTestService(t, clock)
	sess := Session{ID: "sid", UserID: 1, ExpiresAt: testEpoch.Add(time.Hour)}
	c := svc.sessionCookie(sess)

	if c.Name != SessionCookieName {
		t.Errorf("Name = %q", c.Name)
	}
	if c.Domain != "bento.example.org" {
		t.Errorf("Domain = %q, want base domain so subdomains receive the cookie", c.Domain)
	}
	if !c.HttpOnly {
		t.Error("cookie is not HttpOnly")
	}
	if !c.Secure {
		t.Error("cookie is not Secure")
	}
	if c.SameSite == 0 {
		t.Error("cookie has no SameSite attribute")
	}
	if c.Path != "/" {
		t.Errorf("Path = %q", c.Path)
	}
	if !c.Expires.Equal(sess.ExpiresAt) {
		t.Errorf("Expires = %v, want %v", c.Expires, sess.ExpiresAt)
	}

	clear := svc.clearSessionCookie()
	if clear.MaxAge != -1 {
		t.Errorf("clear cookie MaxAge = %d, want -1", clear.MaxAge)
	}
	if clear.Domain != "bento.example.org" {
		t.Errorf("clear cookie Domain = %q", clear.Domain)
	}
}
