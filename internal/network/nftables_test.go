package network

import (
	"context"
	"errors"
	"flag"
	"net/netip"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

var update = flag.Bool("update", false, "rewrite golden files")

// twoUserRuleset is the fixture from the task: two users, each with a
// bridge, one with two instances and one with one.
func twoUserRuleset() Ruleset {
	return Ruleset{
		PrivateRange: netip.MustParsePrefix("10.77.0.0/16"),
		Users: []FirewallUser{
			{
				Network: UserNetwork{
					Name:   "bento-user-2",
					Bridge: "bento2",
					Subnet: netip.MustParsePrefix("10.77.2.0/24"),
				},
				Instances: []PublishedInstance{
					{Address: netip.MustParseAddr("10.77.2.2"), HTTPPorts: []int{3000}},
				},
			},
			{
				Network: UserNetwork{
					Name:   "bento-user-1",
					Bridge: "bento1",
					Subnet: netip.MustParsePrefix("10.77.1.0/24"),
				},
				Instances: []PublishedInstance{
					{
						Address:    netip.MustParseAddr("10.77.1.2"),
						HTTPPorts:  []int{80},
						PortRanges: []PortRange{{From: 3000, To: 9999}},
					},
					{Address: netip.MustParseAddr("10.77.1.3"), HTTPPorts: []int{8080, 80, 80}},
				},
			},
		},
	}
}

func TestRulesetRenderGolden(t *testing.T) {
	got, err := twoUserRuleset().Render()
	if err != nil {
		t.Fatal(err)
	}
	want := readGolden(t, "two_users.nft", got)
	if got != want {
		t.Errorf("ruleset mismatch:\ngot:\n%s\nwant:\n%s", got, want)
	}
}

func TestRulesetInterUserDropPrecedesEgressAccept(t *testing.T) {
	got, err := twoUserRuleset().Render()
	if err != nil {
		t.Fatal(err)
	}
	drop := strings.Index(got, `iifname "bento1" oifname "bento2" drop`)
	dropBack := strings.Index(got, `iifname "bento2" oifname "bento1" drop`)
	egress1 := strings.Index(got, `iifname "bento1" accept`)
	egress2 := strings.Index(got, `iifname "bento2" accept`)
	for name, idx := range map[string]int{
		"drop 1->2": drop, "drop 2->1": dropBack, "egress 1": egress1, "egress 2": egress2,
	} {
		if idx < 0 {
			t.Fatalf("ruleset is missing the %s rule:\n%s", name, got)
		}
	}
	if drop > egress1 || drop > egress2 || dropBack > egress1 || dropBack > egress2 {
		t.Errorf("inter-user drop must precede the egress accept:\n%s", got)
	}
}

func TestRulesetRenderContents(t *testing.T) {
	got, err := twoUserRuleset().Render()
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{
		// Atomic full-table reload framing.
		"add table inet bento\n",
		"delete table inet bento\n",
		"table inet bento {\n",
		// Rule 1: host to instance on 22 + published HTTP ports and
		// ranges (the 3000-9999 proxy range of SPEC 9.1).
		`oifname "bento1" ip daddr 10.77.1.2 tcp dport { 22, 80, 3000-9999 } accept`,
		`oifname "bento1" ip daddr 10.77.1.3 tcp dport { 22, 80, 8080 } accept`,
		`oifname "bento2" ip daddr 10.77.2.2 tcp dport { 22, 3000 } accept`,
		// Rule 4: within a user's bridge.
		`iifname "bento1" oifname "bento1" accept`,
		`iifname "bento2" oifname "bento2" accept`,
		// Rule 5: masquerade behind the host address.
		`ip saddr 10.77.1.0/24 ip daddr != 10.77.0.0/16 masquerade`,
		`ip saddr 10.77.2.0/24 ip daddr != 10.77.0.0/16 masquerade`,
	} {
		if !strings.Contains(got, want) {
			t.Errorf("ruleset is missing %q:\n%s", want, got)
		}
	}
	if strings.Count(got, "table inet bento") != 3 {
		t.Errorf("want exactly one table (add, delete, define):\n%s", got)
	}
}

func TestRulesetRenderStableOrder(t *testing.T) {
	a, err := twoUserRuleset().Render()
	if err != nil {
		t.Fatal(err)
	}
	// Same fixture with the users pre-sorted the other way round.
	r := twoUserRuleset()
	r.Users[0], r.Users[1] = r.Users[1], r.Users[0]
	b, err := r.Render()
	if err != nil {
		t.Fatal(err)
	}
	if a != b {
		t.Error("Render output depends on user order")
	}
}

func TestRulesetRenderRejectsBadInput(t *testing.T) {
	base := func() Ruleset { return twoUserRuleset() }
	tests := []struct {
		name   string
		mutate func(*Ruleset)
	}{
		{"unsafe bridge name", func(r *Ruleset) { r.Users[0].Network.Bridge = `br"; flush ruleset; #` }},
		{"duplicate bridge", func(r *Ruleset) { r.Users[0].Network.Bridge = r.Users[1].Network.Bridge }},
		{"address outside subnet", func(r *Ruleset) {
			r.Users[0].Instances[0].Address = netip.MustParseAddr("10.99.0.2")
		}},
		{"invalid port", func(r *Ruleset) { r.Users[0].Instances[0].HTTPPorts = []int{70000} }},
		{"zero port", func(r *Ruleset) { r.Users[0].Instances[0].HTTPPorts = []int{0} }},
		{"inverted range", func(r *Ruleset) {
			r.Users[0].Instances[0].PortRanges = []PortRange{{From: 9999, To: 3000}}
		}},
		{"range past 65535", func(r *Ruleset) {
			r.Users[0].Instances[0].PortRanges = []PortRange{{From: 3000, To: 70000}}
		}},
		{"zero range", func(r *Ruleset) {
			r.Users[0].Instances[0].PortRanges = []PortRange{{From: 0, To: 100}}
		}},
		{"invalid private range", func(r *Ruleset) { r.PrivateRange = netip.Prefix{} }},
		{"not a /24", func(r *Ruleset) {
			r.Users[0].Network.Subnet = netip.MustParsePrefix("10.77.0.0/16")
		}},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			r := base()
			tt.mutate(&r)
			if _, err := r.Render(); err == nil {
				t.Error("Render(): want error")
			}
		})
	}
}

