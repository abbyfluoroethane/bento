# Deploying Bento

A runbook for bringing Bento up on a single host, written from a real
first deployment: Fedora 44 on aarch64 (Asahi), behind a Caddy that
already owned port 443 for other domains.

[README.md](README.md) is the short version and [SPEC.md](SPEC.md) is
authoritative. This file is the order to do things in, and the traps
that cost time the first time through.

## 1. Host requirements

`bentod serve` refuses to start when one of these is missing (SPEC 4.2):

- `/dev/kvm`
- `libvirtd` answering on a local socket
- `qemu-img`, `xorriso`, and `nft` on `PATH`
- a writable image directory and storage directory

On Fedora, `libvirt-daemon-kvm`, `qemu-img`, `xorriso`, and `nftables`
cover it. KSM is a warning, not a requirement; enable it or run
`ksmtuned` if you want the SPEC 5.4 memory sharing.

Bootc OCI images additionally need rootful Podman. Bento pulls the OCI
image into `/var/lib/containers/storage` and runs the configured
image-builder container with `--privileged`, so budget substantial disk
space in that filesystem and the image directory. Bento refuses an OCI
configuration unless `bootc.builder_image` is pinned with an
`@sha256:<digest>` reference, and pulls that exact builder before each
build. `serve` checks Podman and writable container storage as fatal
requirements when a static OCI entry is configured; without one, failures
of those checks are warnings.

**Treat every name in `operators` as host root.** An operator chooses the
OCI image that Bento supplies to a privileged container with host container
storage mounted read-write. This is an inherent trust boundary of the
image-builder workflow, not ordinary image-view permission.

Bento needs root: it runs `nft`, and by default binds ports 22 and 443.

### Modular libvirt daemons

Bento's default socket is the monolithic
`/var/run/libvirt/libvirt-sock`. Modern Fedora ships the modular
daemons instead and that path does not exist. Point the URI at
`virtqemud` — it forwards the network driver calls Bento makes on to
`virtnetworkd`, so one socket is enough:

```toml
libvirt_uri = "qemu:///system?socket=/run/libvirt/virtqemud-sock"
```

```
systemctl enable --now virtqemud.socket virtnetworkd.socket
```

Check it before going further:

```
virsh -c 'qemu:///system?socket=/run/libvirt/virtqemud-sock' net-list --all
```

### Guest architecture

The guest architecture is the host's — the domains are `type='kvm'`, so
there is no other option. On aarch64 that means **the image allowlist
must list arm64 images**; an amd64 cloud image will not boot. The domain
XML adapts itself (machine `virt`, GICv3, host-passthrough, the seed
CD-ROM on virtio-scsi), so nothing else needs configuring.

The `[[images]]` entry in `bento.example.toml` is the **amd64** Debian
image, because that is the common case. On aarch64 swap the URL for the
`arm64` one of the same build before running `fetch-images` — nothing
checks the architecture of a fetched image, so the mistake surfaces as
an instance that boots to nothing.

### Building the binary

Rust nightly, pinned by `rust-toolchain.toml`; `rustup` picks it up on
its own. `make build` produces `target/release/bentod` with the
dashboard assets embedded, so the deployed artifact is one file and
needs no Node runtime.

The build needs a C compiler for the bundled SQLite. It deliberately
does **not** need cmake or clang: every TLS user is pinned to the `ring`
crypto provider, because `aws-lc-rs` cannot build without them. If a
dependency bump ever drags `aws-lc-rs` back in, the build breaks on a
host like this one — `cargo tree -i aws-lc-rs` names the culprit.

## 2. Ports

Bento binds three things:

| What | Default | Notes |
|------|---------|-------|
| control plane | `127.0.0.1:10080` | must be **outside** the proxy range |
| proxy, main port | `:443` | carries the base domain and instances' default HTTP port |
| proxy, high ports | `:3000-9999` | SPEC 9.1; port N goes to port N on the guest |
| SSH frontend | `:22` | |

**The proxy binds every port of the high range and fails if any one is
taken.** Check the range before the first start:

```
ss -tlnp | awk '{split($4,a,":"); p=a[length(a)]; if (p+0>=3000 && p+0<=9999) print $4, $6}'
```

On a stock Fedora desktop this finds two:

