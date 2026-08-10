package cli

// The lifecycle commands of SPEC 15: new, rm, start, stop, restart,
// rename, cp, resize, console.

import (
	"flag"
	"fmt"

	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/types"
)

func (c *CLI) newCmd(e *env) int {
	fs := flag.NewFlagSet("new", flag.ContinueOnError)
	fs.SetOutput(e.errW)
	image := fs.String("image", c.opts.DefaultImage, "image name from the operator allowlist")
	memory := fs.String("memory", "", "memory size (MiB, or a G/GiB suffix)")
	cpu := fs.Int("cpu", c.opts.DefaultVCPU, "vCPU count")
	disk := fs.String("disk", "", "disk size (GiB)")
	nested := fs.Bool("nested", false, "enable nested virtualization")
	noKSM := fs.Bool("no-ksm", false, "opt out of kernel same-page merging")
	if fs.Parse(e.args) != nil {
		return 2
	}
	if fs.NArg() != 1 {
		return e.usage("new <name> [--image --memory --cpu --disk --nested --no-ksm]")
	}
	name := fs.Arg(0)
	if err := validateName(name); err != nil {
		return e.fail(err)
	}
	if *image == "" {
		return e.usage("new: --image is required (no default image is configured)")
	}
	req := CreateRequest{
		OwnerID:   e.user.ID,
		Name:      name,
		Image:     *image,
		VCPU:      *cpu,
		MemoryMiB: c.opts.DefaultMemoryMiB,
		DiskGiB:   c.opts.DefaultDiskGiB,
		Nested:    *nested,
		KSM:       !*noKSM,
	}
	var err error
	if *memory != "" {
		if req.MemoryMiB, err = parseMemoryMiB(*memory); err != nil {
			return e.fail(err)
		}
	}
	if *disk != "" {
		if req.DiskGiB, err = parseDiskGiB(*disk); err != nil {
			return e.fail(err)
		}
	}
	if req.VCPU < 1 {
		return e.fail(fmt.Errorf("--cpu must be at least 1"))
	}
	inst, err := c.lc.Create(e.ctx, req)
	if err != nil {
		return e.fail(err)
	}
	fmt.Fprintf(e.out, "created %s: image %s, %d vCPU, %d MiB memory, %d GiB disk\n",
		inst.Name, inst.ImageName, inst.VCPU, inst.MemoryMiB, inst.DiskGiB)
	fmt.Fprintf(e.out, "address %s\n", inst.Address)
	if c.opts.Domain != "" {
		fmt.Fprintf(e.out, "connect with: ssh %s@%s\n", inst.Name, c.opts.Domain)
	}
	return 0
}

func (c *CLI) rm(e *env) int {
	fs := flag.NewFlagSet("rm", flag.ContinueOnError)
	fs.SetOutput(e.errW)
	force := fs.Bool("force", false, "delete without confirmation")
	if fs.Parse(e.args) != nil {
		return 2
	}
	if fs.NArg() != 1 {
		return e.usage("rm <name> [--force]")
	}
	inst, ok := c.resolveOwned(e, fs.Arg(0))
	if !ok {
		return 1
	}
	// SPEC 11.1: rm asks for confirmation; --force is for scripts. The
	// prompt names the instance (SPEC 14.4).
	if !*force && !confirm(e.in, e.out, fmt.Sprintf("delete instance %q? this destroys its disk. [y/N] ", inst.Name)) {
		fmt.Fprintln(e.errW, "rm: aborted")
		return 1
	}
	if err := c.lc.Remove(e.ctx, inst); err != nil {
		return e.fail(err)
	}
	fmt.Fprintf(e.out, "rm: deleted %s\n", inst.Name)
	return 0
}

func (c *CLI) start(e *env) int {
	if len(e.args) != 1 {
		return e.usage("start <name>")
	}
	inst, ok := c.resolve(e, e.args[0])
	if !ok {
		return 1
	}
	if inst.State == types.StateRunning {
		fmt.Fprintf(e.out, "start: %s is already running\n", inst.Name)
		return 0
	}
	if err := c.lc.Start(e.ctx, inst); err != nil {
		return e.fail(err)
	}
	fmt.Fprintf(e.out, "start: %s is starting\n", inst.Name)
	return 0
}

func (c *CLI) stop(e *env) int {
	if len(e.args) != 1 {
		return e.usage("stop <name>")
	}
	inst, ok := c.resolve(e, e.args[0])
	if !ok {
		return 1
	}
	result, err := c.lc.Stop(e.ctx, inst)
	if err != nil {
		return e.fail(err)
	}
	// SPEC 11.1: report which path the stop took.
	switch result {
	case hypervisor.StopGraceful:
		fmt.Fprintf(e.out, "stop: %s shut down after the ACPI request\n", inst.Name)
	case hypervisor.StopForced:
		fmt.Fprintf(e.out, "stop: %s ignored the ACPI request for 60s and was forced off\n", inst.Name)
	case hypervisor.StopNoop:
		fmt.Fprintf(e.out, "stop: %s was already stopped\n", inst.Name)
	default:
		fmt.Fprintf(e.out, "stop: %s stopped (%s)\n", inst.Name, result)
	}
	return 0
}

func (c *CLI) restart(e *env) int {
	if len(e.args) != 1 {
		return e.usage("restart <name>")
	}
	inst, ok := c.resolve(e, e.args[0])
	if !ok {
		return 1
	}
	if err := c.lc.Restart(e.ctx, inst); err != nil {
		return e.fail(err)
	}
	fmt.Fprintf(e.out, "restart: %s is restarting\n", inst.Name)
	return 0
}

