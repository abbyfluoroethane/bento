package proxy

import (
	"bytes"
	"context"
	"crypto/tls"
	"errors"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"reflect"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/abbyfluoroethane/bento/internal/types"
)

const testBase = "bento.example.org"

type fakeSource struct {
	instances map[string]types.Instance
	err       error
}

func (f *fakeSource) InstanceByName(_ context.Context, name string) (types.Instance, bool, error) {
	if f.err != nil {
		return types.Instance{}, false, f.err
	}
	inst, ok := f.instances[name]
	return inst, ok, nil
}

// fakeSessions answers the per-request authorization question with a
// fixed decision and records which instance UUIDs were checked.
type fakeSessions struct {
	access  Access
	checked []string
}

func (f *fakeSessions) Access(_ *http.Request, uuid string) Access {
	f.checked = append(f.checked, uuid)
	return f.access
}

// granted and unauthenticated build the two common fakes.
func granted() *fakeSessions         { return &fakeSessions{access: AccessGranted} }
func unauthenticated() *fakeSessions { return &fakeSessions{access: AccessUnauthenticated} }

type roundTripperFunc func(*http.Request) (*http.Response, error)

func (f roundTripperFunc) RoundTrip(r *http.Request) (*http.Response, error) { return f(r) }

func okTransport(record *[]*http.Request) roundTripperFunc {
	var mu sync.Mutex
	return func(r *http.Request) (*http.Response, error) {
		mu.Lock()
		if record != nil {
			*record = append(*record, r.Clone(context.Background()))
		}
		mu.Unlock()
		return &http.Response{
			StatusCode: http.StatusOK,
			Header:     http.Header{},
			Body:       io.NopCloser(strings.NewReader("backend")),
			Request:    r,
		}, nil
	}
}

func newProxy(t *testing.T, src InstanceSource, sessions SessionChecker, control http.Handler, opts ...Option) *Proxy {
	t.Helper()
	p, err := New(testBase, src, sessions, control, opts...)
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	return p
}

// request builds an HTTPS request as the proxy sees it: SNI set, and the
// listener port in the context as net/http does.
func request(host string, port int) *http.Request {
	r := httptest.NewRequest(http.MethodGet, "https://"+host+"/", nil)
	ctx := context.WithValue(r.Context(), http.LocalAddrContextKey,
		&net.TCPAddr{IP: net.IPv4(127, 0, 0, 1), Port: port})
	return r.WithContext(ctx)
}

func runningInstance(name string, vis types.Visibility) types.Instance {
	return types.Instance{
		UUID:       "uuid-" + name,
		Name:       name,
		OwnerID:    42,
		Address:    "10.42.1.2",
		State:      types.StateRunning,
		Visibility: vis,
	}
}

func TestBaseDomainRouting(t *testing.T) {
	control := http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		io.WriteString(w, "control")
	})
	p := newProxy(t, &fakeSource{}, &fakeSessions{}, control)

	t.Run("main port goes to control plane", func(t *testing.T) {
		rec := httptest.NewRecorder()
		p.ServeHTTP(rec, request(testBase, DefaultPort))
		if rec.Code != http.StatusOK || rec.Body.String() != "control" {
			t.Fatalf("got %d %q, want 200 control", rec.Code, rec.Body.String())
		}
	})

	t.Run("high port is 404", func(t *testing.T) {
		rec := httptest.NewRecorder()
		p.ServeHTTP(rec, request(testBase, 3456))
		if rec.Code != http.StatusNotFound {
			t.Fatalf("got %d, want 404", rec.Code)
		}
	})
}

