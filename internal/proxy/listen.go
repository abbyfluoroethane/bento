package proxy

import (
	"context"
	"crypto/tls"
	"fmt"
	"net"
	"net/http"
	"strconv"
	"time"
)

// Ports returns every port the proxy listens on: the main port followed
// by the high port range (SPEC 9, 9.1). The defaults are 443 and
// 3000-9999; WithPorts moves them.
func (p *Proxy) Ports() []int {
	ports := make([]int, 0, 1+p.highMax-p.highMin+1)
	ports = append(ports, p.mainPort)
	for port := p.highMin; port <= p.highMax; port++ {
		ports = append(ports, port)
	}
	return ports
}

// ListenFunc binds one address. net.Listen is the production value; tests
// inject fakes so no socket is opened.
type ListenFunc func(network, addr string) (net.Listener, error)

// Serve binds every proxy port on bindHost and serves p on all of them
// with one http.Server. When tlsConf is non-nil each listener terminates
// TLS. Serve blocks until ctx is canceled (returning ctx.Err() after a
// graceful shutdown) or a listener fails.
func (p *Proxy) Serve(ctx context.Context, bindHost string, tlsConf *tls.Config, listen ListenFunc) error {
	if listen == nil {
		listen = net.Listen
	}
	listeners, err := listenAll(bindHost, p.Ports(), listen)
	if err != nil {
		return err
	}

	srv := &http.Server{
		Handler:           p,
		ReadHeaderTimeout: 10 * time.Second,
	}
	errc := make(chan error, len(listeners))
	for _, ln := range listeners {
		if tlsConf != nil {
			ln = tls.NewListener(ln, tlsConf)
		}
		go func(ln net.Listener) { errc <- srv.Serve(ln) }(ln)
	}

	select {
	case <-ctx.Done():
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = srv.Shutdown(shutdownCtx)
		return ctx.Err()
	case err := <-errc:
		_ = srv.Close()
		return err
	}
}

// listenAll binds every port or none: a failure closes what was already
// bound and reports the port that failed.
func listenAll(bindHost string, ports []int, listen ListenFunc) ([]net.Listener, error) {
	listeners := make([]net.Listener, 0, len(ports))
	for _, port := range ports {
		ln, err := listen("tcp", net.JoinHostPort(bindHost, strconv.Itoa(port)))
		if err != nil {
			for _, open := range listeners {
				_ = open.Close()
			}
			return nil, fmt.Errorf("proxy: bind port %d: %w", port, err)
		}
		listeners = append(listeners, ln)
	}
	return listeners, nil
}
