package main

// Thin adapters between the packages' consumer-side interfaces and the
// concrete implementations. Every deliberate mapping decision lives
// here, not in the packages themselves.

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/netip"
	"net/url"
	"time"

	"github.com/abbyfluoroethane/bento/internal/api"
	"github.com/abbyfluoroethane/bento/internal/auth"
	"github.com/abbyfluoroethane/bento/internal/cli"
	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/images"
	"github.com/abbyfluoroethane/bento/internal/lifecycle"
	"github.com/abbyfluoroethane/bento/internal/network"
	"github.com/abbyfluoroethane/bento/internal/proxy"
	"github.com/abbyfluoroethane/bento/internal/sshfront"
	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// ---- images ----

// imagesDB satisfies images.DB over the store.
type imagesDB struct{ st *store.Store }

var _ images.DB = imagesDB{}

func (d imagesDB) Images(context.Context) ([]types.Image, error) { return d.st.Images() }

func (d imagesDB) HasImageVersion(ctx context.Context, checksum string) (bool, error) {
	versions, err := d.ImageVersions(ctx)
	if err != nil {
		return false, err
	}
	for _, v := range versions {
		if v.Checksum == checksum {
			return true, nil
		}
	}
	return false, nil
}

func (d imagesDB) InsertImageVersion(_ context.Context, v types.ImageVersion) error {
	return d.st.AddImageVersion(v)
}

func (d imagesDB) SetCurrentChecksum(_ context.Context, imageName, checksum string) error {
	return d.st.SetCurrentChecksum(imageName, checksum)
}

func (d imagesDB) ImageVersions(context.Context) ([]types.ImageVersion, error) {
	imgs, err := d.st.Images()
	if err != nil {
		return nil, err
	}
	var all []types.ImageVersion
	for _, img := range imgs {
		versions, err := d.st.ImageVersions(img.Name)
		if err != nil {
			return nil, err
		}
		all = append(all, versions...)
	}
	return all, nil
}

func (d imagesDB) DeleteImageVersion(_ context.Context, checksum string) error {
	return d.st.DeleteImageVersion(checksum)
}

func (d imagesDB) ChecksumInUse(_ context.Context, checksum string) (bool, error) {
	insts, err := d.st.Instances()
	if err != nil {
		return false, err
	}
	for _, inst := range insts {
		if inst.BaseChecksum == checksum {
			return true, nil
		}
	}
	return false, nil
}

// reportSource satisfies images.ReportSource for the images command.
type reportSource struct{ st *store.Store }

var _ images.ReportSource = reportSource{}

func (r reportSource) Images(context.Context) ([]types.Image, error) { return r.st.Images() }

func (r reportSource) CountInstancesOnOtherVersions(_ context.Context, imageName, checksum string) (int, error) {
	insts, err := r.st.Instances()
	if err != nil {
		return 0, err
	}
	n := 0
	for _, inst := range insts {
		if inst.ImageName == imageName && inst.BaseChecksum != checksum {
			n++
		}
	}
	return n, nil
}

// ---- lifecycle backends ----

// backend drives the lifecycle manager for the CLI and the API. It owns
// the request assembly both share: the owner row, the owner's keys, and
// the frontend key that lets the SSH frontend reach the guest
// (SPEC 10 step 9). Every change that alters the published addresses or
// ports — create, delete, port, visibility — reloads the nftables table
// (SPEC 6.3: reload the whole table on every change).
type backend struct {
	m           *lifecycle.Manager
	st          *store.Store
	hostID      int64
	frontendKey string    // authorized_keys line, appended to every seed
	firewall    *firewall // nil skips the reload (tests without nft)
}

// reloadFirewall applies the SPEC 6.3 rule: the nftables table is
// rebuilt and reloaded on every change, not on the next poll tick.
func (b *backend) reloadFirewall(ctx context.Context) error {
	if b.firewall == nil {
		return nil
	}
	return b.firewall.reload(ctx)
}

