# Bento

Bento is a self-hosted platform for Linux virtual machines on a single
libvirt/KVM host. A user creates an instance over SSH or from a web
dashboard; Bento gives it a static address on the owner's private /24,
boots it from an operator-allowlisted cloud image with cloud-init, and
publishes it on the internet as `NAME.<your-domain>` (HTTPS through a
wildcard-TLS proxy) and `ssh NAME@<your-domain>` (through an SSH
frontend). One binary, `bentod`, runs everything.

The full system specification is [SPEC.md](SPEC.md). It is authoritative;
read it before changing anything. [MULTI-NODE.md](MULTI-NODE.md) is the
proposed, not-yet-implemented design for more than one VM runner.
[DEPLOYING.md](DEPLOYING.md) is the runbook for bringing a host up, with the
traps the quickstart below leaves out.

## Operator quickstart

**DNS** (SPEC 7.1) — create two records, both pointing at the host:

1. an `A` record for `bento.example.org`
2. an `A` record for `*.bento.example.org`

**Host** — a Linux machine with `/dev/kvm`, `libvirtd` answering on the
local socket at `qemu:///system`, `qemu-img`, `xorriso`, and `nft` on
`PATH`. Run `bentod` as a user in the `libvirt` group. `bentod serve`
refuses to start if a requirement is missing (SPEC 4.2).
Bootc OCI sources additionally require rootful Podman; Bento runs the
configured, digest-pinned image-builder container privileged to produce
qcow2 disks. `serve` checks Podman and its writable container storage when
an OCI source is configured.

**Config** — copy [`bento.example.toml`](bento.example.toml) to
`/etc/bento/bento.toml` and set at least `base_domain`, the `[acme]`
Cloudflare token (the wildcard certificate needs DNS-01), the `[oidc]`
provider for the dashboard, and one `[[images]]` entry. Then:

```
bentod fetch-images       # download and verify the image allowlist
```

**Three processes** (SPEC 4) — run each under systemd (a simple unit
with `ExecStart=/usr/local/bin/bentod <cmd>`, `Restart=on-failure`,
`After=libvirtd.service` is enough):

```
bentod serve    # control plane: database, policy, restore, dashboard (port 10080)
bentod proxy    # HTTPS proxy: port 443 and ports 3000-9999
bentod sshd     # SSH frontend and CLI: port 22
```

`serve` owns the database and prints its path at startup; back it up
with `bentod dump-db` (never a raw file copy — WAL makes that unsafe)
together with the image and storage directories (SPEC 12.1). The other
operator commands are `bentod reconcile` (prints libvirt/database
disagreements, changes nothing) and `bentod images`.

**`bento-monitor`** — the same work on a screen: install the binary, the
directories, the configuration, and the three units; start, stop,
restart, enable, and disable them; read the configuration and the
journal; and watch the host and the libvirt domains. It is a shim over
`systemctl`, so it shows each command before it runs and then runs it in
your own terminal, where `sudo` can still ask for a password. See
`DEPLOYING.md` section 6.

Users sign themselves up through OIDC: the first login for an identity
your provider authenticates creates the account and allocates its /24
and libvirt network (SPEC 13). To use the command line, they then run
`ssh bento.example.org` with an unknown key, open the three-minute link
it prints, and confirm the fingerprint shown. Set `allow_signup = false`
under `[oidc]` to freeze the user list. Grant quota with a `quotas` row.
Names listed in `operators` in the config get the database download.
They can also append bootc-compatible OCI OS images while Bento is
running, either from the Images dashboard or over SSH:

```
ssh bento.example.org images add fedora-bootc quay.io/fedora/fedora-bootc:latest
```

The command pulls and builds immediately, and the durable database row
survives process restarts. A failed first build rolls the row back so the
name can be corrected and retried. An OCI source must be a bootable OS
image with a kernel, `cloud-init` NoCloud support, and `qemu-guest-agent`
baked in; Bento checks these before the privileged build. Ordinary
application containers are not bootable by Bento. Granting operator access
also grants an effective path to host root because operators choose input
to that privileged build.

