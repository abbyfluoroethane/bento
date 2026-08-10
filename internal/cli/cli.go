// Package cli implements the Bento command line interface served over the
// SSH frontend (SPEC section 15). The form is:
//
//	ssh bento.example.org <command> [arguments]
//
// Every command parses its own flags with the standard flag package,
// writes deterministic tabular output, and returns a shell exit code.
package cli

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"strings"
	"time"

	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// Fallback defaults for `new`, used when Options leaves them zero.
const (
	fallbackVCPU      = 2
	fallbackMemoryMiB = 2048
	fallbackDiskGiB   = 20
)

// Options configures the CLI.
type Options struct {
	// Domain is the base domain, e.g. "bento.foid.space". It appears in
	// help text, rename confirmations, and visibility messages.
	Domain string
	// DefaultImage is the image `new` uses when --image is absent.
	// Empty makes --image mandatory.
	DefaultImage string
	// Defaults for `new` when the flag is absent.
	DefaultVCPU      int
	DefaultMemoryMiB int64
	DefaultDiskGiB   int64
	// NameCooldown is the operator cooldown setting (SPEC 7.2), used in
	// messages only; the store enforces it.
	NameCooldown time.Duration
	// Now is the time source for "last seen" formatting. Defaults to
	// time.Now.
	Now func() time.Time
}

func (o Options) withDefaults() Options {
	if o.DefaultVCPU == 0 {
		o.DefaultVCPU = fallbackVCPU
	}
	if o.DefaultMemoryMiB == 0 {
		o.DefaultMemoryMiB = fallbackMemoryMiB
	}
	if o.DefaultDiskGiB == 0 {
		o.DefaultDiskGiB = fallbackDiskGiB
	}
	if o.NameCooldown == 0 {
		o.NameCooldown = 24 * time.Hour
	}
	if o.Now == nil {
		o.Now = time.Now
	}
	return o
}

// CLI executes commands for one authenticated user at a time.
type CLI struct {
	store Store
	lc    Lifecycle
	opts  Options
}

// New builds a CLI over the data layer and the lifecycle actions.
func New(st Store, lc Lifecycle, opts Options) *CLI {
	return &CLI{store: st, lc: lc, opts: opts.withDefaults()}
}

// env is the per-invocation context of one command.
type env struct {
	ctx  context.Context
	user types.User
	args []string
	in   io.Reader
	out  io.Writer
	errW io.Writer
}

// stdio joins the command's input and output for interactive commands
// such as console.
type stdio struct {
	io.Reader
	io.Writer
}

// Run executes one command line for user and returns the exit code:
// 0 on success, 1 on failure, 2 on a usage error.
func (c *CLI) Run(ctx context.Context, user types.User, args []string, stdin io.Reader, stdout, stderr io.Writer) int {
	if len(args) == 0 || args[0] == "help" {
		c.help(stdout)
		return 0
	}
	e := &env{ctx: ctx, user: user, args: args[1:], in: stdin, out: stdout, errW: stderr}
	switch args[0] {
	case "ls":
		return c.ls(e)
	case "new":
		return c.newCmd(e)
	case "rm":
		return c.rm(e)
	case "start":
		return c.start(e)
	case "stop":
		return c.stop(e)
	case "restart":
		return c.restart(e)
	case "rename":
		return c.rename(e)
	case "cp":
		return c.cp(e)
	case "resize":
		return c.resize(e)
	case "console":
		return c.console(e)
	case "port":
		return c.port(e)
	case "visibility":
		return c.visibility(e)
	case "share":
		return c.share(e)
	case "images":
		return c.images(e)
	case "ssh-key":
		return c.sshKey(e)
	case "whoami":
		return c.whoami(e)
	default:
		fmt.Fprintf(stderr, "bento: unknown command %q; run \"help\" for the command list\n", args[0])
		return 2
	}
}

func (c *CLI) help(w io.Writer) {
	host := c.opts.Domain
	if host == "" {
		host = "bento"
	}
	fmt.Fprintf(w, `bento — usage: ssh %s <command> [arguments]

  ls                                 list your instances
  new <name> [--image --memory --cpu --disk --nested --no-ksm]
                                     create an instance
  rm <name> [--force]                delete an instance
  start <name>                       start a stopped instance
  stop <name>                        stop a running instance
  restart <name>                     restart an instance
  rename <old> <new>                 rename an instance
  cp <source> <target>               copy a stopped instance
  resize <name> [--memory --cpu --disk --nested|--no-nested]
                                     change instance resources
  console <name>                     attach to the serial console
  port <name> <port>                 set the default HTTP port
  visibility <name> <off|private|public>
                                     set who can reach the instance URL
  share [--revoke] <name> [<user>]   grant, revoke, or list access
  images                             list images and versions in use
  ssh-key [add|list|remove]          manage your SSH keys
  whoami                             show your account and quota
`, host)
}

