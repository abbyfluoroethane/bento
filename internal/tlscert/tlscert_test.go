package tlscert

import (
	"context"
	"crypto/tls"
	"reflect"
	"testing"
	"time"

	"github.com/caddyserver/certmagic"
	"github.com/libdns/cloudflare"
	"github.com/libdns/libdns"
)

// fakeProvider satisfies DNSProvider without touching any DNS zone.
type fakeProvider struct{}

func (fakeProvider) AppendRecords(_ context.Context, _ string, recs []libdns.Record) ([]libdns.Record, error) {
	return recs, nil
}

func (fakeProvider) DeleteRecords(_ context.Context, _ string, recs []libdns.Record) ([]libdns.Record, error) {
	return recs, nil
}

func validConfig(t *testing.T) Config {
	t.Helper()
	return Config{
		BaseDomain: "bento.example.org",
		Email:      "operator@example.org",
		Provider:   fakeProvider{},
		StorageDir: t.TempDir(),
	}
}

func TestDomains(t *testing.T) {
	got := Domains("bento.example.org")
	want := []string{"bento.example.org", "*.bento.example.org"}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("Domains = %v, want %v", got, want)
	}
}

func TestNewValidation(t *testing.T) {
	tests := []struct {
		name    string
		mutate  func(*Config)
		wantErr bool
	}{
		{"valid", func(*Config) {}, false},
		{"empty domain", func(c *Config) { c.BaseDomain = "" }, true},
		{"wildcard domain", func(c *Config) { c.BaseDomain = "*.bento.example.org" }, true},
		{"url not domain", func(c *Config) { c.BaseDomain = "https://bento.example.org" }, true},
		{"nil provider", func(c *Config) { c.Provider = nil }, true},
		{"empty storage dir", func(c *Config) { c.StorageDir = "" }, true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg := validConfig(t)
			tt.mutate(&cfg)
			m, err := New(cfg)
			if (err != nil) != tt.wantErr {
				t.Fatalf("New err = %v, wantErr = %v", err, tt.wantErr)
			}
			if m != nil {
				m.Close()
			}
		})
	}
}

func TestNewNormalizesDomain(t *testing.T) {
	cfg := validConfig(t)
	cfg.BaseDomain = "Bento.Example.Org."
	m, err := New(cfg)
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	defer m.Close()

	want := []string{"bento.example.org", "*.bento.example.org"}
	if got := m.Domains(); !reflect.DeepEqual(got, want) {
		t.Errorf("Domains = %v, want %v", got, want)
	}
}

// TestNewIssuerWiring checks the SPEC 8 requirements on the ACME issuer:
// DNS-01 only, production CA by default, pluggable provider carried
// through.
func TestNewIssuerWiring(t *testing.T) {
	cfg := validConfig(t)
	cfg.PropagationTimeout = 4 * time.Minute
	m, err := New(cfg)
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	defer m.Close()

	if n := len(m.magic.Issuers); n != 1 {
		t.Fatalf("got %d issuers, want 1", n)
	}
	issuer, ok := m.magic.Issuers[0].(*certmagic.ACMEIssuer)
	if !ok {
		t.Fatalf("issuer is %T, want *certmagic.ACMEIssuer", m.magic.Issuers[0])
	}
	if issuer.CA != certmagic.LetsEncryptProductionCA {
		t.Errorf("CA = %q, want production default", issuer.CA)
	}
	if issuer.Email != cfg.Email {
		t.Errorf("Email = %q, want %q", issuer.Email, cfg.Email)
	}
	if !issuer.Agreed {
		t.Error("Agreed = false")
	}
	if !issuer.DisableHTTPChallenge || !issuer.DisableTLSALPNChallenge {
		t.Error("HTTP and TLS-ALPN challenges must be disabled: a wildcard requires DNS-01 (SPEC 8)")
	}
	solver, ok := issuer.DNS01Solver.(*certmagic.DNS01Solver)
	if !ok {
		t.Fatalf("DNS01Solver is %T, want *certmagic.DNS01Solver", issuer.DNS01Solver)
	}
	if _, ok := solver.DNSProvider.(fakeProvider); !ok {
		t.Errorf("DNSProvider is %T, want the injected fakeProvider", solver.DNSProvider)
	}
	if solver.PropagationTimeout != cfg.PropagationTimeout {
		t.Errorf("PropagationTimeout = %v, want %v", solver.PropagationTimeout, cfg.PropagationTimeout)
	}
}

func TestNewCustomCA(t *testing.T) {
	cfg := validConfig(t)
	cfg.CA = certmagic.LetsEncryptStagingCA
	m, err := New(cfg)
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	defer m.Close()

	issuer := m.magic.Issuers[0].(*certmagic.ACMEIssuer)
	if issuer.CA != certmagic.LetsEncryptStagingCA {
		t.Errorf("CA = %q, want staging", issuer.CA)
	}
}

func TestTLSConfig(t *testing.T) {
	m, err := New(validConfig(t))
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	defer m.Close()

	tc := m.TLSConfig()
	if tc.GetCertificate == nil {
		t.Error("GetCertificate is nil")
	}
	if tc.MinVersion != tls.VersionTLS12 {
		t.Errorf("MinVersion = %x, want TLS 1.2", tc.MinVersion)
	}
}

func TestCloudflare(t *testing.T) {
	p := Cloudflare("token-123")
	cf, ok := p.(*cloudflare.Provider)
	if !ok {
		t.Fatalf("Cloudflare returned %T, want *cloudflare.Provider", p)
	}
	if cf.APIToken != "token-123" {
		t.Errorf("APIToken = %q, want token-123", cf.APIToken)
	}
}
