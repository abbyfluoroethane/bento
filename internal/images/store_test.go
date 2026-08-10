package images

import (
	"context"
	"crypto/sha256"
	"fmt"
	"io"
	"net/http"
	"path/filepath"
	"strings"
	"sync"
	"testing"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// fakeClient serves canned HTTP responses by URL.
type fakeClient struct {
	mu        sync.Mutex
	responses map[string]fakeResponse
	requests  []string
}

type fakeResponse struct {
	status int
	body   string
	err    error
}

func (f *fakeClient) Do(req *http.Request) (*http.Response, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.requests = append(f.requests, req.URL.String())
	r, ok := f.responses[req.URL.String()]
	if !ok {
		return &http.Response{StatusCode: http.StatusNotFound, Body: io.NopCloser(strings.NewReader(""))}, nil
	}
	if r.err != nil {
		return nil, r.err
	}
	status := r.status
	if status == 0 {
		status = http.StatusOK
	}
	return &http.Response{StatusCode: status, Body: io.NopCloser(strings.NewReader(r.body))}, nil
}

// fakeDB is an in-memory DB implementation.
type fakeDB struct {
	mu       sync.Mutex
	images   []types.Image // CurrentChecksum mutated by SetCurrentChecksum
	versions map[string]types.ImageVersion
	inUse    map[string]bool

	inserted []types.ImageVersion
	deleted  []string
}

func newFakeDB() *fakeDB {
	return &fakeDB{versions: map[string]types.ImageVersion{}, inUse: map[string]bool{}}
}

func (d *fakeDB) Images(ctx context.Context) ([]types.Image, error) {
	d.mu.Lock()
	defer d.mu.Unlock()
	out := make([]types.Image, len(d.images))
	copy(out, d.images)
	return out, nil
}

func (d *fakeDB) HasImageVersion(ctx context.Context, checksum string) (bool, error) {
	d.mu.Lock()
	defer d.mu.Unlock()
	_, ok := d.versions[checksum]
	return ok, nil
}

func (d *fakeDB) InsertImageVersion(ctx context.Context, v types.ImageVersion) error {
	d.mu.Lock()
	defer d.mu.Unlock()
	if _, ok := d.versions[v.Checksum]; ok {
		return fmt.Errorf("duplicate version %s", v.Checksum)
	}
	d.versions[v.Checksum] = v
	d.inserted = append(d.inserted, v)
	return nil
}

func (d *fakeDB) SetCurrentChecksum(ctx context.Context, imageName, checksum string) error {
	d.mu.Lock()
	defer d.mu.Unlock()
	for i := range d.images {
		if d.images[i].Name == imageName {
			d.images[i].CurrentChecksum = checksum
			return nil
		}
	}
	return fmt.Errorf("no image %s", imageName)
}

func (d *fakeDB) ImageVersions(ctx context.Context) ([]types.ImageVersion, error) {
	d.mu.Lock()
	defer d.mu.Unlock()
	out := make([]types.ImageVersion, 0, len(d.versions))
	for _, v := range d.versions {
		out = append(out, v)
	}
	return out, nil
}

func (d *fakeDB) DeleteImageVersion(ctx context.Context, checksum string) error {
	d.mu.Lock()
	defer d.mu.Unlock()
	delete(d.versions, checksum)
	d.deleted = append(d.deleted, checksum)
	return nil
}

func (d *fakeDB) ChecksumInUse(ctx context.Context, checksum string) (bool, error) {
	d.mu.Lock()
	defer d.mu.Unlock()
	return d.inUse[checksum], nil
}

// fakeRunner records commands instead of executing them.
type fakeRunner struct {
	mu    sync.Mutex
	calls [][]string
	fail  map[int]error // call index -> error
}

func (r *fakeRunner) Run(ctx context.Context, name string, args ...string) ([]byte, error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	call := append([]string{name}, args...)
	idx := len(r.calls)
	r.calls = append(r.calls, call)
	if err := r.fail[idx]; err != nil {
		return []byte("boom"), err
	}
	return nil, nil
}

// sha256Hex returns the hex sha256 of content.
func sha256Hex(content string) string {
	return fmt.Sprintf("%x", sha256.Sum256([]byte(content)))
}

func TestPathDerivation(t *testing.T) {
	hex := strings.Repeat("ab", 32)
	tests := []struct {
		name     string
		checksum string
		wantErr  bool
	}{
		{"bare hex", hex, false},
		{"sha256 colon prefix", "sha256:" + hex, false},
		{"sha256 dash prefix", "sha256-" + hex, false},
		{"uppercase", strings.ToUpper(hex), false},
		{"too short", hex[:10], true},
		{"not hex", strings.Repeat("zz", 32), true},
		{"empty", "", true},
		{"path traversal", "../../etc/passwd", true},
	}
	s := New("/var/lib/bento/images", nil)
	want := "/var/lib/bento/images/sha256-" + hex + ".qcow2"
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := s.Path(tt.checksum)
			if tt.wantErr {
				if err == nil {
					t.Fatalf("Path(%q) = %q, want error", tt.checksum, got)
				}
				return
			}
			if err != nil {
				t.Fatalf("Path(%q): %v", tt.checksum, err)
			}
			if got != want {
				t.Fatalf("Path(%q) = %q, want %q", tt.checksum, got, want)
			}
		})
	}
}

func TestDefaultDir(t *testing.T) {
	s := New("", nil)
	if s.Dir() != DefaultDir {
		t.Fatalf("Dir() = %q, want %q", s.Dir(), DefaultDir)
	}
	if DefaultDir != "/var/lib/bento/images" {
		t.Fatalf("DefaultDir = %q, want the SPEC 5.1 path", DefaultDir)
	}
}

func TestBackingPathMatchesPath(t *testing.T) {
	hex := strings.Repeat("cd", 32)
	s := New(t.TempDir(), nil)
	a, err := s.Path(hex)
	if err != nil {
		t.Fatal(err)
	}
	b, err := s.BackingPath(hex)
	if err != nil {
		t.Fatal(err)
	}
	if a != b {
		t.Fatalf("BackingPath %q != Path %q", b, a)
	}
	if filepath.Base(a) != "sha256-"+hex+".qcow2" {
		t.Fatalf("unexpected file name %q", filepath.Base(a))
	}
}
