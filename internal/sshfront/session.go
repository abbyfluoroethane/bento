package sshfront

// Session handling: SPEC 10 steps 4-10 (instance forwarding), the CLI
// session, and the SPEC 13 registration flow.

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"regexp"
	"strings"
	"time"

	gliderssh "github.com/gliderlabs/ssh"
	gossh "golang.org/x/crypto/ssh"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// termSession is the slice of gliderssh.Session the handlers use. Tests
// implement it with an in-memory fake.
type termSession interface {
	io.ReadWriter
	User() string
	Stderr() io.ReadWriter
	Exit(code int) error
	Command() []string
	RawCommand() string
	Pty() (gliderssh.Pty, <-chan gliderssh.Window, bool)
}

var _ termSession = (gliderssh.Session)(nil)

func (s *Server) handleSession(sess gliderssh.Session) {
	s.dispatch(sess.Context(), sess)
}

// dispatch routes one session. A known user whose SSH user name is an
// instance they own or hold a share on is forwarded to that instance
// (SPEC 10 steps 4-6). Every other known-user session runs the command
// line interface: a stock ssh client always sends a user name — `ssh
// bento.foid.space ls` sends the local login name — so the SPEC 15
// interface must not require an empty one. A name the user cannot reach
// falls to the CLI too, which keeps one uniform behavior and reveals
// nothing about which names exist. An unknown key runs registration
// (SPEC 13).
func (s *Server) dispatch(ctx context.Context, sess termSession) {
	if user, ok := ctx.Value(userKey{}).(types.User); ok {
		if inst, ok := s.resolveInstance(sess.User(), user.ID); ok {
			s.proxy(ctx, inst, sess)
			return
		}
		code := s.CLI.Run(ctx, user, sess.Command(), sess, sess, sess.Stderr())
		sess.Exit(code)
		return
	}
	if reg, ok := ctx.Value(registrationKey{}).(Registration); ok {
		s.register(ctx, reg, sess)
		return
	}
	// Unreachable when authentication ran; be safe anyway.
	sess.Exit(1)
}

// resolveInstance implements SPEC 10 steps 4-6: the SSH user name is
// the instance name, resolved to a UUID, and the connecting user must
// own the instance or hold a share on that UUID.
func (s *Server) resolveInstance(name string, userID int64) (types.Instance, bool) {
	if name == "" {
		return types.Instance{}, false
	}
	inst, err := s.Instances.InstanceByName(name)
	if err != nil {
		return types.Instance{}, false
	}
	ok, err := s.Instances.HasAccess(inst.UUID, userID)
	if err != nil || !ok {
		return types.Instance{}, false
	}
	return inst, true
}

// proxy implements SPEC 10 steps 7-10 for one resolved connection.
func (s *Server) proxy(ctx context.Context, inst types.Instance, sess termSession) {
	// Step 7: start a stopped instance, without changing the desired
	// state (the Starter interface has no way to change it; SPEC 11.2).
	if inst.State == types.StateStopped {
		fmt.Fprintf(sess, "bento: starting %s\r\n", inst.Name)
		if err := s.Starter.StartInstance(ctx, inst); err != nil {
			fmt.Fprintf(sess.Stderr(), "bento: starting %s failed: %v\r\n", inst.Name, err)
			sess.Exit(1)
			return
		}
	}
	// Step 8: wait for sshd in the guest, 120 s default.
	addr := net.JoinHostPort(inst.Address, "22")
	conn, err := waitSSH(ctx, s.dial(), addr, s.startTimeout(), s.dialInterval(), s.now, s.sleep)
	if err != nil {
		fmt.Fprintf(sess.Stderr(), "bento: %s did not accept an SSH connection within %s: %v\r\n",
			inst.Name, s.startTimeout(), err)
		sess.Exit(1)
		return
	}
	// SPEC 12: last_seen_at records the last SSH connection.
	_ = s.Instances.TouchLastSeen(inst.UUID)
	// Steps 9 and 10: connect to the guest and join the two sessions.
	s.join(inst, conn, addr, sess)
}

// waitSSH dials addr until it accepts a connection or timeout passes.
// The successful connection is returned for reuse. Clock, sleep, and
// dialer are injectable so tests run without waiting.
func waitSSH(ctx context.Context, dial DialFunc, addr string, timeout, interval time.Duration,
	now func() time.Time, sleep func(time.Duration)) (net.Conn, error) {
	start := now()
	for {
		conn, err := dial(ctx, "tcp", addr)
		if err == nil {
			return conn, nil
		}
		if now().Sub(start)+interval >= timeout {
			return nil, err
		}
		sleep(interval)
		if ctx.Err() != nil {
			return nil, ctx.Err()
		}
	}
}

