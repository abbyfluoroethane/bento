package main

// Shared bootstrap for the subcommands: configuration, logging, the
// store, the libvirt connection, and the lifecycle manager.

import (
	"fmt"
	"log/slog"
	"net"
	"net/netip"
	"net/url"
	"os"
	"strings"

	"github.com/abbyfluoroethane/bento/internal/cloudinit"
	"github.com/abbyfluoroethane/bento/internal/config"
	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/images"
	"github.com/abbyfluoroethane/bento/internal/lifecycle"
	"github.com/abbyfluoroethane/bento/internal/network"
	"github.com/abbyfluoroethane/bento/internal/store"
)

// app bundles what every subcommand needs.
type app struct {
	cfg  config.Config
	plan network.Plan
	log  *slog.Logger
	st   *store.Store
}

// newApp loads the configuration and opens the database. It logs the
// database path: SPEC 12.1 wants the one documented path in the startup
// log.
func newApp(configPath string) (*app, error) {
	cfg, err := config.Load(configPath)
	if err != nil {
		return nil, err
	}
	plan, err := network.NewPlan(cfg.PrivateRange)
	if err != nil {
		return nil, err
	}
	log := slog.New(slog.NewTextHandler(os.Stderr, nil))
	st, err := store.Open(cfg.DBPath)
	if err != nil {
		return nil, fmt.Errorf("open database %s: %w", cfg.DBPath, err)
	}
	log.Info("database open", "path", cfg.DBPath,
		"note", "back it up with `bentod dump-db`, never with a file copy (SPEC 12.1)")
	return &app{cfg: cfg, plan: plan, log: log, st: st}, nil
}

func (a *app) close() {
	if err := a.st.Close(); err != nil {
		a.log.Warn("closing database", "error", err)
	}
}

// connectLibvirt dials libvirtd over the local socket named by the
// configured URI (SPEC 4.1).
func (a *app) connectLibvirt() (*hypervisor.Client, error) {
	return hypervisor.Connect(socketPath(a.cfg.LibvirtURI))
}

// socketPath extracts the unix socket from a qemu:///system style URI.
// The default URI and an empty string select the default socket; a
// ?socket= parameter overrides it.
func socketPath(uri string) string {
	if uri == "" {
		return ""
	}
	if u, err := url.Parse(uri); err == nil {
		if s := u.Query().Get("socket"); s != "" {
			return s
		}
	}
	return ""
}

// imageStore returns the content-addressed image store over the
// database (SPEC 5.1).
func (a *app) imageStore() *images.Store {
	return images.New(a.cfg.ImageDir, imagesDB{a.st}, images.WithLogger(a.log))
}

// manager builds the lifecycle manager over the given hypervisor
// connection.
func (a *app) manager(hyp hypervisor.Hypervisor) (*lifecycle.Manager, error) {
	dns, err := a.dnsAddrs()
	if err != nil {
		return nil, err
	}
	return lifecycle.NewManager(lifecycle.Config{
		Hypervisor:   hyp,
		Store:        a.st,
		Images:       a.imageStore(),
		ISO:          cloudinit.NewBuilder(),
		Plan:         a.plan,
		StorageDir:   a.cfg.StorageDir,
		NameCooldown: a.cfg.NameCooldown.Std(),
		BatchSize:    a.cfg.RestoreBatchSize,
		DNS:          dns,
		Logger:       a.log,
	})
}

// dnsAddrs parses the configured resolvers; empty means the built-in
// defaults.
func (a *app) dnsAddrs() ([]netip.Addr, error) {
	var out []netip.Addr
	for _, d := range a.cfg.DNS {
		addr, err := netip.ParseAddr(d)
		if err != nil {
			return nil, fmt.Errorf("config dns: %w", err)
		}
		out = append(out, addr)
	}
	return out, nil
}

// defaultImage resolves the image a flagless `new` uses: the configured
// default, or the first allowlist entry.
func defaultImage(cfg config.Config) string {
	if cfg.Defaults.Image != "" {
		return cfg.Defaults.Image
	}
	if len(cfg.Images) > 0 {
		return cfg.Images[0].Name
	}
	return ""
}

// isOperator builds the operator predicate from the configured names
// (SPEC 12.1: the database download is operator-only).
func isOperator(names []string) func(name string) bool {
	set := make(map[string]bool, len(names))
	for _, n := range names {
		set[n] = true
	}
	return func(name string) bool { return set[name] }
}

// controlURL turns the control plane listen address into a URL the
// proxy can dial: an unspecified host becomes loopback.
func controlURL(listenHTTP string) string {
	host, port, err := net.SplitHostPort(listenHTTP)
	if err != nil {
		return "http://" + listenHTTP
	}
	if host == "" || host == "0.0.0.0" || host == "::" {
		host = "127.0.0.1"
	}
	return "http://" + net.JoinHostPort(host, port)
}

// bindHost extracts the host half of a listen address for the proxy's
// port fan-out.
func bindHost(listen string) string {
	host, _, err := net.SplitHostPort(listen)
	if err != nil {
		return strings.TrimPrefix(listen, ":")
	}
	return host
}
