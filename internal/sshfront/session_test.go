package sshfront

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"strings"
	"sync"
	"testing"
	"time"

	gliderssh "github.com/gliderlabs/ssh"

	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// fakeSession implements termSession in memory.
type fakeSession struct {
	user string
	raw  string
	cmd  []string
	pty  bool

	in     io.Reader
	out    bytes.Buffer
	errOut bytes.Buffer

	exitCode int
	exited   bool
}

func newFakeSession(user, input string) *fakeSession {
	return &fakeSession{user: user, in: strings.NewReader(input)}
}

func (f *fakeSession) Read(p []byte) (int, error)  { return f.in.Read(p) }
func (f *fakeSession) Write(p []byte) (int, error) { return f.out.Write(p) }
func (f *fakeSession) User() string                { return f.user }
func (f *fakeSession) Stderr() io.ReadWriter       { return &f.errOut }
func (f *fakeSession) Command() []string           { return f.cmd }
func (f *fakeSession) RawCommand() string          { return f.raw }
func (f *fakeSession) Exit(code int) error {
	f.exitCode, f.exited = code, true
	return nil
}
func (f *fakeSession) Pty() (gliderssh.Pty, <-chan gliderssh.Window, bool) {
	return gliderssh.Pty{}, nil, f.pty
}

type fakeKeys struct {
	byFingerprint map[string]types.SSHKey
	users         map[int64]types.User
	err           error
}

func (f *fakeKeys) SSHKeyByFingerprint(fp string) (types.SSHKey, error) {
	if f.err != nil {
		return types.SSHKey{}, f.err
	}
	k, ok := f.byFingerprint[fp]
	if !ok {
		return types.SSHKey{}, store.ErrNotFound
	}
	return k, nil
}

func (f *fakeKeys) UserByID(id int64) (types.User, error) {
	u, ok := f.users[id]
	if !ok {
		return types.User{}, store.ErrNotFound
	}
	return u, nil
}

type fakeInstances struct {
	byName  map[string]types.Instance
	access  map[string][]int64
	touched []string
}

func (f *fakeInstances) InstanceByName(name string) (types.Instance, error) {
	inst, ok := f.byName[name]
	if !ok {
		return types.Instance{}, store.ErrNotFound
	}
	return inst, nil
}

func (f *fakeInstances) HasAccess(uuid string, userID int64) (bool, error) {
	for _, id := range f.access[uuid] {
		if id == userID {
			return true, nil
		}
	}
	return false, nil
}

func (f *fakeInstances) TouchLastSeen(uuid string) error {
	f.touched = append(f.touched, uuid)
	return nil
}

type fakeStarter struct {
	started []string
	err     error
}

func (f *fakeStarter) StartInstance(_ context.Context, inst types.Instance) error {
	if f.err != nil {
		return f.err
	}
	f.started = append(f.started, inst.Name)
	return nil
}

type fakeCLI struct {
	user types.User
	args []string
	code int
}

func (f *fakeCLI) Run(_ context.Context, user types.User, args []string, _ io.Reader, stdout, _ io.Writer) int {
	f.user, f.args = user, args
	fmt.Fprintln(stdout, "cli ran")
	return f.code
}

type fakeRegistrar struct {
	got  Registration
	user types.User
	err  error
}

func (f *fakeRegistrar) Register(_ context.Context, reg Registration) (types.User, error) {
	if f.err != nil {
		return types.User{}, f.err
	}
	f.got = reg
	return f.user, nil
}

// fakeClock backs Now and Sleep: sleeping advances the clock.
type fakeClock struct {
	mu     sync.Mutex
	t      time.Time
	sleeps int
}

func (c *fakeClock) Now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.t
}

func (c *fakeClock) Sleep(d time.Duration) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.t = c.t.Add(d)
	c.sleeps++
}

var (
	frank = types.User{ID: 1, Name: "frank", Subnet: "10.100.0.0/24"}
	webVM = types.Instance{
		UUID: "uuid-web", Name: "web", OwnerID: 1,
		State: types.StateRunning, Address: "10.100.0.2",
	}
)

func testServer() (*Server, *fakeInstances, *fakeStarter, *fakeClock) {
	instances := &fakeInstances{
		byName: map[string]types.Instance{"web": webVM},
		access: map[string][]int64{"uuid-web": {1}},
	}
	starter := &fakeStarter{}
	clock := &fakeClock{t: time.Date(2026, 8, 10, 12, 0, 0, 0, time.UTC)}
	s := &Server{
		Instances: instances,
		Starter:   starter,
		Dial: func(context.Context, string, string) (net.Conn, error) {
			return nil, errors.New("connection refused")
		},
		Now:   clock.Now,
		Sleep: clock.Sleep,
	}
	return s, instances, starter, clock
}

func userContext(u types.User) context.Context {
	return context.WithValue(context.Background(), userKey{}, u)
}

