package main

// The one Bento nftables table (SPEC 6.3), built from the database and
// applied atomically. The control plane applies it at startup and on
// every poll tick; applying an unchanged ruleset is skipped.

import (
	"context"
	"log/slog"
	"net/netip"
	"sync"

	"github.com/abbyfluoroethane/bento/internal/network"
	"github.com/abbyfluoroethane/bento/internal/proxy"
	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// firewall renders and applies the Bento nftables table.
type firewall struct {
	st      *store.Store
	plan    network.Plan
	applier network.Applier
	log     *slog.Logger

	mu   sync.Mutex
	last string
}

// reload rebuilds the ruleset from the database and applies it when it
// changed since the last successful apply.
func (f *firewall) reload(ctx context.Context) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	ruleset, err := buildRuleset(f.st, f.plan)
	if err != nil {
		return err
	}
	text, err := ruleset.Render()
	if err != nil {
		return err
	}
	if text == f.last {
		return nil
	}
	if err := f.applier.ApplyRuleset(ctx, text); err != nil {
		return err
	}
	f.last = text
	f.log.Info("firewall: nftables table reloaded", "users", len(ruleset.Users))
	return nil
}

// buildRuleset derives the SPEC 6.3 input from the users and instances
// tables. Every registered user contributes their bridge (so the
// inter-user drops exist before the first instance). Every instance is
// published: SPEC 6.3 rule 1 permits host traffic to port 22 on ANY
// instance, whatever its visibility, or the SSH frontend could not
// reach it. An instance the proxy may forward to (visibility private or
// public) additionally publishes its default HTTP port and the
// 3000-9999 proxy range of SPEC 9.1.
func buildRuleset(st *store.Store, plan network.Plan) (network.Ruleset, error) {
	var zero network.Ruleset
	users, err := st.Users()
	if err != nil {
		return zero, err
	}
	instances, err := st.Instances()
	if err != nil {
		return zero, err
	}
	byOwner := make(map[int64][]network.PublishedInstance)
	for _, inst := range instances {
		addr, err := netip.ParseAddr(inst.Address)
		if err != nil {
			continue // a bad row must not take the firewall down
		}
		pub := network.PublishedInstance{Address: addr}
		if inst.Visibility == types.VisibilityPrivate || inst.Visibility == types.VisibilityPublic {
			port := inst.HTTPPort
			if port == 0 {
				port = 80 // SPEC 9.1: the default target port
			}
			pub.HTTPPorts = []int{port}
			pub.PortRanges = []network.PortRange{{From: proxy.HighPortMin, To: proxy.HighPortMax}}
		}
		byOwner[inst.OwnerID] = append(byOwner[inst.OwnerID], pub)
	}
	rs := network.Ruleset{PrivateRange: plan.Range()}
	for _, u := range users {
		prefix, err := netip.ParsePrefix(u.Subnet)
		if err != nil {
			continue
		}
		index, err := plan.Index(prefix)
		if err != nil {
			continue
		}
		un, err := network.NewUserNetwork(plan, index)
		if err != nil {
			return zero, err
		}
		rs.Users = append(rs.Users, network.FirewallUser{
			Network:   un,
			Instances: byOwner[u.ID],
		})
	}
	return rs, nil
}
