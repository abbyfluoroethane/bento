package images

import (
	"context"
	"errors"
	"reflect"
	"testing"

	"github.com/abbyfluoroethane/bento/internal/types"
)

type fakeReportSource struct {
	images []types.Image
	older  map[string]int // imageName -> count
	err    error
}

func (f *fakeReportSource) Images(ctx context.Context) ([]types.Image, error) {
	return f.images, f.err
}

func (f *fakeReportSource) CountInstancesOnOtherVersions(ctx context.Context, imageName, checksum string) (int, error) {
	return f.older[imageName], nil
}

func TestReport(t *testing.T) {
	src := &fakeReportSource{
		images: []types.Image{
			{Name: "ubuntu-24.04", CurrentChecksum: "bbb"},
			{Name: "debian-13", CurrentChecksum: "aaa"},
			{Name: "never-fetched"}, // no current checksum yet
		},
		older: map[string]int{"debian-13": 2, "never-fetched": 9},
	}
	got, err := Report(context.Background(), src)
	if err != nil {
		t.Fatal(err)
	}
	want := []Status{
		{Name: "debian-13", CurrentChecksum: "aaa", OlderInstances: 2},
		{Name: "never-fetched"}, // count query skipped without a current version
		{Name: "ubuntu-24.04", CurrentChecksum: "bbb"},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("Report = %+v, want %+v", got, want)
	}
}

func TestReportError(t *testing.T) {
	src := &fakeReportSource{err: errors.New("db down")}
	if _, err := Report(context.Background(), src); err == nil {
		t.Fatal("want error")
	}
}
