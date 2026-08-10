package auth

import (
	"net/http"
	"sync"
	"time"
)

// SessionCookieName is the name of the base-domain session cookie.
const SessionCookieName = "bento_session"

// Session is one server-side login session. The cookie carries only the
// opaque ID; everything else lives on the server (SPEC 13).
type Session struct {
	ID        string
	UserID    int64
	CreatedAt time.Time
	ExpiresAt time.Time
}

// SessionStore holds sessions server side. The default implementation is
// in memory; a restart logs everyone out, which is acceptable for a
// dashboard session.
type SessionStore interface {
	Put(s Session) error
	Get(id string) (Session, bool)
	Delete(id string)
}

// MemorySessionStore is a mutex-guarded in-memory SessionStore. The zero
// value is not usable; call NewMemorySessionStore.
type MemorySessionStore struct {
	mu       sync.RWMutex
	sessions map[string]Session
}

// NewMemorySessionStore returns an empty in-memory session store.
func NewMemorySessionStore() *MemorySessionStore {
	return &MemorySessionStore{sessions: make(map[string]Session)}
}

// Put stores or replaces a session.
func (m *MemorySessionStore) Put(s Session) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.sessions[s.ID] = s
	return nil
}

// Get returns the session with the given ID.
func (m *MemorySessionStore) Get(id string) (Session, bool) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	s, ok := m.sessions[id]
	return s, ok
}

// Delete removes the session with the given ID. Deleting a missing
// session is a no-op.
func (m *MemorySessionStore) Delete(id string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	delete(m.sessions, id)
}

// DeleteExpired removes every session that expired at or before now.
// Callers may run it periodically; expiry is also enforced on every
// lookup, so the sweep only reclaims memory.
func (m *MemorySessionStore) DeleteExpired(now time.Time) {
	m.mu.Lock()
	defer m.mu.Unlock()
	for id, s := range m.sessions {
		if !s.ExpiresAt.After(now) {
			delete(m.sessions, id)
		}
	}
}

// newSession creates and stores a session for the user.
func (s *Service) newSession(userID int64) (Session, error) {
	now := s.now()
	sess := Session{
		ID:        randomToken(),
		UserID:    userID,
		CreatedAt: now,
		ExpiresAt: now.Add(s.sessionTTL),
	}
	if err := s.sessions.Put(sess); err != nil {
		return Session{}, err
	}
	return sess, nil
}

// session resolves a session ID to a live session, enforcing expiry.
func (s *Service) session(id string) (Session, bool) {
	if id == "" {
		return Session{}, false
	}
	sess, ok := s.sessions.Get(id)
	if !ok {
		return Session{}, false
	}
	if !sess.ExpiresAt.After(s.now()) {
		s.sessions.Delete(id)
		return Session{}, false
	}
	return sess, true
}

// sessionCookie builds the base-domain session cookie. Domain is set to
// the base domain so the cookie is sent to every subdomain; HttpOnly,
// Secure, and SameSite=Lax per SPEC 13.
func (s *Service) sessionCookie(sess Session) *http.Cookie {
	return &http.Cookie{
		Name:     SessionCookieName,
		Value:    sess.ID,
		Domain:   s.baseDomain,
		Path:     "/",
		Expires:  sess.ExpiresAt,
		HttpOnly: true,
		Secure:   true,
		SameSite: http.SameSiteLaxMode,
	}
}

// clearSessionCookie expires the session cookie in the browser.
func (s *Service) clearSessionCookie() *http.Cookie {
	return &http.Cookie{
		Name:     SessionCookieName,
		Value:    "",
		Domain:   s.baseDomain,
		Path:     "/",
		MaxAge:   -1,
		HttpOnly: true,
		Secure:   true,
		SameSite: http.SameSiteLaxMode,
	}
}

// SessionFromRequest returns the live session identified by the request's
// session cookie, if any.
func (s *Service) SessionFromRequest(r *http.Request) (Session, bool) {
	c, err := r.Cookie(SessionCookieName)
	if err != nil {
		return Session{}, false
	}
	return s.session(c.Value)
}
