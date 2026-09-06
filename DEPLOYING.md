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
needs no Node runtime. The same build produces
`target/release/bento-monitor`, the terminal screen over the units
(section 6); the host needs it only if you want it.

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

### The runner port, when there is more than one machine

A deployment that runs guests on more than one machine adds a runner port
on each of them. **Bento trusts the network that port sits on.** It runs
no certificate authority and authenticates nothing between the controller
and its runners, because a protected LAN or a routed VPN has already
decided who can reach the port (MULTI-NODE 11.1).

That makes two rules absolute:

* Put every Bento machine on a **protected LAN, or a routed VPN** such as
  WireGuard or Tailscale. Never expose a runner port to the public
  internet, and never run Bento on a network you share with people you do
  not trust.
* Bind the runner listener to the **underlay address**, never to a
  wildcard. Bento denies every guest prefix all management addresses, so a
  guest cannot reach it; a wildcard bind would undo that.

Guests are not trusted and never were. The trust here is in your own
network, not in the virtual machines running on it.

### The host firewall

**Do this before you give a second machine a slot.** A host firewall on a
Bento machine has to let guest traffic be forwarded onto the user
bridges. Bento cannot arrange that from its own rules.

nftables runs every base chain at a hook. Bento's `accept` ends only its
own chain; the packet still meets the next table's, and one `drop` or
`reject` anywhere is final. So a host firewall that rejects forwarded
traffic overrides Bento, whatever Bento's table says.

This costs nothing on one machine, because nothing is forwarded from off
the machine. It matters the moment a guest on one machine has to reach a
guest on another: the packet arrives on the underlay interface and has to
be forwarded onto a user bridge. A firewall that rejects it produces the
same signature as a missing route — the guest sees nothing, and every
Bento rule still looks right.

On a Fedora or RHEL machine running firewalld, give the bridges and the
guest range a zone that accepts, on **every** Bento machine:

```
firewall-cmd --permanent --new-zone=bento
firewall-cmd --permanent --zone=bento --set-target=ACCEPT
firewall-cmd --permanent --zone=bento --add-source=10.100.0.0/16
firewall-cmd --permanent --zone=bento --add-interface=bento0
firewall-cmd --permanent --zone=bento --add-interface=bento1
firewall-cmd --reload
```

Use your own `private_range` in place of `10.100.0.0/16`, and name every
user bridge you have. Add the bridge of each new user as you create one;
`ip -br addr | grep bento` lists them.

The source line is the one that matters. A zone holding only the
interfaces governs traffic *leaving* those bridges, and firewalld judges
a forwarded packet by where it came *from*. Adding the guest range as a
source is what lets a guest packet arriving over the underlay be
forwarded onto a bridge. It is also the durable half: a source rule names
no interface, so it keeps working when libvirt destroys and recreates a
bridge, which drops that bridge out of the zone until the next reload.

This does not widen what a guest may reach. Bento's own table still
carries the whole policy: it permits traffic only between two addresses
of the same user, and it permits a frontend on another machine only to an
instance's published ports (MULTI-NODE 8.4). The firewalld change stops
the host firewall from pre-empting that decision; Bento still makes it.

On a machine whose network is already protected, turning the host
firewall off is the other answer, and it leaves Bento's table as the one
policy for guest traffic:

```
systemctl disable --now firewalld
```

That is the same trust decision as section 2: the network is the
boundary. It does not widen what a guest may reach, because Bento's table
still carries the whole guest policy.

On a machine with no host firewall, there is nothing to do. `bentod
runner` names any other table it finds filtering the forward hook, once
the machine has a slot another machine routes to:

```
another nftables table filters the forward hook; Bento cannot accept
what it rejects  tables="inet firewalld filter_FORWARD"
```

That message is advice, not a fault. It appears on any machine running
firewalld, including one that is configured correctly.

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

Four units, one per process (SPEC 4). `bentod-serve` owns the database;
start it first. The fourth, `bentod-runner`, is only needed once a
deployment runs guests on more than one machine; see below.

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

### bentod-runner, on a machine that holds guests

`bentod-runner` is the service the controller calls to act on one
machine's libvirt (MULTI-NODE 11). A single-host deployment does not need
it, and nothing breaks if it never runs.

**Which units go where.** The controller machine runs all four: it holds
the database, the proxy, and the SSH frontend, and it also holds guests,
so it runs a runner service of its own (MULTI-NODE 19). A machine that
only holds guests runs `bentod-runner` **alone**. Do not enable
`bentod-serve`, `bentod-proxy`, or `bentod-sshd` there: one deployment has
one control plane, and a second `serve` against a second database would
be a second Bento.

