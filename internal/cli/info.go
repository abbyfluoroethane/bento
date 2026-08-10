package cli

// The read and settings commands of SPEC 15: ls, port, visibility,
// share, images, ssh-key, whoami.

import (
	"errors"
	"flag"
	"fmt"
	"io"
	"sort"
	"strconv"
	"strings"
	"text/tabwriter"

	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
	gossh "golang.org/x/crypto/ssh"
)

func (c *CLI) ls(e *env) int {
	if len(e.args) != 0 {
		return e.usage("ls")
	}
	// SPEC 15/6.1: ls shows the quota use.
	quota, err := c.quotaLine(e.user.ID)
	if err != nil {
		return e.fail(err)
	}
	fmt.Fprintln(e.out, quota)

	own, err := c.store.InstancesByOwner(e.user.ID)
	if err != nil {
		return e.fail(err)
	}
	sortInstances(own)
	now := c.opts.Now()
	if len(own) == 0 {
		fmt.Fprintln(e.out, "no instances; create one with: new <name>")
	} else {
		tw := newTable(e.out)
		fmt.Fprintln(tw, "NAME\tSTATE\tADDRESS\tIMAGE\tVISIBILITY\tLAST USE")
		for _, inst := range own {
			fmt.Fprintf(tw, "%s\t%s\t%s\t%s\t%s\t%s\n",
				inst.Name, inst.State, inst.Address, inst.ImageName, inst.Visibility, ago(now, inst.LastSeenAt))
		}
		tw.Flush()
	}

	shared, err := c.store.InstancesSharedWith(e.user.ID)
	if err != nil {
		return e.fail(err)
	}
	if len(shared) > 0 {
		sortInstances(shared)
		fmt.Fprintln(e.out, "\nshared with you:")
		tw := newTable(e.out)
		fmt.Fprintln(tw, "NAME\tSTATE\tADDRESS\tOWNER\tLAST USE")
		for _, inst := range shared {
			owner := "?"
			if u, err := c.store.UserByID(inst.OwnerID); err == nil {
				owner = u.Name
			}
			fmt.Fprintf(tw, "%s\t%s\t%s\t%s\t%s\n",
				inst.Name, inst.State, inst.Address, owner, ago(now, inst.LastSeenAt))
		}
		tw.Flush()
	}
	return 0
}

func (c *CLI) port(e *env) int {
	if len(e.args) != 2 {
		return e.usage("port <name> <port>")
	}
	inst, ok := c.resolveOwned(e, e.args[0])
	if !ok {
		return 1
	}
	port, err := strconv.Atoi(e.args[1])
	if err != nil || port < 1 || port > 65535 {
		return e.fail(fmt.Errorf("port: %q is not a port between 1 and 65535", e.args[1]))
	}
	// Through the lifecycle: a port change reloads the nftables table
	// (SPEC 6.3), so it must not wait for the convergence tick.
	if err := c.lc.SetHTTPPort(e.ctx, inst, port); err != nil {
		return e.fail(err)
	}
	fmt.Fprintf(e.out, "port: the default HTTP port of %s is now %d\n", inst.Name, port)
	return 0
}

func (c *CLI) visibility(e *env) int {
	if len(e.args) != 2 {
		return e.usage("visibility <name> <off|private|public>")
	}
	inst, ok := c.resolveOwned(e, e.args[0])
	if !ok {
		return 1
	}
	var v types.Visibility
	switch e.args[1] {
	case "off":
		v = types.VisibilityOff
	case "private":
		v = types.VisibilityPrivate
	case "public":
		v = types.VisibilityPublic
	default:
		return e.usage("visibility <name> <off|private|public>")
	}
	// Through the lifecycle: the published ports follow the visibility,
	// and SPEC 6.3 reloads the nftables table on every change.
	if err := c.lc.SetVisibility(e.ctx, inst, v); err != nil {
		return e.fail(err)
	}
	switch v {
	case types.VisibilityOff:
		fmt.Fprintf(e.out, "visibility: %s is now off; %s returns 404\n", inst.Name, c.instanceURL(inst.Name))
	case types.VisibilityPrivate:
		fmt.Fprintf(e.out, "visibility: %s is now private; %s requires a login\n", inst.Name, c.instanceURL(inst.Name))
	case types.VisibilityPublic:
		fmt.Fprintf(e.out, "visibility: %s is now public; anyone can reach %s\n", inst.Name, c.instanceURL(inst.Name))
	}
	return 0
}

