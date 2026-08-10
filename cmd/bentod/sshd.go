package main

// The SSH frontend (SPEC 4, 10, 15): public key authentication,
// instance forwarding, the registration flow, and the command line
// interface.

import (
	"context"
	"errors"
	"fmt"
	"net"
	"os"
	"os/signal"
	"syscall"

	gossh "golang.org/x/crypto/ssh"

	"github.com/abbyfluoroethane/bento/internal/cli"
	"github.com/abbyfluoroethane/bento/internal/lifecycle"
	"github.com/abbyfluoroethane/bento/internal/network"
	"github.com/abbyfluoroethane/bento/internal/sshfront"
)

func runSSHD(configPath string, _ []string) error {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	a, err := newApp(configPath)
	if err != nil {
		return err
	}
	defer a.close()

	hyp, err := a.connectLibvirt()
	if err != nil {
		return err
	}
	defer hyp.Close()

	mgr, err := a.manager(hyp)
	if err != nil {
		return err
	}

	// One host key for every connection: a rename or a name reuse never
	// produces a known_hosts warning (SPEC 10).
	hostKey, err := ensureKey(keyPath(a, hostKeyFile), "bento-host")
	if err != nil {
		return fmt.Errorf("host key: %w", err)
	}
	frontendKey, err := ensureKey(keyPath(a, frontendKeyFile), "bento-frontend")
	if err != nil {
		return fmt.Errorf("frontend key: %w", err)
	}
	frontendPub := authorizedKeyLine(frontendKey.PublicKey(), "bento-frontend")

	hostname, _ := os.Hostname()
	host, err := a.st.EnsureHost(hostname, a.cfg.LibvirtURI)
	if err != nil {
		return fmt.Errorf("hosts row: %w", err)
	}

	// The sshd process applies its own nftables reloads: SPEC 6.3
	// reloads the whole table on every change, and a CLI `new`, `rm`,
	// `port`, or `visibility` must not wait for the serve process's
	// convergence tick. The table is rebuilt from the shared database,
	// so both processes always apply the same content.
	fw := &firewall{st: a.st, plan: a.plan, applier: network.NFTApplier{}, log: a.log}

	cliRunner := cli.New(a.st, &cliBackend{backend{
		m:           mgr,
		st:          a.st,
		hostID:      host.ID,
		frontendKey: frontendPub,
		firewall:    fw,
	}}, cli.Options{
		Domain:           a.cfg.BaseDomain,
		DefaultImage:     defaultImage(a.cfg),
		DefaultVCPU:      a.cfg.Defaults.VCPU,
		DefaultMemoryMiB: a.cfg.Defaults.MemoryMiB,
		DefaultDiskGiB:   a.cfg.Defaults.DiskGiB,
		NameCooldown:     a.cfg.NameCooldown.Std(),
	})

	srv := &sshfront.Server{
		Keys:      a.st,
		Instances: a.st,
		Starter:   starter{hyp},
		CLI:       cliRunner,
		Registrar: &registrar{st: a.st, plan: a.plan, networks: hyp, fw: fw, log: a.log},
		HostKey:   hostKey,
		GuestUser: lifecycle.GuestUser,
		GuestAuth: []gossh.AuthMethod{gossh.PublicKeys(frontendKey)},
	}

	l, err := net.Listen("tcp", a.cfg.Listen.SSH)
	if err != nil {
		return err
	}
	go func() {
		<-ctx.Done()
		l.Close()
	}()
	a.log.Info("ssh frontend listening", "addr", a.cfg.Listen.SSH, "domain", a.cfg.BaseDomain)
	if err := srv.Serve(l); err != nil && ctx.Err() == nil && !errors.Is(err, net.ErrClosed) {
		return err
	}
	return nil
}
