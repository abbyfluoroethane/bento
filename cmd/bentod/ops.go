package main

// The operator commands of SPEC 15: fetch-images, reconcile, dump-db,
// and images.

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"syscall"
	"text/tabwriter"
	"time"

	"github.com/abbyfluoroethane/bento/internal/images"
)

// runFetchImages syncs the allowlist into the database, then downloads,
// verifies, and stores every entry and garbage-collects unreferenced
// versions (SPEC 5.1).
func runFetchImages(configPath string, _ []string) error {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	a, err := newApp(configPath)
	if err != nil {
		return err
	}
	defer a.close()
	if err := syncImageAllowlist(a); err != nil {
		return err
	}
	return a.imageStore().FetchImages(ctx)
}

// runImages lists the images with their current checksum and how many
// instances hold an older version (SPEC 15).
func runImages(configPath string, _ []string) error {
	a, err := newApp(configPath)
	if err != nil {
		return err
	}
	defer a.close()
	if err := syncImageAllowlist(a); err != nil {
		return err
	}
	report, err := images.Report(context.Background(), reportSource{a.st})
	if err != nil {
		return err
	}
	w := tabwriter.NewWriter(os.Stdout, 0, 4, 2, ' ', 0)
	fmt.Fprintln(w, "IMAGE\tCURRENT CHECKSUM\tOLDER INSTANCES")
	for _, s := range report {
		checksum := s.CurrentChecksum
		if checksum == "" {
			checksum = "(not fetched; run bentod fetch-images)"
		}
		fmt.Fprintf(w, "%s\t%s\t%d\n", s.Name, checksum, s.OlderInstances)
	}
	return w.Flush()
}

// runReconcile prints the disagreement between libvirt and the database
// (SPEC 6.1). It changes nothing; the operator corrects by hand.
func runReconcile(configPath string, _ []string) error {
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
	report, err := mgr.Reconcile(ctx)
	if err != nil {
		return err
	}
	if report.Empty() {
		fmt.Println("libvirt and the database agree")
		return nil
	}
	if len(report.DomainsWithoutRows) > 0 {
		fmt.Println("domains without a database row:")
		for _, d := range report.DomainsWithoutRows {
			fmt.Printf("  %s (%s, %s)\n", d.Name, d.UUID, d.State)
		}
	}
	if len(report.RowsWithoutDomains) > 0 {
		fmt.Println("database rows without a libvirt domain:")
		for _, inst := range report.RowsWithoutDomains {
			fmt.Printf("  %s (%s, desired %s)\n", inst.Name, inst.UUID, inst.DesiredState)
		}
	}
	return nil
}

// runDumpDB writes a consistent database copy with the SQLite backup
// API (SPEC 12.1). A direct file copy of a WAL database is unsafe.
func runDumpDB(configPath string, args []string) error {
	a, err := newApp(configPath)
	if err != nil {
		return err
	}
	defer a.close()
	dest := fmt.Sprintf("bento-%s.db", time.Now().UTC().Format("20060102-150405"))
	if len(args) > 0 {
		dest = args[0]
	}
	if err := a.st.DumpDB(dest); err != nil {
		return err
	}
	fmt.Printf("wrote a consistent copy of %s to %s\n", a.cfg.DBPath, dest)
	return nil
}
