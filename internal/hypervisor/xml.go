package hypervisor

import (
	"encoding/xml"
	"fmt"
	"net"
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
	return nil
}

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
    <type arch='x86_64' machine='q35'>hvm</type>
  </os>
{{- if not .KSM}}
  <memoryBacking>
    <nosharepages/>
  </memoryBacking>
{{- end}}
{{- if .Nested}}
  <cpu mode='host-passthrough'/>
{{- else}}
  <cpu mode='host-model'/>
{{- end}}
  <features>
    <acpi/>
    <apic/>
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
      <target dev='sda' bus='sata'/>
      <readonly/>
    </disk>
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
	var b strings.Builder
	if err := domainTemplate.Execute(&b, spec); err != nil {
		return "", fmt.Errorf("render domain xml: %w", err)
	}
	return b.String(), nil
}
