package sshfront

import (
	"bytes"
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"errors"
	"fmt"
	"io"
	"net"
	"sync"
	"testing"
	"time"

	gliderssh "github.com/gliderlabs/ssh"
	gossh "golang.org/x/crypto/ssh"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// fakeContext implements gliderssh.Context for publicKeyHandler tests.
type fakeContext struct {
	context.Context
	sync.Mutex
	user   string
	values map[any]any
}

func newFakeContext(user string) *fakeContext {
	return &fakeContext{Context: context.Background(), user: user, values: map[any]any{}}
}

func (c *fakeContext) User() string                        { return c.user }
func (c *fakeContext) SessionID() string                   { return "test" }
func (c *fakeContext) ClientVersion() string               { return "SSH-2.0-test" }
func (c *fakeContext) ServerVersion() string               { return "SSH-2.0-test" }
func (c *fakeContext) RemoteAddr() net.Addr                { return nil }
func (c *fakeContext) LocalAddr() net.Addr                 { return nil }
func (c *fakeContext) Permissions() *gliderssh.Permissions { return nil }
func (c *fakeContext) SetValue(key, value any)             { c.values[key] = value }
func (c *fakeContext) Value(key any) any {
	if v, ok := c.values[key]; ok {
		return v
	}
	return c.Context.Value(key)
}

func newTestKey(t *testing.T) (gossh.Signer, gossh.PublicKey, string) {
	t.Helper()
	pub, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	signer, err := gossh.NewSignerFromKey(priv)
	if err != nil {
		t.Fatal(err)
	}
	sshPub, err := gossh.NewPublicKey(pub)
	if err != nil {
		t.Fatal(err)
	}
	return signer, sshPub, gossh.FingerprintSHA256(sshPub)
}

func TestPublicKeyHandler(t *testing.T) {
	_, knownPub, knownFP := newTestKey(t)
	_, unknownPub, _ := newTestKey(t)

	keys := &fakeKeys{
		byFingerprint: map[string]types.SSHKey{knownFP: {ID: 1, UserID: 1, Fingerprint: knownFP}},
		users:         map[int64]types.User{1: frank},
	}

	tests := []struct {
		name         string
		key          gossh.PublicKey
		username     string
		registrar    Registrar
		keysErr      error
		wantAllow    bool
		wantUser     bool
		wantRegister bool
	}{
		{name: "known key any username", key: knownPub, username: "web", wantAllow: true, wantUser: true},
		{name: "known key cli", key: knownPub, username: "", wantAllow: true, wantUser: true},
		// A stock ssh client always sends a username (the local login
		// name), so registration must not depend on the username field
		// (SPEC 13).
		{name: "unknown key with username registers", key: unknownPub, username: "web", registrar: &fakeRegistrar{}, wantAllow: true, wantRegister: true},
		{name: "unknown key empty username registers", key: unknownPub, username: "", registrar: &fakeRegistrar{}, wantAllow: true, wantRegister: true},
		{name: "unknown key without registrar rejected", key: unknownPub, username: ""},
		{name: "unknown key without registrar rejected with username", key: unknownPub, username: "web"},
		{name: "store failure rejects, never registers", key: unknownPub, username: "", registrar: &fakeRegistrar{}, keysErr: errors.New("db locked")},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			s, _, _, _ := testServer()
			keys.err = tt.keysErr
			s.Keys = keys
			s.Registrar = tt.registrar
			ctx := newFakeContext(tt.username)
			got := s.publicKeyHandler(ctx, tt.key)
			if got != tt.wantAllow {
				t.Fatalf("allow = %v, want %v", got, tt.wantAllow)
			}
			_, hasUser := ctx.values[userKey{}]
			if hasUser != tt.wantUser {
				t.Errorf("user in context = %v, want %v", hasUser, tt.wantUser)
			}
			reg, hasReg := ctx.values[registrationKey{}]
			if hasReg != tt.wantRegister {
				t.Errorf("registration in context = %v, want %v", hasReg, tt.wantRegister)
			}
			if hasReg {
				r := reg.(Registration)
				if r.Fingerprint != gossh.FingerprintSHA256(tt.key) || r.PublicKey == "" {
					t.Errorf("registration %+v misses the presented key", r)
				}
			}
		})
	}
}

