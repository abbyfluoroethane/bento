// Package proxy is the HTTP proxy: hostname-based routing to instances,
// visibility enforcement, and the static 503 error page (SPEC sections 9
// and 14.5).
//
// The proxy resolves the instance name from the request hostname on every
// request (SPEC 7.1), reads the instance address from the store — known
// before the instance boots (SPEC 6.2) — and forwards over plain HTTP on
// the private network. TLS terminates here with the wildcard certificate
// from internal/tlscert.
package proxy

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"strconv"
	"strings"
	"time"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// DefaultPort is the main HTTPS port. A request on this port goes to the
// instance's default HTTP port, set with the `port` command (SPEC 9.1).
// An operator who terminates TLS in front of Bento moves the listener
// with WithPorts; the routing rules follow the port, not the number.
const DefaultPort = 443

// HighPortMin and HighPortMax bound the extra listener range. A request on
// port N in this range goes to port N on the instance, and is always
// private regardless of the visibility value (SPEC 9.1, 9.2).
const (
	HighPortMin = 3000
	HighPortMax = 9999
)

// defaultHTTPPort is the instance-side default when no `port` command has
// run (SPEC 9.1).
const defaultHTTPPort = 80

// InstanceSource resolves an instance name to its row. The address in the
// row is assigned at creation time (SPEC 6.2), so a lookup never waits for
// a boot. ok is false when no instance holds the name: a name that never
// existed, a deleted name, and a name in the release cooldown all look the
// same here, which lets the proxy answer all three identically (SPEC 9.2).
type InstanceSource interface {
	InstanceByName(ctx context.Context, name string) (inst types.Instance, ok bool, err error)
}

// Access is the proxy's view of one request's standing on one instance.
type Access int

const (
	// AccessUnauthenticated means the request carries no valid session
	// or token. The proxy redirects to the login page (SPEC 9.2).
	AccessUnauthenticated Access = iota
	// AccessForbidden means the caller is authenticated but neither owns
	// the instance nor holds a share on its UUID (SPEC 13). The proxy
	// answers with the uniform 404, hiding the instance.
	AccessForbidden
	// AccessGranted means the caller owns the instance or holds a share.
	AccessGranted
)

// SessionChecker answers the SPEC 13 authorization question for one
// request: not only whether a valid session or token is present, but
// whether that identity owns the instance or holds a share keyed on its
// UUID. Authorization runs on every request, so a cookie held from
// before a name changed hands grants nothing. The control plane
// implements it; the proxy never inspects credentials itself.
type SessionChecker interface {
	Access(r *http.Request, instanceUUID string) Access
}

// LastSeenRecorder records a forwarded HTTP request against the
// instance's last_seen_at column (SPEC 12: the column records the last
// SSH connection or HTTP request).
type LastSeenRecorder interface {
	TouchLastSeen(ctx context.Context, uuid string) error
}

// Proxy routes requests by hostname. It implements http.Handler; the same
// handler serves port 443 and the 3000-9999 range, reading the listener
// port from the request context.
type Proxy struct {
	baseDomain string
	instances  InstanceSource
	sessions   SessionChecker
	control    http.Handler
	loginURL   string
	transport  http.RoundTripper
	lastSeen   LastSeenRecorder

	// mainPort carries the base domain and an instance's default HTTP
	// port; highMin through highMax are the always-private extra
	// listeners (SPEC 9.1). WithPorts overrides the SPEC defaults.
	mainPort int
	highMin  int
	highMax  int
}

// Option configures a Proxy.
type Option func(*Proxy)

// WithTransport replaces the outbound round tripper. Tests inject fakes;
// production keeps the default short-dial transport.
func WithTransport(rt http.RoundTripper) Option {
	return func(p *Proxy) { p.transport = rt }
}

// WithLoginURL replaces the dashboard login URL that unauthenticated
// requests for private instances redirect to. The default is
// https://<base domain>/login.
func WithLoginURL(u string) Option {
	return func(p *Proxy) { p.loginURL = u }
}

