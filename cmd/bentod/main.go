// Command bentod is the Bento binary. One binary holds every process as a
// subcommand (SPEC section 4): the control plane (serve), the HTTP proxy
// (proxy), and the SSH frontend (sshd), plus the operator commands from
// SPEC section 15.
package main

import (
	"flag"
	"fmt"
	"os"
)

const defaultConfigPath = "/etc/bento/bento.toml"

type command struct {
	name    string
	summary string
	run     func(configPath string, args []string) error
}

var commands = []command{
	{"serve", "run the control plane: database, policy, dashboard", runServe},
	{"proxy", "run the HTTP proxy on port 443 and ports 3000-9999", runProxy},
	{"sshd", "run the SSH frontend on port 22", runSSHD},
	{"fetch-images", "download, verify, and store allowlisted images", runFetchImages},
	{"reconcile", "report disagreements between libvirt and the database", runReconcile},
	{"dump-db", "write a consistent database copy with the backup API", runDumpDB},
	{"images", "list images, current checksums, and stale instance counts", runImages},
}

func main() {
	os.Exit(run(os.Args[1:]))
}

func run(args []string) int {
	fs := flag.NewFlagSet("bentod", flag.ContinueOnError)
	fs.SetOutput(os.Stderr)
	configPath := fs.String("config", defaultConfigPath, "path to the bento configuration file")
	fs.Usage = func() { usage(fs) }
	if err := fs.Parse(args); err != nil {
		return 2
	}
	rest := fs.Args()
	if len(rest) == 0 {
		usage(fs)
		return 2
	}
	name, cmdArgs := rest[0], rest[1:]
	for _, cmd := range commands {
		if cmd.name != name {
			continue
		}
		if err := cmd.run(*configPath, cmdArgs); err != nil {
			fmt.Fprintf(os.Stderr, "bentod %s: %v\n", name, err)
			return 1
		}
		return 0
	}
	fmt.Fprintf(os.Stderr, "bentod: unknown command %q\n\n", name)
	usage(fs)
	return 2
}

func usage(fs *flag.FlagSet) {
	fmt.Fprintln(os.Stderr, "Usage: bentod [flags] <command> [arguments]")
	fmt.Fprintln(os.Stderr, "\nCommands:")
	for _, cmd := range commands {
		fmt.Fprintf(os.Stderr, "  %-14s %s\n", cmd.name, cmd.summary)
	}
	fmt.Fprintln(os.Stderr, "\nFlags:")
	fs.PrintDefaults()
}
