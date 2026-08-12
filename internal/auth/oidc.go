package auth

import (
	"context"
	"fmt"
	"net/http"
	"net/url"
	"strings"

	gooidc "github.com/coreos/go-oidc/v3/oidc"
	"golang.org/x/oauth2"
)

// Claims are the ID token claims Bento uses. The subject maps to the
// users.oidc_subject column (SPEC 13).
type Claims struct {
	Subject string
	Email   string
	Nonce   string
}

// Verifier checks a raw ID token and returns its claims. The production
// implementation wraps the go-oidc verifier; tests use a fake.
type Verifier interface {
	Verify(ctx context.Context, rawIDToken string) (Claims, error)
}

// Exchanger drives the OAuth2 authorization code flow. The production
// implementation wraps golang.org/x/oauth2; tests use a fake.
type Exchanger interface {
	// AuthCodeURL returns the provider URL to redirect the browser to.
	AuthCodeURL(state, nonce string) string
	// Exchange redeems the authorization code and returns the raw ID
	// token from the token response.
	Exchange(ctx context.Context, code string) (rawIDToken string, err error)
}

// ProviderClient implements Exchanger and Verifier against a real OIDC
// provider. Pocket ID is a standard OIDC provider, so nothing here is
// provider specific.
type ProviderClient struct {
	oauth    oauth2.Config
	verifier *gooidc.IDTokenVerifier
}

// NewProviderClient discovers the issuer and returns a client for the
// authorization code flow. redirectURL is the absolute URL of the
// /callback handler on the base domain.
func NewProviderClient(ctx context.Context, issuer, clientID, clientSecret, redirectURL string) (*ProviderClient, error) {
	provider, err := gooidc.NewProvider(ctx, issuer)
	if err != nil {
		return nil, fmt.Errorf("oidc discovery for %s: %w", issuer, err)
	}
	return &ProviderClient{
		oauth: oauth2.Config{
			ClientID:     clientID,
			ClientSecret: clientSecret,
			Endpoint:     provider.Endpoint(),
			RedirectURL:  redirectURL,
			Scopes:       []string{gooidc.ScopeOpenID, "profile", "email"},
		},
		verifier: provider.Verifier(&gooidc.Config{ClientID: clientID}),
	}, nil
}

// AuthCodeURL implements Exchanger.
func (p *ProviderClient) AuthCodeURL(state, nonce string) string {
	return p.oauth.AuthCodeURL(state, gooidc.Nonce(nonce))
}

// Exchange implements Exchanger.
func (p *ProviderClient) Exchange(ctx context.Context, code string) (string, error) {
	tok, err := p.oauth.Exchange(ctx, code)
	if err != nil {
		return "", fmt.Errorf("oauth2 exchange: %w", err)
	}
	raw, ok := tok.Extra("id_token").(string)
	if !ok || raw == "" {
		return "", fmt.Errorf("token response has no id_token")
	}
	return raw, nil
}

// Verify implements Verifier.
func (p *ProviderClient) Verify(ctx context.Context, rawIDToken string) (Claims, error) {
	idt, err := p.verifier.Verify(ctx, rawIDToken)
	if err != nil {
		return Claims{}, fmt.Errorf("verify id token: %w", err)
	}
	var extra struct {
		Email string `json:"email"`
	}
	// Email is informational; a token without the claim is still valid.
	_ = idt.Claims(&extra)
	return Claims{Subject: idt.Subject, Email: extra.Email, Nonce: idt.Nonce}, nil
}

// Names of the short-lived cookies that carry OAuth2 flow state between
// /login and /callback. They are host-only cookies on the base domain.
const (
	stateCookieName = "bento_oauth_state"
	nonceCookieName = "bento_oauth_nonce"
	nextCookieName  = "bento_login_next"
)

// flowCookieTTL bounds how long a login attempt may take.
const flowCookieTTL = 10 * 60 // seconds

func flowCookie(name, value string) *http.Cookie {
	c := &http.Cookie{
		Name:     name,
		Value:    value,
		Path:     "/",
		MaxAge:   flowCookieTTL,
		HttpOnly: true,
		Secure:   true,
		SameSite: http.SameSiteLaxMode,
	}
	if value == "" {
		c.MaxAge = -1
	}
	return c
}

// safeNext returns next when it is a same-site relative path or an
// absolute https URL on the base domain or one of its subdomains,
// otherwise "/". The subdomain form is what the HTTP proxy sends when a
// private instance redirects to the login page (SPEC 9.2, 13): the
// session cookie is valid for every subdomain, so the flow can return
// there. Everything else stops open redirects through the login flow.
func (s *Service) safeNext(next string) string {
	if next == "" {
		return "/"
	}
	if strings.HasPrefix(next, "/") {
		if strings.HasPrefix(next, "//") {
			return "/"
		}
		return next
	}
	u, err := url.Parse(next)
	if err != nil || u.Scheme != "https" {
		return "/"
	}
	host := strings.TrimSuffix(strings.ToLower(u.Hostname()), ".")
	if host == s.baseDomain || strings.HasSuffix(host, "."+s.baseDomain) {
		return next
	}
	return "/"
}

