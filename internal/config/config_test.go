package config

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestParseDefaults(t *testing.T) {
	cfg, err := Parse([]byte(`base_domain = "bento.example.org"`))
	if err != nil {
		t.Fatalf("Parse: %v", err)
	}
	tests := []struct {
		name string
		got  any
		want any
	}{
		{"BaseDomain", cfg.BaseDomain, "bento.example.org"},
		{"LibvirtURI", cfg.LibvirtURI, "qemu:///system"},
		{"ImageDir", cfg.ImageDir, "/var/lib/bento/images"},
		{"StorageDir", cfg.StorageDir, "/var/lib/bento/storage"},
		{"DBPath", cfg.DBPath, "/var/lib/bento/bento.db"},
		{"OvercommitRatio", cfg.OvercommitRatio, 1.0},
		{"NameCooldown", cfg.NameCooldown.Std(), 24 * time.Hour},
		{"RestoreBatchSize", cfg.RestoreBatchSize, 4},
		{"PrivateRange", cfg.PrivateRange, "10.100.0.0/16"},
		{"Listen.HTTP", cfg.Listen.HTTP, "127.0.0.1:8080"},
		{"Listen.HTTPS", cfg.Listen.HTTPS, ":443"},
		{"Listen.SSH", cfg.Listen.SSH, ":22"},
		{"Listen.ProxyPortMin", cfg.Listen.ProxyPortMin, 3000},
		{"Listen.ProxyPortMax", cfg.Listen.ProxyPortMax, 9999},
	}
	for _, tt := range tests {
		if tt.got != tt.want {
			t.Errorf("%s = %v, want %v", tt.name, tt.got, tt.want)
		}
	}
}

func TestParseFull(t *testing.T) {
	src := `
base_domain = "bento.foid.space"
libvirt_uri = "qemu+ssh://vmhost/system"
image_dir = "/srv/bento/images"
storage_dir = "/srv/bento/storage"
db_path = "/srv/bento/bento.db"
overcommit_ratio = 1.5
name_cooldown = "48h"
restore_batch_size = 8
private_range = "172.28.0.0/15"

[listen]
http = "127.0.0.1:9090"
https = ":8443"
ssh = ":2222"
proxy_port_min = 4000
proxy_port_max = 5000

[acme]
email = "op@foid.space"
cloudflare_token = "cf-token"
directory = "https://acme-staging-v02.api.letsencrypt.org/directory"

[oidc]
issuer = "https://id.foid.space"
client_id = "bento"
client_secret = "hunter2"

[[images]]
name = "debian-13"
url = "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2"

[[images]]
name = "fedora-42"
url = "https://example.org/fedora-42.qcow2"
pinned_checksum = "sha256-deadbeef"
`
	cfg, err := Parse([]byte(src))
	if err != nil {
		t.Fatalf("Parse: %v", err)
	}
	if cfg.LibvirtURI != "qemu+ssh://vmhost/system" {
		t.Errorf("LibvirtURI = %q", cfg.LibvirtURI)
	}
	if cfg.NameCooldown.Std() != 48*time.Hour {
		t.Errorf("NameCooldown = %v, want 48h", cfg.NameCooldown.Std())
	}
	if cfg.OvercommitRatio != 1.5 {
		t.Errorf("OvercommitRatio = %v, want 1.5", cfg.OvercommitRatio)
	}
	if cfg.RestoreBatchSize != 8 {
		t.Errorf("RestoreBatchSize = %d, want 8", cfg.RestoreBatchSize)
	}
	if cfg.Listen.SSH != ":2222" {
		t.Errorf("Listen.SSH = %q", cfg.Listen.SSH)
	}
	if cfg.ACME.CloudflareToken != "cf-token" {
		t.Errorf("ACME.CloudflareToken = %q", cfg.ACME.CloudflareToken)
	}
	if cfg.OIDC.Issuer != "https://id.foid.space" {
		t.Errorf("OIDC.Issuer = %q", cfg.OIDC.Issuer)
	}
	if len(cfg.Images) != 2 {
		t.Fatalf("len(Images) = %d, want 2", len(cfg.Images))
	}
	if cfg.Images[1].PinnedChecksum != "sha256-deadbeef" {
		t.Errorf("Images[1].PinnedChecksum = %q", cfg.Images[1].PinnedChecksum)
	}
	if cfg.Images[0].PinnedChecksum != "" {
		t.Errorf("Images[0].PinnedChecksum = %q, want empty (trust on first use)", cfg.Images[0].PinnedChecksum)
	}
}