- `cockpit.socket` on 9090 — `systemctl disable --now cockpit.socket`
- LLMNR on 5355 — drop a file in `/etc/systemd/resolved.conf.d/` with
  `[Resolve]` / `LLMNR=no` and restart `systemd-resolved`

Or narrow `proxy_port_min`/`proxy_port_max` to a clear range.

If the host already runs sshd on 22, move it (or move Bento's
`listen.ssh`). Bento's frontend must own the port users will `ssh` to.

## 3. DNS

Two records pointing at the host (SPEC 7.1):

```
bento.example.org      A   <host address>
*.bento.example.org    A   <host address>
```

**Use an A record for the base domain, not a CNAME.** A CNAME at
`bento.example.org` masks the `_acme-challenge.bento.example.org` name
underneath it, and the DNS-01 challenge for the wildcard certificate
then fails. This is easy to miss because the CNAME itself resolves fine.

A second, related trap: the `*.bento.example.org` wildcard also matches
`_acme-challenge.bento.example.org`, so a resolver with the wildcard
cached can answer the propagation check from the wildcard instead of the
TXT record. Point the ACME client's propagation check at public
resolvers if it lets you.

## 4. Configuration

Copy `bento.example.toml` to `/etc/bento/bento.toml`. The minimum is
`base_domain`, one `[[images]]` entry, and — unless you are terminating
TLS elsewhere, see below — the `[acme]` Cloudflare token.

```
mkdir -p /etc/bento /var/lib/bento/storage
install -m 0600 bento.example.toml /etc/bento/bento.toml
$EDITOR /etc/bento/bento.toml
bentod fetch-images
```

**Create `storage_dir` yourself.** Bento checks that the storage and
image directories exist and are writable but does not create them, so a
plain `mkdir /var/lib/bento` leaves `serve` refusing to start:

```
bentod serve: host requirements not met (SPEC 4.2):
  storage directory: No such file or directory (os error 2)
```

The image directory is easy to miss as a trap because `fetch-images`
creates it on the way past; nothing does the same for storage.

`fetch-images` downloads, verifies, and stores each allowlist entry by
checksum. Instances cannot be created until it has run: the allowlist
row alone is not enough, the image needs a fetched version.
`bentod images` lists what is stored, with the current checksum of each
allowlist entry and how many instances still run an older one.

### Bootc OCI images

A static allowlist entry uses `oci` in place of `url`:

```toml
[bootc]
builder_image = "ghcr.io/osbuild/image-builder-cli@sha256:<digest>"
rootfs = "ext4"
container_storage = "/var/lib/containers/storage"
build_timeout = "30m"

[[images]]
name = "fedora-bootc"
oci = "quay.io/fedora/fedora-bootc:latest"
```

`bentod fetch-images` accepts registry references (not local Podman
transports), pulls the source, resolves its registry digest, and converts it
to qcow2. A moving tag is rebuilt only when that source digest changes. The
output then follows the same content-addressed storage and overlay path as
downloaded qcow2 images. OCI builds are serialized across Bento processes
because Podman and image-builder share rootful container storage.

Names in `operators` may append an OCI entry without editing TOML or
restarting a process. Use the Images dashboard or:

```
ssh bento.example.org images add fedora-bootc quay.io/fedora/fedora-bootc:latest
```

The request waits for the build, which can take several minutes. Closing the
SSH session or browser does not cancel the server-side task; each Podman
operation is bounded by `bootc.build_timeout`. A failed first build removes
the new allowlist row, so the same name can be corrected and retried.
Reusing a successfully built name for a different source is rejected.

Only bootc-compatible operating-system images are accepted. They must
contain a kernel plus `cloud-init` with the NoCloud data source and must
bake in `qemu-guest-agent`; Bento cannot install packages into immutable
`/usr` during first boot. Before invoking privileged image-builder, Bento
runs the source without privileges or host mounts and checks for those
files. This catches the common contract errors, but it cannot prove that a
guest will boot correctly. Ordinary OCI application images do not satisfy
this contract.

## 5. Behind an existing TLS terminator

SPEC 8 has the proxy obtain the wildcard certificate itself and own port
443. If something else already owns 443 on this host — a Caddy or nginx
serving other domains — set:

```toml
[listen]
https = "127.0.0.1:10443"   # private: these listeners have no TLS
tls   = "off"
```

