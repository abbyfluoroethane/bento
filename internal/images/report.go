package images

import (
	"context"
	"fmt"
	"sort"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// ReportSource is the consumer-side view of the queries the images command
// needs (SPEC section 15). The real store package implements it.
type ReportSource interface {
	// Images returns the operator allowlist with current checksums.
	Images(ctx context.Context) ([]types.Image, error)
	// CountInstancesOnOtherVersions returns how many instances rows of the
	// image carry a base_checksum different from the given checksum.
	CountInstancesOnOtherVersions(ctx context.Context, imageName, checksum string) (int, error)
}

// Status is one row of the images command: the image name, its current
// checksum, and the number of instances that hold an older version.
type Status struct {
	Name            string
	CurrentChecksum string
	OlderInstances  int
}

// Report builds the data for the images command, sorted by image name.
func Report(ctx context.Context, src ReportSource) ([]Status, error) {
	imgs, err := src.Images(ctx)
	if err != nil {
		return nil, fmt.Errorf("images: report: %w", err)
	}
	statuses := make([]Status, 0, len(imgs))
	for _, img := range imgs {
		st := Status{Name: img.Name, CurrentChecksum: img.CurrentChecksum}
		if img.CurrentChecksum != "" {
			n, err := src.CountInstancesOnOtherVersions(ctx, img.Name, img.CurrentChecksum)
			if err != nil {
				return nil, fmt.Errorf("images: report %s: %w", img.Name, err)
			}
			st.OlderInstances = n
		}
		statuses = append(statuses, st)
	}
	sort.Slice(statuses, func(i, j int) bool { return statuses[i].Name < statuses[j].Name })
	return statuses, nil
}
