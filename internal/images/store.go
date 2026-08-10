// Package images implements the content-addressed image store and the fetch-images pipeline (SPEC section 5.1).
package images

import (
	"context"
	"fmt"
	"log/slog"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"syscall"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// DefaultDir is the image directory from SPEC section 5.1. It must never
// move after the first instance exists: a qcow2 overlay records the
// absolute path of its backing file.
const DefaultDir = "/var/lib/bento/images"

// lockFileName is the flock file inside the image directory. It guards
// image version creation and deletion across processes (SPEC section 19).
const lockFileName = ".lock"

// Doer sends one HTTP request. *http.Client satisfies it; tests inject a
// fake so no network is touched.
type Doer interface {
	Do(req *http.Request) (*http.Response, error)
}

// Runner executes a host command such as qemu-img. Tests inject a fake so
// nothing runs on the development machine.
type Runner interface {
	Run(ctx context.Context, name string, args ...string) ([]byte, error)
}

type execRunner struct{}

func (execRunner) Run(ctx context.Context, name string, args ...string) ([]byte, error) {
	return exec.CommandContext(ctx, name, args...).CombinedOutput()
}

// DB is the consumer-side view of the queries the image store needs. The
// real store package implements it.
type DB interface {
	// Images returns the operator allowlist with current checksums.
	Images(ctx context.Context) ([]types.Image, error)
	// HasImageVersion reports whether a version with the checksum exists.
	HasImageVersion(ctx context.Context, checksum string) (bool, error)
	// InsertImageVersion inserts one image_versions row.
	InsertImageVersion(ctx context.Context, v types.ImageVersion) error
	// SetCurrentChecksum marks a version as the current one of an image.
	SetCurrentChecksum(ctx context.Context, imageName, checksum string) error
	// ImageVersions returns every image_versions row.
	ImageVersions(ctx context.Context) ([]types.ImageVersion, error)
	// DeleteImageVersion deletes one image_versions row.
	DeleteImageVersion(ctx context.Context, checksum string) error
	// ChecksumInUse reports whether any instances row carries the checksum
	// in base_checksum (SPEC sections 5.1 and 12).
	ChecksumInUse(ctx context.Context, checksum string) (bool, error)
}

// Store is the content-addressed image store. One version lives at
// Dir()/sha256-<hex>.qcow2, the path never changes, and the path never
// holds different content.
type Store struct {
	dir     string
	db      DB
	client  Doer
	run     Runner
	log     *slog.Logger
	qemuImg string

	// mu serializes version creation and deletion inside this process.
	// The flock on lockFileName does the same across processes. Together
	// they close the open item in SPEC section 19: a fetch-images
	// collection cannot delete a version while a create reads it.
	mu sync.Mutex
}

// Option configures a Store.
type Option func(*Store)

// WithHTTPClient sets the HTTP client used to download images.
func WithHTTPClient(d Doer) Option { return func(s *Store) { s.client = d } }

// WithRunner sets the command runner used for qemu-img.
func WithRunner(r Runner) Option { return func(s *Store) { s.run = r } }

// WithLogger sets the logger.
func WithLogger(l *slog.Logger) Option { return func(s *Store) { s.log = l } }

// WithQemuImg sets the qemu-img binary path.
func WithQemuImg(path string) Option { return func(s *Store) { s.qemuImg = path } }

// New returns a Store rooted at dir. An empty dir selects DefaultDir.
func New(dir string, db DB, opts ...Option) *Store {
	if dir == "" {
		dir = DefaultDir
	}
	s := &Store{
		dir:     dir,
		db:      db,
		client:  http.DefaultClient,
		run:     execRunner{},
		log:     slog.Default(),
		qemuImg: "qemu-img",
	}
	for _, o := range opts {
		o(s)
	}
	return s
}

// Dir returns the image directory.
func (s *Store) Dir() string { return s.dir }

// Path returns the content-addressed path for a checksum:
// <dir>/sha256-<hex>.qcow2. The checksum may carry a "sha256:" or
// "sha256-" prefix and any letter case.
func (s *Store) Path(checksum string) (string, error) {
	hex, err := normalizeChecksum(checksum)
	if err != nil {
		return "", err
	}
	return s.path(hex), nil
}

// path derives the store path for an already normalized checksum.
func (s *Store) path(hex string) string {
	return filepath.Join(s.dir, "sha256-"+hex+".qcow2")
}

var hexPattern = regexp.MustCompile(`^[0-9a-f]{64}$`)

// normalizeChecksum lowercases a sha256 checksum, strips an optional
// "sha256:" or "sha256-" prefix, and validates the hex form.
func normalizeChecksum(checksum string) (string, error) {
	hex := strings.ToLower(strings.TrimSpace(checksum))
	hex = strings.TrimPrefix(hex, "sha256:")
	hex = strings.TrimPrefix(hex, "sha256-")
	if !hexPattern.MatchString(hex) {
		return "", fmt.Errorf("images: invalid sha256 checksum %q", checksum)
	}
	return hex, nil
}

// lock takes the in-process mutex and then an exclusive flock on the lock
// file in the image directory. The returned function releases both.
func (s *Store) lock() (func(), error) {
	s.mu.Lock()
	if err := os.MkdirAll(s.dir, 0o755); err != nil {
		s.mu.Unlock()
		return nil, fmt.Errorf("images: create image directory: %w", err)
	}
	f, err := os.OpenFile(filepath.Join(s.dir, lockFileName), os.O_CREATE|os.O_RDWR, 0o644)
	if err != nil {
		s.mu.Unlock()
		return nil, fmt.Errorf("images: open lock file: %w", err)
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX); err != nil {
		f.Close()
		s.mu.Unlock()
		return nil, fmt.Errorf("images: flock: %w", err)
	}
	return func() {
		syscall.Flock(int(f.Fd()), syscall.LOCK_UN)
		f.Close()
		s.mu.Unlock()
	}, nil
}