// WithLastSeen records every forwarded request against the instance
// (SPEC 12). A nil recorder leaves last_seen_at untouched.
func WithLastSeen(rec LastSeenRecorder) Option {
	return func(p *Proxy) { p.lastSeen = rec }
}

// WithPorts moves the listeners off the SPEC 9.1 defaults. main is the
// port that carries the base domain and an instance's default HTTP
// port; highMin through highMax are the always-private extra ports. A
// non-positive value leaves that setting at its default.
//
// The main port moves when something else terminates TLS in front of
// Bento and forwards to it on a private port. Everything the proxy
// decides from the listener port — the control plane on the base
// domain, `public` applying only to the default port — follows main
// (SPEC 9.2).
func WithPorts(main, highMin, highMax int) Option {
	return func(p *Proxy) {
		if main > 0 {
			p.mainPort = main
		}
		if highMin > 0 {
			p.highMin = highMin
		}
		if highMax > 0 {
			p.highMax = highMax
		}
	}
}

// New builds a Proxy. Requests for baseDomain itself go to control (the
// dashboard and the OIDC login flow); requests for <name>.<baseDomain>
// resolve through instances.
func New(baseDomain string, instances InstanceSource, sessions SessionChecker, control http.Handler, opts ...Option) (*Proxy, error) {
	baseDomain = strings.TrimSuffix(strings.ToLower(baseDomain), ".")
	if baseDomain == "" {
		return nil, errors.New("proxy: base domain is empty")
	}
	if instances == nil {
		return nil, errors.New("proxy: instance source is nil")
	}
	p := &Proxy{
		baseDomain: baseDomain,
		instances:  instances,
		sessions:   sessions,
		control:    control,
		loginURL:   "https://" + baseDomain + "/login",
		transport:  defaultTransport(),
		mainPort:   DefaultPort,
		highMin:    HighPortMin,
		highMax:    HighPortMax,
	}
	for _, opt := range opts {
		opt(p)
	}
	if p.highMin > p.highMax {
		return nil, fmt.Errorf("proxy: high port range %d-%d is empty", p.highMin, p.highMax)
	}
	if p.mainPort >= p.highMin && p.mainPort <= p.highMax {
		return nil, fmt.Errorf("proxy: main port %d falls inside the high port range %d-%d", p.mainPort, p.highMin, p.highMax)
	}
	return p, nil
}

// defaultTransport dials with a short timeout so a dead target answers
// with the 503 page quickly instead of holding the request (SPEC 9.3).
func defaultTransport() http.RoundTripper {
	return &http.Transport{
		DialContext:         (&net.Dialer{Timeout: 3 * time.Second}).DialContext,
		MaxIdleConns:        100,
		MaxIdleConnsPerHost: 8,
		IdleConnTimeout:     90 * time.Second,
	}
}

