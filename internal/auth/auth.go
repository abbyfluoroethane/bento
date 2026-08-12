// Package auth implements OIDC login for the dashboard, base-domain
// session cookies, per-request authorization against owner and shares,
// and API tokens (SPEC sections 9.2 and 13).
//
// The session cookie identifies the user and nothing else. Authorization
// runs on every request against the owner and the shares of the instance,
// keyed by instance UUID. A cookie held from before a name changed hands
// therefore grants nothing on the new instance.
package auth

import (
	"context"
	"crypto/rand"
	"encoding/base64"
	"errors"
	"log/slog"
	"time"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// Errors returned by authentication and authorization.
var (
	// ErrUnauthenticated means no valid session or token was presented.
	ErrUnauthenticated = errors.New("auth: unauthenticated")
	// ErrForbidden means the caller is authenticated but neither owns
	// the instance nor holds a share on its UUID.
	ErrForbidden = errors.New("auth: forbidden")
	// ErrTokenExpired means the presented API token exists but its
	// expiry time has passed.
	ErrTokenExpired = errors.New("auth: token expired")
	// ErrNoAccount means the OIDC login succeeded but no users row has
	// the presented subject. Registration happens over SSH (SPEC 13).
	ErrNoAccount = errors.New("auth: no account for OIDC subject")
)

// UserStore resolves an OIDC subject to a Bento user. The store package
// satisfies it through a thin adapter over users.oidc_subject.
type UserStore interface {
	// UserByOIDCSubject returns the user whose oidc_subject column
	// matches subject. ok is false when no such user exists.
	UserByOIDCSubject(subject string) (u types.User, ok bool, err error)
}

// AccessStore answers the per-request authorization question of SPEC 13.
type AccessStore interface {
	// HasAccess reports whether the user owns the instance with the
	// given UUID or holds a shares row keyed on that UUID. It must key
	// on the UUID, never on the name (SPEC 12).
	HasAccess(instanceUUID string, userID int64) (bool, error)
}

// TokenStore persists API tokens. Only the hash of a token is stored
// (SPEC 13).
type TokenStore interface {
	// CreateToken inserts a token row. A zero expiresAt means the token
	// does not expire.
	CreateToken(userID int64, hash string, expiresAt time.Time) (types.Token, error)
	// TokenByHash returns the token row with the given hash. ok is
	// false when no such row exists.
	TokenByHash(hash string) (t types.Token, ok bool, err error)
	// DeleteToken removes a token row.
	DeleteToken(id int64) error
}

// Service ties sessions, OIDC login, authorization, and API tokens
// together. Construct it with New.
type Service struct {
	baseDomain string
	sessions   SessionStore
	users      UserStore
	access     AccessStore
	tokens     TokenStore

	oauth    Exchanger
	verifier Verifier

	now        func() time.Time
	sessionTTL time.Duration
	loginPath  string
	log        *slog.Logger
}

// Option configures a Service.
type Option func(*Service)

// WithSessionStore replaces the default in-memory session store.
func WithSessionStore(st SessionStore) Option {
	return func(s *Service) { s.sessions = st }
}

// WithLogger records why a login was refused. Every rejection in the
// callback is invisible to the operator otherwise: the reason goes to
// the browser and nowhere else, and the reasons are exactly what an
// operator needs — a subject with no users row is the normal state of a
// user who has registered over SSH but has not been linked yet
// (SPEC 13). The default discards.
func WithLogger(l *slog.Logger) Option {
	return func(s *Service) {
		if l != nil {
			s.log = l
		}
	}
}

// WithClock injects the time source, for tests.
func WithClock(now func() time.Time) Option {
	return func(s *Service) { s.now = now }
}

// WithSessionTTL sets the session lifetime. The default is seven days.
func WithSessionTTL(d time.Duration) Option {
	return func(s *Service) { s.sessionTTL = d }
}

// WithOIDC injects the OAuth2 exchanger and the ID token verifier. Wire
// a *ProviderClient for both in production; tests pass fakes.
func WithOIDC(ex Exchanger, ver Verifier) Option {
	return func(s *Service) {
		s.oauth = ex
		s.verifier = ver
	}
}

// WithLoginPath sets the path RequireSession redirects to. The default
// is /login.
func WithLoginPath(p string) Option {
	return func(s *Service) { s.loginPath = p }
}

// DefaultSessionTTL is the session lifetime when WithSessionTTL is not
// given.
const DefaultSessionTTL = 7 * 24 * time.Hour

// New returns a Service for the given base domain. The session cookie is
// issued for the base domain and is therefore valid on every subdomain
// (SPEC 13).
func New(baseDomain string, users UserStore, access AccessStore, tokens TokenStore, opts ...Option) *Service {
	s := &Service{
		baseDomain: baseDomain,
		sessions:   NewMemorySessionStore(),
		users:      users,
		access:     access,
		tokens:     tokens,
		now:        time.Now,
		sessionTTL: DefaultSessionTTL,
		loginPath:  "/login",
		log:        slog.New(slog.DiscardHandler),
	}
	for _, opt := range opts {
		opt(s)
	}
	return s
}

// randomToken returns a 256-bit opaque random value in URL-safe base64.
func randomToken() string {
	var b [32]byte
	if _, err := rand.Read(b[:]); err != nil {
		// crypto/rand never fails on a supported platform; a failure
		// here means the process cannot do anything safely.
		panic("auth: crypto/rand failed: " + err.Error())
	}
	return base64.RawURLEncoding.EncodeToString(b[:])
}

type ctxKey int

const userIDKey ctxKey = iota

// ContextWithUserID returns a context carrying the authenticated user ID.
func ContextWithUserID(ctx context.Context, userID int64) context.Context {
	return context.WithValue(ctx, userIDKey, userID)
}

// UserIDFromContext returns the authenticated user ID placed by the
// middleware, if any.
func UserIDFromContext(ctx context.Context) (int64, bool) {
	id, ok := ctx.Value(userIDKey).(int64)
	return id, ok
}
