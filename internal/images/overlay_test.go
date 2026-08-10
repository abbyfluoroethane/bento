package images

import (
	"context"
	"errors"
	"os"
	"reflect"
	"strings"
	"testing"
)

func TestCreateOverlay(t *testing.T) {
	sum := strings.Repeat("ab", 32)
	run := &fakeRunner{}
	s := New(t.TempDir(), newFakeDB(), WithRunner(run))
	if err := os.MkdirAll(s.Dir(), 0o755); err != nil {
		t.Fatal(err)
	}
	backing, _ := s.Path(sum)
	if err := os.WriteFile(backing, []byte("base"), 0o444); err != nil {
		t.Fatal(err)
	}

	overlay := s.Dir() + "/overlay.qcow2"
	if err := s.CreateOverlay(context.Background(), sum, overlay, 20); err != nil {
		t.Fatal(err)
	}

	want := [][]string{
		{"qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b", backing, overlay},
		{"qemu-img", "resize", overlay, "20G"},
	}
	if !reflect.DeepEqual(run.calls, want) {
		t.Fatalf("qemu-img calls = %v, want %v", run.calls, want)
	}
}

func TestCreateOverlayMissingBackingFile(t *testing.T) {
	sum := strings.Repeat("cd", 32)
	run := &fakeRunner{}
	s := New(t.TempDir(), newFakeDB(), WithRunner(run))

	err := s.CreateOverlay(context.Background(), sum, s.Dir()+"/o.qcow2", 10)
	if err == nil {
		t.Fatal("want error when the backing file is missing")
	}
	if !strings.Contains(err.Error(), sum) {
		t.Fatalf("error should name the missing version: %v", err)
	}
	if len(run.calls) != 0 {
		t.Fatalf("qemu-img must not run without a backing file, got %v", run.calls)
	}
}

func TestCreateOverlayInvalidInput(t *testing.T) {
	run := &fakeRunner{}
	s := New(t.TempDir(), newFakeDB(), WithRunner(run))
	sum := strings.Repeat("ab", 32)

	if err := s.CreateOverlay(context.Background(), "nothex", "/x/o.qcow2", 10); err == nil {
		t.Fatal("want error for a bad checksum")
	}
	if err := s.CreateOverlay(context.Background(), sum, "/x/o.qcow2", 0); err == nil {
		t.Fatal("want error for a non-positive disk size")
	}
	if len(run.calls) != 0 {
		t.Fatalf("qemu-img must not run on invalid input, got %v", run.calls)
	}
}

func TestCreateOverlayResizeFailureCleansUp(t *testing.T) {
	sum := strings.Repeat("ef", 32)
	run := &fakeRunner{fail: map[int]error{1: errors.New("resize failed")}}
	s := New(t.TempDir(), newFakeDB(), WithRunner(run))
	if err := os.MkdirAll(s.Dir(), 0o755); err != nil {
		t.Fatal(err)
	}
	backing, _ := s.Path(sum)
	if err := os.WriteFile(backing, []byte("base"), 0o444); err != nil {
		t.Fatal(err)
	}
	overlay := s.Dir() + "/overlay.qcow2"
	// Simulate qemu-img create having produced the file.
	if err := os.WriteFile(overlay, []byte("overlay"), 0o644); err != nil {
		t.Fatal(err)
	}

	err := s.CreateOverlay(context.Background(), sum, overlay, 10)
	if err == nil {
		t.Fatal("want the resize error")
	}
	if !strings.Contains(err.Error(), "resize failed") || !strings.Contains(err.Error(), "boom") {
		t.Fatalf("error should carry cause and command output: %v", err)
	}
	if _, statErr := os.Stat(overlay); !os.IsNotExist(statErr) {
		t.Fatal("half-built overlay must be removed")
	}
}
