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
mkdir -p /etc/bento /var/lib/bento
install -m 0600 bento.example.toml /etc/bento/bento.toml
$EDITOR /etc/bento/bento.toml
bentod fetch-images
```

`fetch-images` downloads, verifies, and stores each allowlist entry by
checksum. Instances cannot be created until it has run: the allowlist
row alone is not enough, the image needs a fetched version.

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
and `After=bentod-serve.service` added.

```
systemctl enable --now bentod-serve bentod-proxy bentod-sshd
```

> **The SSH frontend is how users register.** An unknown key connecting
> to `bentod sshd` starts the registration flow (SPEC 13), so running it
> on a public address means anyone can create an account. Leave the unit
> stopped until you have decided that is what you want.

Verify:

```
bentod reconcile                       # "libvirt and the database agree"
curl -sI https://bento.example.org/    # dashboard
```

## 7. Users, quota, and the dashboard

A user registers by connecting to the SSH frontend with an unregistered
key and answering two prompts. Registration allocates their /24 and
libvirt network.

**A user with no `quotas` row is unlimited.** `checkQuotaTx` returns
early when the row is missing, so a freshly registered user has no
ceiling until an operator adds one. Add the row as soon as the account
exists.

There is no operator command for this yet — no `bentod quota`, no
`SetQuota` caller — so it is a direct database write:

```sql
INSERT INTO quotas (user_id, max_instances, max_vcpu, max_memory, max_disk)
VALUES (1, 4, 8, 8192, 100);
```

### OIDC

The dashboard authenticates through OIDC; SSH and API tokens do not need
it. With Pocket ID:

1. Create an OIDC client with the callback URL
   **`https://bento.example.org/callback`**, exactly.
2. Put the client ID and secret in `[oidc]` and restart `bentod serve`.
3. Link the account: set `oidc_subject` on the user's row to the
   subject the provider issues.

Step 3 is the awkward one. `oidc_subject` is only writable at user
creation, so it is another direct database write, and the callback
deliberately does not echo the subject back to the browser. The easiest
way to learn it is to attempt a login and read the log:

```
WARN login rejected: no users row carries this OIDC subject subject=<...> email=<...>
```

With Pocket ID the subject is the user's UUID, and it is stable across
clients as long as `subject_types_supported` is `["public"]`.

If a login fails, the log names the branch — missing state cookie, state
mismatch, code exchange failed, ID token invalid, nonce mismatch, or
unmatched subject. If **nothing** is logged, the flow never reached
Bento and the problem is at the provider. Check its logs for a redirect
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

- granting quota (`store.SetQuota` has no caller)
- setting `oidc_subject` on an existing user

Neither host ships `sqlite3` by default, so this means a small program
against `internal/store` or a Python `sqlite3` one-liner. Stop
`bentod serve` first, or rely on the WAL busy timeout for a single small
write.