// newRequest assembles a lifecycle.NewRequest for an owner.
func (b *backend) newRequest(ownerID int64, name, image string, vcpu int, memoryMiB, diskGiB int64, nested, ksm bool) (lifecycle.NewRequest, error) {
	owner, err := b.st.UserByID(ownerID)
	if err != nil {
		return lifecycle.NewRequest{}, err
	}
	keys, err := b.st.SSHKeysForUser(ownerID)
	if err != nil {
		return lifecycle.NewRequest{}, err
	}
	authorized := make([]string, 0, len(keys)+1)
	for _, k := range keys {
		authorized = append(authorized, k.PublicKey)
	}
	if b.frontendKey != "" {
		authorized = append(authorized, b.frontendKey)
	}
	return lifecycle.NewRequest{
		Name:       name,
		Owner:      owner,
		HostID:     b.hostID,
		SSHKeys:    authorized,
		ImageName:  image,
		VCPU:       vcpu,
		MemoryMiB:  memoryMiB,
		DiskGiB:    diskGiB,
		Nested:     nested,
		DisableKSM: !ksm,
	}, nil
}

// cliBackend satisfies cli.Lifecycle.
type cliBackend struct{ backend }

var _ cli.Lifecycle = (*cliBackend)(nil)

func (b *cliBackend) Create(ctx context.Context, req cli.CreateRequest) (types.Instance, error) {
	nr, err := b.newRequest(req.OwnerID, req.Name, req.Image, req.VCPU, req.MemoryMiB, req.DiskGiB, req.Nested, req.KSM)
	if err != nil {
		return types.Instance{}, err
	}
	inst, err := b.m.New(ctx, nr)
	if err != nil {
		return types.Instance{}, err
	}
	if err := b.reloadFirewall(ctx); err != nil {
		return inst, fmt.Errorf("instance created, firewall reload failed: %w", err)
	}
	return inst, nil
}

func (b *cliBackend) Start(ctx context.Context, inst types.Instance) error {
	return b.m.Start(ctx, inst.UUID)
}

func (b *cliBackend) Stop(ctx context.Context, inst types.Instance) (hypervisor.StopResult, error) {
	return b.m.Stop(ctx, inst.UUID)
}

func (b *cliBackend) Restart(ctx context.Context, inst types.Instance) error {
	return b.m.Restart(ctx, inst.UUID)
}

func (b *cliBackend) Remove(ctx context.Context, inst types.Instance) error {
	if err := b.m.Remove(ctx, inst.UUID); err != nil {
		return err
	}
	if err := b.reloadFirewall(ctx); err != nil {
		return fmt.Errorf("instance removed, firewall reload failed: %w", err)
	}
	return nil
}

func (b *cliBackend) Rename(ctx context.Context, inst types.Instance, newName string) error {
	return b.m.Rename(ctx, inst.UUID, newName)
}

func (b *cliBackend) Copy(ctx context.Context, src types.Instance, req cli.CreateRequest) (types.Instance, error) {
	nr, err := b.newRequest(req.OwnerID, req.Name, req.Image, req.VCPU, req.MemoryMiB, req.DiskGiB, req.Nested, req.KSM)
	if err != nil {
		return types.Instance{}, err
	}
	inst, err := b.m.Copy(ctx, src.UUID, nr)
	if err != nil {
		return types.Instance{}, err
	}
	if err := b.reloadFirewall(ctx); err != nil {
		return inst, fmt.Errorf("instance copied, firewall reload failed: %w", err)
	}
	return inst, nil
}

func (b *cliBackend) Resize(ctx context.Context, inst types.Instance, req cli.ResizeRequest) error {
	full := lifecycle.ResizeRequest{
		UUID:      inst.UUID,
		VCPU:      inst.VCPU,
		MemoryMiB: inst.MemoryMiB,
		DiskGiB:   inst.DiskGiB,
		Nested:    inst.Nested,
	}
	if req.VCPU != nil {
		full.VCPU = *req.VCPU
	}
	if req.MemoryMiB != nil {
		full.MemoryMiB = *req.MemoryMiB
	}
	if req.DiskGiB != nil {
		full.DiskGiB = *req.DiskGiB
	}
	if req.Nested != nil {
		full.Nested = *req.Nested
	}
	_, err := b.m.Resize(ctx, full)
	return err
}

func (b *cliBackend) Console(context.Context, types.Instance, io.ReadWriter) error {
	return errors.New("console: the serial console is not wired in this build; connect with ssh <name>@<domain> instead")
}

