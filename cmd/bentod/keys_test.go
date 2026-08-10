package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestEnsureKeyCreatesAndReloads(t *testing.T) {
	path := filepath.Join(t.TempDir(), "keys", frontendKeyFile)

	first, err := ensureKey(path, "bento-frontend")
	if err != nil {
		t.Fatalf("ensureKey (create): %v", err)
	}
	second, err := ensureKey(path, "bento-frontend")
	if err != nil {
		t.Fatalf("ensureKey (reload): %v", err)
	}
	a := authorizedKeyLine(first.PublicKey(), "")
	b := authorizedKeyLine(second.PublicKey(), "")
	if a != b {
		t.Errorf("reloaded key differs:\n%s\n%s", a, b)
	}
	if !strings.HasPrefix(a, "ssh-ed25519 ") {
		t.Errorf("public key line = %q, want ed25519", a)
	}

	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm() != 0o600 {
		t.Errorf("private key mode = %o, want 0600", info.Mode().Perm())
	}
	pub, err := os.ReadFile(path + ".pub")
	if err != nil {
		t.Fatalf("public key file: %v", err)
	}
	if !strings.Contains(string(pub), "bento-frontend") {
		t.Errorf("pub file %q misses the comment", pub)
	}
}

func TestHelperParsing(t *testing.T) {
	tests := []struct {
		name string
		got  string
		want string
	}{
		{"socket default uri", socketPath("qemu:///system"), ""},
		{"socket override", socketPath("qemu:///system?socket=/run/lv.sock"), "/run/lv.sock"},
		{"control url loopback", controlURL("127.0.0.1:8080"), "http://127.0.0.1:8080"},
		{"control url unspecified host", controlURL(":8080"), "http://127.0.0.1:8080"},
		{"bind host empty", bindHost(":443"), ""},
		{"bind host set", bindHost("192.0.2.1:443"), "192.0.2.1"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if tt.got != tt.want {
				t.Errorf("got %q, want %q", tt.got, tt.want)
			}
		})
	}
}