func TestParseErrors(t *testing.T) {
	tests := []struct {
		name    string
		src     string
		wantErr string
	}{
		{
			name:    "missing base domain",
			src:     ``,
			wantErr: "base_domain is required",
		},
		{
			name:    "undercommit",
			src:     "base_domain = \"b.example\"\novercommit_ratio = 0.5",
			wantErr: "overcommit_ratio",
		},
		{
			name:    "zero batch",
			src:     "base_domain = \"b.example\"\nrestore_batch_size = 0",
			wantErr: "restore_batch_size",
		},
		{
			name:    "bad private range",
			src:     "base_domain = \"b.example\"\nprivate_range = \"not-a-cidr\"",
			wantErr: "private_range",
		},
		{
			name:    "ipv6 private range",
			src:     "base_domain = \"b.example\"\nprivate_range = \"fd00::/48\"",
			wantErr: "IPv4",
		},
		{
			name:    "private range too narrow",
			src:     "base_domain = \"b.example\"\nprivate_range = \"10.0.0.0/28\"",
			wantErr: "/24 or wider",
		},
		{
			name:    "inverted proxy port range",
			src:     "base_domain = \"b.example\"\n[listen]\nproxy_port_min = 9000\nproxy_port_max = 4000",
			wantErr: "proxy_port_min",
		},
		{
			name:    "bad cooldown",
			src:     "base_domain = \"b.example\"\nname_cooldown = \"soon\"",
			wantErr: "duration",
		},
		{
			name:    "unknown key",
			src:     "base_domain = \"b.example\"\nbase_domian = \"typo\"",
			wantErr: "unknown key",
		},
		{
			name:    "image without url",
			src:     "base_domain = \"b.example\"\n[[images]]\nname = \"debian-13\"",
			wantErr: "no url",
		},
		{
			name:    "duplicate image",
			src:     "base_domain = \"b.example\"\n[[images]]\nname = \"a\"\nurl = \"https://x/a\"\n[[images]]\nname = \"a\"\nurl = \"https://x/b\"",
			wantErr: "duplicate",
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			_, err := Parse([]byte(tt.src))
			if err == nil {
				t.Fatalf("Parse succeeded, want error containing %q", tt.wantErr)
			}
			if !strings.Contains(err.Error(), tt.wantErr) {
				t.Errorf("error = %q, want it to contain %q", err, tt.wantErr)
			}
		})
	}
}

func TestLoadFile(t *testing.T) {
	path := filepath.Join(t.TempDir(), "bento.toml")
	if err := os.WriteFile(path, []byte("base_domain = \"bento.example.org\"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	cfg, err := Load(path)
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if cfg.BaseDomain != "bento.example.org" {
		t.Errorf("BaseDomain = %q", cfg.BaseDomain)
	}
}

func TestLoadMissingFile(t *testing.T) {
	if _, err := Load(filepath.Join(t.TempDir(), "absent.toml")); err == nil {
		t.Fatal("Load of a missing file succeeded, want error")
	}
}

func TestExampleConfigParses(t *testing.T) {
	data, err := os.ReadFile("../../bento.example.toml")
	if err != nil {
		t.Fatalf("read example config: %v", err)
	}
	if _, err := Parse(data); err != nil {
		t.Fatalf("bento.example.toml does not parse: %v", err)
	}
}
