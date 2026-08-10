package images

import (
	"context"
	"errors"
	"fmt"
	"io/fs"
	"os"
)

// CreateOverlay creates the root volume of a new instance as a
// copy-on-write qcow2 overlay backed by the stored image version with the
// given checksum, then resizes it to the requested disk size (SPEC section
// 5.2). It holds the store lock for the whole operation, so a concurrent
// fetch-images collection cannot delete the backing version mid-create
// (SPEC section 19).
//
// Recording the backing checksum in the instances row is the caller's
// job: the caller passes the checksum in, and BackingPath derives the
// path it was built on.
func (s *Store) CreateOverlay(ctx context.Context, checksum, overlayPath string, diskGiB int64) error {
	hex, err := normalizeChecksum(checksum)
	if err != nil {
		return err
	}
	if diskGiB <= 0 {
		return fmt.Errorf("images: overlay disk size must be positive, got %d GiB", diskGiB)
	}

	unlock, err := s.lock()
	if err != nil {
		return err
	}
	defer unlock()

	backing := s.path(hex)
	if _, err := os.Stat(backing); errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("images: image version %s is not in the store (expected %s)", hex, backing)
	} else if err != nil {
		return fmt.Errorf("images: stat backing file: %w", err)
	}

	// 1. Create a qcow2 file with the image version as its backing file.
	// The backing format is stated explicitly; qemu-img refuses to guess.
	out, err := s.run.Run(ctx, s.qemuImg,
		"create", "-f", "qcow2", "-F", "qcow2", "-b", backing, overlayPath)
	if err != nil {
		return fmt.Errorf("images: qemu-img create: %w: %s", err, out)
	}

	// 2. Resize the overlay to the requested disk size. The G suffix is
	// binary (GiB) in qemu-img.
	out, err = s.run.Run(ctx, s.qemuImg,
		"resize", overlayPath, fmt.Sprintf("%dG", diskGiB))
	if err != nil {
		os.Remove(overlayPath)
		return fmt.Errorf("images: qemu-img resize: %w: %s", err, out)
	}
	return nil
}

// BackingPath returns the content-addressed path an overlay with the given
// base checksum is backed by. It is the same derivation as Path.
func (s *Store) BackingPath(checksum string) (string, error) {
	return s.Path(checksum)
}
