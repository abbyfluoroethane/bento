package main

import (
	"context"
	"io"
	"log/slog"
	"strings"
	"testing"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// recordingApplier records every applied ruleset.
type recordingApplier struct {
	applied []string
	err     error
}

func (r *recordingApplier) ApplyRuleset(_ context.Context, ruleset string) error {
	if r.err != nil {
		return r.err
	}
	r.applied = append(r.applied, ruleset)
	return nil
}

func discardLogger() *slog.Logger {
	return slog.New(slog.NewTextHandler(io.Discard, nil))
}

func TestBuildRuleset(t *testing.T) {
	e := newCmdEnv(t)
	amber := e.addUser(t, "amber")
	e.addUser(t, "blair")
	e.addImage(t, "debian-13", "aa11")
	b := e.backendFor("")
	inst := createInstance(t, e, b, amber, "web")
	if err := e.st.SetVisibility(inst.UUID, types.VisibilityPublic); err != nil {
		t.Fatal(err)
	}
	if err := e.st.SetHTTPPort(inst.UUID, 3000); err != nil {
		t.Fatal(err)
	}

	rs, err := buildRuleset(e.st, e.plan)
	if err != nil {
		t.Fatal(err)
	}
	if len(rs.Users) != 2 {
		t.Fatalf("users in ruleset = %d, want 2 (a user without instances still gets a bridge)", len(rs.Users))
	}
	text, err := rs.Render()
	if err != nil {
		t.Fatal(err)
	}
	// A reachable instance publishes port 22, the default HTTP port, and
	// the 3000-9999 proxy range (SPEC 6.3 rule 1 with 9.1).
	for _, want := range []string{"bento0", "bento1", inst.Address,
		"ip daddr " + inst.Address + " tcp dport { 22, 3000, 3000-9999 } accept"} {
		if !strings.Contains(text, want) {
			t.Errorf("ruleset misses %q:\n%s", want, text)
		}
	}
}

// TestBuildRulesetOffInstanceKeepsSSH pins SPEC 6.3 rule 1: host
// traffic to port 22 must be permitted on ANY instance, including the
// default visibility off, or the SSH frontend cannot dial instance:22.
// The HTTP ports stay closed for an off instance.
func TestBuildRulesetOffInstanceKeepsSSH(t *testing.T) {
	e := newCmdEnv(t)
	amber := e.addUser(t, "amber")
	e.addImage(t, "debian-13", "aa11")
	b := e.backendFor("")
	inst := createInstance(t, e, b, amber, "web") // visibility off by default

	rs, err := buildRuleset(e.st, e.plan)
	if err != nil {
		t.Fatal(err)
	}
	if n := len(rs.Users[0].Instances); n != 1 {
		t.Fatalf("published instances = %d, want 1 (port 22 for an off instance)", n)
	}
	text, err := rs.Render()
	if err != nil {
		t.Fatal(err)
	}
	if want := "ip daddr " + inst.Address + " tcp dport { 22 } accept"; !strings.Contains(text, want) {
		t.Errorf("ruleset misses the port-22 accept %q:\n%s", want, text)
	}
	for _, banned := range []string{"{ 22, 80", "3000-9999"} {
		if strings.Contains(text, banned) {
			t.Errorf("off instance must not publish HTTP ports (%q found):\n%s", banned, text)
		}
	}
}

func TestFirewallReloadSkipsUnchanged(t *testing.T) {
	e := newCmdEnv(t)
	e.addUser(t, "amber")
	applier := &recordingApplier{}
	fw := &firewall{st: e.st, plan: e.plan, applier: applier, log: discardLogger()}

	ctx := context.Background()
	if err := fw.reload(ctx); err != nil {
		t.Fatal(err)
	}
	if err := fw.reload(ctx); err != nil {
		t.Fatal(err)
	}
	if len(applier.applied) != 1 {
		t.Fatalf("applies = %d, want 1 (unchanged ruleset skipped)", len(applier.applied))
	}

	// A new user changes the ruleset; the next reload applies.
	e.addUser(t, "blair")
	if err := fw.reload(ctx); err != nil {
		t.Fatal(err)
	}
	if len(applier.applied) != 2 {
		t.Fatalf("applies = %d, want 2 after a new user", len(applier.applied))
	}
}
