package auth

import (
	"fmt"
	"net/http"
)

// Authorize resolves a session ID and checks access to one instance,
// identified by UUID. This is the check the HTTP proxy runs on every
// request for a private instance (SPEC 9.2, 13).
//
// It returns the user ID on success. It returns ErrUnauthenticated when
// the session is missing, unknown, or expired, and ErrForbidden when the
// session is valid but the user neither owns the instance nor holds a
// share on its UUID. Because the check keys on the UUID, a session held
// from before a name changed hands grants nothing on the new instance.
func (s *Service) Authorize(sessionID, instanceUUID string) (int64, error) {
	sess, ok := s.session(sessionID)
	if !ok {
		return 0, ErrUnauthenticated
	}
	return s.authorizeUser(sess.UserID, instanceUUID)
}

// AuthorizeRequest is Authorize with the session ID read from the
// request's session cookie.
func (s *Service) AuthorizeRequest(r *http.Request, instanceUUID string) (int64, error) {
	c, err := r.Cookie(SessionCookieName)
	if err != nil {
		return 0, ErrUnauthenticated
	}
	return s.Authorize(c.Value, instanceUUID)
}

// AuthorizeUser checks whether an already-authenticated user (for
// example the holder of an API token) may act on the instance.
func (s *Service) AuthorizeUser(userID int64, instanceUUID string) error {
	_, err := s.authorizeUser(userID, instanceUUID)
	return err
}

func (s *Service) authorizeUser(userID int64, instanceUUID string) (int64, error) {
	ok, err := s.access.HasAccess(instanceUUID, userID)
	if err != nil {
		return 0, fmt.Errorf("access check for instance %s: %w", instanceUUID, err)
	}
	if !ok {
		return 0, ErrForbidden
	}
	return userID, nil
}
