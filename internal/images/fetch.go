package images

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net/http"
	"os"
	"time"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// FetchImages runs the fetch-images pipeline from SPEC section 5.1 for
// every image on the allowlist, then garbage-collects unreferenced
// versions. The store lock guards only image version creation and
// deletion (SPEC section 19) — never a download: a multi-gigabyte
// download must not stall a concurrent overlay create that blocks on
// the same lock.
//
// Per image: download, checksum, reject on pin mismatch, return without
// action when the version already exists, store at the content-addressed
// path, insert the image_versions row, mark it current. An unpinned image
// whose content changed is stored under its new checksum and a warning
// names both checksums (trust on first use).
func (s *Store) FetchImages(ctx context.Context) error {
	// The downloads stream into the image directory before any lock is
	// held, so make sure it exists first.
	if err := os.MkdirAll(s.dir, 0o755); err != nil {
		return fmt.Errorf("images: create image directory: %w", err)
	}
	imgs, err := s.db.Images(ctx)
	if err != nil {
		return fmt.Errorf("images: list allowlist: %w", err)
	}
	var errs []error
	for _, img := range imgs {
		if err := s.fetchOne(ctx, img); err != nil {
			errs = append(errs, fmt.Errorf("images: fetch %s: %w", img.Name, err))
		}
	}
	if err := s.collect(ctx); err != nil {
		errs = append(errs, err)
	}
	return errors.Join(errs...)
}

// fetchOne runs steps 1-7 of SPEC section 5.1 for one image. The
// download and checksum run without the store lock; the lock is taken
// only for the version-creation steps 4-7 (SPEC section 19).
func (s *Store) fetchOne(ctx context.Context, img types.Image) error {
	// 1. Download the file from the URL, streaming into a temporary file
	// in the image directory so the final rename is atomic.
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, img.URL, nil)
	if err != nil {
		return fmt.Errorf("build request: %w", err)
	}
	resp, err := s.client.Do(req)
	if err != nil {
		return fmt.Errorf("download %s: %w", img.URL, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("download %s: unexpected status %d", img.URL, resp.StatusCode)
	}

	tmp, err := os.CreateTemp(s.dir, "download-*")
	if err != nil {
		return fmt.Errorf("create temporary file: %w", err)
	}
	tmpName := tmp.Name()
	keep := false
	defer func() {
		if !keep {
			os.Remove(tmpName)
		}
	}()

	// 2. Compute the checksum while writing.
	hash := sha256.New()
	size, err := io.Copy(io.MultiWriter(tmp, hash), resp.Body)
	if err != nil {
		tmp.Close()
		return fmt.Errorf("download %s: %w", img.URL, err)
	}
	if err := tmp.Close(); err != nil {
		return fmt.Errorf("write temporary file: %w", err)
	}
	checksum := hex.EncodeToString(hash.Sum(nil))

	// 3. Reject the file if the allowlist pins a checksum and the two do
	// not match.
	if img.PinnedChecksum != "" {
		pinned, err := normalizeChecksum(img.PinnedChecksum)
		if err != nil {
			return fmt.Errorf("pinned checksum: %w", err)
		}
		if pinned != checksum {
			return fmt.Errorf("checksum mismatch: pinned %s, downloaded %s", pinned, checksum)
		}
	}

	// Steps 4-7 create an image version; only they need the store lock
	// (SPEC section 19).
	unlock, err := s.lock()
	if err != nil {
		return err
	}
	defer unlock()

	// 4. Return without action if a version with that checksum already
	// exists.
	exists, err := s.db.HasImageVersion(ctx, checksum)
	if err != nil {
		return fmt.Errorf("check existing version: %w", err)
	}
	if exists {
		if _, err := os.Stat(s.path(checksum)); errors.Is(err, fs.ErrNotExist) {
			s.log.Warn("image version row exists but the stored file is missing",
				"image", img.Name,
				"checksum", checksum,
				"path", s.path(checksum))
		}
		return nil
	}

	// 5. Store the file at the content-addressed path. The stored file is
	// never written again, so drop the write bits.
	if err := os.Chmod(tmpName, 0o444); err != nil {
		return fmt.Errorf("chmod: %w", err)
	}
	path := s.path(checksum)
	if err := os.Rename(tmpName, path); err != nil {
		return fmt.Errorf("store at %s: %w", path, err)
	}
	keep = true

	// 6. Insert a row in image_versions.
	if err := s.db.InsertImageVersion(ctx, types.ImageVersion{
		Checksum:  checksum,
		ImageName: img.Name,
		Path:      path,
		Size:      size,
		FetchedAt: time.Now().UTC(),
	}); err != nil {
		return fmt.Errorf("insert image version: %w", err)
	}

	// An unpinned image is trusted on first use. A content change is not
	// an error, but the warning must name both checksums (SPEC 5.1).
	if img.PinnedChecksum == "" && img.CurrentChecksum != "" && img.CurrentChecksum != checksum {
		s.log.Warn("unpinned image content changed",
			"image", img.Name,
			"previous_checksum", img.CurrentChecksum,
			"new_checksum", checksum)
	}

	// 7. Mark the new row as the current version of the image.
	if err := s.db.SetCurrentChecksum(ctx, img.Name, checksum); err != nil {
		return fmt.Errorf("mark current: %w", err)
	}
	return nil
}

// collect deletes image versions that no instance depends on. The
// condition from SPEC sections 5.1 and 12 is exact: a version is deletable
// only when no instances row carries its checksum in base_checksum. The
// current version of each image is always kept, because the next new
// instance boots from it. The whole collection holds the store lock, so
// a concurrent overlay create cannot lose its backing file mid-create
// (SPEC section 19).
func (s *Store) collect(ctx context.Context) error {
	unlock, err := s.lock()
	if err != nil {
		return err
	}
	defer unlock()

	imgs, err := s.db.Images(ctx)
	if err != nil {
		return fmt.Errorf("images: collect: list allowlist: %w", err)
	}
	current := make(map[string]bool, len(imgs))
	for _, img := range imgs {
		if img.CurrentChecksum != "" {
			current[img.CurrentChecksum] = true
		}
	}
	versions, err := s.db.ImageVersions(ctx)
	if err != nil {
		return fmt.Errorf("images: collect: list versions: %w", err)
	}
	var errs []error
	for _, v := range versions {
		if current[v.Checksum] {
			continue
		}
		inUse, err := s.db.ChecksumInUse(ctx, v.Checksum)
		if err != nil {
			errs = append(errs, fmt.Errorf("images: collect %s: %w", v.Checksum, err))
			continue
		}
		if inUse {
			continue
		}
		if err := s.db.DeleteImageVersion(ctx, v.Checksum); err != nil {
			errs = append(errs, fmt.Errorf("images: collect %s: delete row: %w", v.Checksum, err))
			continue
		}
		if err := os.Remove(s.path(v.Checksum)); err != nil && !errors.Is(err, fs.ErrNotExist) {
			errs = append(errs, fmt.Errorf("images: collect %s: delete file: %w", v.Checksum, err))
			continue
		}
		s.log.Info("deleted unreferenced image version",
			"image", v.ImageName,
			"checksum", v.Checksum)
	}
	return errors.Join(errs...)
}