// join opens an SSH session inside the guest over conn and wires it to
// the client session: stdin, stdout, stderr, PTY and window changes,
// and the exit status (SPEC 10 steps 9-10).
func (s *Server) join(inst types.Instance, conn net.Conn, addr string, sess termSession) {
	cfg := &gossh.ClientConfig{
		User: s.guestUser(),
		Auth: s.GuestAuth,
		// The frontend reached the guest over the bento-managed bridge
		// at a bento-assigned address; the guest host key was generated
		// on first boot and is not recorded anywhere. Checking it would
		// add no authentication.
		HostKeyCallback: gossh.InsecureIgnoreHostKey(),
		Timeout:         10 * time.Second,
	}
	clientConn, chans, reqs, err := gossh.NewClientConn(conn, addr, cfg)
	if err != nil {
		conn.Close()
		fmt.Fprintf(sess.Stderr(), "bento: connecting to %s failed: %v\r\n", inst.Name, err)
		sess.Exit(1)
		return
	}
	client := gossh.NewClient(clientConn, chans, reqs)
	defer client.Close()
	guest, err := client.NewSession()
	if err != nil {
		fmt.Fprintf(sess.Stderr(), "bento: opening a session on %s failed: %v\r\n", inst.Name, err)
		sess.Exit(1)
		return
	}
	defer guest.Close()

	if ptyReq, winCh, isPty := sess.Pty(); isPty {
		modes := gossh.TerminalModes{gossh.ECHO: 1}
		if err := guest.RequestPty(ptyReq.Term, ptyReq.Window.Height, ptyReq.Window.Width, modes); err != nil {
			fmt.Fprintf(sess.Stderr(), "bento: pty request on %s failed: %v\r\n", inst.Name, err)
			sess.Exit(1)
			return
		}
		go func() {
			for w := range winCh {
				_ = guest.WindowChange(w.Height, w.Width)
			}
		}()
	}

	guest.Stdout = sess
	guest.Stderr = sess.Stderr()
	stdin, err := guest.StdinPipe()
	if err != nil {
		sess.Exit(1)
		return
	}
	go func() {
		_, _ = io.Copy(stdin, sess)
		_ = stdin.Close()
	}()

	if raw := sess.RawCommand(); raw != "" {
		err = guest.Start(raw)
	} else {
		err = guest.Shell()
	}
	if err != nil {
		fmt.Fprintf(sess.Stderr(), "bento: running the command on %s failed: %v\r\n", inst.Name, err)
		sess.Exit(1)
		return
	}
	err = guest.Wait()
	code := 0
	var exitErr *gossh.ExitError
	switch {
	case err == nil:
	case errors.As(err, &exitErr):
		code = exitErr.ExitStatus()
	default:
		code = 1
	}
	sess.Exit(code)
}

// accountNamePattern: account names follow the same label rules as
// instance names; they appear in `share` and in the dashboard.
var accountNamePattern = regexp.MustCompile(`^[a-z0-9]([a-z0-9-]{0,30}[a-z0-9])?$`)

const registrationAttempts = 5

// register runs the SPEC 13 flow for an unknown key: record the key,
// ask for a name and an email address; the Registrar allocates the
// subnet and the libvirt network.
func (s *Server) register(ctx context.Context, reg Registration, sess termSession) {
	_, _, echo := sess.Pty()
	fmt.Fprintf(sess, "bento: this key is not registered (%s)\r\n", reg.Fingerprint)
	fmt.Fprintf(sess, "bento: answer two questions to create an account\r\n")

	name, err := promptValid(sess, "account name: ", echo, func(v string) error {
		if !accountNamePattern.MatchString(v) {
			return fmt.Errorf("an account name uses lowercase letters, digits, and inner hyphens")
		}
		return nil
	})
	if err != nil {
		sess.Exit(1)
		return
	}
	email, err := promptValid(sess, "email: ", echo, func(v string) error {
		at := strings.Index(v, "@")
		if at < 1 || at == len(v)-1 {
			return fmt.Errorf("that does not look like an email address")
		}
		return nil
	})
	if err != nil {
		sess.Exit(1)
		return
	}

	reg.Name, reg.Email = name, email
	user, err := s.Registrar.Register(ctx, reg)
	if err != nil {
		fmt.Fprintf(sess.Stderr(), "bento: registration failed: %v\r\n", err)
		sess.Exit(1)
		return
	}
	fmt.Fprintf(sess, "bento: registered %s, subnet %s\r\n", user.Name, user.Subnet)
	fmt.Fprintf(sess, "bento: reconnect for the command line; run \"help\" for the command list\r\n")
	sess.Exit(0)
}

// promptValid asks until validate accepts the answer or the attempts
// run out.
func promptValid(rw io.ReadWriter, prompt string, echo bool, validate func(string) error) (string, error) {
	for range registrationAttempts {
		io.WriteString(rw, prompt)
		line, err := readLine(rw, echo)
		if err != nil {
			return "", err
		}
		line = strings.TrimSpace(line)
		if err := validate(line); err != nil {
			fmt.Fprintf(rw, "bento: %v\r\n", err)
			continue
		}
		return line, nil
	}
	return "", fmt.Errorf("too many attempts")
}

// readLine reads one line byte by byte, so it works both with a PTY
// (raw input, echo handled here) and with piped line input. It handles
// backspace and treats ^C and ^D as cancel.
func readLine(rw io.ReadWriter, echo bool) (string, error) {
	var buf []byte
	b := make([]byte, 1)
	for {
		n, err := rw.Read(b)
		if err != nil {
			if errors.Is(err, io.EOF) && len(buf) > 0 {
				return string(buf), nil
			}
			return "", err
		}
		if n == 0 {
			continue
		}
		switch c := b[0]; {
		case c == '\r' || c == '\n':
			if echo {
				io.WriteString(rw, "\r\n")
			}
			return string(buf), nil
		case c == 0x7f || c == 0x08: // backspace
			if len(buf) > 0 {
				buf = buf[:len(buf)-1]
				if echo {
					io.WriteString(rw, "\b \b")
				}
			}
		case c == 0x03 || c == 0x04: // ^C, ^D
			return "", fmt.Errorf("cancelled")
		default:
			buf = append(buf, c)
			if echo {
				_, _ = rw.Write(b)
			}
		}
	}
}
