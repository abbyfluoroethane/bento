package hypervisor

import (
	"encoding/xml"
	"flag"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

var update = flag.Bool("update", false, "rewrite golden files")

// baseSpec pins the architecture so the golden files are the same on
// every machine the test suite runs on. The host-derived default has
// its own test below.
func baseSpec() DomainSpec {
	return DomainSpec{
		Name:      "bento-web",
		UUID:      "6d1e0f1c-9a3b-4f6e-8a2d-3c5b7e9f1a2b",
		VCPU:      2,
		MemoryMiB: 2048,
		DiskPath:  "/var/lib/bento/instances/6d1e0f1c.qcow2",
		Network:   "bento-user-1",
		MAC:       "52:54:00:ab:cd:ef",
		KSM:       true,
		Arch:      ArchAMD64,
	}
}

func TestDomainXMLGolden(t *testing.T) {
	tests := []struct {
		name   string
		golden string
		mutate func(*DomainSpec)
	}{
		{name: "default", golden: "default.xml", mutate: func(*DomainSpec) {}},
		{name: "nested", golden: "nested.xml", mutate: func(s *DomainSpec) { s.Nested = true }},
		{name: "no ksm", golden: "noksm.xml", mutate: func(s *DomainSpec) { s.KSM = false }},
		{name: "with iso", golden: "iso.xml", mutate: func(s *DomainSpec) {
			s.ISOPath = "/var/lib/bento/instances/6d1e0f1c-seed.iso"
		}},
		{name: "arm64", golden: "arm64.xml", mutate: func(s *DomainSpec) { s.Arch = ArchARM64 }},
		{name: "arm64 with iso", golden: "arm64-iso.xml", mutate: func(s *DomainSpec) {
			s.Arch = ArchARM64
			s.ISOPath = "/var/lib/bento/instances/6d1e0f1c-seed.iso"
		}},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			spec := baseSpec()
			tt.mutate(&spec)
			got, err := DomainXML(spec)
			if err != nil {
				t.Fatalf("DomainXML: %v", err)
			}
			path := filepath.Join("testdata", tt.golden)
			if *update {
				if err := os.WriteFile(path, []byte(got), 0o644); err != nil {
					t.Fatalf("write golden: %v", err)
				}
			}
			want, err := os.ReadFile(path)
			if err != nil {
				t.Fatalf("read golden: %v (run with -update to create)", err)
			}
			if got != string(want) {
				t.Errorf("XML mismatch with %s\ngot:\n%s\nwant:\n%s", path, got, want)
			}
			// Every variant must be well-formed XML.
			var node struct{}
			if err := xml.Unmarshal([]byte(got), &node); err != nil {
				t.Errorf("generated XML does not parse: %v", err)
			}
		})
	}
}

func TestDomainXMLDeviceSet(t *testing.T) {
	got, err := DomainXML(baseSpec())
	if err != nil {
		t.Fatalf("DomainXML: %v", err)
	}
	// The fixed device set of SPEC section 5, plus the settings of
	// 5.3 through 5.5 in their default form.
	for _, want := range []string{
		`<domain type='kvm'>`,
		`<target dev='vda' bus='virtio'/>`,
		`<model type='virtio'/>`,
		`<mac address='52:54:00:ab:cd:ef'/>`,
		`<backend model='random'>/dev/urandom</backend>`,
		`<memballoon model='virtio' freePageReporting='on'/>`,
		`<console type='pty'>`,
		// SPEC 5: one virtio serial console, not an ISA serial port.
		`<target type='virtio' port='0'/>`,
		`<target type='virtio' name='org.qemu.guest_agent.0'/>`,
		`<os firmware='efi'>`,
		`<vcpu placement='static'>2</vcpu>`,
		`<cpu mode='host-model'/>`,
	} {
		if !strings.Contains(got, want) {
			t.Errorf("XML missing %s", want)
		}
	}
	for _, banned := range []string{"nosharepages", "cdrom", "host-passthrough", "<serial"} {
		if strings.Contains(got, banned) {
			t.Errorf("default XML must not contain %s", banned)
		}
	}
}

func TestDomainXMLNestedAndKSMVariants(t *testing.T) {
	nested := baseSpec()
	nested.Nested = true
	got, err := DomainXML(nested)
	if err != nil {
		t.Fatalf("DomainXML nested: %v", err)
	}
	if !strings.Contains(got, `<cpu mode='host-passthrough'/>`) {
		t.Error("nested=true must use host-passthrough (SPEC 5.5)")
	}

	noKSM := baseSpec()
	noKSM.KSM = false
	got, err = DomainXML(noKSM)
	if err != nil {
		t.Fatalf("DomainXML no-ksm: %v", err)
	}
	if !strings.Contains(got, "<nosharepages/>") {
		t.Error("ksm=false must emit <nosharepages/> (SPEC 5.4)")
	}
}