func (b *cliBackend) SetHTTPPort(ctx context.Context, inst types.Instance, port int) error {
	if err := b.st.SetHTTPPort(inst.UUID, port); err != nil {
		return err
	}
	if err := b.reloadFirewall(ctx); err != nil {
		return fmt.Errorf("port stored, firewall reload failed: %w", err)
	}
	return nil
}

func (b *cliBackend) SetVisibility(ctx context.Context, inst types.Instance, v types.Visibility) error {
	if err := b.st.SetVisibility(inst.UUID, v); err != nil {
		return err
	}
	if err := b.reloadFirewall(ctx); err != nil {
		return fmt.Errorf("visibility stored, firewall reload failed: %w", err)
	}
	return nil
}

// apiBackend satisfies api.Lifecycle. Create, delete, port, and
// visibility changes reload the firewall: nftables rule 1 names the
// published addresses and ports (SPEC 6.3).
type apiBackend struct {
	backend
}

var _ api.Lifecycle = (*apiBackend)(nil)

func (b *apiBackend) Create(ctx context.Context, owner types.User, spec api.CreateSpec) (types.Instance, error) {
	nr, err := b.newRequest(owner.ID, spec.Name, spec.Image, spec.VCPU, spec.MemoryMiB, spec.DiskGiB, spec.Nested, spec.KSM)
	if err != nil {
		return types.Instance{}, err
	}
	inst, err := b.m.New(ctx, nr)
	if err != nil {
		return types.Instance{}, err
	}
	if err := b.reloadFirewall(ctx); err != nil {
		return inst, fmt.Errorf("instance created, firewall reload failed: %w", err)
	}
	return inst, nil
}

func (b *apiBackend) Delete(ctx context.Context, uuid string) error {
	if err := b.m.Remove(ctx, uuid); err != nil {
		return err
	}
	if err := b.reloadFirewall(ctx); err != nil {
		return fmt.Errorf("instance removed, firewall reload failed: %w", err)
	}
	return nil
}

func (b *apiBackend) Start(ctx context.Context, uuid string) error { return b.m.Start(ctx, uuid) }

func (b *apiBackend) Stop(ctx context.Context, uuid string) error {
	_, err := b.m.Stop(ctx, uuid)
	return err
}

func (b *apiBackend) Restart(ctx context.Context, uuid string) error { return b.m.Restart(ctx, uuid) }

func (b *apiBackend) Rename(ctx context.Context, uuid, newName string) error {
	return b.m.Rename(ctx, uuid, newName)
}

func (b *apiBackend) Resize(ctx context.Context, uuid string, spec api.ResizeSpec) error {
	_, err := b.m.Resize(ctx, lifecycle.ResizeRequest{
		UUID:      uuid,
		VCPU:      spec.VCPU,
		MemoryMiB: spec.MemoryMiB,
		DiskGiB:   spec.DiskGiB,
		Nested:    spec.Nested,
	})
	return err
}

func (b *apiBackend) SetHTTPPort(ctx context.Context, uuid string, port int) error {
	if err := b.st.SetHTTPPort(uuid, port); err != nil {
		return err
	}
	if err := b.reloadFirewall(ctx); err != nil {
		return fmt.Errorf("port stored, firewall reload failed: %w", err)
	}
	return nil
}

func (b *apiBackend) SetVisibility(ctx context.Context, uuid string, v types.Visibility) error {
	if err := b.st.SetVisibility(uuid, v); err != nil {
		return err
	}
	if err := b.reloadFirewall(ctx); err != nil {
		return fmt.Errorf("visibility stored, firewall reload failed: %w", err)
	}
	return nil
}

// ---- auth ----

// authUsers satisfies auth.UserStore.
type authUsers struct{ st *store.Store }

var _ auth.UserStore = authUsers{}

func (a authUsers) UserByOIDCSubject(subject string) (types.User, bool, error) {
	if subject == "" {
		return types.User{}, false, nil
	}
	u, err := a.st.UserByOIDCSubject(subject)
	if errors.Is(err, store.ErrNotFound) {
		return types.User{}, false, nil
	}
	if err != nil {
		return types.User{}, false, err
	}
	return u, true, nil
}

// authTokens satisfies auth.TokenStore.
type authTokens struct{ st *store.Store }

var _ auth.TokenStore = authTokens{}