// LoginHandler starts the OIDC authorization code flow: it stores fresh
// state and nonce values in short-lived cookies and redirects the
// browser to the provider. An optional ?next= query parameter names the
// relative path to return to after login.
func (s *Service) LoginHandler() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if s.oauth == nil {
			http.Error(w, "OIDC is not configured", http.StatusInternalServerError)
			return
		}
		state := randomToken()
		nonce := randomToken()
		http.SetCookie(w, flowCookie(stateCookieName, state))
		http.SetCookie(w, flowCookie(nonceCookieName, nonce))
		http.SetCookie(w, flowCookie(nextCookieName, s.safeNext(r.URL.Query().Get("next"))))
		http.Redirect(w, r, s.oauth.AuthCodeURL(state, nonce), http.StatusFound)
	})
}

// CallbackHandler finishes the OIDC flow: it checks the state cookie,
// redeems the code, verifies the ID token and its nonce, maps the OIDC
// subject to a users row, and issues the base-domain session cookie.
// A verified login with no matching users row gets 403: registration
// happens over SSH (SPEC 13).
func (s *Service) CallbackHandler() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if s.oauth == nil || s.verifier == nil {
			http.Error(w, "OIDC is not configured", http.StatusInternalServerError)
			return
		}
		if e := r.URL.Query().Get("error"); e != "" {
			s.log.Warn("login refused by the provider", "error", e)
			http.Error(w, "login failed at the provider: "+e, http.StatusForbidden)
			return
		}
		stateCookie, err := r.Cookie(stateCookieName)
		if err != nil || stateCookie.Value == "" {
			s.log.Warn("login rejected: no state cookie on the callback",
				"hint", "the flow cookies are SameSite=Lax and expire in 10 minutes")
			http.Error(w, "missing login state; start again at /login", http.StatusBadRequest)
			return
		}
		if r.URL.Query().Get("state") != stateCookie.Value {
			s.log.Warn("login rejected: state mismatch")
			http.Error(w, "state mismatch", http.StatusBadRequest)
			return
		}
		rawIDToken, err := s.oauth.Exchange(r.Context(), r.URL.Query().Get("code"))
		if err != nil {
			s.log.Error("login rejected: code exchange failed", "error", err)
			http.Error(w, "code exchange failed", http.StatusBadGateway)
			return
		}
		claims, err := s.verifier.Verify(r.Context(), rawIDToken)
		if err != nil {
			s.log.Error("login rejected: ID token did not verify", "error", err)
			http.Error(w, "invalid ID token", http.StatusUnauthorized)
			return
		}
		nonceCookie, err := r.Cookie(nonceCookieName)
		if err != nil || nonceCookie.Value == "" || claims.Nonce != nonceCookie.Value {
			s.log.Warn("login rejected: nonce mismatch", "cookie_present", err == nil)
			http.Error(w, "nonce mismatch", http.StatusBadRequest)
			return
		}
		user, ok, err := s.users.UserByOIDCSubject(claims.Subject)
		if err != nil {
			s.log.Error("login rejected: user lookup failed", "error", err)
			http.Error(w, "user lookup failed", http.StatusInternalServerError)
			return
		}
		if !ok {
			// The operator has to copy this subject into the users row
			// by hand (SPEC 13), and the provider is the only other
			// place it can be read from, so name it here.
			s.log.Warn("login rejected: no users row carries this OIDC subject",
				"subject", claims.Subject, "email", claims.Email,
				"hint", "set oidc_subject on the user's row to this value")
			http.Error(w, "no Bento account for this login; register by connecting with ssh", http.StatusForbidden)
			return
		}
		s.log.Info("dashboard login", "user", user.Name, "subject", claims.Subject)
		sess, err := s.newSession(user.ID)
		if err != nil {
			http.Error(w, "session creation failed", http.StatusInternalServerError)
			return
		}
		next := "/"
		if c, err := r.Cookie(nextCookieName); err == nil {
			next = s.safeNext(c.Value)
		}
		// Clear the flow cookies and set the session cookie.
		http.SetCookie(w, flowCookie(stateCookieName, ""))
		http.SetCookie(w, flowCookie(nonceCookieName, ""))
		http.SetCookie(w, flowCookie(nextCookieName, ""))
		http.SetCookie(w, s.sessionCookie(sess))
		http.Redirect(w, r, next, http.StatusFound)
	})
}

// LogoutHandler deletes the server-side session and expires the cookie.
func (s *Service) LogoutHandler() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if c, err := r.Cookie(SessionCookieName); err == nil && c.Value != "" {
			s.sessions.Delete(c.Value)
		}
		http.SetCookie(w, s.clearSessionCookie())
		http.Redirect(w, r, "/", http.StatusFound)
	})
}