// TestDispatchRunsCLI pins the SPEC 15 interface for stock ssh clients:
// a real client always sends a user name (its local login name), so any
// known-user session whose user name is not an accessible instance runs
// the command line interface — the empty user name, an unknown name,
// and a name the user has no access to all behave identically, so names
// cannot be probed.
func TestDispatchRunsCLI(t *testing.T) {
	tests := []struct {
		name     string
		username string
		noAccess bool
	}{
		{name: "empty username", username: ""},
		{name: "stock client local login name", username: "shaun"},
		{name: "inaccessible instance name", username: "web", noAccess: true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			s, instances, starter, _ := testServer()
			if tt.noAccess {
				instances.access = nil
			}
			cli := &fakeCLI{code: 3}
			s.CLI = cli
			sess := newFakeSession(tt.username, "")
			sess.cmd = []string{"ls"}
			s.dispatch(userContext(frank), sess)
			if cli.user.Name != "frank" || len(cli.args) != 1 || cli.args[0] != "ls" {
				t.Errorf("CLI ran with user %q args %v", cli.user.Name, cli.args)
			}
			if !sess.exited || sess.exitCode != 3 {
				t.Errorf("exit = %v %d, want true 3", sess.exited, sess.exitCode)
			}
			if len(starter.started) != 0 {
				t.Errorf("starter called for a CLI session")
			}
		})
	}
}

// TestDispatchForwardsAccessibleInstance pins SPEC 10 steps 4-6: a user
// name that resolves to an instance the user owns or shares goes to the
// instance, not the CLI.
func TestDispatchForwardsAccessibleInstance(t *testing.T) {
	s, _, _, _ := testServer()
	cli := &fakeCLI{}
	s.CLI = cli
	sess := newFakeSession("web", "")
	s.dispatch(userContext(frank), sess)
	if cli.args != nil {
		t.Errorf("CLI ran (%v) for an accessible instance name", cli.args)
	}
	// The fake dialer always refuses, so the forward path is visible in
	// the timeout failure — proof the session went to the instance.
	if !strings.Contains(sess.errOut.String(), "did not accept an SSH connection") {
		t.Errorf("stderr %q does not show the instance forward", sess.errOut.String())
	}
}

func TestProxyStartsStoppedInstance(t *testing.T) {
	s, instances, starter, clock := testServer()
	stopped := webVM
	stopped.State = types.StateStopped
	instances.byName["web"] = stopped
	s.StartTimeout = 120 * time.Second
	s.DialInterval = 2 * time.Second

	sess := newFakeSession("web", "")
	s.proxy(context.Background(), stopped, sess)

	// SPEC 10 step 7: the instance is started and the session is told.
	if len(starter.started) != 1 || starter.started[0] != "web" {
		t.Fatalf("started %v, want [web]", starter.started)
	}
	if !strings.Contains(sess.out.String(), "bento: starting web") {
		t.Errorf("stdout %q misses the starting line", sess.out.String())
	}
	// The dialer never succeeds: after the 120 s budget the session
	// gets a clear failure (SPEC 10 step 8).
	if !strings.Contains(sess.errOut.String(), "did not accept an SSH connection within 2m0s") {
		t.Errorf("stderr %q misses the timeout failure", sess.errOut.String())
	}
	if sess.exitCode != 1 {
		t.Errorf("exit %d, want 1", sess.exitCode)
	}
	if elapsed := clock.Now().Sub(time.Date(2026, 8, 10, 12, 0, 0, 0, time.UTC)); elapsed > 120*time.Second {
		t.Errorf("waited %s of fake time, more than the timeout", elapsed)
	}
	if len(instances.touched) != 0 {
		t.Errorf("last_seen_at touched despite a failed connection")
	}
}

func TestProxyRunningInstanceDoesNotStart(t *testing.T) {
	s, _, starter, _ := testServer()
	sess := newFakeSession("web", "")
	s.proxy(context.Background(), webVM, sess)
	if len(starter.started) != 0 {
		t.Errorf("starter called for a running instance")
	}
	if strings.Contains(sess.out.String(), "starting") {
		t.Errorf("stdout %q has a starting line for a running instance", sess.out.String())
	}
}

func TestWaitSSHRetriesThenSucceeds(t *testing.T) {
	clock := &fakeClock{t: time.Unix(0, 0)}
	attempts := 0
	client, server := net.Pipe()
	defer client.Close()
	defer server.Close()
	dial := func(context.Context, string, string) (net.Conn, error) {
		attempts++
		if attempts < 3 {
			return nil, errors.New("connection refused")
		}
		return client, nil
	}
	conn, err := waitSSH(context.Background(), dial, "10.0.0.2:22", 120*time.Second, 2*time.Second, clock.Now, clock.Sleep)
	if err != nil {
		t.Fatalf("waitSSH: %v", err)
	}
	if conn != client {
		t.Errorf("returned a different conn")
	}
	if attempts != 3 || clock.sleeps != 2 {
		t.Errorf("attempts %d sleeps %d, want 3 and 2", attempts, clock.sleeps)
	}
}