// usage prints a usage line and returns the usage exit code.
func (e *env) usage(format string, args ...any) int {
	fmt.Fprintf(e.errW, "usage: "+format+"\n", args...)
	return 2
}

// fail maps store and lifecycle errors to the messages SPEC 15 requires
// and returns exit code 1.
func (e *env) fail(err error) int {
	var cooldown *store.NameCooldownError
	var quota *store.QuotaError
	switch {
	case errors.As(err, &cooldown):
		// SPEC 15/19: a new that names a released name held by another
		// user must report the remaining cooldown.
		fmt.Fprintf(e.errW, "bento: the name %q was released by another user and is in cooldown; try again in %s\n",
			cooldown.Name, formatCooldown(cooldown.Remaining))
	case errors.As(err, &quota):
		fmt.Fprintf(e.errW, "bento: quota exceeded: the %s limit is %d, %d in use, %d requested\n",
			quota.Limit, quota.Max, quota.Used, quota.Requested)
	case errors.Is(err, store.ErrNameTaken):
		fmt.Fprintln(e.errW, "bento: that name is taken by an existing instance")
	default:
		fmt.Fprintf(e.errW, "bento: %v\n", err)
	}
	return 1
}

// accessDenied is one message for both "no such instance" and "no
// access", so a user cannot probe which names exist (compare SPEC 9.2).
func (e *env) accessDenied(name string) int {
	fmt.Fprintf(e.errW, "bento: no such instance or no access: %s\n", name)
	return 1
}

// resolve returns the instance when the user owns it or holds a share on
// it (SPEC 10 step 6 applies the same rule).
func (c *CLI) resolve(e *env, name string) (types.Instance, bool) {
	inst, err := c.store.InstanceByName(name)
	if err != nil {
		e.accessDenied(name)
		return types.Instance{}, false
	}
	if inst.OwnerID == e.user.ID {
		return inst, true
	}
	ok, err := c.store.HasAccess(inst.UUID, e.user.ID)
	if err != nil {
		e.fail(err)
		return types.Instance{}, false
	}
	if !ok {
		e.accessDenied(name)
		return types.Instance{}, false
	}
	return inst, true
}

// resolveOwned returns the instance only when the user owns it. A user
// with a share gets a distinct message: the share proves the instance
// exists, so there is nothing to hide.
func (c *CLI) resolveOwned(e *env, name string) (types.Instance, bool) {
	inst, ok := c.resolve(e, name)
	if !ok {
		return types.Instance{}, false
	}
	if inst.OwnerID != e.user.ID {
		fmt.Fprintf(e.errW, "bento: only the owner of %s may run this command\n", name)
		return types.Instance{}, false
	}
	return inst, true
}

// confirm prints the prompt and reads one line. Only "y" or "yes"
// (case-insensitive) confirms.
func confirm(in io.Reader, out io.Writer, prompt string) bool {
	fmt.Fprint(out, prompt)
	line, err := bufio.NewReader(in).ReadString('\n')
	if err != nil && line == "" {
		return false
	}
	switch strings.ToLower(strings.TrimSpace(line)) {
	case "y", "yes":
		return true
	}
	return false
}

// quotaLine renders usage against limits, e.g.
// "instances 2/4 · vcpu 3/8 · memory 3072/8192 MiB · disk 30/100 GiB".
// A user with no quotas row is unlimited; limits render as "-".
func (c *CLI) quotaLine(userID int64) (string, error) {
	usage, err := c.store.UsageFor(userID)
	if err != nil {
		return "", err
	}
	quota, err := c.store.QuotaFor(userID)
	unlimited := false
	if errors.Is(err, store.ErrNotFound) {
		unlimited = true
	} else if err != nil {
		return "", err
	}
	lim := func(v int64) string {
		if unlimited {
			return "-"
		}
		return fmt.Sprintf("%d", v)
	}
	return fmt.Sprintf("instances %d/%s · vcpu %d/%s · memory %d/%s MiB · disk %d/%s GiB",
		usage.Instances, lim(int64(quota.MaxInstances)),
		usage.VCPU, lim(int64(quota.MaxVCPU)),
		usage.MemoryMiB, lim(quota.MaxMemoryMiB),
		usage.DiskGiB, lim(quota.MaxDiskGiB)), nil
}