func (c *CLI) share(e *env) int {
	fs := flag.NewFlagSet("share", flag.ContinueOnError)
	fs.SetOutput(e.errW)
	revoke := fs.Bool("revoke", false, "revoke instead of grant")
	if fs.Parse(e.args) != nil {
		return 2
	}
	if fs.NArg() < 1 || fs.NArg() > 2 {
		return e.usage("share [--revoke] <name> [<user>]")
	}
	inst, ok := c.resolveOwned(e, fs.Arg(0))
	if !ok {
		return 1
	}
	if fs.NArg() == 1 {
		if *revoke {
			return e.usage("share --revoke <name> <user>")
		}
		return c.listShares(e, inst)
	}
	target, err := c.store.UserByName(fs.Arg(1))
	if err != nil {
		if errors.Is(err, store.ErrNotFound) {
			return e.fail(fmt.Errorf("share: no such user: %s", fs.Arg(1)))
		}
		return e.fail(err)
	}
	if *revoke {
		if err := c.store.RemoveShare(inst.UUID, target.ID); err != nil {
			if errors.Is(err, store.ErrNotFound) {
				return e.fail(fmt.Errorf("share: %s has no share on %s", target.Name, inst.Name))
			}
			return e.fail(err)
		}
		fmt.Fprintf(e.out, "share: %s no longer has access to %s\n", target.Name, inst.Name)
		return 0
	}
	if target.ID == e.user.ID {
		return e.fail(fmt.Errorf("share: you own %s already", inst.Name))
	}
	if err := c.store.AddShare(inst.UUID, target.ID); err != nil {
		return e.fail(err)
	}
	fmt.Fprintf(e.out, "share: %s can now use %s\n", target.Name, inst.Name)
	return 0
}

func (c *CLI) listShares(e *env, inst types.Instance) int {
	shares, err := c.store.SharesFor(inst.UUID)
	if err != nil {
		return e.fail(err)
	}
	if len(shares) == 0 {
		fmt.Fprintf(e.out, "share: %s is shared with nobody\n", inst.Name)
		return 0
	}
	tw := newTable(e.out)
	fmt.Fprintln(tw, "USER\tSINCE")
	for _, sh := range shares {
		name := fmt.Sprintf("user-%d", sh.UserID)
		if u, err := c.store.UserByID(sh.UserID); err == nil {
			name = u.Name
		}
		fmt.Fprintf(tw, "%s\t%s\n", name, sh.CreatedAt.Format("2006-01-02"))
	}
	tw.Flush()
	return 0
}

func (c *CLI) images(e *env) int {
	if len(e.args) != 0 {
		return e.usage("images")
	}
	imgs, err := c.store.Images()
	if err != nil {
		return e.fail(err)
	}
	insts, err := c.store.Instances()
	if err != nil {
		return e.fail(err)
	}
	sort.Slice(imgs, func(i, j int) bool { return imgs[i].Name < imgs[j].Name })
	// SPEC 5.1: show each image, its current checksum, and how many
	// instances hold an older version.
	older := make(map[string]int)
	for _, inst := range insts {
		for _, img := range imgs {
			if inst.ImageName == img.Name && inst.BaseChecksum != img.CurrentChecksum {
				older[img.Name]++
			}
		}
	}
	tw := newTable(e.out)
	fmt.Fprintln(tw, "NAME\tCURRENT CHECKSUM\tON OLDER VERSIONS")
	for _, img := range imgs {
		checksum := img.CurrentChecksum
		if checksum == "" {
			checksum = "(never fetched)"
		}
		fmt.Fprintf(tw, "%s\t%s\t%d\n", img.Name, checksum, older[img.Name])
	}
	tw.Flush()
	return 0
}

