package images

import (
	"bytes"
	"context"
	"errors"
	"io/fs"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/abbyfluoroethane/bento/internal/types"
)

func newTestStore(t *testing.T, db *fakeDB, client *fakeClient) (*Store, *bytes.Buffer) {
	t.Helper()
	var logBuf bytes.Buffer
	log := slog.New(slog.NewTextHandler(&logBuf, nil))
	s := New(t.TempDir(), db, WithHTTPClient(client), WithLogger(log))
	return s, &logBuf
}

func TestFetchImagesStoresNewVersion(t *testing.T) {
	content := "debian-13-content"
	sum := sha256Hex(content)
	db := newFakeDB()
	db.images = []types.Image{{Name: "debian-13", URL: "https://img.example/d13.qcow2"}}
	client := &fakeClient{responses: map[string]fakeResponse{
		"https://img.example/d13.qcow2": {body: content},
	}}
	s, _ := newTestStore(t, db, client)

	if err := s.FetchImages(context.Background()); err != nil {
		t.Fatal(err)
	}

	path, _ := s.Path(sum)
	got, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("stored file: %v", err)
	}
	if string(got) != content {
		t.Fatalf("stored content = %q, want %q", got, content)
	}
	info, _ := os.Stat(path)
	if info.Mode().Perm()&0o222 != 0 {
		t.Fatalf("stored image is writable (mode %v); stored versions must never be written", info.Mode())
	}
	if len(db.inserted) != 1 {
		t.Fatalf("inserted %d versions, want 1", len(db.inserted))
	}
	v := db.inserted[0]
	if v.Checksum != sum || v.ImageName != "debian-13" || v.Path != path || v.Size != int64(len(content)) {
		t.Fatalf("inserted row = %+v", v)
	}
	if v.FetchedAt.IsZero() {
		t.Fatal("FetchedAt not set")
	}
	if db.images[0].CurrentChecksum != sum {
		t.Fatalf("current checksum = %q, want %q", db.images[0].CurrentChecksum, sum)
	}
	// No stray temp files.
	entries, _ := os.ReadDir(s.Dir())
	for _, e := range entries {
		if strings.HasPrefix(e.Name(), "download-") {
			t.Fatalf("leftover temp file %s", e.Name())
		}
	}
}

func TestFetchImagesPinMismatch(t *testing.T) {
	content := "tampered"
	pinned := strings.Repeat("11", 32)
	db := newFakeDB()
	db.images = []types.Image{{Name: "pinned-img", URL: "https://img.example/p.qcow2", PinnedChecksum: pinned}}
	client := &fakeClient{responses: map[string]fakeResponse{
		"https://img.example/p.qcow2": {body: content},
	}}
	s, _ := newTestStore(t, db, client)

	err := s.FetchImages(context.Background())
	if err == nil {
		t.Fatal("want error on pin mismatch")
	}
	if !strings.Contains(err.Error(), pinned) || !strings.Contains(err.Error(), sha256Hex(content)) {
		t.Fatalf("error should name both checksums: %v", err)
	}
	if len(db.inserted) != 0 {
		t.Fatal("rejected file must not be inserted")
	}
	if db.images[0].CurrentChecksum != "" {
		t.Fatal("rejected file must not become current")
	}
	path, _ := s.Path(sha256Hex(content))
	if _, err := os.Stat(path); !errors.Is(err, fs.ErrNotExist) {
		t.Fatal("rejected file must not be stored")
	}
}

func TestFetchImagesPinMatch(t *testing.T) {
	content := "trusted"
	sum := sha256Hex(content)
	db := newFakeDB()
	// Pin with the sha256: prefix to exercise normalization.
	db.images = []types.Image{{Name: "pinned-img", URL: "https://img.example/p.qcow2", PinnedChecksum: "sha256:" + strings.ToUpper(sum)}}
	client := &fakeClient{responses: map[string]fakeResponse{
		"https://img.example/p.qcow2": {body: content},
	}}
	s, _ := newTestStore(t, db, client)

	if err := s.FetchImages(context.Background()); err != nil {
		t.Fatal(err)
	}
	if db.images[0].CurrentChecksum != sum {
		t.Fatalf("current = %q, want %q", db.images[0].CurrentChecksum, sum)
	}
}