// ServeHTTP routes one request: base domain to the control plane, instance
// names through the visibility rules of SPEC 9.2, everything else to the
// uniform 404.
func (p *Proxy) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	host := requestHost(r)
	port := p.listenerPort(r)

	if host == p.baseDomain {
		// The control plane answers only on the main port. The high
		// ports bind nothing for the base domain.
		if port == p.mainPort && p.control != nil {
			p.control.ServeHTTP(w, r)
			return
		}
		p.notFound(w)
		return
	}

	name, ok := strings.CutSuffix(host, "."+p.baseDomain)
	if !ok || name == "" || strings.Contains(name, ".") {
		p.notFound(w)
		return
	}

	inst, ok, err := p.instances.InstanceByName(r.Context(), name)
	if err != nil {
		http.Error(w, "internal error", http.StatusInternalServerError)
		return
	}
	if !ok || inst.Visibility == types.VisibilityOff {
		// A name that does not exist, a name in the release cooldown,
		// and an instance with visibility off answer byte-identically,
		// so a visitor cannot probe which names exist (SPEC 9.2, 7.3).
		p.notFound(w)
		return
	}

	// Ports 3000-9999 are always private; `public` applies only to the
	// default HTTP port (SPEC 9.2). A private request is authorized
	// against the owner and the shares of the instance on every request
	// (SPEC 13): an authenticated user without access gets the same 404
	// as a name that does not exist.
	private := inst.Visibility == types.VisibilityPrivate || port != p.mainPort
	if private {
		switch p.access(r, inst.UUID) {
		case AccessGranted:
		case AccessUnauthenticated:
			p.redirectToLogin(w, r)
			return
		default:
			p.notFound(w)
			return
		}
	}

	targetPort := port
	if port == p.mainPort {
		targetPort = inst.HTTPPort
		if targetPort == 0 {
			targetPort = defaultHTTPPort
		}
	}

	if inst.State != types.StateRunning {
		// Answer at once; never hold the request until the instance is
		// up (SPEC 9.3).
		p.unavailable(w, inst)
		return
	}

	// SPEC 12: last_seen_at records the last HTTP request.
	if p.lastSeen != nil {
		_ = p.lastSeen.TouchLastSeen(r.Context(), inst.UUID)
	}
	p.forward(w, r, inst, targetPort)
}

// access asks the control plane for the SPEC 13 decision. With no
// session checker wired nothing can be authorized, so every private
// request is treated as unauthenticated.
func (p *Proxy) access(r *http.Request, instanceUUID string) Access {
	if p.sessions == nil {
		return AccessUnauthenticated
	}
	return p.sessions.Access(r, instanceUUID)
}

func (p *Proxy) redirectToLogin(w http.ResponseWriter, r *http.Request) {
	next := "https://" + r.Host + r.URL.RequestURI()
	http.Redirect(w, r, p.loginURL+"?next="+url.QueryEscape(next), http.StatusFound)
}

// forward proxies the request to targetPort on the instance address. A
// transport error (refused connection, dial timeout) becomes the 503 page.
func (p *Proxy) forward(w http.ResponseWriter, r *http.Request, inst types.Instance, targetPort int) {
	target := net.JoinHostPort(inst.Address, strconv.Itoa(targetPort))
	rp := &httputil.ReverseProxy{
		Rewrite: func(pr *httputil.ProxyRequest) {
			pr.Out.URL.Scheme = "http"
			pr.Out.URL.Host = target
			pr.Out.Host = pr.In.Host
			// Sets X-Forwarded-Proto, X-Forwarded-Host, and
			// X-Forwarded-For (SPEC 9).
			pr.SetXForwarded()
		},
		Transport: p.transport,
		ErrorHandler: func(w http.ResponseWriter, _ *http.Request, _ error) {
			p.unavailable(w, inst)
		},
	}
	rp.ServeHTTP(w, r)
}

// requestHost extracts the hostname the client asked for. TLS Server Name
// Indication wins when present (SPEC 9); the Host header covers tests and
// any plain listener. The port, case, and a trailing dot are stripped.
func requestHost(r *http.Request) string {
	host := r.Host
	if r.TLS != nil && r.TLS.ServerName != "" {
		host = r.TLS.ServerName
	}
	if h, _, err := net.SplitHostPort(host); err == nil {
		host = h
	}
	return strings.TrimSuffix(strings.ToLower(host), ".")
}

// listenerPort reads the local port the request arrived on. net/http sets
// LocalAddrContextKey on every request; the main port covers the
// fallback, so a request with no local address routes as if it arrived
// on the port that carries the base domain.
func (p *Proxy) listenerPort(r *http.Request) int {
	addr, ok := r.Context().Value(http.LocalAddrContextKey).(net.Addr)
	if !ok {
		return p.mainPort
	}
	_, portStr, err := net.SplitHostPort(addr.String())
	if err != nil {
		return p.mainPort
	}
	port, err := strconv.Atoi(portStr)
	if err != nil {
		return p.mainPort
	}
	return port
}
