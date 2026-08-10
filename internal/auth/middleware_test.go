package auth

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

// echoUserID writes the context user ID, proving the middleware ran and
// populated the context.
func echoUserID(t *testing.T) http.Handler {
	t.Helper()
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		uid, ok := UserIDFromContext(r.Context())
		if !ok {
			t.Error("handler ran without a user ID in context")
		}
		_, _ = w.Write([]byte{byte('0' + uid)})
	})
}

func TestRequireSession(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, _, _ := newTestService(t, clock, WithSessionTTL(time.Hour))
	sess, err := svc.newSession(7)
	if err != nil {
		t.Fatalf("newSession: %v", err)
	}
	h := svc.RequireSession(echoUserID(t))

	t.Run("no cookie redirects to login with next", func(t *testing.T) {
		r := httptest.NewRequest("GET", "/instances/web?tab=ports", nil)
		w := httptest.NewRecorder()
		h.ServeHTTP(w, r)
		if w.Code != http.StatusFound {
			t.Fatalf("status = %d, want 302", w.Code)
		}
		loc := w.Header().Get("Location")
		if !strings.HasPrefix(loc, "/login?next=") || !strings.Contains(loc, "%2Finstances%2Fweb") {
			t.Fatalf("Location = %q, want /login?next=<original>", loc)
		}
	})

	t.Run("valid session passes with user in context", func(t *testing.T) {
		r := httptest.NewRequest("GET", "/", nil)
		r.AddCookie(&http.Cookie{Name: SessionCookieName, Value: sess.ID})
		w := httptest.NewRecorder()
		h.ServeHTTP(w, r)
		if w.Code != http.StatusOK || w.Body.String() != "7" {
			t.Fatalf("got %d %q, want 200 \"7\"", w.Code, w.Body.String())
		}
	})

	t.Run("expired session redirects", func(t *testing.T) {
		clock.Advance(2 * time.Hour)
		r := httptest.NewRequest("GET", "/", nil)
		r.AddCookie(&http.Cookie{Name: SessionCookieName, Value: sess.ID})
		w := httptest.NewRecorder()
		h.ServeHTTP(w, r)
		if w.Code != http.StatusFound {
			t.Fatalf("status = %d, want 302", w.Code)
		}
	})
}

func TestRequireToken(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, _, _ := newTestService(t, clock)
	plaintext, _, err := svc.MintToken(3, time.Hour)
	if err != nil {
		t.Fatalf("MintToken: %v", err)
	}
	h := svc.RequireToken(echoUserID(t))

	t.Run("no token is 401 with challenge", func(t *testing.T) {
		w := httptest.NewRecorder()
		h.ServeHTTP(w, httptest.NewRequest("GET", "/api/instances", nil))
		if w.Code != http.StatusUnauthorized {
			t.Fatalf("status = %d, want 401", w.Code)
		}
		if got := w.Header().Get("WWW-Authenticate"); !strings.Contains(got, "Bearer") {
			t.Fatalf("WWW-Authenticate = %q, want a Bearer challenge", got)
		}
	})

	t.Run("valid token passes", func(t *testing.T) {
		r := httptest.NewRequest("GET", "/api/instances", nil)
		r.Header.Set("Authorization", "Bearer "+plaintext)
		w := httptest.NewRecorder()
		h.ServeHTTP(w, r)
		if w.Code != http.StatusOK || w.Body.String() != "3" {
			t.Fatalf("got %d %q, want 200 \"3\"", w.Code, w.Body.String())
		}
	})

	t.Run("expired token is 401", func(t *testing.T) {
		clock.Advance(2 * time.Hour)
		r := httptest.NewRequest("GET", "/api/instances", nil)
		r.Header.Set("Authorization", "Bearer "+plaintext)
		w := httptest.NewRecorder()
		h.ServeHTTP(w, r)
		if w.Code != http.StatusUnauthorized {
			t.Fatalf("status = %d, want 401", w.Code)
		}
	})
}

func TestRequireSessionOrToken(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, _, _ := newTestService(t, clock, WithSessionTTL(time.Hour))
	sess, err := svc.newSession(7)
	if err != nil {
		t.Fatalf("newSession: %v", err)
	}
	plaintext, _, err := svc.MintToken(3, time.Hour)
	if err != nil {
		t.Fatalf("MintToken: %v", err)
	}
	h := svc.RequireSessionOrToken(echoUserID(t))

	t.Run("bearer token wins", func(t *testing.T) {
		r := httptest.NewRequest("GET", "/", nil)
		r.Header.Set("Authorization", "Bearer "+plaintext)
		r.AddCookie(&http.Cookie{Name: SessionCookieName, Value: sess.ID})
		w := httptest.NewRecorder()
		h.ServeHTTP(w, r)
		if w.Body.String() != "3" {
			t.Fatalf("body = %q, want token user \"3\"", w.Body.String())
		}
	})

	t.Run("session works without a token", func(t *testing.T) {
		r := httptest.NewRequest("GET", "/", nil)
		r.AddCookie(&http.Cookie{Name: SessionCookieName, Value: sess.ID})
		w := httptest.NewRecorder()
		h.ServeHTTP(w, r)
		if w.Body.String() != "7" {
			t.Fatalf("body = %q, want session user \"7\"", w.Body.String())
		}
	})

	t.Run("bad bearer is 401, not a redirect", func(t *testing.T) {
		r := httptest.NewRequest("GET", "/", nil)
		r.Header.Set("Authorization", "Bearer bento_forged")
		r.AddCookie(&http.Cookie{Name: SessionCookieName, Value: sess.ID})
		w := httptest.NewRecorder()
		h.ServeHTTP(w, r)
		if w.Code != http.StatusUnauthorized {
			t.Fatalf("status = %d, want 401", w.Code)
		}
	})

	t.Run("neither credential redirects to login", func(t *testing.T) {
		w := httptest.NewRecorder()
		h.ServeHTTP(w, httptest.NewRequest("GET", "/", nil))
		if w.Code != http.StatusFound {
			t.Fatalf("status = %d, want 302", w.Code)
		}
	})
}

func TestUserIDFromContext(t *testing.T) {
	r := httptest.NewRequest("GET", "/", nil)
	if _, ok := UserIDFromContext(r.Context()); ok {
		t.Fatal("empty context reported a user ID")
	}
	ctx := ContextWithUserID(r.Context(), 9)
	if uid, ok := UserIDFromContext(ctx); !ok || uid != 9 {
		t.Fatalf("got (%d, %v), want (9, true)", uid, ok)
	}
}
