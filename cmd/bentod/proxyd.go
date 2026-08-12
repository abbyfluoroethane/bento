package main

// The HTTP proxy (SPEC 4, 9): TLS termination with the wildcard
// certificate, hostname routing to instances, and the base domain
// forwarded to the control plane. The proxy process only reads the
// database; sessions are checked against the control plane over HTTP.

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"github.com/abbyfluoroethane/bento/internal/proxy"
	"github.com/abbyfluoroethane/bento/internal/tlscert"
)

func runProxy(configPath string, _ []string) error {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	a, err := newApp(configPath)
	if err != nil {
		return err
	}
	defer a.close()

	if a.cfg.ACME.CloudflareToken == "" {
		return errors.New("acme.cloudflare_token is required: the wildcard certificate needs the DNS-01 challenge (SPEC 8)")
	}
	tlsm, err := tlscert.New(tlscert.Config{
		BaseDomain: a.cfg.BaseDomain,
		Email:      a.cfg.ACME.Email,
		Provider:   tlscert.Cloudflare(a.cfg.ACME.CloudflareToken),
		StorageDir: filepath.Join(filepath.Dir(a.cfg.DBPath), "acme"),
		CA:         a.cfg.ACME.Directory,
	})
	if err != nil {
		return err
	}
	defer tlsm.Close()
	a.log.Info("obtaining the wildcard certificate", "domains", tlsm.Domains())
	if err := tlsm.ManageSync(ctx); err != nil {
		return fmt.Errorf("certificate: %w", err)
	}

	control, err := url.Parse(controlURL(a.cfg.Listen.HTTP))
	if err != nil {
		return err
	}
	controlProxy := httputil.NewSingleHostReverseProxy(control)

	sessions := &remoteSession{
		base:   control.String(),
		client: &http.Client{Timeout: 5 * time.Second},
	}
	src := proxySource{a.st}
	p, err := proxy.New(a.cfg.BaseDomain, src, sessions, controlProxy,
		proxy.WithLastSeen(src), // SPEC 12: an HTTP request updates last_seen_at
		proxy.WithPorts(mainPort(a.cfg.Listen.HTTPS), a.cfg.Listen.ProxyPortMin, a.cfg.Listen.ProxyPortMax))
	if err != nil {
		return err
	}
	ports := p.Ports()
	a.log.Info("proxy listening", "bind", bindHost(a.cfg.Listen.HTTPS),
		"main_port", ports[0],
		"high_ports", fmt.Sprintf("%d-%d", ports[1], ports[len(ports)-1]),
		"control", control.String())
	return p.Serve(ctx, bindHost(a.cfg.Listen.HTTPS), tlsm.TLSConfig(), nil)
}