// TestNotFoundIdentical is the SPEC 9.2 requirement: a name that never
// existed, a name in the release cooldown, and an instance with visibility
// off produce byte-identical responses.
func TestNotFoundIdentical(t *testing.T) {
	src := &fakeSource{instances: map[string]types.Instance{
		"hidden": runningInstance("hidden", types.VisibilityOff),
	}}
	p := newProxy(t, src, granted(), nil)

	responses := map[string]*httptest.ResponseRecorder{}
	for _, host := range []string{
		"never-existed." + testBase, // no row
		"cooling-down." + testBase,  // released name: also no row
		"hidden." + testBase,        // visibility off
		"a.b." + testBase,           // nested label
	} {
		rec := httptest.NewRecorder()
		p.ServeHTTP(rec, request(host, DefaultPort))
		responses[host] = rec
	}

	var refHost string
	var ref *httptest.ResponseRecorder
	for host, rec := range responses {
		if rec.Code != http.StatusNotFound {
			t.Fatalf("%s: got %d, want 404", host, rec.Code)
		}
		if ref == nil {
			refHost, ref = host, rec
			continue
		}
		if !bytes.Equal(rec.Body.Bytes(), ref.Body.Bytes()) {
			t.Errorf("%s body differs from %s", host, refHost)
		}
		if !reflect.DeepEqual(rec.Header(), ref.Header()) {
			t.Errorf("%s headers %v differ from %s headers %v", host, rec.Header(), refHost, ref.Header())
		}
	}
	if strings.Contains(ref.Body.String(), "hidden") {
		t.Error("404 page leaks an instance name")
	}
}

func TestVisibilityMatrix(t *testing.T) {
	tests := []struct {
		name       string
		visibility types.Visibility
		port       int
		access     Access
		wantStatus int
	}{
		{"off unauthenticated", types.VisibilityOff, DefaultPort, AccessUnauthenticated, http.StatusNotFound},
		{"off authorized", types.VisibilityOff, DefaultPort, AccessGranted, http.StatusNotFound},
		{"off high port authorized", types.VisibilityOff, 3456, AccessGranted, http.StatusNotFound},
		{"private unauthenticated", types.VisibilityPrivate, DefaultPort, AccessUnauthenticated, http.StatusFound},
		{"private authorized", types.VisibilityPrivate, DefaultPort, AccessGranted, http.StatusOK},
		{"private forbidden", types.VisibilityPrivate, DefaultPort, AccessForbidden, http.StatusNotFound},
		{"public unauthenticated", types.VisibilityPublic, DefaultPort, AccessUnauthenticated, http.StatusOK},
		{"public authorized", types.VisibilityPublic, DefaultPort, AccessGranted, http.StatusOK},
		{"public high port unauthenticated", types.VisibilityPublic, 3456, AccessUnauthenticated, http.StatusFound},
		{"public high port authorized", types.VisibilityPublic, 3456, AccessGranted, http.StatusOK},
		{"public high port forbidden", types.VisibilityPublic, 3456, AccessForbidden, http.StatusNotFound},
		{"private high port unauthenticated", types.VisibilityPrivate, 9999, AccessUnauthenticated, http.StatusFound},
		{"private high port authorized", types.VisibilityPrivate, 9999, AccessGranted, http.StatusOK},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			src := &fakeSource{instances: map[string]types.Instance{
				"box": runningInstance("box", tt.visibility),
			}}
			p := newProxy(t, src, &fakeSessions{access: tt.access}, nil,
				WithTransport(okTransport(nil)))
			rec := httptest.NewRecorder()
			p.ServeHTTP(rec, request("box."+testBase, tt.port))
			if rec.Code != tt.wantStatus {
				t.Fatalf("got %d, want %d", rec.Code, tt.wantStatus)
			}
		})
	}
}