func (a authTokens) CreateToken(userID int64, hash string, expiresAt time.Time) (types.Token, error) {
	id, err := a.st.CreateToken(userID, hash, expiresAt)
	if err != nil {
		return types.Token{}, err
	}
	return types.Token{ID: id, UserID: userID, Hash: hash, ExpiresAt: expiresAt}, nil
}

func (a authTokens) TokenByHash(hash string) (types.Token, bool, error) {
	t, err := a.st.TokenByHash(hash)
	switch {
	case errors.Is(err, store.ErrNotFound):
		return types.Token{}, false, nil
	case errors.Is(err, store.ErrTokenExpired):
		// Return the row; the auth service enforces expiry with its
		// own clock.
		return t, true, nil
	case err != nil:
		return types.Token{}, false, err
	}
	return t, true, nil
}

func (a authTokens) DeleteToken(id int64) error {
	err := a.st.DeleteTokenByID(id)
	if errors.Is(err, store.ErrNotFound) {
		return nil // revoking a token twice reaches the same state
	}
	return err
}

// authenticator satisfies api.Authenticator: a request carries either
// the base-domain session cookie or a bearer token (SPEC 13).
type authenticator struct {
	svc *auth.Service
	st  *store.Store
}

var _ api.Authenticator = (*authenticator)(nil)

func (a *authenticator) UserFromRequest(r *http.Request) (types.User, error) {
	if sess, ok := a.svc.SessionFromRequest(r); ok {
		return a.st.UserByID(sess.UserID)
	}
	if plaintext := auth.BearerToken(r); plaintext != "" {
		tok, err := a.svc.AuthenticateToken(plaintext)
		if err != nil {
			return types.User{}, err
		}
		return a.st.UserByID(tok.UserID)
	}
	return types.User{}, auth.ErrUnauthenticated
}

// ---- proxy ----

// proxySource satisfies proxy.InstanceSource. A name that never
// existed, a deleted name, and a name in cooldown all map to ok=false,
// which the proxy answers identically (SPEC 9.2).
type proxySource struct{ st *store.Store }

var _ proxy.InstanceSource = proxySource{}

func (p proxySource) InstanceByName(_ context.Context, name string) (types.Instance, bool, error) {
	inst, err := p.st.InstanceByName(name)
	if errors.Is(err, store.ErrNotFound) {
		return types.Instance{}, false, nil
	}
	if err != nil {
		return types.Instance{}, false, err
	}
	return inst, true, nil
}

// TouchLastSeen satisfies proxy.LastSeenRecorder: a forwarded HTTP
// request updates last_seen_at (SPEC 12).
func (p proxySource) TouchLastSeen(_ context.Context, uuid string) error {
	return p.st.TouchLastSeen(uuid)
}

var _ proxy.LastSeenRecorder = proxySource{}

// remoteSession satisfies proxy.SessionChecker from the proxy process:
// sessions live in the control plane's memory, so the proxy asks the
// control plane by forwarding the request's credentials to the
// /access/{uuid} endpoint, which runs the SPEC 13 authorization —
// owner or share on the instance UUID — on every request.
type remoteSession struct {
	base   string // control plane URL, e.g. http://127.0.0.1:8080
	client *http.Client
}

var _ proxy.SessionChecker = (*remoteSession)(nil)

func (s *remoteSession) Access(r *http.Request, instanceUUID string) proxy.Access {
	req, err := http.NewRequestWithContext(r.Context(), http.MethodGet,
		s.base+"/access/"+url.PathEscape(instanceUUID), nil)
	if err != nil {
		return proxy.AccessForbidden
	}
	if c := r.Header.Get("Cookie"); c != "" {
		req.Header.Set("Cookie", c)
	}
	if a := r.Header.Get("Authorization"); a != "" {
		req.Header.Set("Authorization", a)
	}
	resp, err := s.client.Do(req)
	if err != nil {
		// Fail closed: an unreachable control plane must not open a
		// private instance, and the uniform 404 reveals nothing.
		return proxy.AccessForbidden
	}
	defer resp.Body.Close()
	io.Copy(io.Discard, resp.Body)
	switch resp.StatusCode {
	case http.StatusNoContent, http.StatusOK:
		return proxy.AccessGranted
	case http.StatusUnauthorized:
		return proxy.AccessUnauthenticated
	default:
		return proxy.AccessForbidden
	}
}