The proxy then skips ACME and speaks plain HTTP, and the front proxy
owns the one certificate. Routing is unaffected: the proxy reads the
hostname from SNI when there is one and the `Host` header otherwise,
which is what a forwarded request carries.

The port in `listen.https` becomes the proxy's main port, so pick one
outside the high range. A matching Caddy site:

```caddy
*.bento.example.org, bento.example.org {
	tls {
		dns cloudflare {env.CF_API_TOKEN}
		resolvers 1.1.1.1 9.9.9.9
	}

	reverse_proxy 127.0.0.1:10443
}
```

Two things about that block:

- A wildcard certificate covers **one** label. `*.example.org` does not
  cover `*.bento.example.org`; the site needs its own certificate.
- The `resolvers` line is the wildcard-shadowing fix from section 3.
  Without it the DNS-01 check times out with "timed out waiting for
  record to fully propagate" even though the TXT record is correct at
  the authoritative servers. Verify with
  `dig +short TXT _acme-challenge.bento.example.org @1.1.1.1` against a
  public resolver and against the local one — a difference is the tell.

The high ports stay on whatever `listen.https` binds. On loopback they
are not reachable from the internet, and a Caddyfile site address cannot
express a port range, so publishing them through Caddy means either one
site block per port (narrow the range first) or giving Bento a
public bind and its own certificate.

### Reaching instances under a second domain

A front proxy can also publish instances under a domain that is not
`base_domain` — a short alias zone, say, with `<service>.example.net` a
CNAME onto `<service>.bento.example.org`:

```
git.example.net   CNAME   git.bento.example.org
wiki.example.net  CNAME   outline.bento.example.org
```

Both names already resolve to the host, so the requests arrive at the
front proxy either way. Two things are needed to serve them.

The alias domain needs **its own certificate**, for the same
one-label reason as above: `*.bento.example.org` does not cover
`*.example.net`. If the alias zone sits in a different DNS account, that
is a second API token, not the one in `[acme]`.

The alias name must then be **rewritten to the `base_domain` name it
stands for** before the request is forwarded. The proxy reads the
hostname from SNI when there is one and the `Host` header otherwise, and
the hop to `127.0.0.1:10443` is plaintext — so there is no SNI, and it
routes on `Host` alone. A `Host` outside `base_domain` fails the suffix
strip and answers 404:

```caddy
*.example.net, example.net {
	tls {
		dns cloudflare {env.ALIAS_CF_API_TOKEN}
	}

	map {host} {bento_host} {
		wiki.example.net           outline.bento.example.org
		~^([^.]+)\.example\.net$   "${1}.bento.example.org"
		default                    ""
	}

	@instance vars_regexp {bento_host} .
	handle @instance {
		reverse_proxy 127.0.0.1:10443 {
			header_up Host {bento_host}
		}
	}

	handle {
		abort
	}
}
```

The regex row carries every alias whose label already matches the
instance name; spell out the ones that differ above it, since `map`
takes the first matching row. The empty `default` drops the apex and
anything more than one label deep — the regex is deliberately
`[^.]+`, because the proxy rejects a name containing a dot before it
ever reaches the instance lookup.

That 404 is the trap worth knowing about. It is the same 404 that a
missing name, a released name, and an instance with visibility off all
return, byte for byte and by design (SPEC 9.2) — so a missing `Host`
rewrite reads as "the instance does not exist" rather than as a routing
mistake. `curl -sI --resolve <alias>:443:<host address> https://<alias>/`
against both the alias and the `base_domain` name is the quick tell: the
`.bento` name answers and the alias 404s.

One consequence to expect. The proxy forwards the rewritten name to the
guest, in both `Host` and `X-Forwarded-Host`, so the application inside
the instance sees `<service>.bento.example.org` and never learns the
alias. An application configured with a canonical URL will redirect
visitors from the alias back to that name. Nothing in the proxy can fix
this — the guest's own configuration has to name the alias.

## 6. Running it

Three units, one per process (SPEC 4). `bentod-serve` owns the database;
start it first.

```ini
[Unit]
Description=Bento control plane
After=network-online.target virtqemud.socket virtnetworkd.socket
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/bentod serve
Restart=on-failure
RestartSec=5s

[Install]
WantedBy=multi-user.target
```

The `proxy` and `sshd` units are the same with the subcommand changed
and `After=bentod-serve.service` added — but the proxy needs one more
line:

