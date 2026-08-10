// Package tlscert obtains and renews the one wildcard certificate for the
// base domain and *.<base domain> over the ACME DNS-01 challenge (SPEC
// section 8).
//
// A wildcard requires DNS-01, and one wildcard is a deliberate choice: a
// per-instance certificate would publish every instance name to the
// Certificate Transparency logs, and would burn a Let's Encrypt issuance
// on every create and rename. The HTTP and TLS-ALPN challenges are
// disabled so a misconfiguration can never silently fall back to a
// challenge that cannot issue the wildcard.
//
// The DNS provider is pluggable: anything satisfying DNSProvider (the
// libdns record appender + deleter pair) works. Cloudflare is the provider
// Bento configures by default, with an API token that should be scoped to
// the _acme-challenge records where the DNS provider supports such a
// limit.
package tlscert

import (
	"context"
	"crypto/tls"
	"errors"
	"fmt"
	"slices"
	"strings"
	"time"

	"github.com/caddyserver/certmagic"
	"github.com/libdns/cloudflare"
)

// DNSProvider sets and deletes the temporary _acme-challenge TXT records.
// It is the certmagic DNS provider pair from libdns, aliased here so
// callers depend on this package, not on certmagic.
type DNSProvider = certmagic.DNSProvider

// Cloudflare returns the Cloudflare DNS provider. Use a scoped API token
// (Zone.DNS:Write on the one zone), never the global API key.
func Cloudflare(apiToken string) DNSProvider {
	return &cloudflare.Provider{APIToken: apiToken}
}

// Config configures the certificate manager.
type Config struct {
	// BaseDomain is the deployment domain, e.g. "bento.foid.space". The
	// certificate covers it and its direct wildcard (SPEC 8).
	BaseDomain string

	// Email is the ACME account contact.
	Email string

	// Provider solves the DNS-01 challenge. Required.
	Provider DNSProvider

	// StorageDir holds the ACME account and issued certificates so a
	// restart does not re-issue. Required.
	StorageDir string

	// CA is the ACME directory endpoint. Empty means Let's Encrypt
	// production; use certmagic.LetsEncryptStagingCA in development to
	// stay clear of the 50-per-week limit (SPEC 8).
	CA string

	// PropagationTimeout bounds the wait for the TXT record to appear
	// in authoritative lookups. Zero keeps the certmagic default.
	PropagationTimeout time.Duration
}

// Manager owns the wildcard certificate: it issues on first need, renews
// in the background, and hands the certificate to the TLS listener.
type Manager struct {
	magic   *certmagic.Config
	cache   *certmagic.Cache
	domains []string
}

// Domains returns the certificate's subject set for baseDomain: the base
// domain itself and its direct wildcard.
func Domains(baseDomain string) []string {
	return []string{baseDomain, "*." + baseDomain}
}

// New validates cfg and builds a Manager. No network traffic happens here;
// issuance starts with Manage.
func New(cfg Config) (*Manager, error) {
	base := strings.TrimSuffix(strings.ToLower(cfg.BaseDomain), ".")
	switch {
	case base == "":
		return nil, errors.New("tlscert: base domain is empty")
	case strings.ContainsAny(base, "*/ "):
		return nil, fmt.Errorf("tlscert: base domain %q must be a bare domain, not a wildcard or URL", cfg.BaseDomain)
	case strings.HasPrefix(base, "."):
		return nil, fmt.Errorf("tlscert: base domain %q starts with a dot", cfg.BaseDomain)
	}
	if cfg.Provider == nil {
		return nil, errors.New("tlscert: DNS provider is nil")
	}
	if cfg.StorageDir == "" {
		return nil, errors.New("tlscert: storage dir is empty")
	}
	ca := cfg.CA
	if ca == "" {
		ca = certmagic.LetsEncryptProductionCA
	}

	m := &Manager{domains: Domains(base)}
	m.cache = certmagic.NewCache(certmagic.CacheOptions{
		GetConfigForCert: func(certmagic.Certificate) (*certmagic.Config, error) {
			return m.magic, nil
		},
	})
	m.magic = certmagic.New(m.cache, certmagic.Config{
		Storage:           &certmagic.FileStorage{Path: cfg.StorageDir},
		DefaultServerName: base,
	})
	issuer := certmagic.NewACMEIssuer(m.magic, certmagic.ACMEIssuer{
		CA:     ca,
		Email:  cfg.Email,
		Agreed: true,
		// The wildcard requires DNS-01 (SPEC 8). Never fall back.
		DisableHTTPChallenge:    true,
		DisableTLSALPNChallenge: true,
		DNS01Solver: &certmagic.DNS01Solver{
			DNSManager: certmagic.DNSManager{
				DNSProvider:        cfg.Provider,
				PropagationTimeout: cfg.PropagationTimeout,
			},
		},
	})
	m.magic.Issuers = []certmagic.Issuer{issuer}
	return m, nil
}

// Domains returns the certificate's subject set.
func (m *Manager) Domains() []string {
	return slices.Clone(m.domains)
}

// Manage obtains the certificate if it is absent and keeps it renewed in
// the background until ctx is canceled. It returns once management is
// started; issuance errors surface through the certmagic maintenance log
// and through GetCertificate.
func (m *Manager) Manage(ctx context.Context) error {
	return m.magic.ManageAsync(ctx, m.domains)
}

// ManageSync obtains the certificate before returning. Use it at first
// start when the proxy must not come up without a certificate.
func (m *Manager) ManageSync(ctx context.Context) error {
	return m.magic.ManageSync(ctx, m.domains)
}

// GetCertificate plugs into tls.Config.GetCertificate.
func (m *Manager) GetCertificate(hello *tls.ClientHelloInfo) (*tls.Certificate, error) {
	return m.magic.GetCertificate(hello)
}

// TLSConfig returns a server TLS configuration serving the managed
// certificate, for the proxy's listeners.
func (m *Manager) TLSConfig() *tls.Config {
	return &tls.Config{
		MinVersion:     tls.VersionTLS12,
		GetCertificate: m.GetCertificate,
		NextProtos:     []string{"h2", "http/1.1"},
	}
}

// Close stops the background renewal maintenance.
func (m *Manager) Close() {
	m.cache.Stop()
}
