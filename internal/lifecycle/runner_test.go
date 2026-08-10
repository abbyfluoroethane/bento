package lifecycle

import (
	"context"
	"errors"
	"strings"
	"testing"
)

type recordingRunner struct {
	name string
	args []string
	out  []byte
	err  error
}

func (r *recordingRunner) Run(_ context.Context, name string, args ...string) ([]byte, error) {
	r.name = name
	r.args = args
	return r.out, r.err
}

func TestQemuImgResizer(t *testing.T) {
	runner := &recordingRunner{}
	r := QemuImgResizer{Runner: runner}

	if err := r.ResizeOverlay(context.Background(), "/var/lib/bento/storage/u1.qcow2", 30); err != nil {
		t.Fatal(err)
	}
	if runner.name != "qemu-img" {
		t.Errorf("binary = %s, want qemu-img", runner.name)
	}
	if got := strings.Join(runner.args, " "); got != "resize /var/lib/bento/storage/u1.qcow2 30G" {
		t.Errorf("args = %q, want %q", got, "resize /var/lib/bento/storage/u1.qcow2 30G")
	}
}

func TestQemuImgResizerCustomBinary(t *testing.T) {
	runner := &recordingRunner{}
	r := QemuImgResizer{Runner: runner, QemuImg: "/opt/qemu/bin/qemu-img"}
	if err := r.ResizeOverlay(context.Background(), "/x.qcow2", 5); err != nil {
		t.Fatal(err)
	}
	if runner.name != "/opt/qemu/bin/qemu-img" {
		t.Errorf("binary = %s, want the configured path", runner.name)
	}
}

func TestQemuImgResizerFailure(t *testing.T) {
	runner := &recordingRunner{out: []byte("image is locked"), err: errors.New("exit status 1")}
	r := QemuImgResizer{Runner: runner}
	err := r.ResizeOverlay(context.Background(), "/x.qcow2", 5)
	if err == nil {
		t.Fatal("no error")
	}
	for _, want := range []string{"exit status 1", "image is locked"} {
		if !strings.Contains(err.Error(), want) {
			t.Errorf("error %q missing %q", err, want)
		}
	}
}

func TestQemuImgResizerRejectsNonPositive(t *testing.T) {
	runner := &recordingRunner{}
	r := QemuImgResizer{Runner: runner}
	if err := r.ResizeOverlay(context.Background(), "/x.qcow2", 0); err == nil {
		t.Fatal("no error for zero size")
	}
	if runner.name != "" {
		t.Error("command ran for a zero size")
	}
}
