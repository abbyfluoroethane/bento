package main

// The control plane (SPEC 4): the only writer of the database, the
// policy layer, and the dashboard. Startup order follows SPEC 4.2 and
// 11.2: host checks, libvirt, user networks, firewall, reboot restore,
// then HTTP.

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"github.com/abbyfluoroethane/bento/internal/api"
	"github.com/abbyfluoroethane/bento/internal/auth"
	"github.com/abbyfluoroethane/bento/internal/dashboard"
	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/lifecycle"
	"github.com/abbyfluoroethane/bento/internal/network"
	"github.com/abbyfluoroethane/bento/internal/types"
)

func runServe(configPath string, _ []string) error {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	a, err := newApp(configPath)
	if err != nil {
		return err
	}
	defer a.close()

	// SPEC 4.2: refuse to start when a hard host requirement is
	// missing; warn about KSM and nested.
	if err := hostChecks(a); err != nil {
		return err
	}

	hyp, err := a.connectLibvirt()
	if err != nil {
		return err
	}
	defer hyp.Close()

	hostname, _ := os.Hostname()
	host, err := a.st.EnsureHost(hostname, a.cfg.LibvirtURI)
	if err != nil {
		return fmt.Errorf("hosts row: %w", err)
	}

	// The frontend public key rides along in every seed so the SSH
	// frontend can reach the guests (SPEC 10 step 9). Creating it here
	// keeps serve and sshd in agreement regardless of start order.
	frontendKey, err := ensureKey(keyPath(a, frontendKeyFile), "bento-frontend")
	if err != nil {
		return fmt.Errorf("frontend key: %w", err)
	}
	frontendPub := authorizedKeyLine(frontendKey.PublicKey(), "bento-frontend")

	mgr, err := a.manager(hyp)
	if err != nil {
		return err
	}

	// Sync the image allowlist so `new` sees the configured images even
	// before the first fetch-images run.
	if err := syncImageAllowlist(a); err != nil {
		return err
	}

	// Per-user libvirt networks and the nftables table (SPEC 6.2, 6.3).
	fw := &firewall{st: a.st, plan: a.plan, applier: network.NFTApplier{}, log: a.log}
	if err := ensureUserNetworks(ctx, a, hyp); err != nil {
		return err
	}
	if err := fw.reload(ctx); err != nil {
		return fmt.Errorf("nftables: %w", err)
	}

	// SPEC 11.2: restore desired state in batches. HTTP comes up while
	// the restore runs so a user sees `starting`, not an error.
	go func() {
		if err := mgr.Restore(ctx); err != nil && !errors.Is(err, context.Canceled) {
			a.log.Error("restore failed", "error", err)
		}
	}()

	// The 30-second observed-state poll (SPEC 12), plus the network and
	// firewall convergence on the same cadence: registrations from the
	// sshd process and port or visibility changes are picked up here.
	go func() {
		if err := mgr.RunPoller(ctx); err != nil && !errors.Is(err, context.Canceled) {
			a.log.Error("poller stopped", "error", err)
		}
	}()
	go converge(ctx, a, hyp, fw)

	handler, err := controlPlaneHandler(ctx, a, mgr, fw, frontendPub, host.ID)
	if err != nil {
		return err
	}
	srv := &http.Server{Addr: a.cfg.Listen.HTTP, Handler: handler}
	go func() {
		<-ctx.Done()
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		srv.Shutdown(shutdownCtx)
	}()
	a.log.Info("control plane listening", "addr", a.cfg.Listen.HTTP, "domain", a.cfg.BaseDomain)
	if err := srv.ListenAndServe(); !errors.Is(err, http.ErrServerClosed) {
		return err
	}
	return nil
}

// hostChecks runs the SPEC 4.2 list. Failures are fatal; KSM and nested
// are warnings.
func hostChecks(a *app) error {
	nestedWanted := false
	if insts, err := a.st.Instances(); err == nil {
		for _, inst := range insts {
			if inst.Nested {
				nestedWanted = true
				break
			}
		}
	}
	report := hypervisor.Check(hypervisor.CheckConfig{
		SocketPath:   socketPath(a.cfg.LibvirtURI),
		ImageDir:     a.cfg.ImageDir,
		StorageDir:   a.cfg.StorageDir,
		NestedWanted: nestedWanted,
	}, hypervisor.DefaultCheckDeps())
	for _, w := range report.Warnings() {
		a.log.Warn("host check", "check", w.Name, "detail", w.Detail)
	}
	if !report.OK() {
		var failed []string
		for _, r := range report.Results {
			if r.Fatal && !r.OK {
				failed = append(failed, fmt.Sprintf("%s: %s", r.Name, r.Detail))
			}
		}
		return fmt.Errorf("host requirements not met (SPEC 4.2):\n  %s", strings.Join(failed, "\n  "))
	}
	return nil
}