func (c *CLI) rename(e *env) int {
	if len(e.args) != 2 {
		return e.usage("rename <old> <new>")
	}
	oldName, newName := e.args[0], e.args[1]
	inst, ok := c.resolveOwned(e, oldName)
	if !ok {
		return 1
	}
	if err := validateName(newName); err != nil {
		return e.fail(err)
	}
	// SPEC 7.3: confirm when the visibility is public, stating two
	// facts: the old URL stops working (no redirect), and the SSH user
	// name changes.
	if inst.Visibility == types.VisibilityPublic {
		prompt := fmt.Sprintf("rename: %q is public. Two things change:\n", oldName) +
			fmt.Sprintf("  1. every existing link to %s stops working; there is no redirect\n", c.instanceURL(oldName)) +
			fmt.Sprintf("  2. the SSH user name changes: ssh %s@%s becomes ssh %s@%s\n", oldName, c.sshHost(), newName, c.sshHost()) +
			fmt.Sprintf("rename %q to %q? [y/N] ", oldName, newName)
		if !confirm(e.in, e.out, prompt) {
			fmt.Fprintln(e.errW, "rename: aborted")
			return 1
		}
	}
	if err := c.lc.Rename(e.ctx, inst, newName); err != nil {
		return e.fail(err)
	}
	fmt.Fprintf(e.out, "rename: %s is now %s; the name %q enters a %s cooldown\n",
		oldName, newName, oldName, formatCooldown(c.opts.NameCooldown))
	return 0
}

func (c *CLI) cp(e *env) int {
	if len(e.args) != 2 {
		return e.usage("cp <source> <target>")
	}
	src, ok := c.resolve(e, e.args[0])
	if !ok {
		return 1
	}
	// SPEC 15: cp copies a stopped instance.
	if src.State != types.StateStopped {
		return e.fail(fmt.Errorf("cp: the source %q must be stopped, its state is %s", src.Name, src.State))
	}
	target := e.args[1]
	if err := validateName(target); err != nil {
		return e.fail(err)
	}
	req := CreateRequest{
		OwnerID:   e.user.ID,
		Name:      target,
		Image:     src.ImageName,
		VCPU:      src.VCPU,
		MemoryMiB: src.MemoryMiB,
		DiskGiB:   src.DiskGiB,
		Nested:    src.Nested,
		KSM:       src.KSM,
	}
	inst, err := c.lc.Copy(e.ctx, src, req)
	if err != nil {
		return e.fail(err)
	}
	fmt.Fprintf(e.out, "cp: created %s from %s, address %s\n", inst.Name, src.Name, inst.Address)
	return 0
}

func (c *CLI) resize(e *env) int {
	fs := flag.NewFlagSet("resize", flag.ContinueOnError)
	fs.SetOutput(e.errW)
	memory := fs.String("memory", "", "new memory size (MiB, or a G/GiB suffix)")
	cpu := fs.Int("cpu", 0, "new vCPU count")
	disk := fs.String("disk", "", "new disk size (GiB); the disk can only grow")
	nested := fs.Bool("nested", false, "enable nested virtualization")
	noNested := fs.Bool("no-nested", false, "disable nested virtualization")
	if fs.Parse(e.args) != nil {
		return 2
	}
	if fs.NArg() != 1 {
		return e.usage("resize <name> [--memory --cpu --disk --nested|--no-nested]")
	}
	if *nested && *noNested {
		return e.usage("resize: --nested and --no-nested exclude each other")
	}
	inst, ok := c.resolveOwned(e, fs.Arg(0))
	if !ok {
		return 1
	}
	var req ResizeRequest
	if *memory != "" {
		m, err := parseMemoryMiB(*memory)
		if err != nil {
			return e.fail(err)
		}
		req.MemoryMiB = &m
	}
	if *cpu != 0 {
		if *cpu < 1 {
			return e.fail(fmt.Errorf("--cpu must be at least 1"))
		}
		req.VCPU = cpu
	}
	if *disk != "" {
		d, err := parseDiskGiB(*disk)
		if err != nil {
			return e.fail(err)
		}
		if d < inst.DiskGiB {
			return e.fail(fmt.Errorf("resize: the disk of %q is %d GiB and can only grow", inst.Name, inst.DiskGiB))
		}
		req.DiskGiB = &d
	}
	if *nested || *noNested {
		v := *nested
		req.Nested = &v
	}
	if req.MemoryMiB == nil && req.VCPU == nil && req.DiskGiB == nil && req.Nested == nil {
		return e.usage("resize: name at least one of --memory, --cpu, --disk, --nested, --no-nested")
	}
	// SPEC 11.1: tell the user before the change that a restart is
	// needed.
	fmt.Fprintf(e.out, "resize: the change takes effect after a restart of %s\n", inst.Name)
	if err := c.lc.Resize(e.ctx, inst, req); err != nil {
		return e.fail(err)
	}
	fmt.Fprintf(e.out, "resize: %s updated; run: restart %s\n", inst.Name, inst.Name)
	return 0
}

func (c *CLI) console(e *env) int {
	if len(e.args) != 1 {
		return e.usage("console <name>")
	}
	inst, ok := c.resolve(e, e.args[0])
	if !ok {
		return 1
	}
	fmt.Fprintf(e.out, "console: attached to %s\n", inst.Name)
	if err := c.lc.Console(e.ctx, inst, stdio{e.in, e.out}); err != nil {
		return e.fail(err)
	}
	return 0
}

func (c *CLI) sshHost() string {
	if c.opts.Domain == "" {
		return "bento"
	}
	return c.opts.Domain
}

func (c *CLI) instanceURL(name string) string {
	if c.opts.Domain == "" {
		return "the URL of " + name
	}
	return fmt.Sprintf("https://%s.%s/", name, c.opts.Domain)
}
