package cloudinit

import (
	"context"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
)

// Runner executes a host command such as xorriso. Tests inject a fake so
// nothing runs on the development machine.
type Runner interface {
	Run(ctx context.Context, name string, args ...string) ([]byte, error)
}

type execRunner struct{}

func (execRunner) Run(ctx context.Context, name string, args ...string) ([]byte, error) {
	return exec.CommandContext(ctx, name, args...).CombinedOutput()
}

// Builder builds NoCloud seed ISOs with xorriso.
type Builder struct {
	run     Runner
	xorriso string
}

// Option configures a Builder.
type Option func(*Builder)

// WithRunner sets the command runner used for xorriso.
func WithRunner(r Runner) Option { return func(b *Builder) { b.run = r } }

// WithXorriso sets the xorriso binary path.
func WithXorriso(path string) Option { return func(b *Builder) { b.xorriso = path } }

// NewBuilder returns a Builder that runs the real xorriso unless options
// say otherwise.
func NewBuilder(opts ...Option) *Builder {
	b := &Builder{run: execRunner{}, xorriso: "xorriso"}
	for _, o := range opts {
		o(b)
	}
	return b
}

// Build renders the seed files and writes the NoCloud ISO at isoPath. The
// volume label "cidata" is what makes cloud-init recognize the disk as a
// NoCloud seed. The ISO holds the public keys of the owner; the caller
// detaches it and calls Delete after the first successful boot (SPEC
// section 5.2).
func (b *Builder) Build(ctx context.Context, seed Seed, isoPath string) error {
	meta, err := seed.MetaData()
	if err != nil {
		return err
	}
	user, err := seed.UserData()
	if err != nil {
		return err
	}
	network, err := seed.NetworkConfig()
	if err != nil {
		return err
	}

	dir, err := os.MkdirTemp("", "bento-seed-")
	if err != nil {
		return fmt.Errorf("cloudinit: create staging directory: %w", err)
	}
	defer os.RemoveAll(dir)

	for name, content := range map[string]string{
		"meta-data":      meta,
		"user-data":      user,
		"network-config": network,
	} {
		if err := os.WriteFile(filepath.Join(dir, name), []byte(content), 0o600); err != nil {
			return fmt.Errorf("cloudinit: write %s: %w", name, err)
		}
	}

	out, err := b.run.Run(ctx, b.xorriso,
		"-as", "mkisofs",
		"-output", isoPath,
		"-volid", "cidata",
		"-joliet", "-rational-rock",
		dir)
	if err != nil {
		return fmt.Errorf("cloudinit: xorriso: %w: %s", err, out)
	}
	return nil
}

// Delete removes a seed ISO. An ISO that is already gone is not an error:
// the goal state is "the keys are not on disk", and it is reached.
func Delete(isoPath string) error {
	if err := os.Remove(isoPath); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("cloudinit: delete seed ISO: %w", err)
	}
	return nil
}