// syncImageAllowlist upserts the configured images. Current checksums
// are never touched here (that is fetch-images' job), so a config
// reload cannot roll an image back.
func syncImageAllowlist(a *app) error {
	for _, img := range a.cfg.Images {
		err := a.st.UpsertImage(types.Image{
			Name:           img.Name,
			URL:            img.URL,
			PinnedChecksum: img.PinnedChecksum,
		})
		if err != nil {
			return fmt.Errorf("image allowlist %s: %w", img.Name, err)
		}
	}
	return nil
}

// ensureUserNetworks defines and starts the libvirt network of every
// registered user (SPEC 6.2). It runs at startup and on every
// convergence tick, so a registration taken by the sshd process while
// libvirt was unreachable heals here.
func ensureUserNetworks(ctx context.Context, a *app, networks networkEnsurer) error {
	users, err := a.st.Users()
	if err != nil {
		return err
	}
	for _, u := range users {
		name, xml, err := userNetwork(a.plan, u.Subnet)
		if err != nil {
			a.log.Warn("user network skipped", "user", u.Name, "error", err)
			continue
		}
		if err := networks.EnsureNetwork(ctx, name, xml); err != nil {
			return fmt.Errorf("network %s of %s: %w", name, u.Name, err)
		}
	}
	return nil
}

// converge re-ensures the user networks and the firewall on the poll
// cadence.
func converge(ctx context.Context, a *app, networks networkEnsurer, fw *firewall) {
	ticker := time.NewTicker(30 * time.Second)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			if err := ensureUserNetworks(ctx, a, networks); err != nil {
				a.log.Warn("network convergence", "error", err)
			}
			if err := fw.reload(ctx); err != nil {
				a.log.Warn("firewall convergence", "error", err)
			}
		}
	}
}

// controlPlaneHandler assembles the one HTTP handler of the control
// plane: OIDC login, the JSON API, and the dashboard (SPEC 13, 14).
func controlPlaneHandler(ctx context.Context, a *app, mgr *lifecycle.Manager, fw *firewall, frontendPub string, hostID int64) (http.Handler, error) {
	authOpts := []auth.Option{}
	if a.cfg.OIDC.Issuer != "" {
		redirect := "https://" + a.cfg.BaseDomain + "/callback"
		pc, err := auth.NewProviderClient(ctx, a.cfg.OIDC.Issuer, a.cfg.OIDC.ClientID, a.cfg.OIDC.ClientSecret, redirect)
		if err != nil {
			// The dashboard login degrades; SSH and tokens still work.
			a.log.Warn("OIDC discovery failed; dashboard login disabled until restart",
				"issuer", a.cfg.OIDC.Issuer, "error", err)
		} else {
			authOpts = append(authOpts, auth.WithOIDC(pc, pc))
		}
	} else {
		a.log.Warn("no OIDC issuer configured; dashboard login disabled")
	}
	authSvc := auth.New(a.cfg.BaseDomain, authUsers{a.st}, a.st, authTokens{a.st}, authOpts...)

	apiSrv := api.New(api.Config{
		Store: a.st,
		Lifecycle: &apiBackend{
			backend: backend{m: mgr, st: a.st, hostID: hostID, frontendKey: frontendPub, firewall: fw},
		},
		Auth: &authenticator{svc: authSvc, st: a.st},
		IsOperator: func(u types.User) bool {
			return isOperator(a.cfg.Operators)(u.Name)
		},
		DBPath: a.cfg.DBPath,
	})

	mux := http.NewServeMux()
	mux.Handle("/api/", apiSrv)
	// The HTTP proxy's per-request authorization check for private
	// instances (SPEC 13): owner or share on the instance UUID.
	mux.Handle("GET /access/{uuid}", accessHandler(authSvc))
	mux.Handle("/login", authSvc.LoginHandler())
	mux.Handle("/callback", authSvc.CallbackHandler())
	mux.Handle("/logout", authSvc.LogoutHandler())
	mux.Handle("/", dashboard.Handler())
	return mux, nil
}