`bento-monitor` writes all four unit files. Enabling them is per unit on
its Services tab, so a runner-only machine enables only the one.

```ini
[Unit]
Description=Bento runner service
After=network-online.target virtqemud.socket virtnetworkd.socket
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/bentod -config /etc/bento/runner.toml runner
Restart=on-failure
RestartSec=5s

[Install]
WantedBy=multi-user.target
```

Its configuration needs the `[runner]` section from section 2, and the
`listen` address must be that machine's own underlay address. A wildcard
is refused at startup, because it would put the management port on the
user bridges.

### Adding a machine to the fleet

The order matters. The new machine is prepared first, and the controller
is told about it last, so the controller never calls an address that
answers with something unexpected.

**On the new machine**, run `bento-monitor` and work down the Install
tab, exactly as for a first host:

1. Build and install the binary.
2. Write the configuration. Set `[runner] listen` to this machine's own
   underlay address, and `fence_db` to a path on local disk.
3. Create the directories.
4. Install the unit files.

Then, on the Services tab, enable and start **`bentod-runner` only**.
Leave `bentod-serve`, `bentod-proxy`, and `bentod-sshd` alone: one
deployment has one control plane.

Check the log says what you expect:

```
runner listening addr=10.0.0.97:10443 machine_id=167eeb68... accepted_epoch=0
```

The machine ID is that machine's `/etc/machine-id`. It is the identity
Bento keys the host row on, so a rename never makes a second row.

**On the controller**, add the machine to the configuration:

```toml
[[runners]]
name = "tsukasa"
endpoint = "http://10.0.0.97:10443"
```

Restart `bentod-serve`. It creates the host row, calls the endpoint, and
learns the machine ID from the first answer. Nothing needs an enrollment
secret: the network is the trust boundary (MULTI-NODE 11.1).

**Give it a slot.** A machine with no slot owns no addresses and takes no
instances (MULTI-NODE 7.2). Slot ownership is deployment-wide: the runner
that owns slot 1 owns slot 1 of every user's `/24`.

```
bentod slots                     # what the division is now
bentod slots set-prefix 25       # divide each user /24 into two halves
bentod slots give 1 tsukasa      # give the second half to the new machine
bentod slots plan tsukasa        # what it will be told, before it is told
```

Subdivision moves nothing. Each old slot splits into children that stay
with their old owner, and every guest keeps its address and its `/24`
configuration (MULTI-NODE 17.1). A guest treats the whole user network as
on-link and asks for a remote address by ARP; the machine it is on
answers and routes the packet (MULTI-NODE 8.1). Nothing inside a guest
changes, and nothing needs restarting.

`bentod slots plan` prints the routes, the proxy ARP settings, and the
firewall a machine will be given, without applying any of it. Read it
before a prefix change to see what the change will do.

The controller installs the network on its next poll, at most 30 seconds
later. Check both machines:

```
ip -4 route | grep 10.100        # a route for each slot another machine owns
```

**Wait for the image sync.** The new machine fetches the current version
of every allowlisted image. Creating instances is refused until every
machine holds the same current version, so `base_checksum` means one
thing across the deployment. `bentod images` shows which machine holds
what, and the refusal names the machine that is not ready yet:

```
the fleet is still fetching debian-13: tsukasa is not ready
```

This clears by itself. A large image on a slow link takes a while.

> **A machine keeps every version its own guests need.** An image
> version is the qcow2 backing file of every overlay built from it, so
> Bento never replaces one; a newer build lands beside it (SPEC 5.1).
> Pulling a new version with `bentod sync-images` is safe with guests
> running.

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

### bento-monitor, the terminal screen

Everything in sections 4 and 6 also has a screen. `make build` produces
`target/release/bento-monitor` beside `bentod` and names both when it
finishes. Run it on the host:

```
sudo bento-monitor                     # -config, -binary, and -source override the paths
```

Run it as root, or as a user who can `sudo` — it adds the `sudo` itself,
per action. **Do not make it setuid.** The monitor's whole method is to
run a command the operator chose, in the operator's terminal: setuid root
would turn `EDITOR`, `-source` (which builds, so `build.rs` runs), and
`-binary` into a local root shell, and it would drop the authentication
and the audit line that `sudo` supplies. Where the password prompt is
unwanted, a sudoers rule that names the `systemctl` commands, or a polkit
rule on `org.freedesktop.systemd1.manage-units`, gives the same relief
without the escalation.