// TestAuthorizationPerRequest pins SPEC 13: a private instance is
// authorized against the owner and the shares by instance UUID on every
// request, and a session that is valid but unauthorized (the stale
// cookie of the name-change scenario) gets the identical 404 a
// nonexistent name gets — never the content, never a distinct error.
func TestAuthorizationPerRequest(t *testing.T) {
	src := &fakeSource{instances: map[string]types.Instance{
		"box": runningInstance("box", types.VisibilityPrivate),
	}}

	t.Run("check keys on the instance UUID", func(t *testing.T) {
		sessions := granted()
		p := newProxy(t, src, sessions, nil, WithTransport(okTransport(nil)))
		rec := httptest.NewRecorder()
		p.ServeHTTP(rec, request("box."+testBase, DefaultPort))
		if rec.Code != http.StatusOK {
			t.Fatalf("got %d, want 200", rec.Code)
		}
		if len(sessions.checked) != 1 || sessions.checked[0] != "uuid-box" {
			t.Errorf("authorization checked %v, want [uuid-box]", sessions.checked)
		}
	})

	t.Run("forbidden matches a nonexistent name byte for byte", func(t *testing.T) {
		p := newProxy(t, src, &fakeSessions{access: AccessForbidden}, nil,
			WithTransport(okTransport(nil)))
		forbidden := httptest.NewRecorder()
		p.ServeHTTP(forbidden, request("box."+testBase, DefaultPort))
		missing := httptest.NewRecorder()
		p.ServeHTTP(missing, request("ghost."+testBase, DefaultPort))
		if forbidden.Code != http.StatusNotFound {
			t.Fatalf("forbidden request: got %d, want 404", forbidden.Code)
		}
		if !bytes.Equal(forbidden.Body.Bytes(), missing.Body.Bytes()) {
			t.Error("forbidden body differs from the nonexistent-name body")
		}
		if !reflect.DeepEqual(forbidden.Header(), missing.Header()) {
			t.Error("forbidden headers differ from the nonexistent-name headers")
		}
	})

	t.Run("public default port skips the check", func(t *testing.T) {
		pubSrc := &fakeSource{instances: map[string]types.Instance{
			"pub": runningInstance("pub", types.VisibilityPublic),
		}}
		sessions := &fakeSessions{access: AccessForbidden}
		p := newProxy(t, pubSrc, sessions, nil, WithTransport(okTransport(nil)))
		rec := httptest.NewRecorder()
		p.ServeHTTP(rec, request("pub."+testBase, DefaultPort))
		if rec.Code != http.StatusOK {
			t.Fatalf("got %d, want 200 (public forwards without authentication)", rec.Code)
		}
		if len(sessions.checked) != 0 {
			t.Errorf("authorization ran for a public default-port request: %v", sessions.checked)
		}
	})
}

func TestRedirectToLogin(t *testing.T) {
	src := &fakeSource{instances: map[string]types.Instance{
		"box": runningInstance("box", types.VisibilityPrivate),
	}}
	p := newProxy(t, src, unauthenticated(), nil)
	rec := httptest.NewRecorder()
	r := request("box."+testBase, DefaultPort)
	r.URL.Path = "/admin"
	r.URL.RawQuery = "x=1"
	p.ServeHTTP(rec, r)

	if rec.Code != http.StatusFound {
		t.Fatalf("got %d, want 302", rec.Code)
	}
	loc, err := url.Parse(rec.Header().Get("Location"))
	if err != nil {
		t.Fatalf("parse Location: %v", err)
	}
	if got, want := loc.Scheme+"://"+loc.Host+loc.Path, "https://"+testBase+"/login"; got != want {
		t.Errorf("login URL = %q, want %q", got, want)
	}
	next := loc.Query().Get("next")
	if want := "https://box." + testBase + "/admin?x=1"; next != want {
		t.Errorf("next = %q, want %q", next, want)
	}
}

