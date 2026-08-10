package network

import (
	"net/netip"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestNewUserNetwork(t *testing.T) {
	p, err := NewPlan("10.77.0.0/16")
	if err != nil {
		t.Fatal(err)
	}
	n, err := NewUserNetwork(p, 3)
	if err != nil {
		t.Fatal(err)
	}
	if n.Name != "bento-user-3" {
		t.Errorf("Name = %q, want %q", n.Name, "bento-user-3")
	}
	if n.Bridge != "bento3" {
		t.Errorf("Bridge = %q, want %q", n.Bridge, "bento3")
	}
	if n.Subnet.String() != "10.77.3.0/24" {
		t.Errorf("Subnet = %s, want 10.77.3.0/24", n.Subnet)
	}
	if _, err := NewUserNetwork(p, 256); err == nil {
		t.Error("NewUserNetwork(256) on /16: want error")
	}
}

func TestUserNetworkXMLGolden(t *testing.T) {
	p, err := NewPlan("10.77.0.0/16")
	if err != nil {
		t.Fatal(err)
	}
	n, err := NewUserNetwork(p, 3)
	if err != nil {
		t.Fatal(err)
	}
	got, err := n.XML()
	if err != nil {
		t.Fatal(err)
	}
	want := readGolden(t, "user_network.xml", got)
	if got != want {
		t.Errorf("network XML mismatch:\ngot:\n%s\nwant:\n%s", got, want)
	}
}

func TestUserNetworkXMLEscapes(t *testing.T) {
	// SPEC 4.2: treat every string that reaches XML as hostile.
	n := UserNetwork{
		Name:   `evil"/><script>alert(1)</script>`,
		Bridge: "bento3",
		Subnet: netip.MustParsePrefix("10.77.3.0/24"),
	}
	got, err := n.XML()
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(got, "<script>") {
		t.Errorf("XML contains unescaped markup:\n%s", got)
	}
	if !strings.Contains(got, "&lt;script&gt;") {
		t.Errorf("XML does not escape the hostile name:\n%s", got)
	}
}

func TestUserNetworkXMLRejectsBadInput(t *testing.T) {
	subnet := netip.MustParsePrefix("10.77.3.0/24")
	tests := []struct {
		name string
		net  UserNetwork
	}{
		{name: "empty name", net: UserNetwork{Bridge: "bento3", Subnet: subnet}},
		{name: "empty bridge", net: UserNetwork{Name: "bento-user-3", Subnet: subnet}},
		{name: "long bridge", net: UserNetwork{Name: "n", Bridge: strings.Repeat("b", 16), Subnet: subnet}},
		{name: "not a /24", net: UserNetwork{Name: "n", Bridge: "bento3", Subnet: netip.MustParsePrefix("10.77.0.0/16")}},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if _, err := tt.net.XML(); err == nil {
				t.Error("XML(): want error")
			}
		})
	}
}

// readGolden reads testdata/<name>. With -update it writes got first.
func readGolden(t *testing.T, name, got string) string {
	t.Helper()
	path := filepath.Join("testdata", name)
	if *update {
		if err := os.MkdirAll("testdata", 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(got), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read golden %s (run with -update to create): %v", path, err)
	}
	return string(data)
}