type fakeApplier struct {
	applied []string
	err     error
}

func (f *fakeApplier) ApplyRuleset(_ context.Context, ruleset string) error {
	f.applied = append(f.applied, ruleset)
	return f.err
}

func TestReload(t *testing.T) {
	ctx := context.Background()
	fake := &fakeApplier{}
	if err := Reload(ctx, fake, twoUserRuleset()); err != nil {
		t.Fatal(err)
	}
	if len(fake.applied) != 1 {
		t.Fatalf("applied %d rulesets, want 1", len(fake.applied))
	}
	want, err := twoUserRuleset().Render()
	if err != nil {
		t.Fatal(err)
	}
	if fake.applied[0] != want {
		t.Error("Reload applied a different ruleset than Render produced")
	}

	// A render error must not reach the applier.
	bad := twoUserRuleset()
	bad.Users[0].Network.Bridge = "no good"
	fake2 := &fakeApplier{}
	if err := Reload(ctx, fake2, bad); err == nil {
		t.Error("Reload with bad ruleset: want error")
	}
	if len(fake2.applied) != 0 {
		t.Error("Reload applied a ruleset that failed to render")
	}

	// An applier error is returned.
	fake3 := &fakeApplier{err: errors.New("nft exploded")}
	if err := Reload(ctx, fake3, twoUserRuleset()); err == nil {
		t.Error("Reload with failing applier: want error")
	}
}

func TestNFTApplierExec(t *testing.T) {
	ctx := context.Background()
	dir := t.TempDir()

	ok := filepath.Join(dir, "nft-ok")
	if err := os.WriteFile(ok, []byte("#!/bin/sh\ncat > \"$0.stdin\"\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := (NFTApplier{Path: ok}).ApplyRuleset(ctx, "table inet bento {\n}\n"); err != nil {
		t.Fatalf("ApplyRuleset: %v", err)
	}
	stdin, err := os.ReadFile(ok + ".stdin")
	if err != nil {
		t.Fatal(err)
	}
	if string(stdin) != "table inet bento {\n}\n" {
		t.Errorf("nft received %q on stdin", stdin)
	}

	fail := filepath.Join(dir, "nft-fail")
	if err := os.WriteFile(fail, []byte("#!/bin/sh\necho 'syntax error' >&2\nexit 1\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	err = (NFTApplier{Path: fail}).ApplyRuleset(ctx, "bogus")
	if err == nil {
		t.Fatal("ApplyRuleset with failing nft: want error")
	}
	if !strings.Contains(err.Error(), "syntax error") {
		t.Errorf("error does not carry nft stderr: %v", err)
	}
}