func TestPortSelection(t *testing.T) {
	tests := []struct {
		name         string
		httpPort     int
		listenerPort int
		wantTarget   string
	}{
		{"default port 80", 0, DefaultPort, "10.42.1.2:80"},
		{"port command applied", 8080, DefaultPort, "10.42.1.2:8080"},
		{"high port overrides default", 8080, 3456, "10.42.1.2:3456"},
		{"high port range end", 0, 9999, "10.42.1.2:9999"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			inst := runningInstance("box", types.VisibilityPublic)
			inst.HTTPPort = tt.httpPort
			src := &fakeSource{instances: map[string]types.Instance{"box": inst}}
			var seen []*http.Request
			p := newProxy(t, src, granted(), nil,
				WithTransport(okTransport(&seen)))
			rec := httptest.NewRecorder()
			p.ServeHTTP(rec, request("box."+testBase, tt.listenerPort))
			if rec.Code != http.StatusOK {
				t.Fatalf("got %d, want 200", rec.Code)
			}
			if len(seen) != 1 {
				t.Fatalf("transport called %d times, want 1", len(seen))
			}
			out := seen[0]
			if out.URL.Scheme != "http" || out.URL.Host != tt.wantTarget {
				t.Errorf("forwarded to %s://%s, want http://%s", out.URL.Scheme, out.URL.Host, tt.wantTarget)
			}
		})
	}
}

func TestForwardedHeaders(t *testing.T) {
	inst := runningInstance("box", types.VisibilityPublic)
	src := &fakeSource{instances: map[string]types.Instance{"box": inst}}
	var seen []*http.Request
	p := newProxy(t, src, &fakeSessions{}, nil, WithTransport(okTransport(&seen)))

	rec := httptest.NewRecorder()
	r := request("box."+testBase, DefaultPort)
	r.RemoteAddr = "192.0.2.7:55555"
	p.ServeHTTP(rec, r)

	if len(seen) != 1 {
		t.Fatalf("transport called %d times, want 1", len(seen))
	}
	out := seen[0]
	if got := out.Header.Get("X-Forwarded-For"); got != "192.0.2.7" {
		t.Errorf("X-Forwarded-For = %q, want 192.0.2.7", got)
	}
	if got := out.Header.Get("X-Forwarded-Host"); got != "box."+testBase {
		t.Errorf("X-Forwarded-Host = %q, want box.%s", got, testBase)
	}
	if got := out.Header.Get("X-Forwarded-Proto"); got != "https" {
		t.Errorf("X-Forwarded-Proto = %q, want https", got)
	}
	if out.Host != "box."+testBase {
		t.Errorf("outbound Host = %q, want box.%s", out.Host, testBase)
	}
}

// TestForwardEndToEnd runs a real backend to prove the whole path works,
// not only the fake transport.
func TestForwardEndToEnd(t *testing.T) {
	backend := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		io.WriteString(w, "hello from "+r.Host)
	}))
	defer backend.Close()

	host, portStr, err := net.SplitHostPort(strings.TrimPrefix(backend.URL, "http://"))
	if err != nil {
		t.Fatal(err)
	}
	port, _ := strconv.Atoi(portStr)

	inst := runningInstance("box", types.VisibilityPublic)
	inst.Address = host
	inst.HTTPPort = port
	src := &fakeSource{instances: map[string]types.Instance{"box": inst}}
	p := newProxy(t, src, &fakeSessions{}, nil)

	rec := httptest.NewRecorder()
	p.ServeHTTP(rec, request("box."+testBase, DefaultPort))
	if rec.Code != http.StatusOK {
		t.Fatalf("got %d, want 200", rec.Code)
	}
	if want := "hello from box." + testBase; rec.Body.String() != want {
		t.Errorf("body = %q, want %q", rec.Body.String(), want)
	}
}

