package hypervisor

import (
	"encoding/xml"
	"fmt"
	"net"
	"runtime"
	"strings"
	"text/template"
)

// DomainSpec holds every value that Bento interpolates into the domain
// XML. The device set is fixed (SPEC section 5); the spec only carries
// the per-instance values.
type DomainSpec struct {
	Name      string
	UUID      string
	VCPU      int
	MemoryMiB int64

	// DiskPath is the absolute path of the qcow2 overlay for the
	// virtio-blk root disk.
	DiskPath string
	// ISOPath, when set, attaches the cloud-init NoCloud ISO as a
	// read-only CD-ROM (SPEC 5.2). Empty means no CD-ROM.
	ISOPath string

	// Network is the libvirt network name of the owner (SPEC 6.2).
	Network string
	// MAC is the Bento-assigned MAC address. Bento never lets libvirt
	// generate it (SPEC section 5).
	MAC string

	// Nested switches the CPU model from host-model to
	// host-passthrough (SPEC 5.5).
	Nested bool
	// KSM defaults to true. False adds
	// <memoryBacking><nosharepages/></memoryBacking> (SPEC 5.4).
	KSM bool

	// Arch is the guest architecture in libvirt's spelling, such as
	// x86_64 or aarch64. Empty selects the host architecture. Bento
	// only runs guests of the host's own architecture: the domains are
	// type='kvm' (SPEC section 5), and KVM offers nothing else.
	Arch string
}

// Architectures Bento knows how to render domain XML for.
const (
	ArchAMD64 = "x86_64"
	ArchARM64 = "aarch64"
)

// HostArch returns the running host's architecture in libvirt's
// spelling. It is what an empty DomainSpec.Arch resolves to.
func HostArch() string {
	switch runtime.GOARCH {
	case "amd64":
		return ArchAMD64
	case "arm64":
		return ArchARM64
	default:
		return runtime.GOARCH
	}
}

// Validate rejects a spec that cannot produce valid domain XML.
func (s DomainSpec) Validate() error {
	switch {
	case s.Name == "":
		return fmt.Errorf("domain spec: name is empty")
	case s.UUID == "":
		return fmt.Errorf("domain spec: uuid is empty")
	case s.VCPU < 1:
		return fmt.Errorf("domain spec: vcpu %d < 1", s.VCPU)
	case s.MemoryMiB < 1:
		return fmt.Errorf("domain spec: memory %d MiB < 1", s.MemoryMiB)
	case s.DiskPath == "":
		return fmt.Errorf("domain spec: disk path is empty")
	case s.Network == "":
		return fmt.Errorf("domain spec: network is empty")
	}
	if _, err := net.ParseMAC(s.MAC); err != nil {
		return fmt.Errorf("domain spec: mac %q: %w", s.MAC, err)
	}
	switch s.Arch {
	case "", ArchAMD64, ArchARM64:
	default:
		return fmt.Errorf("domain spec: arch %q is not one of %s, %s", s.Arch, ArchAMD64, ArchARM64)
	}
	return nil
}

// resolve fills in the values the host decides, so the template never
// has to reach outside its data.
func (s DomainSpec) resolve() DomainSpec {
	if s.Arch == "" {
		s.Arch = HostArch()
	}
	return s
}

// ARM64 reports whether the guest is aarch64. The template branches on
// it wherever the two architectures disagree.
func (s DomainSpec) ARM64() bool { return s.Arch == ArchARM64 }

// Machine is the libvirt machine type. Both are the only sensible
// modern choice for their architecture, and both get UEFI from the
// firmware='efi' autoselection.
func (s DomainSpec) Machine() string {
	if s.ARM64() {
		return "virt"
	}
	return "q35"
}

// HostPassthrough reports whether the CPU model is host-passthrough
// rather than host-model. Nested asks for it (SPEC 5.5); aarch64 gets
// it regardless, because KVM on aarch64 implements no other model.
func (s DomainSpec) HostPassthrough() bool { return s.Nested || s.ARM64() }

// escapeXML escapes a value for interpolation into XML text or a
// single- or double-quoted attribute. Every string that reaches the
// domain XML is hostile (SPEC 4.2).
func escapeXML(s string) string {
	var b strings.Builder
	// strings.Builder never returns a write error.
	_ = xml.EscapeText(&b, []byte(s))
	// EscapeText leaves the single quote alone; the template uses
	// single-quoted attributes, so close that hole too.
	return strings.ReplaceAll(b.String(), "'", "&#39;")
}

// domainTemplate is the one Go template for domain XML (SPEC 4.1: one
// template, no general XML object model). The device set is fixed per
// SPEC section 5.
var domainTemplate = template.Must(template.New("domain").Funcs(template.FuncMap{
	"esc": escapeXML,
}).Parse(`<domain type='kvm'>
  <name>{{esc .Name}}</name>
  <uuid>{{esc .UUID}}</uuid>
  <memory unit='MiB'>{{.MemoryMiB}}</memory>
  <vcpu placement='static'>{{.VCPU}}</vcpu>
  <os firmware='efi'>
    <type arch='{{esc .Arch}}' machine='{{.Machine}}'>hvm</type>
  </os>
{{- if not .KSM}}
  <memoryBacking>
    <nosharepages/>
  </memoryBacking>
{{- end}}
{{- if .HostPassthrough}}
  <cpu mode='host-passthrough'/>
{{- else}}
  <cpu mode='host-model'/>
{{- end}}
  <features>
    <acpi/>
{{- if .ARM64}}
    <gic version='3'/>
{{- else}}
    <apic/>
{{- end}}
  </features>
  <on_poweroff>destroy</on_poweroff>
  <on_reboot>restart</on_reboot>
  <on_crash>destroy</on_crash>
  <devices>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2'/>
      <source file='{{esc .DiskPath}}'/>
      <target dev='vda' bus='virtio'/>
    </disk>
{{- if .ISOPath}}
    <disk type='file' device='cdrom'>
      <driver name='qemu' type='raw'/>
      <source file='{{esc .ISOPath}}'/>
{{- if .ARM64}}
      <target dev='sda' bus='scsi'/>
{{- else}}
      <target dev='sda' bus='sata'/>
{{- end}}
      <readonly/>
    </disk>
{{- if .ARM64}}
    <!-- The aarch64 virt machine has no SATA controller, so the seed
         CD-ROM of SPEC 5.2 hangs off virtio-scsi instead. -->
    <controller type='scsi' index='0' model='virtio-scsi'/>
{{- end}}
{{- end}}
    <interface type='network'>
      <mac address='{{esc .MAC}}'/>
      <source network='{{esc .Network}}'/>
      <model type='virtio'/>
    </interface>
    <console type='pty'>
      <target type='virtio' port='0'/>
    </console>
    <channel type='unix'>
      <source mode='bind'/>
      <target type='virtio' name='org.qemu.guest_agent.0'/>
    </channel>
    <rng model='virtio'>
      <backend model='random'>/dev/urandom</backend>
    </rng>
    <memballoon model='virtio' freePageReporting='on'/>
  </devices>
</domain>
`))

// DomainXML renders the domain XML for one instance from the single
// template. Every interpolated string is escaped.
func DomainXML(spec DomainSpec) (string, error) {
	if err := spec.Validate(); err != nil {
		return "", err
	}
	spec = spec.resolve()
	var b strings.Builder
	if err := domainTemplate.Execute(&b, spec); err != nil {
		return "", fmt.Errorf("render domain xml: %w", err)
	}
	return b.String(), nil
}
