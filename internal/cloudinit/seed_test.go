package cloudinit

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// testSeed is the fixed seed the golden files were rendered from.
func testSeed() Seed {
	return Seed{
		InstanceID: "6f1d2c3a-9b8e-4f5a-a1b2-c3d4e5f60789",
		Hostname:   "web-1",
		UserName:   "alice",
		AuthorizedKeys: []string{
			"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIB6C5rzYtZQoYXsQ2N4YFJmXW4L0Yw1v9uW3o2n8m4Qq alice@laptop",
			"ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCk7example alice@desktop",
		},
		MAC:         "52:54:00:aa:bb:cc",
		AddressCIDR: "10.20.3.5/24",
		Gateway:     "10.20.3.1",
		DNS:         "10.20.3.1",
	}
}

func golden(t *testing.T, name string) string {
	t.Helper()
	b, err := os.ReadFile(filepath.Join("testdata", name))
	if err != nil {
		t.Fatal(err)
	}
	return string(b)
}

func TestRenderGolden(t *testing.T) {
	seed := testSeed()
	tests := []struct {
		name   string
		render func() (string, error)
		golden string
	}{
		{"meta-data", seed.MetaData, "meta-data.golden"},
		{"user-data", seed.UserData, "user-data.golden"},
		{"network-config", seed.NetworkConfig, "network-config.golden"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := tt.render()
			if err != nil {
				t.Fatal(err)
			}
			want := golden(t, tt.golden)
			if got != want {
				t.Errorf("%s mismatch\n--- got ---\n%s\n--- want ---\n%s", tt.name, got, want)
			}
		})
	}
}

func TestUserDataStartsWithCloudConfigHeader(t *testing.T) {
	got, err := testSeed().UserData()
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(got, "#cloud-config\n") {
		t.Fatalf("user-data must start with #cloud-config, got %q", got[:min(len(got), 20)])
	}
}

func TestValidate(t *testing.T) {
	tests := []struct {
		name    string
		mutate  func(*Seed)
		wantErr string
	}{
		{"valid", func(s *Seed) {}, ""},
		{"empty instance id", func(s *Seed) { s.InstanceID = "" }, "instance id"},
		{"empty hostname", func(s *Seed) { s.Hostname = "" }, "hostname"},
		{"hostname with space", func(s *Seed) { s.Hostname = "web 1" }, "whitespace"},
		{"hostname with newline", func(s *Seed) { s.Hostname = "web\n1" }, "control character"},
		{"empty user", func(s *Seed) { s.UserName = "" }, "user name"},
		{"user with space", func(s *Seed) { s.UserName = "a b" }, "whitespace"},
		{"no keys", func(s *Seed) { s.AuthorizedKeys = nil }, "no authorized keys"},
		{"blank key", func(s *Seed) { s.AuthorizedKeys = []string{"  "} }, "empty authorized key"},
		{"key with newline injection", func(s *Seed) {
			s.AuthorizedKeys = []string{"ssh-ed25519 AAAA x\nusers:"}
		}, "control character"},
		{"bad mac", func(s *Seed) { s.MAC = "not-a-mac" }, "MAC"},
		{"address without prefix", func(s *Seed) { s.AddressCIDR = "10.20.3.5" }, "address"},
		{"bad gateway", func(s *Seed) { s.Gateway = "10.20.3.999" }, "gateway"},
		{"bad dns", func(s *Seed) { s.DNS = "" }, "DNS"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			seed := testSeed()
			tt.mutate(&seed)
			err := seed.Validate()
			if tt.wantErr == "" {
				if err != nil {
					t.Fatalf("Validate: %v", err)
				}
				return
			}
			if err == nil {
				t.Fatal("want error")
			}
			if !strings.Contains(err.Error(), tt.wantErr) {
				t.Fatalf("error %q does not mention %q", err, tt.wantErr)
			}
		})
	}
}

func TestQuoteEscapes(t *testing.T) {
	tests := []struct{ in, want string }{
		{`plain`, `"plain"`},
		{`has "quotes"`, `"has \"quotes\""`},
		{`back\slash`, `"back\\slash"`},
	}
	for _, tt := range tests {
		if got := quote(tt.in); got != tt.want {
			t.Errorf("quote(%q) = %s, want %s", tt.in, got, tt.want)
		}
	}
}
