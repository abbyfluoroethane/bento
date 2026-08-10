package auth

import (
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// newOIDCService wires a Service to a fake provider with one known user.
func newOIDCService(t *testing.T) (*Service, *fakeExchanger, *fakeUserStore) {
	t.Helper()
	clock := newFakeClock(testEpoch)
	ex := &fakeExchanger{
		authURL:    "https://id.example.org/authorize",
		code:       "good-code",
		rawIDToken: "raw-token",
	}
	ver := &fakeVerifier{claims: map[string]Claims{}}
	svc, users, _, _ := newTestService(t, clock, WithOIDC(ex, ver))
	users.users["subject-1"] = types.User{ID: 42, Name: "shaun", OIDCSubject: "subject-1"}
	return svc, ex, users
}

// doLogin runs the login handler and returns the recorder plus the flow
// cookies it set.
func doLogin(t *testing.T, svc *Service, target string) (*httptest.ResponseRecorder, []*http.Cookie) {
	t.Helper()
	r := httptest.NewRequest("GET", target, nil)
	w := httptest.NewRecorder()
	svc.LoginHandler().ServeHTTP(w, r)
	return w, w.Result().Cookies()
}

func cookieByName(cs []*http.Cookie, name string) *http.Cookie {
	for _, c := range cs {
		if c.Name == name {
			return c
		}
	}
	return nil
}

// callbackRequest builds a callback request carrying the flow cookies.
func callbackRequest(target string, cookies []*http.Cookie) *http.Request {
	r := httptest.NewRequest("GET", target, nil)
	for _, c := range cookies {
		if c.MaxAge >= 0 {
			r.AddCookie(&http.Cookie{Name: c.Name, Value: c.Value})
		}
	}
	return r
}

func TestLoginRedirectsToProvider(t *testing.T) {
	svc, ex, _ := newOIDCService(t)
	w, cookies := doLogin(t, svc, "/login")

	if w.Code != http.StatusFound {
		t.Fatalf("status = %d, want 302", w.Code)
	}
	loc := w.Header().Get("Location")
	if !strings.HasPrefix(loc, "https://id.example.org/authorize") {
		t.Fatalf("Location = %q, want provider URL", loc)
	}
	state := cookieByName(cookies, stateCookieName)
	nonce := cookieByName(cookies, nonceCookieName)
	if state == nil || state.Value == "" {
		t.Fatal("no state cookie set")
	}
	if nonce == nil || nonce.Value == "" {
		t.Fatal("no nonce cookie set")
	}
	if state.Value != ex.lastState || nonce.Value != ex.lastNonce {
		t.Fatal("cookies do not match the state and nonce sent to the provider")
	}
	for _, c := range []*http.Cookie{state, nonce} {
		if !c.HttpOnly || !c.Secure {
			t.Errorf("flow cookie %s is missing HttpOnly or Secure", c.Name)
		}
	}
}

func TestCallbackHappyPath(t *testing.T) {
	svc, ex, _ := newOIDCService(t)
	_, cookies := doLogin(t, svc, "/login?next=/instances/web")
	svc.verifier.(*fakeVerifier).claims["raw-token"] = Claims{
		Subject: "subject-1",
		Email:   "shaunloo10@gmail.com",
		Nonce:   ex.lastNonce,
	}

	r := callbackRequest("/callback?code=good-code&state="+url.QueryEscape(ex.lastState), cookies)
	w := httptest.NewRecorder()
	svc.CallbackHandler().ServeHTTP(w, r)

	if w.Code != http.StatusFound {
		t.Fatalf("status = %d, body %q; want 302", w.Code, w.Body.String())
	}
	if loc := w.Header().Get("Location"); loc != "/instances/web" {
		t.Errorf("Location = %q, want /instances/web", loc)
	}
	sc := cookieByName(w.Result().Cookies(), SessionCookieName)
	if sc == nil {
		t.Fatal("no session cookie set")
	}
	if sc.Domain != "bento.example.org" {
		t.Errorf("session cookie Domain = %q, want base domain", sc.Domain)
	}
	if !sc.HttpOnly || !sc.Secure {
		t.Error("session cookie missing HttpOnly or Secure")
	}
	sess, ok := svc.sessions.Get(sc.Value)
	if !ok {
		t.Fatal("cookie value is not a stored session ID")
	}
	if sess.UserID != 42 {
		t.Errorf("session UserID = %d, want 42", sess.UserID)
	}
	// Flow cookies are cleared.
	for _, name := range []string{stateCookieName, nonceCookieName, nextCookieName} {
		c := cookieByName(w.Result().Cookies(), name)
		if c == nil || c.MaxAge != -1 {
			t.Errorf("flow cookie %s was not cleared", name)
		}
	}
}

func TestCallbackRejections(t *testing.T) {
	tests := []struct {
		name       string
		setup      func(svc *Service, ex *fakeExchanger, users *fakeUserStore)
		target     func(ex *fakeExchanger) string
		dropState  bool
		wantStatus int
	}{
		{
			name:       "state mismatch",
			target:     func(ex *fakeExchanger) string { return "/callback?code=good-code&state=forged" },
			wantStatus: http.StatusBadRequest,
		},
		{
			name:       "missing state cookie",
			target:     func(ex *fakeExchanger) string { return "/callback?code=good-code&state=" + ex.lastState },
			dropState:  true,
			wantStatus: http.StatusBadRequest,
		},
		{
			name: "provider error param",
			target: func(ex *fakeExchanger) string {
				return "/callback?error=access_denied&state=" + ex.lastState
			},
			wantStatus: http.StatusForbidden,
		},
		{
			name:       "bad code",
			target:     func(ex *fakeExchanger) string { return "/callback?code=wrong&state=" + ex.lastState },
			wantStatus: http.StatusBadGateway,
		},
		{
			name: "unverifiable token",
			setup: func(svc *Service, ex *fakeExchanger, users *fakeUserStore) {
				delete(svc.verifier.(*fakeVerifier).claims, "raw-token")
			},
			target:     func(ex *fakeExchanger) string { return "/callback?code=good-code&state=" + ex.lastState },
			wantStatus: http.StatusUnauthorized,
		},
		{
			name: "nonce mismatch",
			setup: func(svc *Service, ex *fakeExchanger, users *fakeUserStore) {
				svc.verifier.(*fakeVerifier).claims["raw-token"] = Claims{Subject: "subject-1", Nonce: "replayed"}
			},
			target:     func(ex *fakeExchanger) string { return "/callback?code=good-code&state=" + ex.lastState },
			wantStatus: http.StatusBadRequest,
		},
		{
			name: "unknown subject gets 403",
			setup: func(svc *Service, ex *fakeExchanger, users *fakeUserStore) {
				svc.verifier.(*fakeVerifier).claims["raw-token"] = Claims{Subject: "stranger", Nonce: ex.lastNonce}
			},
			target:     func(ex *fakeExchanger) string { return "/callback?code=good-code&state=" + ex.lastState },
			wantStatus: http.StatusForbidden,
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			svc, ex, users := newOIDCService(t)
			_, cookies := doLogin(t, svc, "/login")
			// Default to a verifiable token with the right nonce; each
			// case breaks exactly one thing.
			svc.verifier.(*fakeVerifier).claims["raw-token"] = Claims{Subject: "subject-1", Nonce: ex.lastNonce}
			if tt.setup != nil {
				tt.setup(svc, ex, users)
			}
			if tt.dropState {
				var kept []*http.Cookie
				for _, c := range cookies {
					if c.Name != stateCookieName {
						kept = append(kept, c)
					}
				}
				cookies = kept
			}
			r := callbackRequest(tt.target(ex), cookies)
			w := httptest.NewRecorder()
			svc.CallbackHandler().ServeHTTP(w, r)
			if w.Code != tt.wantStatus {
				t.Fatalf("status = %d, want %d (body %q)", w.Code, tt.wantStatus, w.Body.String())
			}
			if c := cookieByName(w.Result().Cookies(), SessionCookieName); c != nil {
				t.Error("a session cookie was set on a rejected callback")
			}
		})
	}
}