func TestUnavailable(t *testing.T) {
	t.Run("stopped instance answers 503 without dialing", func(t *testing.T) {
		inst := runningInstance("dbbox", types.VisibilityPublic)
		inst.State = types.StateStopped
		src := &fakeSource{instances: map[string]types.Instance{"dbbox": inst}}
		p := newProxy(t, src, &fakeSessions{}, nil,
			WithTransport(roundTripperFunc(func(*http.Request) (*http.Response, error) {
				t.Error("transport must not be called for a stopped instance")
				return nil, errors.New("unreachable")
			})))
		rec := httptest.NewRecorder()
		p.ServeHTTP(rec, request("dbbox."+testBase, DefaultPort))

		if rec.Code != http.StatusServiceUnavailable {
			t.Fatalf("got %d, want 503", rec.Code)
		}
		if got := rec.Header().Get("Retry-After"); got != "5" {
			t.Errorf("Retry-After = %q, want 5", got)
		}
		body := rec.Body.String()
		if !strings.Contains(body, "dbbox") {
			t.Error("503 page does not name the instance")
		}
		if !strings.Contains(body, "stopped") {
			t.Error("503 page does not name the state")
		}
		if strings.Contains(body, "42") {
			t.Error("503 page must not name the owner (SPEC 14.5)")
		}
		if strings.Contains(body, "<script") {
			t.Error("503 page must not contain JavaScript")
		}
	})

	t.Run("starting instance answers 503 immediately", func(t *testing.T) {
		inst := runningInstance("slowbox", types.VisibilityPublic)
		inst.State = types.StateStarting
		src := &fakeSource{instances: map[string]types.Instance{"slowbox": inst}}
		p := newProxy(t, src, &fakeSessions{}, nil,
			WithTransport(roundTripperFunc(func(*http.Request) (*http.Response, error) {
				t.Error("transport must not be called while starting")
				return nil, errors.New("unreachable")
			})))
		rec := httptest.NewRecorder()
		start := time.Now()
		p.ServeHTTP(rec, request("slowbox."+testBase, DefaultPort))
		if elapsed := time.Since(start); elapsed > time.Second {
			t.Errorf("503 took %v; the request must not be held", elapsed)
		}
		if rec.Code != http.StatusServiceUnavailable {
			t.Fatalf("got %d, want 503", rec.Code)
		}
		if !strings.Contains(rec.Body.String(), "starting") {
			t.Error("503 page does not name the starting state")
		}
	})

	t.Run("refused connection answers 503", func(t *testing.T) {
		inst := runningInstance("box", types.VisibilityPublic)
		src := &fakeSource{instances: map[string]types.Instance{"box": inst}}
		p := newProxy(t, src, &fakeSessions{}, nil,
			WithTransport(roundTripperFunc(func(*http.Request) (*http.Response, error) {
				return nil, errors.New("dial tcp: connection refused")
			})))
		rec := httptest.NewRecorder()
		p.ServeHTTP(rec, request("box."+testBase, DefaultPort))
		if rec.Code != http.StatusServiceUnavailable {
			t.Fatalf("got %d, want 503", rec.Code)
		}
		if got := rec.Header().Get("Retry-After"); got != "5" {
			t.Errorf("Retry-After = %q, want 5", got)
		}
		if !strings.Contains(rec.Body.String(), "box") {
			t.Error("503 page does not name the instance")
		}
	})
}

// TestUnavailablePageEscapes proves a hostile instance name cannot inject
// markup into the 503 page.
func TestUnavailablePageEscapes(t *testing.T) {
	inst := runningInstance("evil", types.VisibilityPublic)
	inst.Name = `<script>alert(1)</script>`
	inst.State = types.StateStopped
	src := &fakeSource{instances: map[string]types.Instance{"evil": inst}}
	p := newProxy(t, src, &fakeSessions{}, nil)

	rec := httptest.NewRecorder()
	p.ServeHTTP(rec, request("evil."+testBase, DefaultPort))
	body := rec.Body.String()
	if strings.Contains(body, "<script>alert(1)</script>") {
		t.Error("instance name was not escaped")
	}
	if !strings.Contains(body, "&lt;script&gt;") {
		t.Error("escaped instance name missing from page")
	}
}

// fakeLastSeen records TouchLastSeen calls.
type fakeLastSeen struct{ touched []string }

