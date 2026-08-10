package lifecycle

import (
	"context"
	"fmt"
	"os/exec"
)

// Runner executes a host command such as qemu-img. Tests inject a fake so
// nothing runs on the development machine.
type Runner interface {
	Run(ctx context.Context, name string, args ...string) ([]byte, error)
}

// execRunner runs the real command.
type execRunner struct{}

func (execRunner) Run(ctx context.Context, name string, args ...string) ([]byte, error) {
	return exec.CommandContext(ctx, name, args...).CombinedOutput()
}

// QemuImgResizer grows a qcow2 overlay with `qemu-img resize`
// (SPEC 11.1: a resize that grows the disk edits the overlay).
type QemuImgResizer struct {
	// Runner overrides the command runner. Nil runs the real qemu-img.
	Runner Runner
	// QemuImg overrides the binary path. Empty means "qemu-img" from PATH.
	QemuImg string
}

// ResizeOverlay implements OverlayResizer.
func (r QemuImgResizer) ResizeOverlay(ctx context.Context, overlayPath string, diskGiB int64) error {
	if diskGiB <= 0 {
		return fmt.Errorf("lifecycle: resize overlay %s: disk size %d GiB is not positive", overlayPath, diskGiB)
	}
	runner := r.Runner
	if runner == nil {
		runner = execRunner{}
	}
	bin := r.QemuImg
	if bin == "" {
		bin = "qemu-img"
	}
	out, err := runner.Run(ctx, bin, "resize", overlayPath, fmt.Sprintf("%dG", diskGiB))
	if err != nil {
		return fmt.Errorf("lifecycle: qemu-img resize %s to %d GiB: %w: %s", overlayPath, diskGiB, err, out)
	}
	return nil
}
