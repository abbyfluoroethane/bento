package lifecycle

import (
	"testing"

	"github.com/abbyfluoroethane/bento/internal/cloudinit"
	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/images"
	"github.com/abbyfluoroethane/bento/internal/network"
	"github.com/abbyfluoroethane/bento/internal/store"
)

// The consumer-side interfaces must stay satisfied by the real
// implementations, so the wiring in cmd compiles.
var (
	_ Store          = (*store.Store)(nil)
	_ ImageStore     = (*images.Store)(nil)
	_ ISOBuilder     = (*cloudinit.Builder)(nil)
	_ OverlayResizer = QemuImgResizer{}
)

func TestNewManagerValidation(t *testing.T) {
	plan, err := network.NewPlan("10.77.0.0/16")
	if err != nil {
		t.Fatal(err)
	}
	valid := Config{
		Hypervisor: &hypervisor.Fake{},
		Store:      newFakeStore(),
		Images:     &fakeImages{},
		ISO:        newFakeISO(),
		Plan:       plan,
		StorageDir: t.TempDir(),
	}
	if _, err := NewManager(valid); err != nil {
		t.Fatalf("valid config rejected: %v", err)
	}

	tests := []struct {
		name   string
		mutate func(*Config)
	}{
		{"no hypervisor", func(c *Config) { c.Hypervisor = nil }},
		{"no store", func(c *Config) { c.Store = nil }},
		{"no image store", func(c *Config) { c.Images = nil }},
		{"no iso builder", func(c *Config) { c.ISO = nil }},
		{"no storage dir", func(c *Config) { c.StorageDir = "" }},
		{"no plan", func(c *Config) { c.Plan = network.Plan{} }},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg := valid
			tt.mutate(&cfg)
			if _, err := NewManager(cfg); err == nil {
				t.Error("bad config accepted")
			}
		})
	}
}

func TestManagerDefaults(t *testing.T) {
	plan, err := network.NewPlan("10.77.0.0/16")
	if err != nil {
		t.Fatal(err)
	}
	m, err := NewManager(Config{
		Hypervisor: &hypervisor.Fake{},
		Store:      newFakeStore(),
		Images:     &fakeImages{},
		ISO:        newFakeISO(),
		Plan:       plan,
		StorageDir: "/var/lib/bento/storage",
	})
	if err != nil {
		t.Fatal(err)
	}
	if m.batchSize != 4 {
		t.Errorf("batch size = %d, want the SPEC 11.2 default of 4", m.batchSize)
	}
	if m.cooldown.Hours() != 24 {
		t.Errorf("name cooldown = %v, want the SPEC 7.2 default of 24h", m.cooldown)
	}
	if m.pollEvery.Seconds() != 30 {
		t.Errorf("poll interval = %v, want the SPEC 12 default of 30s", m.pollEvery)
	}
	if _, ok := m.resizer.(QemuImgResizer); !ok {
		t.Errorf("resizer = %T, want QemuImgResizer", m.resizer)
	}
	if got := m.OverlayPath("u-1"); got != "/var/lib/bento/storage/u-1.qcow2" {
		t.Errorf("overlay path = %s", got)
	}
	if got := m.SeedISOPath("u-1"); got != "/var/lib/bento/storage/u-1-seed.iso" {
		t.Errorf("seed iso path = %s", got)
	}
}

func TestRandomUUID(t *testing.T) {
	seen := map[string]bool{}
	for i := 0; i < 100; i++ {
		id := randomUUID()
		if len(id) != 36 {
			t.Fatalf("uuid %q has length %d", id, len(id))
		}
		if id[14] != '4' {
			t.Fatalf("uuid %q is not version 4", id)
		}
		if seen[id] {
			t.Fatalf("uuid %q repeated", id)
		}
		seen[id] = true
	}
}