func TestSafeNext(t *testing.T) {
	svc, _, _ := newOIDCService(t) // base domain bento.example.org
	tests := []struct{ in, want string }{
		{"", "/"},
		{"/", "/"},
		{"/instances/web", "/instances/web"},
		// The proxy's redirect from a private instance carries an
		// absolute URL on a subdomain of the base domain (SPEC 9.2, 13);
		// the login flow must return there, ports included.
		{"https://web.bento.example.org/admin?x=1", "https://web.bento.example.org/admin?x=1"},
		{"https://web.bento.example.org:3456/", "https://web.bento.example.org:3456/"},
		{"https://bento.example.org/instances", "https://bento.example.org/instances"},
		// Everything off-site or non-https stays an open-redirect block.
		{"https://evil.example.com/", "/"},
		{"https://evilbento.example.org/", "/"},
		{"http://web.bento.example.org/", "/"},
		{"//evil.example.com/", "/"},
		{"javascript:alert(1)", "/"},
		{"relative/path", "/"},
	}
	for _, tt := range tests {
		if got := svc.safeNext(tt.in); got != tt.want {
			t.Errorf("safeNext(%q) = %q, want %q", tt.in, got, tt.want)
		}
	}
}

func TestLogout(t *testing.T) {
	svc, _, _ := newOIDCService(t)
	sess, err := svc.newSession(42)
	if err != nil {
		t.Fatalf("newSession: %v", err)
	}
	r := httptest.NewRequest("GET", "/logout", nil)
	r.AddCookie(&http.Cookie{Name: SessionCookieName, Value: sess.ID})
	w := httptest.NewRecorder()
	svc.LogoutHandler().ServeHTTP(w, r)

	if w.Code != http.StatusFound {
		t.Fatalf("status = %d, want 302", w.Code)
	}
	if _, ok := svc.sessions.Get(sess.ID); ok {
		t.Error("session survived logout")
	}
	c := cookieByName(w.Result().Cookies(), SessionCookieName)
	if c == nil || c.MaxAge != -1 {
		t.Error("session cookie was not expired")
	}
}

func TestHandlersWithoutOIDCConfigured(t *testing.T) {
	clock := newFakeClock(testEpoch)
	svc, _, _, _ := newTestService(t, clock) // no WithOIDC
	for _, h := range []http.Handler{svc.LoginHandler(), svc.CallbackHandler()} {
		w := httptest.NewRecorder()
		h.ServeHTTP(w, httptest.NewRequest("GET", "/", nil))
		if w.Code != http.StatusInternalServerError {
			t.Errorf("status = %d, want 500 when OIDC is unset", w.Code)
		}
	}
}
