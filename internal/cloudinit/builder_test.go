package cloudinit

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// fakeRunner captures the xorriso invocation and snapshots the staging
// directory contents at call time, before Build removes it.
type fakeRunner struct {
	calls  [][]string
	staged map[string]string
	err    error
}

func (r *fakeRunner) Run(ctx context.Context, name string, args ...string) ([]byte, error) {
	r.calls = append(r.calls, append([]string{name}, args...))
	if len(args) > 0 {
		dir := args[len(args)-1]
		r.staged = map[string]string{}
		entries, err := os.ReadDir(dir)
		if err == nil {
			for _, e := range entries {
				b, _ := os.ReadFile(filepath.Join(dir, e.Name()))
				r.staged[e.Name()] = string(b)
			}
		}
	}
	if r.err != nil {
		return []byte("xorriso says no"), r.err
	}
	return nil, nil
}

func TestBuild(t *testing.T) {
	run := &fakeRunner{}
	b := NewBuilder(WithRunner(run))
	isoPath := filepath.Join(t.TempDir(), "seed.iso")

	if err := b.Build(context.Background(), testSeed(), isoPath); err != nil {
		t.Fatal(err)
	}

	if len(run.calls) != 1 {
		t.Fatalf("xorriso ran %d times, want 1", len(run.calls))
	}
	call := run.calls[0]
	if call[0] != "xorriso" {
		t.Fatalf("command = %q, want xorriso", call[0])
	}
	args := strings.Join(call[1:], " ")
	for _, want := range []string{"-as mkisofs", "-output " + isoPath, "-volid cidata"} {
		if !strings.Contains(args, want) {
			t.Errorf("xorriso args missing %q: %s", want, args)
		}
	}

	for _, name := range []string{"meta-data", "user-data", "network-config"} {
		content, ok := run.staged[name]
		if !ok {
			t.Fatalf("staging directory missing %s (had %v)", name, run.staged)
		}
		want := golden(t, name+".golden")
		if content != want {
			t.Errorf("staged %s does not match golden output", name)
		}
	}

	// The staging directory (with the owner's keys) must be gone.
	stagingDir := run.calls[0][len(run.calls[0])-1]
	if _, err := os.Stat(stagingDir); !os.IsNotExist(err) {
		t.Fatalf("staging directory %s must be removed after the build", stagingDir)
	}
}

func TestBuildInvalidSeed(t *testing.T) {
	run := &fakeRunner{}
	b := NewBuilder(WithRunner(run))
	seed := testSeed()
	seed.AuthorizedKeys = nil

	if err := b.Build(context.Background(), seed, filepath.Join(t.TempDir(), "seed.iso")); err == nil {
		t.Fatal("want validation error")
	}
	if len(run.calls) != 0 {
		t.Fatal("xorriso must not run for an invalid seed")
	}
}

func TestBuildXorrisoFailure(t *testing.T) {
	run := &fakeRunner{err: errors.New("exit status 1")}
	b := NewBuilder(WithRunner(run), WithXorriso("/opt/xorriso"))

	err := b.Build(context.Background(), testSeed(), filepath.Join(t.TempDir(), "seed.iso"))
	if err == nil {
		t.Fatal("want error")
	}
	if !strings.Contains(err.Error(), "xorriso says no") {
		t.Fatalf("error should carry the command output: %v", err)
	}
	if run.calls[0][0] != "/opt/xorriso" {
		t.Fatalf("binary = %q, want the configured path", run.calls[0][0])
	}
}

func TestDelete(t *testing.T) {
	iso := filepath.Join(t.TempDir(), "seed.iso")
	if err := os.WriteFile(iso, []byte("iso"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := Delete(iso); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(iso); !os.IsNotExist(err) {
		t.Fatal("ISO must be removed")
	}
	// Deleting again is not an error: the ISO is already gone.
	if err := Delete(iso); err != nil {
		t.Fatalf("second delete: %v", err)
	}
}
