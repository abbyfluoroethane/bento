package api

import (
	"context"
	"net/http"

	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// Store is the subset of the data layer that the API reads and writes
// directly. *store.Store satisfies it as-is; tests use a fake.
type Store interface {
	UserByID(id int64) (types.User, error)
	UserByName(name string) (types.User, error)
	QuotaFor(userID int64) (types.Quota, error)
	UsageFor(userID int64) (store.Usage, error)

	Instance(uuid string) (types.Instance, error)
	InstancesByOwner(ownerID int64) ([]types.Instance, error)
	InstancesSharedWith(userID int64) ([]types.Instance, error)
	Instances() ([]types.Instance, error)

	AddShare(instanceUUID string, userID int64) error
	RemoveShare(instanceUUID string, userID int64) error
	SharesFor(instanceUUID string) ([]types.Share, error)

	Images() ([]types.Image, error)

	AddSSHKey(userID int64, publicKey, fingerprint, comment string) (int64, error)
	SSHKeysForUser(userID int64) ([]types.SSHKey, error)
	DeleteSSHKey(userID, keyID int64) error

	// DumpDB writes a consistent snapshot of the database to destPath
	// with the SQLite backup API (SPEC 12.1). destPath must not exist.
	DumpDB(destPath string) error
}

// Compile-time proof that the real store satisfies the consumer-side
// interface, so integration wires it with no adapter.
var _ Store = (*store.Store)(nil)

// CreateSpec is what the dashboard sends to create an instance. Zero
// values for VCPU, MemoryMiB, and DiskGiB mean "use the operator default";
// the lifecycle layer applies those defaults.
type CreateSpec struct {
	Name      string
	Image     string
	VCPU      int
	MemoryMiB int64
	DiskGiB   int64
	Nested    bool
	KSM       bool
}

// ResizeSpec carries the full target shape of an instance. The handler
// fills unspecified fields from the current row before calling the
// lifecycle, so the lifecycle always sees a complete spec.
type ResizeSpec struct {
	VCPU      int
	MemoryMiB int64
	DiskGiB   int64
	Nested    bool
}

// Lifecycle is the consumer-side view of the instance lifecycle layer
// (SPEC 11.1). Every operation that touches libvirt, the overlay, the
// firewall, or cloud-init goes through it; integration wires the real
// implementation from internal/lifecycle.
type Lifecycle interface {
	Create(ctx context.Context, owner types.User, spec CreateSpec) (types.Instance, error)
	Delete(ctx context.Context, uuid string) error
	Start(ctx context.Context, uuid string) error
	Stop(ctx context.Context, uuid string) error
	Restart(ctx context.Context, uuid string) error
	Rename(ctx context.Context, uuid, newName string) error
	Resize(ctx context.Context, uuid string, spec ResizeSpec) error
	// SetHTTPPort lives on the lifecycle, not the store, because a port
	// change must also reload the nftables table (SPEC 6.3 rule 1).
	SetHTTPPort(ctx context.Context, uuid string, port int) error
	// SetVisibility lives here for the same reason: the published ports
	// of an instance follow its visibility, and SPEC 6.3 reloads the
	// whole table on every change.
	SetVisibility(ctx context.Context, uuid string, v types.Visibility) error
}

// Authenticator resolves the user behind a request. internal/auth's
// session and token middleware implements this shape; integration wires
// it. An error means the request is unauthenticated and the API answers
// HTTP 401.
type Authenticator interface {
	UserFromRequest(r *http.Request) (types.User, error)
}

// StatusError lets a lifecycle or auth error carry its own HTTP status.
type StatusError interface {
	error
	HTTPStatus() int
}