## Development quickstart

Rust nightly (see `rust-toolchain.toml`); Node only if you touch the
dashboard. The build needs a C compiler for the bundled SQLite, but not
cmake or clang: every TLS user is pinned to the `ring` crypto provider,
never `aws-lc-rs`.

```
make build       # target/release/bentod and target/release/bento-monitor
make monitor     # build and start the terminal screen against this host
make check       # cargo clippy -D warnings && cargo test --workspace
make unit        # the in-process tests only
make e2e         # the end-to-end suite: the real binary, over real sockets
make dashboard   # rebuild web/dist after changing web/src (npm ci && npm run build)
```

`make build` produces two binaries and names both when it finishes:
`bentod`, the deployment, with the dashboard assets embedded from
`web/dist`; and `bento-monitor`, the operator's terminal screen over it.

Everything host-touching (libvirt RPC, qemu-img, xorriso, nft, /dev/kvm)
sits behind small traits with in-memory fakes, so the full test suite
runs anywhere. On top of those, `bentod/tests/e2e/` runs the shipped
binary against a whole deployment in a temporary directory, with a fake
libvirtd on a unix socket. See `TESTING.md` for what is real, what is
substituted, and why. CI runs both tiers on x86_64 and arm64. Each crate declares the narrow trait it needs rather than
depending on the data layer; `bentod` is the one place that knows every
concrete type.

- `bentod` — subcommand dispatch and the wiring of all crates.
- `bento-monitor` — the operator's terminal screen over the systemd
  units, the configuration, and the host.
- `crates/types` — shared domain types (SPEC 11-12).
- `crates/config` — TOML operator configuration; see `bento.example.toml`.
- `crates/store` — SQLite schema and persistence (SPEC 12).
- `crates/hypervisor`, `crates/images`, `crates/cloudinit`,
  `crates/network`, `crates/lifecycle` — host-side machinery
  (SPEC 5, 6, 11).
- `crates/sshfront`, `crates/cli` — SSH frontend and command line
  interface (SPEC 10, 15).
- `crates/proxy`, `crates/tlscert` — HTTP proxy and wildcard TLS
  (SPEC 8, 9).
- `crates/auth`, `crates/api`, `crates/dashboard`, `web/` — identity,
  API, and dashboard (SPEC 13, 14).

`crates/hypervisor` speaks the libvirt XDR RPC protocol directly over the
unix socket rather than binding a C library, so the binary stays
dependency-light. It implements only the procedures the control plane
calls.

## Status

Built to SPEC v0.9. Every crate has unit tests against fakes, and the
end-to-end suite drives the real binary through the whole instance
lifecycle (`TESTING.md`). The system has still **not** been run against a
live libvirtd: no guest ever boots in a test, and the nftables ruleset and
the ACME issuance are exercised only through their fakes. `MULTI-NODE.md`
section 23 lists what a live acceptance run would have to cover.
Known gaps and deviations:

- `console` (serial console attach) is not wired; it returns a clear
  error. Use `ssh NAME@<domain>` instead.
- `rename` requires the instance to be stopped: the libvirt domain
  carries the name and is redefined under the new one.
- The three processes share the SQLite database over WAL from one host.
  SPEC 4 makes the control plane the only writer; in this build the SSH
  frontend also writes (CLI commands, pending key links). Single-host
  WAL with a busy timeout serializes them.
- Dashboard sessions live in control-plane memory; a `serve` restart
  logs dashboard users out (API tokens are unaffected). The proxy
  checks sessions by forwarding credentials to the control plane.
- OIDC is the only thing that creates an account, so a deployment with
  no working provider cannot admit anyone — not even over SSH, since
  the key-linking page needs a session.