func TestWaitSSHTimesOut(t *testing.T) {
	clock := &fakeClock{t: time.Unix(0, 0)}
	attempts := 0
	dial := func(context.Context, string, string) (net.Conn, error) {
		attempts++
		return nil, errors.New("connection refused")
	}
	_, err := waitSSH(context.Background(), dial, "10.0.0.2:22", 10*time.Second, 2*time.Second, clock.Now, clock.Sleep)
	if err == nil {
		t.Fatal("waitSSH succeeded, want timeout")
	}
	// 10 s budget at one attempt per 2 s: 5 attempts, then give up
	// without sleeping past the deadline.
	if attempts != 5 {
		t.Errorf("attempts = %d, want 5", attempts)
	}
	if clock.Now().Sub(time.Unix(0, 0)) > 10*time.Second {
		t.Errorf("slept past the timeout")
	}
}

func TestRegisterFlow(t *testing.T) {
	s, _, _, _ := testServer()
	reg := &fakeRegistrar{user: types.User{ID: 9, Name: "carol", Subnet: "10.100.7.0/24"}}
	s.Registrar = reg
	sess := newFakeSession("", "carol\ncarol@example.com\n")
	s.register(context.Background(), Registration{
		PublicKey:   "ssh-ed25519 AAAA carol@laptop",
		Fingerprint: "SHA256:abcdef",
	}, sess)

	if !sess.exited || sess.exitCode != 0 {
		t.Fatalf("exit %v %d, stderr %q", sess.exited, sess.exitCode, sess.errOut.String())
	}
	if reg.got.Name != "carol" || reg.got.Email != "carol@example.com" {
		t.Errorf("registered %+v", reg.got)
	}
	if reg.got.PublicKey != "ssh-ed25519 AAAA carol@laptop" || reg.got.Fingerprint != "SHA256:abcdef" {
		t.Errorf("key not preserved: %+v", reg.got)
	}
	out := sess.out.String()
	if !strings.Contains(out, "registered carol") || !strings.Contains(out, "10.100.7.0/24") {
		t.Errorf("stdout %q", out)
	}
}

func TestRegisterRetriesInvalidInput(t *testing.T) {
	s, _, _, _ := testServer()
	reg := &fakeRegistrar{user: types.User{Name: "carol", Subnet: "10.100.7.0/24"}}
	s.Registrar = reg
	sess := newFakeSession("", "Carol Smith\ncarol\nnot-an-email\ncarol@example.com\n")
	s.register(context.Background(), Registration{}, sess)
	if sess.exitCode != 0 {
		t.Fatalf("exit %d, stderr %q", sess.exitCode, sess.errOut.String())
	}
	if reg.got.Name != "carol" || reg.got.Email != "carol@example.com" {
		t.Errorf("registered %+v after retries", reg.got)
	}
	out := sess.out.String()
	if !strings.Contains(out, "lowercase") || !strings.Contains(out, "email address") {
		t.Errorf("stdout %q misses the validation messages", out)
	}
}

func TestRegisterFailure(t *testing.T) {
	s, _, _, _ := testServer()
	s.Registrar = &fakeRegistrar{err: errors.New("subnets exhausted")}
	sess := newFakeSession("", "carol\ncarol@example.com\n")
	s.register(context.Background(), Registration{}, sess)
	if sess.exitCode != 1 || !strings.Contains(sess.errOut.String(), "registration failed") {
		t.Errorf("exit %d, stderr %q", sess.exitCode, sess.errOut.String())
	}
}

func TestReadLine(t *testing.T) {
	tests := []struct {
		name  string
		input string
		echo  bool
		want  string
		wantE string // echoed output
	}{
		{name: "newline", input: "abc\n", want: "abc"},
		{name: "carriage return", input: "abc\r", want: "abc"},
		{name: "backspace", input: "abd\x7fc\r", echo: true, want: "abc", wantE: "abd\b \bc\r\n"},
		{name: "eof ends line", input: "abc", want: "abc"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			sess := newFakeSession("", tt.input)
			got, err := readLine(sess, tt.echo)
			if err != nil || got != tt.want {
				t.Errorf("readLine = %q, %v; want %q", got, err, tt.want)
			}
			if tt.echo && sess.out.String() != tt.wantE {
				t.Errorf("echo %q, want %q", sess.out.String(), tt.wantE)
			}
		})
	}
	t.Run("ctrl-c cancels", func(t *testing.T) {
		sess := newFakeSession("", "ab\x03")
		if _, err := readLine(sess, false); err == nil {
			t.Error("readLine accepted ^C")
		}
	})
}