func TestFetchImagesNoOpWhenVersionExists(t *testing.T) {
	content := "same-content"
	sum := sha256Hex(content)
	db := newFakeDB()
	db.images = []types.Image{{Name: "debian-13", URL: "https://img.example/d13.qcow2", CurrentChecksum: sum}}
	db.versions[sum] = types.ImageVersion{Checksum: sum, ImageName: "debian-13"}
	client := &fakeClient{responses: map[string]fakeResponse{
		"https://img.example/d13.qcow2": {body: content},
	}}
	s, _ := newTestStore(t, db, client)
	// Put the stored file in place so no warning fires.
	path, _ := s.Path(sum)
	if err := os.MkdirAll(s.Dir(), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o444); err != nil {
		t.Fatal(err)
	}

	if err := s.FetchImages(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(db.inserted) != 0 {
		t.Fatal("existing version must not be inserted again")
	}
}

func TestFetchImagesUnpinnedChangeWarns(t *testing.T) {
	oldContent, newContent := "v1", "v2"
	oldSum, newSum := sha256Hex(oldContent), sha256Hex(newContent)
	db := newFakeDB()
	db.images = []types.Image{{Name: "debian-13", URL: "https://img.example/d13.qcow2", CurrentChecksum: oldSum}}
	db.versions[oldSum] = types.ImageVersion{Checksum: oldSum, ImageName: "debian-13"}
	// An instance still uses the old version, so GC keeps it.
	db.inUse[oldSum] = true
	client := &fakeClient{responses: map[string]fakeResponse{
		"https://img.example/d13.qcow2": {body: newContent},
	}}
	s, logBuf := newTestStore(t, db, client)

	if err := s.FetchImages(context.Background()); err != nil {
		t.Fatal(err)
	}
	if db.images[0].CurrentChecksum != newSum {
		t.Fatalf("current = %q, want new checksum %q", db.images[0].CurrentChecksum, newSum)
	}
	logs := logBuf.String()
	if !strings.Contains(logs, oldSum) || !strings.Contains(logs, newSum) {
		t.Fatalf("warning must name both checksums, got: %s", logs)
	}
	if !strings.Contains(logs, "level=WARN") {
		t.Fatalf("expected a warning, got: %s", logs)
	}
}

func TestFetchImagesDownloadErrorDoesNotStopOthers(t *testing.T) {
	okContent := "fine"
	db := newFakeDB()
	db.images = []types.Image{
		{Name: "broken", URL: "https://img.example/broken.qcow2"},
		{Name: "fine", URL: "https://img.example/fine.qcow2"},
	}
	client := &fakeClient{responses: map[string]fakeResponse{
		"https://img.example/broken.qcow2": {status: 500},
		"https://img.example/fine.qcow2":   {body: okContent},
	}}
	s, _ := newTestStore(t, db, client)

	err := s.FetchImages(context.Background())
	if err == nil {
		t.Fatal("want error for the broken image")
	}
	if !strings.Contains(err.Error(), "broken") {
		t.Fatalf("error should name the broken image: %v", err)
	}
	if db.images[1].CurrentChecksum != sha256Hex(okContent) {
		t.Fatal("the healthy image must still be fetched")
	}
}

// blockingClient is a Doer whose Do runs a callback before answering.
// The callback stands in for work that happens while the download is in
// flight.
type blockingClient struct {
	inner  *fakeClient
	during func()
}

func (c *blockingClient) Do(req *http.Request) (*http.Response, error) {
	if c.during != nil {
		c.during()
	}
	return c.inner.Do(req)
}

// TestFetchImagesDoesNotHoldLockDuringDownload pins the SPEC 19 lock
// scope: the store lock guards image version creation and deletion
// only, never a download, so a `new` command's CreateOverlay must
// succeed while a fetch-images download is in flight. If FetchImages
// held the lock across the download, the CreateOverlay below would
// deadlock on the in-process mutex and the test would time out.
func TestFetchImagesDoesNotHoldLockDuringDownload(t *testing.T) {
	existing := "already-stored"
	existingSum := sha256Hex(existing)
	content := "new-image"
	db := newFakeDB()
	db.images = []types.Image{{Name: "debian-13", URL: "https://img.example/d13.qcow2"}}
	db.versions[existingSum] = types.ImageVersion{Checksum: existingSum, ImageName: "debian-13"}
	db.inUse[existingSum] = true

	run := &fakeRunner{}
	var s *Store
	overlayDone := make(chan error, 1)
	client := &blockingClient{
		inner: &fakeClient{responses: map[string]fakeResponse{
			"https://img.example/d13.qcow2": {body: content},
		}},
		during: func() {
			overlayDone <- s.CreateOverlay(context.Background(),
				existingSum, filepath.Join(s.Dir(), "overlay.qcow2"), 10)
		},
	}
	var logBuf bytes.Buffer
	s = New(t.TempDir(), db, WithHTTPClient(client), WithRunner(run),
		WithLogger(slog.New(slog.NewTextHandler(&logBuf, nil))))
	// The backing file of the existing version must be present for
	// CreateOverlay.
	if err := os.MkdirAll(s.Dir(), 0o755); err != nil {
		t.Fatal(err)
	}
	backing, _ := s.Path(existingSum)
	if err := os.WriteFile(backing, []byte(existing), 0o444); err != nil {
		t.Fatal(err)
	}

	if err := s.FetchImages(context.Background()); err != nil {
		t.Fatal(err)
	}
	if err := <-overlayDone; err != nil {
		t.Fatalf("CreateOverlay during a download: %v", err)
	}
	if db.images[0].CurrentChecksum != sha256Hex(content) {
		t.Fatalf("current = %q, want the downloaded checksum", db.images[0].CurrentChecksum)
	}
}

func TestCollectDeletesOnlyUnreferencedNonCurrentVersions(t *testing.T) {
	current, older, orphan := "cur", "old", "gone"
	curSum, oldSum, orphanSum := sha256Hex(current), sha256Hex(older), sha256Hex(orphan)

	db := newFakeDB()
	db.images = []types.Image{{Name: "debian-13", URL: "https://img.example/d13.qcow2", CurrentChecksum: curSum}}
	for sum, name := range map[string]string{curSum: "debian-13", oldSum: "debian-13", orphanSum: "debian-13"} {
		db.versions[sum] = types.ImageVersion{Checksum: sum, ImageName: name}
	}
	// One instance still runs the older version; nothing runs the orphan.
	db.inUse[oldSum] = true
	// The current version has no instances yet: it must survive anyway,
	// because the next new instance boots from it.
	client := &fakeClient{responses: map[string]fakeResponse{
		"https://img.example/d13.qcow2": {body: current},
	}}
	s, _ := newTestStore(t, db, client)
	if err := os.MkdirAll(s.Dir(), 0o755); err != nil {
		t.Fatal(err)
	}
	for _, sum := range []string{curSum, oldSum, orphanSum} {
		p, _ := s.Path(sum)
		if err := os.WriteFile(p, []byte(sum), 0o444); err != nil {
			t.Fatal(err)
		}
	}

	if err := s.FetchImages(context.Background()); err != nil {
		t.Fatal(err)
	}

	if len(db.deleted) != 1 || db.deleted[0] != orphanSum {
		t.Fatalf("deleted = %v, want only %s", db.deleted, orphanSum)
	}
	for sum, wantGone := range map[string]bool{curSum: false, oldSum: false, orphanSum: true} {
		p, _ := s.Path(sum)
		_, err := os.Stat(p)
		gone := errors.Is(err, fs.ErrNotExist)
		if gone != wantGone {
			t.Fatalf("file for %s: gone=%v, want %v", sum, gone, wantGone)
		}
	}
	if _, ok := db.versions[oldSum]; !ok {
		t.Fatal("in-use version row must be kept")
	}
	if _, ok := db.versions[curSum]; !ok {
		t.Fatal("current version row must be kept")
	}
}