// accessHandler is the control plane side of the proxy's per-request
// authorization (SPEC 13): it resolves the session cookie or bearer
// token and answers whether that user owns the instance or holds a
// share on its UUID. 204 grants, 401 means no valid credential, 403
// means a valid credential without access.
func accessHandler(svc *auth.Service) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		uuid := r.PathValue("uuid")
		_, err := svc.AuthorizeRequest(r, uuid)
		if errors.Is(err, auth.ErrUnauthenticated) {
			if plaintext := auth.BearerToken(r); plaintext != "" {
				tok, tokErr := svc.AuthenticateToken(plaintext)
				if tokErr == nil {
					err = svc.AuthorizeUser(tok.UserID, uuid)
				}
			}
		}
		switch {
		case err == nil:
			w.WriteHeader(http.StatusNoContent)
		case errors.Is(err, auth.ErrForbidden):
			http.Error(w, "forbidden", http.StatusForbidden)
		default:
			http.Error(w, "unauthenticated", http.StatusUnauthorized)
		}
	})
}

// ---- SSH frontend ----

// starter satisfies sshfront.Starter. It starts the domain and touches
// nothing else: SPEC 10 step 7 must not change the desired state, so a
// later host reboot returns the instance to what the user last asked
// for (SPEC 11.2).
type starter struct{ hyp hypervisor.Hypervisor }

var _ sshfront.Starter = starter{}

func (s starter) StartInstance(ctx context.Context, inst types.Instance) error {
	return s.hyp.Start(ctx, inst.Name)
}

// networkEnsurer defines the named libvirt network when missing and
// starts it when inactive. *hypervisor.Client implements it.
type networkEnsurer interface {
	EnsureNetwork(ctx context.Context, name, xml string) error
}

// registrar satisfies sshfront.Registrar: user row with the lowest free
// /24, key row, and the libvirt network of the user (SPEC 13). A failed
// network define does not fail the registration; the control plane
// re-ensures every user network on its poll loop. A successful
// registration reloads the nftables table (SPEC 6.3): the new bridge
// must appear in the inter-user drop rules at once, not on the next
// convergence tick.
type registrar struct {
	st       *store.Store
	plan     network.Plan
	networks networkEnsurer
	fw       *firewall // nil skips the reload
	log      *slog.Logger
}

var _ sshfront.Registrar = (*registrar)(nil)

func (r *registrar) Register(ctx context.Context, reg sshfront.Registration) (types.User, error) {
	u, err := r.st.RegisterUser(reg.Name, reg.Email, "", r.plan.Range())
	if err != nil {
		return types.User{}, err
	}
	if _, err := r.st.AddSSHKey(u.ID, reg.PublicKey, reg.Fingerprint, reg.Comment); err != nil {
		return types.User{}, err
	}
	name, xml, err := userNetwork(r.plan, u.Subnet)
	if err != nil {
		return types.User{}, err
	}
	if r.networks == nil {
		r.log.Warn("registration: no libvirt connection; the control plane will define the user network", "user", u.Name)
		return u, nil
	}
	if err := r.networks.EnsureNetwork(ctx, name, xml); err != nil {
		r.log.Warn("registration: user network not defined yet; the control plane will retry",
			"user", u.Name, "network", name, "error", err)
	}
	// SPEC 6.3: reload the table on every change. Without this the new
	// bridge would sit unlisted — and reachable through every existing
	// user's egress accept — until the next convergence tick.
	if r.fw != nil {
		if err := r.fw.reload(ctx); err != nil {
			r.log.Warn("registration: firewall reload failed; the control plane will retry",
				"user", u.Name, "error", err)
		}
	}
	return u, nil
}

// userNetwork renders the libvirt network of a user subnet (SPEC 6.2).
func userNetwork(plan network.Plan, subnet string) (name, xml string, err error) {
	prefix, err := netip.ParsePrefix(subnet)
	if err != nil {
		return "", "", fmt.Errorf("user subnet %q: %w", subnet, err)
	}
	index, err := plan.Index(prefix)
	if err != nil {
		return "", "", err
	}
	un, err := network.NewUserNetwork(plan, index)
	if err != nil {
		return "", "", err
	}
	xml, err = un.XML()
	if err != nil {
		return "", "", err
	}
	return un.Name, xml, nil
}