func (f *fakeLastSeen) TouchLastSeen(_ context.Context, uuid string) error {
	f.touched = append(f.touched, uuid)
	return nil
}

// TestLastSeenTouchedOnForward pins SPEC 12: last_seen_at records the
// last HTTP request, so a forwarded request touches the column and a
// request that never reaches the instance does not.
func TestLastSeenTouchedOnForward(t *testing.T) {
	running := runningInstance("box", types.VisibilityPublic)
	stopped := runningInstance("idle", types.VisibilityPublic)
	stopped.State = types.StateStopped
	src := &fakeSource{instances: map[string]types.Instance{
		"box": running, "idle": stopped,
	}}
	rec := &fakeLastSeen{}
	p := newProxy(t, src, granted(), nil,
		WithTransport(okTransport(nil)), WithLastSeen(rec))

	w := httptest.NewRecorder()
	p.ServeHTTP(w, request("box."+testBase, DefaultPort))
	if w.Code != http.StatusOK {
		t.Fatalf("got %d, want 200", w.Code)
	}
	if len(rec.touched) != 1 || rec.touched[0] != "uuid-box" {
		t.Fatalf("touched %v, want [uuid-box]", rec.touched)
	}

	// A 503 for a stopped instance is not a served request.
	w = httptest.NewRecorder()
	p.ServeHTTP(w, request("idle."+testBase, DefaultPort))
	if w.Code != http.StatusServiceUnavailable {
		t.Fatalf("got %d, want 503", w.Code)
	}
	if len(rec.touched) != 1 {
		t.Errorf("touched %v after a 503, want no new touch", rec.touched)
	}
}

func TestSourceErrorIs500(t *testing.T) {
	p := newProxy(t, &fakeSource{err: errors.New("db locked")}, &fakeSessions{}, nil)
	rec := httptest.NewRecorder()
	p.ServeHTTP(rec, request("box."+testBase, DefaultPort))
	if rec.Code != http.StatusInternalServerError {
		t.Fatalf("got %d, want 500", rec.Code)
	}
}

func TestRequestHost(t *testing.T) {
	tests := []struct {
		name string
		host string
		sni  string
		want string
	}{
		{"plain host", "box." + testBase, "", "box." + testBase},
		{"host with port", "box." + testBase + ":3456", "", "box." + testBase},
		{"uppercase", "BOX." + strings.ToUpper(testBase), "", "box." + testBase},
		{"trailing dot", "box." + testBase + ".", "", "box." + testBase},
		{"sni wins over host header", "other.example.net", "box." + testBase, "box." + testBase},
		{"empty", "", "", ""},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			r := httptest.NewRequest(http.MethodGet, "http://placeholder/", nil)
			r.Host = tt.host
			r.TLS = nil
			if tt.sni != "" {
				r.TLS = &tls.ConnectionState{ServerName: tt.sni}
			}
			if got := requestHost(r); got != tt.want {
				t.Errorf("requestHost = %q, want %q", got, tt.want)
			}
		})
	}
}

func TestListenerPort(t *testing.T) {
	r := httptest.NewRequest(http.MethodGet, "https://x/", nil)
	if got := listenerPort(r); got != DefaultPort {
		t.Errorf("no local addr: got %d, want %d", got, DefaultPort)
	}
	if got := listenerPort(request("x", 4242)); got != 4242 {
		t.Errorf("got %d, want 4242", got)
	}
}

func TestNewValidation(t *testing.T) {
	if _, err := New("", &fakeSource{}, nil, nil); err == nil {
		t.Error("empty base domain accepted")
	}
	if _, err := New(testBase, nil, nil, nil); err == nil {
		t.Error("nil instance source accepted")
	}
	p, err := New("Bento.Example.Org.", &fakeSource{}, nil, nil)
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	if p.baseDomain != testBase {
		t.Errorf("base domain = %q, want normalized %q", p.baseDomain, testBase)
	}
}