func (c *CLI) sshKey(e *env) int {
	sub := "list"
	if len(e.args) > 0 {
		sub = e.args[0]
	}
	switch sub {
	case "list":
		return c.sshKeyList(e)
	case "add":
		return c.sshKeyAdd(e, e.args[1:])
	case "remove":
		if len(e.args) != 2 {
			return e.usage("ssh-key remove <id>")
		}
		id, err := strconv.ParseInt(e.args[1], 10, 64)
		if err != nil {
			return e.fail(fmt.Errorf("ssh-key: %q is not a key id; find the id with: ssh-key list", e.args[1]))
		}
		if err := c.store.DeleteSSHKey(e.user.ID, id); err != nil {
			if errors.Is(err, store.ErrNotFound) {
				return e.fail(fmt.Errorf("ssh-key: you have no key with id %d", id))
			}
			return e.fail(err)
		}
		fmt.Fprintf(e.out, "ssh-key: removed key %d\n", id)
		return 0
	default:
		return e.usage("ssh-key [add <public key>|list|remove <id>]")
	}
}

func (c *CLI) sshKeyList(e *env) int {
	keys, err := c.store.SSHKeysForUser(e.user.ID)
	if err != nil {
		return e.fail(err)
	}
	if len(keys) == 0 {
		fmt.Fprintln(e.out, "no keys; add one with: ssh-key add <public key>")
		return 0
	}
	sort.Slice(keys, func(i, j int) bool { return keys[i].ID < keys[j].ID })
	tw := newTable(e.out)
	fmt.Fprintln(tw, "ID\tFINGERPRINT\tCOMMENT\tADDED")
	for _, k := range keys {
		fmt.Fprintf(tw, "%d\t%s\t%s\t%s\n", k.ID, k.Fingerprint, k.Comment, k.CreatedAt.Format("2006-01-02"))
	}
	tw.Flush()
	return 0
}

func (c *CLI) sshKeyAdd(e *env, args []string) int {
	raw := strings.Join(args, " ")
	if strings.TrimSpace(raw) == "" {
		data, err := io.ReadAll(io.LimitReader(e.in, 64*1024))
		if err != nil {
			return e.fail(err)
		}
		raw = string(data)
	}
	pub, comment, _, _, err := gossh.ParseAuthorizedKey([]byte(strings.TrimSpace(raw)))
	if err != nil {
		return e.fail(fmt.Errorf("ssh-key: not a public key in authorized_keys format"))
	}
	line := strings.TrimSpace(string(gossh.MarshalAuthorizedKey(pub)))
	if comment != "" {
		line += " " + comment
	}
	fingerprint := gossh.FingerprintSHA256(pub)
	if _, err := c.store.AddSSHKey(e.user.ID, line, fingerprint, comment); err != nil {
		return e.fail(err)
	}
	fmt.Fprintf(e.out, "ssh-key: added %s\n", fingerprint)
	return 0
}

func (c *CLI) whoami(e *env) int {
	if len(e.args) != 0 {
		return e.usage("whoami")
	}
	quota, err := c.quotaLine(e.user.ID)
	if err != nil {
		return e.fail(err)
	}
	tw := newTable(e.out)
	fmt.Fprintf(tw, "name\t%s\n", e.user.Name)
	fmt.Fprintf(tw, "email\t%s\n", e.user.Email)
	fmt.Fprintf(tw, "subnet\t%s\n", e.user.Subnet)
	fmt.Fprintf(tw, "quota\t%s\n", quota)
	tw.Flush()
	return 0
}

func newTable(w io.Writer) *tabwriter.Writer {
	return tabwriter.NewWriter(w, 0, 0, 2, ' ', 0)
}

func sortInstances(insts []types.Instance) {
	sort.Slice(insts, func(i, j int) bool { return insts[i].Name < insts[j].Name })
}
