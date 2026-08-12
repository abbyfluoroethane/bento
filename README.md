# Bento

Bento is a self-hosted platform for Linux virtual machines on a single
libvirt/KVM host. A user creates an instance over SSH or from a web
dashboard; Bento gives it a static address on the owner's private /24,
boots it from an operator-allowlisted cloud image with cloud-init, and
publishes it on the internet as `NAME.<your-domain>` (HTTPS through a
wildcard-TLS proxy) and `ssh NAME@<your-domain>` (through an SSH
frontend). One binary, `bentod`, runs everything.

The full system specification is [SPEC.md](SPEC.md). It is authoritative;
read it before changing anything.

## Operator quickstart

**DNS** (SPEC 7.1) — create two records, both pointing at the host:

1. an `A` record for `bento.example.org`
2. an `A` record for `*.bento.example.org`

**Host** — a Linux machine with `/dev/kvm`, `libvirtd` answering on the
local socket at `qemu:///system`, `qemu-img`, `xorriso`, and `nft` on
`PATH`. Run `bentod` as a user in the `libvirt` group. `bentod serve`
refuses to start if a requirement is missing (SPEC 4.2).

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

Users register themselves: `ssh bento.example.org` with an unknown key
starts the registration flow, which allocates their /24 and libvirt
network (SPEC 13). Grant quota with a `quotas` row, and add dashboard
access by setting the user's `oidc_subject` to their OIDC subject. Names
listed in `operators` in the config get the database download.

## Development quickstart

Go 1.26; Node only if you touch the dashboard.

```
make build       # bin/bentod (dashboard assets embedded from web/dist)
make check       # go vet ./... && go test ./...
make dashboard   # rebuild web/dist after changing web/src (npm ci && npm run build)
```

Everything host-touching (libvirt RPC, qemu-img, xorriso, nft, /dev/kvm)
sits behind small interfaces with in-memory fakes, so the full test
suite runs on any OS, including macOS.

- `cmd/bentod` — subcommand dispatch and the wiring of all packages.
- `internal/types` — shared domain types (SPEC 11-12).
- `internal/config` — TOML operator configuration; see `bento.example.toml`.
- `internal/store` — SQLite schema and persistence (SPEC 12).
- `internal/hypervisor`, `internal/images`, `internal/cloudinit`,
  `internal/network`, `internal/lifecycle` — host-side machinery
  (SPEC 5, 6, 11).
- `internal/sshfront`, `internal/cli` — SSH frontend and command line
  interface (SPEC 10, 15).
- `internal/proxy`, `internal/tlscert` — HTTP proxy and wildcard TLS
  (SPEC 8, 9).
- `internal/auth`, `internal/api`, `internal/dashboard`, `web/` —
  identity, API, and dashboard (SPEC 13, 14).

## Status

Built to SPEC v0.9. Every package has unit tests against fakes and the
whole suite passes, but the system has **not** been run against a live
libvirtd yet — the libvirt RPC calls, the nftables ruleset, the ACME
issuance, and real guest boots are exercised only through their fakes.
Known gaps and deviations:

- `console` (serial console attach) is not wired; it returns a clear
  error. Use `ssh NAME@<domain>` instead.
- `rename` requires the instance to be stopped: the libvirt domain
  carries the name and is redefined under the new one.
- The three processes share the SQLite database over WAL from one host.
  SPEC 4 makes the control plane the only writer; in this build the SSH
  frontend also writes (CLI commands, registration). Single-host WAL
  with a busy timeout serializes them.
- Dashboard sessions live in control-plane memory; a `serve` restart
  logs dashboard users out (API tokens are unaffected). The proxy
  checks sessions by forwarding credentials to the control plane.
- OIDC accounts link by `oidc_subject`; the SSH registration flow
  cannot know it, so the operator sets it once per user.