Four screens: **Services** drives the three units (start, stop, restart,
enable, disable, and the journal); **Install** reports each step of
sections 4 and 6 as done, missing, or waiting on an earlier one, and runs
the ones that are missing; **Config** shows what `/etc/bento/bento.toml`
parses to, with the ACME and OIDC secrets reported as set or missing but
never printed, and runs `fetch-images`, `images`, and `reconcile`;
**Host** shows the SPEC 4.2 requirement checks, processor, memory, swap,
free space on the image and storage directories, and the libvirt domains.

The monitor is a shim, not a second control plane. It holds no state, it
starts no process of its own, and it changes nothing quietly: an action
first shows the exact command, and the command then runs in your own
terminal. So `sudo` can still ask for a password, `journalctl -f` scrolls
as it always does, and what the monitor did is what you would have typed.

Read-only from the start: with no configuration and no units installed,
every screen still draws, and the Install tab is the list of what is
missing.

## 7. Users, capacity, and the dashboard

A user signs in to the dashboard through OIDC; the first such login
creates the account and allocates its /24 and libvirt network. To use
the command line, they then run `ssh bento.example.org`, open the link
it prints, and confirm the fingerprint. The same flow adds a second key
later — a laptop, a phone — from an already signed-in browser.

**There is no per-user quota, and nothing to grant.** An account can use
whatever the host still has. The only ceiling is the host itself
(SPEC 6.1). Bento refuses a create or a resize in two cases. The first
is when the memory of every instance together would pass the host memory
times `overcommit_ratio`. The second is when the virtual disk of every
instance together would pass the size of the storage volume. Bento sets
no ceiling on vCPU.

`bentod serve` reads both figures once at startup and logs them:

```
host capacity: the ceiling on create and resize (SPEC 6.1)
  memory_mib=65536 disk_gib=900 overcommit_ratio=1
```

Two consequences are easy to miss:

- The sums cover **every** instance on the host, so the machines of one
  user take room from another user. A refusal names the resource, the
  ceiling, and what the host already holds.
- The disk figure is the whole filesystem that holds `storage_dir`. If
  that directory sits on the root filesystem, the ceiling is the root
  filesystem, not a share of it. Give storage its own volume when that
  matters.

Raise `overcommit_ratio` to fit more memory than the host has, after
reading the two conditions in SPEC 5.3.

### The dashboard charts

The charts read every machine every 30 seconds: `/proc/stat` for
processor time, `/proc/meminfo` for memory, and the storage volume for
disk. The per-instance figures come from libvirt, and the disk figure of
an instance is the real size of its overlay rather than the virtual size
that the capacity check counts.

`bentod serve` reads its own machine through the local libvirt socket
and asks every other machine over the runner endpoint. A machine that
holds guests therefore needs `bentod-runner` running and its endpoint
reachable, or its guests have no charts. The front page shows one card
for each machine, and each card counts only the instances that machine
holds. Capacity belongs to a machine, so the tiles above the table show
a ceiling only when there is one machine to name.

The series live in memory. **A restart of `bentod-serve` empties every
chart**, and they refill over the following hour. Nothing is lost that
was not a picture.

Two figures can be missing rather than wrong:

- A machine that did not answer keeps its card, with the size it last
  reported, and its charts stay flat until it answers again.
- An instance that is not running has no processor or memory reading.
- A guest whose balloon driver never reported has no memory reading, so
  its memory chart stays empty while its processor chart fills. The
  processor figure comes from the host, so it does not need the guest.

A chart with no readings says "No samples yet." A chart drawn from
generated figures carries a "sample data" badge; a deployed `bentod`
never generates them.

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

`bentod restore-db <copy>` puts one back. Stop the three units first: a
restore replaces the whole database and does not coordinate with a
running writer. It copies the current database aside before it replaces
it, to `<db_path>.before-restore-<stamp>`, so a restore of the wrong file
is still recoverable. It then applies any schema migrations the copy has
not had, which is what lets an older backup come back on a newer build.

`bento-monitor` has both on the Config tab: `b` copies the database
beside itself, and `r` offers the newest copy that is there. Take a copy
before an upgrade that carries a schema migration.

## Known operator gaps

One thing still needs a direct database write, because no command
exists:

- setting `oidc_subject` on an existing user

It is a `sqlite3` one-liner against the database. Install `sqlite3`
first if the host lacks it — a host without it needs a throwaway program
instead, which is a great deal more work for one `UPDATE`. Stop
`bentod serve` first, or rely on the WAL busy timeout for a single small
write.