func TestDomainXMLArch(t *testing.T) {
	// An empty Arch is the host's own: Bento runs type='kvm' domains
	// (SPEC section 5), so the guest architecture is never a choice.
	spec := baseSpec()
	spec.Arch = ""
	got, err := DomainXML(spec)
	if err != nil {
		t.Fatalf("DomainXML: %v", err)
	}
	if want := `arch='` + HostArch() + `'`; !strings.Contains(got, want) {
		t.Errorf("empty arch must resolve to the host's %s\n%s", want, got)
	}

	arm := baseSpec()
	arm.Arch = ArchARM64
	arm.ISOPath = "/var/lib/bento/instances/6d1e0f1c-seed.iso"
	got, err = DomainXML(arm)
	if err != nil {
		t.Fatalf("DomainXML aarch64: %v", err)
	}
	for _, want := range []string{
		`<type arch='aarch64' machine='virt'>hvm</type>`,
		// KVM on aarch64 implements host-passthrough and nothing else.
		`<cpu mode='host-passthrough'/>`,
		`<gic version='3'/>`,
		// The virt machine has no SATA controller (SPEC 5.2).
		`<target dev='sda' bus='scsi'/>`,
		`<controller type='scsi' index='0' model='virtio-scsi'/>`,
	} {
		if !strings.Contains(got, want) {
			t.Errorf("aarch64 XML missing %s\n%s", want, got)
		}
	}
	for _, banned := range []string{`<apic/>`, `bus='sata'`, `machine='q35'`} {
		if strings.Contains(got, banned) {
			t.Errorf("aarch64 XML must not contain %s\n%s", banned, got)
		}
	}
}

func TestDomainSpecValidateArch(t *testing.T) {
	spec := baseSpec()
	spec.Arch = "riscv64"
	if err := spec.Validate(); err == nil {
		t.Error("an architecture Bento has no template branch for must not validate")
	}
}

func TestDomainXMLEscapesHostileValues(t *testing.T) {
	// SPEC 4.2: treat every string that reaches the domain XML as
	// hostile.
	hostile := []struct {
		name  string
		apply func(*DomainSpec)
		raw   string
	}{
		{
			name:  "element injection in name",
			apply: func(s *DomainSpec) { s.Name = `x</name><devices><disk type="block"/></devices>` },
			raw:   `</name><devices>`,
		},
		{
			name:  "attribute breakout in disk path",
			apply: func(s *DomainSpec) { s.DiskPath = `/tmp/x' bus='0'/><serial type='tcp` },
			raw:   `' bus='0'/>`,
		},
		{
			name:  "double quote and ampersand in network",
			apply: func(s *DomainSpec) { s.Network = `net"&<evil>` },
			raw:   `"&<evil>`,
		},
		{
			name:  "injection in uuid",
			apply: func(s *DomainSpec) { s.UUID = `1</uuid><vcpu>999</vcpu>` },
			raw:   `</uuid><vcpu>`,
		},
	}
	for _, tt := range hostile {
		t.Run(tt.name, func(t *testing.T) {
			spec := baseSpec()
			tt.apply(&spec)
			got, err := DomainXML(spec)
			if err != nil {
				t.Fatalf("DomainXML: %v", err)
			}
			if strings.Contains(got, tt.raw) {
				t.Errorf("hostile payload survived unescaped:\n%s", got)
			}
			var node struct{}
			if err := xml.Unmarshal([]byte(got), &node); err != nil {
				t.Errorf("XML with hostile input does not parse: %v", err)
			}
		})
	}
}

func TestDomainXMLEscapeRoundTrip(t *testing.T) {
	// The escaped name must decode back to the original string, so a
	// weird but honest name survives intact.
	spec := baseSpec()
	spec.Name = `a<b>&'"c`
	got, err := DomainXML(spec)
	if err != nil {
		t.Fatalf("DomainXML: %v", err)
	}
	var parsed struct {
		Name string `xml:"name"`
	}
	if err := xml.Unmarshal([]byte(got), &parsed); err != nil {
		t.Fatalf("parse: %v", err)
	}
	if parsed.Name != spec.Name {
		t.Errorf("round trip: got %q want %q", parsed.Name, spec.Name)
	}
}

func TestDomainSpecValidate(t *testing.T) {
	tests := []struct {
		name    string
		mutate  func(*DomainSpec)
		wantErr bool
	}{
		{"valid", func(*DomainSpec) {}, false},
		{"empty name", func(s *DomainSpec) { s.Name = "" }, true},
		{"empty uuid", func(s *DomainSpec) { s.UUID = "" }, true},
		{"zero vcpu", func(s *DomainSpec) { s.VCPU = 0 }, true},
		{"zero memory", func(s *DomainSpec) { s.MemoryMiB = 0 }, true},
		{"empty disk", func(s *DomainSpec) { s.DiskPath = "" }, true},
		{"empty network", func(s *DomainSpec) { s.Network = "" }, true},
		{"bad mac", func(s *DomainSpec) { s.MAC = "not-a-mac" }, true},
		{"empty mac", func(s *DomainSpec) { s.MAC = "" }, true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			spec := baseSpec()
			tt.mutate(&spec)
			_, err := DomainXML(spec)
			if (err != nil) != tt.wantErr {
				t.Errorf("err = %v, wantErr %v", err, tt.wantErr)
			}
		})
	}
}