// TestEndToEndExec drives the full path of SPEC 10 over loopback: a
// real SSH client connects to the frontend, which authenticates the
// key, resolves the instance, dials a fake guest sshd, and joins the
// two connections including the exit status.
func TestEndToEndExec(t *testing.T) {
	clientSigner, _, clientFP := newTestKey(t)
	frontendSigner, _, _ := newTestKey(t)
	guestSigner, _, _ := newTestKey(t)

	// The fake guest: an SSH server that accepts any key, echoes the
	// command, and exits 7.
	guestLn, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer guestLn.Close()
	guest := &gliderssh.Server{
		Handler: func(sess gliderssh.Session) {
			fmt.Fprintf(sess, "guest ran: %s\n", sess.RawCommand())
			sess.Exit(7)
		},
		PublicKeyHandler: func(gliderssh.Context, gliderssh.PublicKey) bool { return true },
		HostSigners:      []gliderssh.Signer{guestSigner},
	}
	go guest.Serve(guestLn)
	defer guest.Close()

	// The frontend, with the instance address rewritten to the guest
	// listener.
	instances := &fakeInstances{
		byName: map[string]types.Instance{"web": webVM},
		access: map[string][]int64{"uuid-web": {1}},
	}
	front := &Server{
		Keys: &fakeKeys{
			byFingerprint: map[string]types.SSHKey{clientFP: {ID: 1, UserID: 1, Fingerprint: clientFP}},
			users:         map[int64]types.User{1: frank},
		},
		Instances: instances,
		Starter:   &fakeStarter{},
		HostKey:   frontendSigner,
		GuestUser: "bento",
		GuestAuth: []gossh.AuthMethod{gossh.PublicKeys(clientSigner)},
		Dial: func(ctx context.Context, network, _ string) (net.Conn, error) {
			var d net.Dialer
			return d.DialContext(ctx, network, guestLn.Addr().String())
		},
	}
	frontLn, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer frontLn.Close()
	srv := front.SSHServer("")
	go srv.Serve(frontLn)
	defer srv.Close()

	// A real client: ssh web@frontend "uname -a".
	client, err := gossh.Dial("tcp", frontLn.Addr().String(), &gossh.ClientConfig{
		User:            "web",
		Auth:            []gossh.AuthMethod{gossh.PublicKeys(clientSigner)},
		HostKeyCallback: gossh.FixedHostKey(frontendSigner.PublicKey()),
		Timeout:         5 * time.Second,
	})
	if err != nil {
		t.Fatalf("dial frontend: %v", err)
	}
	defer client.Close()
	sess, err := client.NewSession()
	if err != nil {
		t.Fatal(err)
	}
	defer sess.Close()
	var out bytes.Buffer
	sess.Stdout = &out
	err = sess.Run("uname -a")

	var exitErr *gossh.ExitError
	if !errors.As(err, &exitErr) || exitErr.ExitStatus() != 7 {
		t.Fatalf("run: %v, want exit status 7", err)
	}
	if got := out.String(); got != "guest ran: uname -a\n" {
		t.Errorf("stdout %q", got)
	}
	if len(instances.touched) != 1 || instances.touched[0] != "uuid-web" {
		t.Errorf("last_seen_at touched %v, want [uuid-web]", instances.touched)
	}
}

// TestEndToEndUnknownKeyRejected proves SPEC 10 step 3 over a real
// handshake: with no registrar wired, an unregistered key never reaches
// a session, whatever the username says.
func TestEndToEndUnknownKeyRejected(t *testing.T) {
	clientSigner, _, _ := newTestKey(t)
	frontendSigner, _, _ := newTestKey(t)
	front := &Server{
		Keys:      &fakeKeys{byFingerprint: map[string]types.SSHKey{}},
		Instances: &fakeInstances{},
		Starter:   &fakeStarter{},
		HostKey:   frontendSigner,
		Registrar: nil, // registration disabled: unknown keys are rejected
	}
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	srv := front.SSHServer("")
	go srv.Serve(ln)
	defer srv.Close()

	for _, user := range []string{"web", ""} {
		_, err = gossh.Dial("tcp", ln.Addr().String(), &gossh.ClientConfig{
			User:            user,
			Auth:            []gossh.AuthMethod{gossh.PublicKeys(clientSigner)},
			HostKeyCallback: gossh.InsecureIgnoreHostKey(),
			Timeout:         5 * time.Second,
		})
		if err == nil {
			t.Fatalf("an unknown key authenticated as %q", user)
		}
	}
}

// TestEndToEndCLISession proves the CLI path over a real handshake,
// including stdout and the exit code. The client sends a username, as
// every stock ssh client does (`ssh bento.foid.space ls` sends the
// local login name); the name matches no instance, so the session runs
// the command line interface (SPEC 15).
func TestEndToEndCLISession(t *testing.T) {
	clientSigner, clientPub, clientFP := newTestKey(t)
	_ = clientPub
	frontendSigner, _, _ := newTestKey(t)
	cli := &fakeCLI{code: 0}
	front := &Server{
		Keys: &fakeKeys{
			byFingerprint: map[string]types.SSHKey{clientFP: {ID: 1, UserID: 1}},
			users:         map[int64]types.User{1: frank},
		},
		Instances: &fakeInstances{},
		Starter:   &fakeStarter{},
		CLI:       cli,
		HostKey:   frontendSigner,
	}
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	srv := front.SSHServer("")
	go srv.Serve(ln)
	defer srv.Close()

	client, err := gossh.Dial("tcp", ln.Addr().String(), &gossh.ClientConfig{
		User:            "shaun", // the local login name of a stock client
		Auth:            []gossh.AuthMethod{gossh.PublicKeys(clientSigner)},
		HostKeyCallback: gossh.FixedHostKey(frontendSigner.PublicKey()),
		Timeout:         5 * time.Second,
	})
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer client.Close()
	sess, err := client.NewSession()
	if err != nil {
		t.Fatal(err)
	}
	defer sess.Close()
	out, err := sess.Output("ls")
	if err != nil {
		t.Fatalf("run: %v", err)
	}
	if string(out) != "cli ran\n" {
		t.Errorf("stdout %q", out)
	}
	if cli.user.Name != "frank" || len(cli.args) != 1 || cli.args[0] != "ls" {
		t.Errorf("CLI saw user %q args %v", cli.user.Name, cli.args)
	}
}

var _ io.Reader = (*fakeSession)(nil)