func TestPorts(t *testing.T) {
	ports := Ports()
	if len(ports) != 7001 {
		t.Fatalf("len(Ports()) = %d, want 7001", len(ports))
	}
	if ports[0] != DefaultPort {
		t.Errorf("first port = %d, want %d", ports[0], DefaultPort)
	}
	if ports[1] != HighPortMin || ports[len(ports)-1] != HighPortMax {
		t.Errorf("range = %d..%d, want %d..%d", ports[1], ports[len(ports)-1], HighPortMin, HighPortMax)
	}
}

func TestListenAll(t *testing.T) {
	t.Run("binds every port", func(t *testing.T) {
		var addrs []string
		listen := func(network, addr string) (net.Listener, error) {
			if network != "tcp" {
				t.Errorf("network = %q, want tcp", network)
			}
			addrs = append(addrs, addr)
			return newFakeListener(addr), nil
		}
		listeners, err := listenAll("0.0.0.0", Ports(), listen)
		if err != nil {
			t.Fatalf("listenAll: %v", err)
		}
		defer closeAll(listeners)
		if len(listeners) != 7001 {
			t.Fatalf("bound %d listeners, want 7001", len(listeners))
		}
		if addrs[0] != "0.0.0.0:443" {
			t.Errorf("first addr = %q, want 0.0.0.0:443", addrs[0])
		}
		if last := addrs[len(addrs)-1]; last != "0.0.0.0:9999" {
			t.Errorf("last addr = %q, want 0.0.0.0:9999", last)
		}
	})

	t.Run("failure closes what was bound", func(t *testing.T) {
		var opened []*fakeListener
		listen := func(_, addr string) (net.Listener, error) {
			if strings.HasSuffix(addr, ":3001") {
				return nil, errors.New("address in use")
			}
			ln := newFakeListener(addr)
			opened = append(opened, ln)
			return ln, nil
		}
		_, err := listenAll("127.0.0.1", []int{443, 3000, 3001, 3002}, listen)
		if err == nil {
			t.Fatal("listenAll succeeded, want error")
		}
		if !strings.Contains(err.Error(), "3001") {
			t.Errorf("error %q does not name the failing port", err)
		}
		if len(opened) != 2 {
			t.Fatalf("opened %d listeners before failure, want 2", len(opened))
		}
		for _, ln := range opened {
			if !ln.isClosed() {
				t.Errorf("listener %s left open after failure", ln.Addr())
			}
		}
	})
}

func TestServeStopsOnContextCancel(t *testing.T) {
	p := newProxy(t, &fakeSource{}, &fakeSessions{}, nil)
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		done <- p.Serve(ctx, "127.0.0.1", nil, func(_, addr string) (net.Listener, error) {
			return newFakeListener(addr), nil
		})
	}()
	// Let the servers start, then cancel.
	time.Sleep(50 * time.Millisecond)
	cancel()
	select {
	case err := <-done:
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("Serve returned %v, want context.Canceled", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("Serve did not return after cancel")
	}
}

func closeAll(listeners []net.Listener) {
	for _, ln := range listeners {
		_ = ln.Close()
	}
}

type fakeListener struct {
	addr   string
	closed chan struct{}
	once   sync.Once
}

func newFakeListener(addr string) *fakeListener {
	return &fakeListener{addr: addr, closed: make(chan struct{})}
}

func (l *fakeListener) Accept() (net.Conn, error) {
	<-l.closed
	return nil, net.ErrClosed
}

func (l *fakeListener) Close() error {
	l.once.Do(func() { close(l.closed) })
	return nil
}

func (l *fakeListener) Addr() net.Addr {
	return &net.UnixAddr{Name: l.addr, Net: "tcp"}
}

func (l *fakeListener) isClosed() bool {
	select {
	case <-l.closed:
		return true
	default:
		return false
	}
}
