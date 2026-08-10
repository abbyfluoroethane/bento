package auth

import (
	"net/http"
	"net/url"
)

// RequireSession wraps a browser-facing handler. A request without a
// live session is redirected to the login page with the original path in
// ?next=. On success the user ID is placed in the request context.
func (s *Service) RequireSession(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		sess, ok := s.SessionFromRequest(r)
		if !ok {
			target := s.loginPath + "?next=" + url.QueryEscape(r.URL.RequestURI())
			http.Redirect(w, r, target, http.StatusFound)
			return
		}
		next.ServeHTTP(w, r.WithContext(ContextWithUserID(r.Context(), sess.UserID)))
	})
}

// RequireToken wraps an API handler. A request without a valid bearer
// token gets 401 with a WWW-Authenticate header. On success the token
// owner's user ID is placed in the request context.
func (s *Service) RequireToken(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		tok, err := s.AuthenticateToken(BearerToken(r))
		if err != nil {
			w.Header().Set("WWW-Authenticate", `Bearer realm="bento"`)
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		next.ServeHTTP(w, r.WithContext(ContextWithUserID(r.Context(), tok.UserID)))
	})
}

// RequireSessionOrToken accepts either credential: a bearer token first,
// then the session cookie. Browser requests without either are
// redirected to login; requests carrying a bad bearer token get 401.
func (s *Service) RequireSessionOrToken(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if bearer := BearerToken(r); bearer != "" {
			tok, err := s.AuthenticateToken(bearer)
			if err != nil {
				w.Header().Set("WWW-Authenticate", `Bearer realm="bento"`)
				http.Error(w, "unauthorized", http.StatusUnauthorized)
				return
			}
			next.ServeHTTP(w, r.WithContext(ContextWithUserID(r.Context(), tok.UserID)))
			return
		}
		s.RequireSession(next).ServeHTTP(w, r)
	})
}
