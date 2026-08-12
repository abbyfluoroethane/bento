// Package sshfront is the SSH frontend: public key authentication,
// instance forwarding, the registration flow, and the command line
// session (SPEC sections 10, 13, 15).
//
// One server on one address answers every connection with one host key.
// The SSH user name field carries the instance name. A stock ssh client
// always sends a user name — `ssh bento.foid.space` sends the local
// login name — so the frontend cannot demand an empty one: a known key
// whose user name is not an instance the user can reach runs the
// command line interface, and an unknown key runs the registration flow
// whatever the user name says.
package sshfront

import (
	"context"
	"errors"
	"io"
	"net"
	"strings"
	"time"

	gliderssh "github.com/gliderlabs/ssh"
	gossh "golang.org/x/crypto/ssh"

	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// Defaults.
const (
	// DefaultStartTimeout is how long the frontend waits for sshd in a
	// freshly started instance (SPEC 10 step 8).
	DefaultStartTimeout = 120 * time.Second
	// DefaultDialInterval is the pause between connection attempts
	// during that wait.
	DefaultDialInterval = 2 * time.Second
	// DefaultGuestUser is the one account cloud-init creates in every
	// instance (SPEC 5.2).
	DefaultGuestUser = "bento"
)

// KeyStore resolves a presented public key to a user. The SSH frontend
// looks keys up by fingerprint on every connection (SPEC 12).
// *store.Store satisfies it.
type KeyStore interface {
	SSHKeyByFingerprint(fingerprint string) (types.SSHKey, error)
	UserByID(id int64) (types.User, error)
}

// InstanceStore resolves names and authorization. *store.Store
// satisfies it. The interface has no way to change the desired state:
// SPEC 10 step 7 starts an instance without recording a user intent.
type InstanceStore interface {
	InstanceByName(name string) (types.Instance, error)
	HasAccess(instanceUUID string, userID int64) (bool, error)
	TouchLastSeen(uuid string) error
}

// Starter starts a stopped instance on behalf of an SSH connection.
// Implementations must not change the desired state (SPEC 10 step 7,
// 11.2): a later host reboot returns the instance to what the user
// last asked for.
type Starter interface {
	StartInstance(ctx context.Context, inst types.Instance) error
}

// CLIRunner runs one command line session (SPEC 15). *cli.CLI
// satisfies it.
type CLIRunner interface {
	Run(ctx context.Context, user types.User, args []string, stdin io.Reader, stdout, stderr io.Writer) int
}

// Registration is a new-user request from the SPEC 13 flow.
type Registration struct {
	Name        string
	Email       string
	PublicKey   string // authorized_keys format
	Fingerprint string // SHA256 fingerprint of PublicKey
	Comment     string
}

// Registrar creates an account: user row, key row, subnet, and the
// libvirt network of the user (SPEC 13). Integration wires it to the
// store and the network packages.
type Registrar interface {
	Register(ctx context.Context, reg Registration) (types.User, error)
}

// DialFunc dials the internal address of an instance. Tests inject a
// fake; production uses a net.Dialer.
type DialFunc func(ctx context.Context, network, addr string) (net.Conn, error)

// Server is the SSH frontend. Fill the exported fields and call
// ListenAndServe or Serve.
type Server struct {
	Keys      KeyStore
	Instances InstanceStore
	Starter   Starter
	CLI       CLIRunner
	Registrar Registrar // nil disables registration; unknown keys are rejected

	// HostKey is the one host key every connection sees (SPEC 10).
	HostKey gossh.Signer

	// GuestUser and GuestAuth authenticate the frontend to sshd inside
	// the instance (SPEC 10 step 9). GuestAuth normally holds the
	// frontend key that cloud-init installed alongside the owner keys.
	GuestUser string
	GuestAuth []gossh.AuthMethod

	// Dial, StartTimeout, DialInterval, Now, and Sleep are injectable
	// for tests. Zero values select the production defaults.
	Dial         DialFunc
	StartTimeout time.Duration
	DialInterval time.Duration
	Now          func() time.Time
	Sleep        func(time.Duration)
}

func (s *Server) dial() DialFunc {
	if s.Dial != nil {
		return s.Dial
	}
	d := &net.Dialer{Timeout: 5 * time.Second}
	return d.DialContext
}

func (s *Server) startTimeout() time.Duration {
	if s.StartTimeout > 0 {
		return s.StartTimeout
	}
	return DefaultStartTimeout
}

func (s *Server) dialInterval() time.Duration {
	if s.DialInterval > 0 {
		return s.DialInterval
	}
	return DefaultDialInterval
}

func (s *Server) now() time.Time {
	if s.Now != nil {
		return s.Now()
	}
	return time.Now()
}

func (s *Server) sleep(d time.Duration) {
	if s.Sleep != nil {
		s.Sleep(d)
		return
	}
	time.Sleep(d)
}

func (s *Server) guestUser() string {
	if s.GuestUser != "" {
		return s.GuestUser
	}
	return DefaultGuestUser
}

// SSHServer builds the underlying gliderlabs server listening on addr.
func (s *Server) SSHServer(addr string) *gliderssh.Server {
	return &gliderssh.Server{
		Addr:             addr,
		Handler:          s.handleSession,
		PublicKeyHandler: s.publicKeyHandler,
		HostSigners:      []gliderssh.Signer{s.HostKey},
	}
}

// ListenAndServe serves on addr until the listener fails.
func (s *Server) ListenAndServe(addr string) error {
	return s.SSHServer(addr).ListenAndServe()
}

// Serve serves on an existing listener.
func (s *Server) Serve(l net.Listener) error {
	return s.SSHServer("").Serve(l)
}

// Context keys for values set during authentication.
type userKey struct{}
type registrationKey struct{}

// publicKeyHandler implements SPEC 10 steps 1-3. An unknown key is
// rejected (step 3) unless a Registrar is wired: SPEC 13 registers a
// new user on connecting with an unknown key, and because a stock ssh
// client always sends a user name (its local login name), registration
// must not depend on the user name field.
func (s *Server) publicKeyHandler(ctx gliderssh.Context, key gliderssh.PublicKey) bool {
	fingerprint := gossh.FingerprintSHA256(key)
	k, err := s.Keys.SSHKeyByFingerprint(fingerprint)
	switch {
	case err == nil:
		user, err := s.Keys.UserByID(k.UserID)
		if err != nil {
			return false
		}
		ctx.SetValue(userKey{}, user)
		return true
	case errors.Is(err, store.ErrNotFound):
		if s.Registrar == nil {
			return false
		}
		ctx.SetValue(registrationKey{}, Registration{
			// MarshalAuthorizedKey ends the line with a newline. The
			// stored key goes into a guest's authorized_keys through
			// the cloud-init seed, which rejects a control character
			// in a key (SPEC 4.2), so the line is stored without one.
			PublicKey:   strings.TrimSpace(string(gossh.MarshalAuthorizedKey(key))),
			Fingerprint: fingerprint,
		})
		return true
	default:
		// A data layer failure is not an unknown key. Reject; never
		// fall through to registration.
		return false
	}
}
