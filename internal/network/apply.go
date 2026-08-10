package network

import (
	"bytes"
	"context"
	"fmt"
	"os/exec"
	"strings"
)

// Applier applies a complete nftables ruleset atomically. The real
// implementation execs nft; tests use a fake.
type Applier interface {
	ApplyRuleset(ctx context.Context, ruleset string) error
}

// NFTApplier applies a ruleset by feeding it to `nft -f -` on stdin.
// The ruleset text itself carries the delete-and-redefine of the Bento
// table, and nft applies a file as one transaction, so the reload is
// atomic.
type NFTApplier struct {
	// Path overrides the nft binary path. Empty means "nft" from PATH.
	Path string
}

// ApplyRuleset implements Applier.
func (a NFTApplier) ApplyRuleset(ctx context.Context, ruleset string) error {
	path := a.Path
	if path == "" {
		path = "nft"
	}
	cmd := exec.CommandContext(ctx, path, "-f", "-")
	cmd.Stdin = strings.NewReader(ruleset)
	out, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("network: nft -f -: %w: %s", err, bytes.TrimSpace(out))
	}
	return nil
}

// Reload renders the ruleset and applies it as one atomic full-table
// reload (SPEC 6.3). Call it on every change to the network policy.
func Reload(ctx context.Context, applier Applier, ruleset Ruleset) error {
	text, err := ruleset.Render()
	if err != nil {
		return err
	}
	return applier.ApplyRuleset(ctx, text)
}
