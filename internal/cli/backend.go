package cli

import (
	"context"
	"io"

	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// Store is the slice of the data layer the CLI uses. *store.Store
// satisfies it; tests use a fake.
type Store interface {
	UserByID(id int64) (types.User, error)
	UserByName(name string) (types.User, error)
	QuotaFor(userID int64) (types.Quota, error)
	UsageFor(userID int64) (store.Usage, error)

	InstanceByName(name string) (types.Instance, error)
	InstancesByOwner(ownerID int64) ([]types.Instance, error)
	InstancesSharedWith(userID int64) ([]types.Instance, error)
	Instances() ([]types.Instance, error)
	HasAccess(instanceUUID string, userID int64) (bool, error)

	AddShare(instanceUUID string, userID int64) error
	RemoveShare(instanceUUID string, userID int64) error
	SharesFor(instanceUUID string) ([]types.Share, error)

	AddSSHKey(userID int64, publicKey, fingerprint, comment string) (int64, error)
	SSHKeysForUser(userID int64) ([]types.SSHKey, error)
	DeleteSSHKey(userID, keyID int64) error

	Images() ([]types.Image, error)
}

var _ Store = (*store.Store)(nil)

// CreateRequest describes a `new` or the target of a `cp`.
type CreateRequest struct {
	OwnerID   int64
	Name      string
	Image     string
	VCPU      int
	MemoryMiB int64
	DiskGiB   int64
	Nested    bool
	KSM       bool
}

// ResizeRequest carries the changed values of a `resize`; nil means
// unchanged (SPEC 11.1).
type ResizeRequest struct {
	VCPU      *int
	MemoryMiB *int64
	DiskGiB   *int64
	Nested    *bool
}

// Lifecycle is the consumer-side view of the instance lifecycle actions
// (SPEC 11.1). The lifecycle package implements it; integration wires the
// two together. Implementations own quota checks, the name cooldown
// (errors surface as *store.QuotaError and *store.NameCooldownError),
// desired-state bookkeeping, and the hypervisor calls.
type Lifecycle interface {
	// Create builds and starts a new instance (`new`).
	Create(ctx context.Context, req CreateRequest) (types.Instance, error)
	// Start starts a stopped instance and sets desired state running.
	Start(ctx context.Context, inst types.Instance) error
	// Stop stops the instance (ACPI request, 60 s wait, then destroy)
	// and reports which path the stop took.
	Stop(ctx context.Context, inst types.Instance) (hypervisor.StopResult, error)
	// Restart reboots the instance.
	Restart(ctx context.Context, inst types.Instance) error
	// Remove runs the four `rm` steps of SPEC 11.1 in order.
	Remove(ctx context.Context, inst types.Instance) error
	// Rename renames the instance; the old name enters the cooldown
	// (SPEC 7.3). No alias or redirect is created.
	Rename(ctx context.Context, inst types.Instance, newName string) error
	// Copy clones a stopped source into a new instance (`cp`).
	Copy(ctx context.Context, src types.Instance, req CreateRequest) (types.Instance, error)
	// Resize applies a resize; the change needs a restart.
	Resize(ctx context.Context, inst types.Instance, req ResizeRequest) error
	// Console attaches the serial console to rw until rw or the console
	// closes.
	Console(ctx context.Context, inst types.Instance, rw io.ReadWriter) error
	// SetHTTPPort sets the default HTTP port. It lives on the lifecycle,
	// not the store, because the published ports feed the nftables table
	// and SPEC 6.3 reloads the whole table on every change.
	SetHTTPPort(ctx context.Context, inst types.Instance, port int) error
	// SetVisibility sets the visibility value, which also changes the
	// published ports; the same SPEC 6.3 reload applies.
	SetVisibility(ctx context.Context, inst types.Instance, v types.Visibility) error
}