```ini
LimitNOFILE=65536
```

**Without it the proxy dies partway through binding the high range**:

```
bentod proxy: proxy: bind port 4011: Too many open files (os error 24)
```

The range is one listening descriptor per port — about 7000 of them —
and systemd hands a service a soft `RLIMIT_NOFILE` of 1024 even where
the hard limit is 524288. The failing port number moves around, which
makes this look like the "some other process holds a port" failure from
section 2; the `Too many open files` text is what tells the two apart.
Check the limit a unit will actually get with
`systemctl show bentod-proxy -p LimitNOFILESoft`.

This one is new in the Rust build and is worth knowing if you deployed
the Go one: the Go runtime raised its own soft limit to the hard limit
at startup, so the range bound cleanly on a stock unit and no such line
was ever needed. Rust does not do this, so the limit has to be set.

```
systemctl enable --now bentod-serve bentod-proxy bentod-sshd
```

> **The SSH frontend creates nothing.** An unknown key connecting to
> `bentod sshd` gets a three-minute link to sign in with and nothing
> else — no user row, no /24, no libvirt network (SPEC 13). It is
> designed to answer the public internet.
>
> Who gets an account is decided by your OIDC provider, because a
> verified login for an identity Bento has not seen creates the account.
> Set `allow_signup = false` under `[oidc]` to refuse those logins and
> freeze the user list at whoever already exists.

Verify:

```
bentod reconcile                       # "libvirt and the database agree"
curl -sI https://bento.example.org/    # dashboard
```

## 7. Users, quota, and the dashboard

A user signs in to the dashboard through OIDC; the first such login
creates the account and allocates its /24 and libvirt network. To use
the command line, they then run `ssh bento.example.org`, open the link
it prints, and confirm the fingerprint. The same flow adds a second key
later — a laptop, a phone — from an already signed-in browser.

**A user with no `quotas` row is unlimited.** The quota check returns
early when the row is missing, so a new user has no ceiling until an
operator adds one. Add the row as soon as the account exists.

There is no operator command for this yet — no `bentod quota`, no
`SetQuota` caller — so it is a direct database write:

```sql
INSERT INTO quotas (user_id, max_instances, max_vcpu, max_memory, max_disk)
VALUES (1, 4, 8, 8192, 100);
```

### OIDC

OIDC is how accounts are created, so `bentod serve` needs it configured
before anyone can sign in — including over SSH, since the key-linking
page requires a session. API tokens, once minted, do not need it. With
Pocket ID:

1. Create an OIDC client with the callback URL
   **`https://bento.example.org/callback`**, exactly.
2. Put the client ID and secret in `[oidc]` and restart `bentod serve`.
3. Sign in. That is the whole of it — the first login for an identity
   creates the account, records its subject, and allocates its /24.

The account name comes from the provider's `preferred_username`, then
the email's local part, then the display name, reduced to lowercase
letters, digits, and inner hyphens; a name already taken is suffixed
`-2`. Rename with a direct database write if you dislike the result —
but do it before instances exist, because nothing renames the user's
libvirt network with them.

With Pocket ID the subject is the user's UUID, and it is stable across
clients as long as `subject_types_supported` is `["public"]`.

If a login fails, the log names the branch — missing state cookie, state
mismatch, code exchange failed, ID token invalid, nonce mismatch, or
(with `allow_signup = false`) an unmatched subject. If **nothing** is
logged, the flow never reached Bento and the problem is at the provider. Check its logs for a redirect
to its own error page after a successful authentication; with Pocket ID
the usual cause is a client marked group-restricted with no groups in
its allowed list, which refuses every user.

Names listed in `operators` in the config get the operator-only
dashboard controls, such as the database download.

## 8. Backups

`bentod dump-db` writes a consistent copy through the SQLite backup API.
**Never copy the database file directly** — WAL makes that unsafe. Back
it up together with the image and storage directories (SPEC 12.1).

## Known operator gaps

Things that currently need a direct database write, because no command
exists:

- granting quota (`Store::set_quota` has no caller)
- setting `oidc_subject` on an existing user

Both are a `sqlite3` one-liner against the database. Install `sqlite3`
first if the host lacks it — a host without it needs a throwaway program
instead, which is a great deal more work for one `UPDATE`. Stop
`bentod serve` first, or rely on the WAL busy timeout for a single small
write.
