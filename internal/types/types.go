// Package types holds the core domain types shared across Bento packages.
// The definitions follow SPEC.md sections 11 and 12. This package contains
// types only, no behavior.
package types

import "time"

// State is the observed state of an instance. libvirt is authoritative for
// this value (SPEC section 11.1).
type State string

// Observed instance states.
const (
	StateRunning  State = "running"
	StateStopped  State = "stopped"
	StateStarting State = "starting"
)

// DesiredState is the state the last user action asked for. Bento is
// authoritative for this value (SPEC section 11.1). It never holds
// "starting".
type DesiredState string

// Desired instance states.
const (
	DesiredRunning DesiredState = "running"
	DesiredStopped DesiredState = "stopped"
)

// Visibility controls how the HTTP proxy treats requests for an instance
// name (SPEC section 9.2). The default is VisibilityOff.
type Visibility string

// Visibility values.
const (
	VisibilityOff     Visibility = "off"
	VisibilityPrivate Visibility = "private"
	VisibilityPublic  Visibility = "public"
)

// Instance is one virtual machine. One instance is one libvirt domain.
// The UUID is the identifier; the name is a label that can change
// (SPEC section 7.2).
type Instance struct {
	UUID         string
	Name         string
	OwnerID      int64
	HostID       int64
	ImageName    string
	BaseChecksum string
	State        State
	DesiredState DesiredState
	Address      string
	MAC          string
	VCPU         int
	MemoryMiB    int64
	DiskGiB      int64
	Nested       bool
	KSM          bool
	HTTPPort     int
	Visibility   Visibility
	CreatedAt    time.Time
	LastSeenAt   time.Time
}

// User is a person with a Bento account (SPEC section 12).
type User struct {
	ID          int64
	Name        string
	Email       string
	OIDCSubject string
	Subnet      string
	CreatedAt   time.Time
}

// Quota holds the four per-user limits: instance count, total vCPU count,
// total memory, and total virtual disk size (SPEC section 6.1).
type Quota struct {
	UserID       int64
	MaxInstances int
	MaxVCPU      int
	MaxMemoryMiB int64
	MaxDiskGiB   int64
}

// SSHKey is one public key registered by a user. The SSH frontend looks
// keys up by fingerprint on every connection (SPEC section 12).
type SSHKey struct {
	ID          int64
	UserID      int64
	PublicKey   string
	Fingerprint string
	Comment     string
	CreatedAt   time.Time
}

// Host is a machine that runs libvirtd and holds instances. Version 1
// supports one host (SPEC section 12).
type Host struct {
	ID         int64
	Name       string
	LibvirtURI string
	CreatedAt  time.Time
}

// Image is a named entry in the operator allowlist (SPEC section 5.1).
type Image struct {
	Name            string
	URL             string
	PinnedChecksum  string
	CurrentChecksum string
}

// ImageVersion is one downloaded file for an image, identified by its
// checksum and stored at a content-addressed path (SPEC section 5.1).
type ImageVersion struct {
	Checksum  string
	ImageName string
	Path      string
	Size      int64
	FetchedAt time.Time
}

// Share grants a second user access to an instance. Shares key on the
// instance UUID, never on the name (SPEC sections 7.2, 12).
type Share struct {
	InstanceUUID string
	UserID       int64
	CreatedAt    time.Time
}

// ReleasedName records a name released by a delete or a rename, for the
// cooldown in SPEC section 7.2. Rows are kept after the cooldown expires.
type ReleasedName struct {
	Name            string
	PreviousOwnerID int64
	ReleasedAt      time.Time
}

// Token gives programmatic access. Only the hash of the token is stored
// (SPEC section 13).
type Token struct {
	ID        int64
	UserID    int64
	Hash      string
	ExpiresAt time.Time
}
