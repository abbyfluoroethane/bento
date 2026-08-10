package auth

import (
	"errors"
	"net/http"
	"net/http/httptest"
	"testing"
)

// TestStaleSessionAfterNameChangesHands is the exact SPEC 13 scenario:
// authorization keys on the instance UUID, so a session held from before
// a name changed hands grants nothing on the new instance with that name.
func TestStaleSessionAfterNameChangesHands(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, access, _ := newTestService(t, clock)

	const (
		alice = int64(1) // owned "web" (uuid-old), shared it with bob
		bob   = int64(2) // holds a session from before the change
		carol = int64(3) // owns the new instance named "web"
	)
	const oldUUID, newUUID = "uuid-old", "uuid-new"

	access.grant(oldUUID, alice)
	access.grant(oldUUID, bob)

	sess, err := svc.newSession(bob)
	if err != nil {
		t.Fatalf("newSession: %v", err)
	}
	if _, err := svc.Authorize(sess.ID, oldUUID); err != nil {
		t.Fatalf("bob cannot reach the shared instance before the change: %v", err)
	}

	// Alice deletes "web". The name cools down and carol creates a new
	// instance with the same name but a new UUID. Shares key on the
	// UUID and die with the old instance (SPEC 12).
	access.revoke(oldUUID, alice)
	access.revoke(oldUUID, bob)
	access.grant(newUUID, carol)

	// Bob's still-live session grants nothing on the new instance.
	uid, err := svc.Authorize(sess.ID, newUUID)
	if !errors.Is(err, ErrForbidden) {
		t.Fatalf("Authorize(bob session, new uuid) = (%d, %v), want ErrForbidden", uid, err)
	}
	// And nothing on the deleted one either.
	if _, err := svc.Authorize(sess.ID, oldUUID); !errors.Is(err, ErrForbidden) {
		t.Fatalf("Authorize(bob session, old uuid) = %v, want ErrForbidden", err)
	}
	// Carol, with her own session, reaches her instance.
	carolSess, err := svc.newSession(carol)
	if err != nil {
		t.Fatalf("newSession: %v", err)
	}
	if uid, err := svc.Authorize(carolSess.ID, newUUID); err != nil || uid != carol {
		t.Fatalf("Authorize(carol session, new uuid) = (%d, %v), want (%d, nil)", uid, err, carol)
	}
}

func TestAuthorize(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, access, _ := newTestService(t, clock)
	access.grant("uuid-1", 7)
	sess, err := svc.newSession(7)
	if err != nil {
		t.Fatalf("newSession: %v", err)
	}

	tests := []struct {
		name      string
		sessionID string
		uuid      string
		wantUID   int64
		wantErr   error
	}{
		{"owner or share", sess.ID, "uuid-1", 7, nil},
		{"no access", sess.ID, "uuid-2", 0, ErrForbidden},
		{"unknown session", "no-such-session", "uuid-1", 0, ErrUnauthenticated},
		{"empty session", "", "uuid-1", 0, ErrUnauthenticated},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			uid, err := svc.Authorize(tt.sessionID, tt.uuid)
			if !errors.Is(err, tt.wantErr) {
				t.Fatalf("err = %v, want %v", err, tt.wantErr)
			}
			if uid != tt.wantUID {
				t.Fatalf("uid = %d, want %d", uid, tt.wantUID)
			}
		})
	}
}

func TestAuthorizeStoreError(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, access, _ := newTestService(t, clock)
	access.err = errors.New("db is on fire")
	sess, err := svc.newSession(7)
	if err != nil {
		t.Fatalf("newSession: %v", err)
	}
	_, err = svc.Authorize(sess.ID, "uuid-1")
	if err == nil || errors.Is(err, ErrForbidden) || errors.Is(err, ErrUnauthenticated) {
		t.Fatalf("store failure must surface as its own error, got %v", err)
	}
}

func TestAuthorizeRequest(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, access, _ := newTestService(t, clock)
	access.grant("uuid-1", 7)
	sess, err := svc.newSession(7)
	if err != nil {
		t.Fatalf("newSession: %v", err)
	}

	r := httptest.NewRequest("GET", "https://web.bento.example.org/", nil)
	r.AddCookie(&http.Cookie{Name: SessionCookieName, Value: sess.ID})
	if uid, err := svc.AuthorizeRequest(r, "uuid-1"); err != nil || uid != 7 {
		t.Fatalf("AuthorizeRequest = (%d, %v), want (7, nil)", uid, err)
	}

	bare := httptest.NewRequest("GET", "https://web.bento.example.org/", nil)
	if _, err := svc.AuthorizeRequest(bare, "uuid-1"); !errors.Is(err, ErrUnauthenticated) {
		t.Fatalf("no cookie: err = %v, want ErrUnauthenticated", err)
	}
}

func TestAuthorizeUser(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, access, _ := newTestService(t, clock)
	access.grant("uuid-1", 7)
	if err := svc.AuthorizeUser(7, "uuid-1"); err != nil {
		t.Fatalf("AuthorizeUser(owner) = %v", err)
	}
	if err := svc.AuthorizeUser(8, "uuid-1"); !errors.Is(err, ErrForbidden) {
		t.Fatalf("AuthorizeUser(stranger) = %v, want ErrForbidden", err)
	}
}
