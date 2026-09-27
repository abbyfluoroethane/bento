# Deploying Bento

This runbook tells you how to put Bento on a host. It also gives the
problems that we found during real deployments, and their solutions.

[README.md](README.md) is the short version. [SPEC.md](SPEC.md) is the
authority. This file gives the sequence of steps.

## Tested hosts

| Host | Date | Result |
|------|------|--------|
| Fedora 44, aarch64 (Asahi), behind Caddy | 2026 | Full deployment |
| Fedora 44, x86_64, rootless distrobox on NixOS | 2026-09-27 | Diagnostic run only. Refer to section 9. |

Sections 1 to 8 apply to all hosts. Section 9 gives the x86_64 results.

## 1. Host requirements

### Checks at startup

`bentod serve` does not start if one of these items is missing
(SPEC 4.2):

- `/dev/kvm`
- a libvirt daemon that answers on the local socket
- `qemu-img` and `xorriso` on `PATH`
- an image directory and a storage directory that Bento can write to

On Fedora, the packages `libvirt-daemon-kvm`, `qemu-img`, `xorriso`, and
`nftables` supply these items.

`serve` does not check for `nft` at startup. But `serve` loads its
nftables table before it opens the control-plane port. If `nft` fails,
`serve` stops with this error:

```
bentod serve: nftables: network: nft -f -: exit status: 1: netlink: Error: cache initialization failed: Operation not permitted
```

KSM is not a requirement. If KSM is off, `serve` writes a warning. To
get the memory sharing of SPEC 5.4, enable KSM or run `ksmtuned`.

### Root access

Bento must run as root. A user in the `libvirt` group is not sufficient.
Bento needs root for two operations:

- `nft` needs `CAP_NET_ADMIN` in the network namespace of the host.
- The default ports 22 and 443 need `CAP_NET_BIND_SERVICE`.

### Bootc OCI images

Bootc OCI images also need rootful Podman. Bento pulls the OCI image
into `/var/lib/containers/storage`. Then Bento runs the image-builder
container with `--privileged`. Make sure that this filesystem and the
image directory have much free disk space.

Bento does not accept an OCI configuration if `bootc.builder_image` has
no `@sha256:<digest>` reference. Before each build, Bento pulls that
builder image.

If the configuration has a static OCI entry, the Podman checks are
fatal. If it has no static OCI entry, a failed Podman check gives only a
warning.

**Each name in `operators` has root access to the host.** An operator
selects the OCI image. Bento gives that image to a privileged container
that can write to the host container storage. This is part of the
image-builder design. It is not a usual image permission.

### Modular libvirt daemons

The default socket of Bento is `/var/run/libvirt/libvirt-sock`. The
monolithic `libvirtd` supplies this socket. Current Fedora uses the
modular daemons, and that socket does not exist.

On a host with modular daemons, set the URI to `virtqemud`. `virtqemud`
sends the network calls of Bento to `virtnetworkd`. Thus one socket is
sufficient:

```toml
libvirt_uri = "qemu:///system?socket=/run/libvirt/virtqemud-sock"
```

```
systemctl enable --now virtqemud.socket virtnetworkd.socket
```

Do a check of the socket before you continue:

```
virsh -c 'qemu:///system?socket=/run/libvirt/virtqemud-sock' net-list --all
```

If the host runs the monolithic `libvirtd`, keep the default URI.

### Guest architecture

The guest architecture is the same as the host architecture. The domains
are `type='kvm'`, thus no other architecture is possible. Bento adapts
the domain XML to the architecture:

| Item | x86_64 | aarch64 |
|------|--------|---------|
| Machine type | `q35` | `virt` |
| CPU mode | `host-model` | `host-passthrough` |
| Interrupt controller | APIC | GICv3 |
| Seed CD-ROM bus | SATA | virtio-scsi |

Both use UEFI firmware. An instance with nested virtualization uses
`host-passthrough` on both architectures.

**The images in the allowlist must have the same architecture as the
host.** Nothing checks the architecture of a downloaded image. An
incorrect image gives an instance that does not boot.

The `[[images]]` entry in `bento.example.toml` is the **amd64** Debian
image. On x86_64, use it as it is. On aarch64, change the URL to the
`arm64` image of the same build. Do this before you run `fetch-images`.

### The guest user

cloud-init makes one user in each guest. The name of this user is
`bento`, for all owners. The SSH keys of the owner go to this user. The
user can use `sudo` without a password.

```
ssh bento@10.100.0.2
```

### Guest network

Each user network has libvirt forward mode `open`. Thus libvirt does no
NAT for it. Only the nftables table of Bento gives the guests access to
the internet (the `masquerade` rule).

If the table is not loaded, a guest boots and answers on its address.
But it cannot get to the internet. Then cloud-init waits on
`package_update`, and `qemu-guest-agent` is not installed.

### Building the binaries

`rust-toolchain.toml` sets the toolchain. `rustup` reads this file and
installs the toolchain and its components.

A distribution `cargo` does not read `rust-toolchain.toml`. On Fedora,
the `rust` and `cargo` packages build Bento. But `make check` also needs
the `clippy` and `rustfmt` packages:

```
dnf install rust cargo clippy rustfmt
```

The build needs a C compiler for the bundled SQLite. It does **not**
need cmake or clang. All TLS users use the `ring` crypto provider,
because `aws-lc-rs` needs cmake and clang. If a dependency update adds
`aws-lc-rs` again, the build fails on such a host. To find the
dependency that adds it, run `cargo tree -i aws-lc-rs`.

`make build` makes two binaries:

- `target/release/bentod`. The dashboard assets are in the binary. Thus
  the deployed binary is one file and needs no Node runtime.
- `target/release/bento-monitor`, the terminal screen over the units
  (section 6). The host needs it only if you want to use it.

Both binaries link only to glibc and libgcc.

## 2. Ports

Bento opens these ports:

| Listener | Default | Notes |
|----------|---------|-------|
| Control plane | `127.0.0.1:10080` | Must be **outside** the proxy range |
| Proxy, main port | `:443` | The base domain and the default HTTP port of each instance |
| Proxy, high ports | `:3000-9999` | SPEC 9.1. Port N goes to port N on the guest |
| SSH frontend | `:22` | |

**The proxy opens all ports of the high range. If one port is in use,
the proxy fails.** Do a check of the range before the first start:

```
ss -tlnp | awk '{split($4,a,":"); p=a[length(a)]; if (p+0>=3000 && p+0<=9999) print $4, $6}'
```

On a standard Fedora desktop, this check finds two ports:

- `cockpit.socket` on 9090. Run `systemctl disable --now cockpit.socket`.
- LLMNR on 5355. Put a file in `/etc/systemd/resolved.conf.d/` with
  `[Resolve]` and `LLMNR=no`. Then restart `systemd-resolved`.

Or set `proxy_port_min` and `proxy_port_max` to a smaller range that is
free.

If `sshd` of the host uses port 22, move it or change `listen.ssh`. The
Bento frontend must have the port that users connect to.

### The runner port, for more than one machine

A deployment that runs guests on more than one machine adds a runner
port on each machine. **Bento trusts the network of that port.** Bento
has no certificate authority. It does no authentication between the
controller and its runners. The protected LAN or the routed VPN controls
who can get to the port (MULTI-NODE 11.1).

Thus you must obey two rules:

- Put all Bento machines on a **protected LAN or a routed VPN**, for
  example WireGuard or Tailscale. Do not make a runner port available
  on the public internet. Do not run Bento on a network that you share
  with persons that you do not trust.
- Bind the runner listener to the **underlay address**. Do not bind it
  to a wildcard. Bento blocks all management addresses from all guest
  prefixes, thus a guest cannot get to the runner. A wildcard bind
  removes this protection.

Bento does not trust the guests. The trust is in your network, not in
the virtual machines on it.

### The host firewall

The nftables table of Bento and a host firewall have an effect on each
other in two directions.

**The table of Bento affects the full host.** Its `forward` chain has
`policy drop`. In nftables, a drop in one base chain drops the packet.
Thus Bento drops **all** forwarded traffic on the host that does not
come from a Bento bridge. Examples of traffic that stops:

- NAT for virtual machines on the libvirt `default` network (`virbr0`)
- rootful Podman or Docker networks
- a Tailscale subnet router or exit node

Use a dedicated host for Bento. Do not deploy Bento on a workstation
that has other virtual machines or containers with network access.

**A host firewall can also stop Bento traffic.** Do the steps below
before you give a slot to a second machine. The host firewall of a Bento
machine must let guest traffic go to the user bridges. The rules of
Bento cannot do this.

nftables runs each base chain at a hook. An `accept` in Bento ends only
the chain of Bento. The packet then goes to the chain of the next table.
One `drop` or `reject` in any table is final. Thus a host firewall that
rejects forwarded traffic overrides Bento.

On one machine, this has no effect, because no traffic comes from other
machines. With two machines, a packet from a guest on one machine
arrives on the underlay interface of the other machine. That machine
must forward it to a user bridge. If a firewall rejects the packet, the
result looks like a missing route. The guest receives nothing, and all
Bento rules look correct.

On a Fedora or RHEL machine with firewalld, make a zone that accepts the
bridges and the guest range. Do this on **each** Bento machine:

```
firewall-cmd --permanent --new-zone=bento
firewall-cmd --permanent --zone=bento --set-target=ACCEPT
firewall-cmd --permanent --zone=bento --add-source=10.100.0.0/16
firewall-cmd --permanent --zone=bento --add-interface=bento0
firewall-cmd --permanent --zone=bento --add-interface=bento1
firewall-cmd --reload
```

Replace `10.100.0.0/16` with your `private_range`. Add each user bridge.
When you make a new user, add the bridge of that user. To see the
bridges, run `ip -br addr | grep bento`.

The source line is the important line. firewalld uses the source of a
forwarded packet to select the zone. A zone with only the interfaces
applies to traffic that *leaves* those bridges. The source rule lets a
guest packet from the underlay go to a bridge.

The source rule also stays correct when libvirt destroys and makes a
bridge again. It does not name an interface. A destroyed bridge is not
in the zone until the next reload.

This change does not increase what a guest can get to. The table of
Bento keeps the full guest policy. It permits traffic only between two
addresses of the same user. It permits a frontend on a different machine
only to the published ports of an instance (MULTI-NODE 8.4). The
firewalld change only stops the host firewall from overriding Bento.

On a machine with a protected network, you can also stop the host
firewall. Then the table of Bento is the only policy for guest traffic:

```
systemctl disable --now firewalld
```

This is the same trust decision as for the runner port: the network is
the boundary. It does not increase what a guest can get to.

On a machine with no host firewall, do nothing. When a different machine
routes to a slot on this machine, `bentod runner` names each other table
that filters the forward hook:

```
another nftables table filters the forward hook; Bento cannot accept
what it rejects  tables="inet firewalld filter_FORWARD"
```

This message is advice, not a fault. It shows on each machine with
firewalld, also on a machine with a correct configuration.

## 3. DNS

Make two records that point to the host (SPEC 7.1):

```
bento.example.org      A   <host address>
*.bento.example.org    A   <host address>
```

**Use an A record for the base domain. Do not use a CNAME.** A CNAME at
`bento.example.org` hides the name `_acme-challenge.bento.example.org`.
Then the DNS-01 challenge for the wildcard certificate fails. The CNAME
itself resolves correctly, thus this problem is not easy to see.

There is a related problem. The `*.bento.example.org` wildcard also
matches `_acme-challenge.bento.example.org`. A resolver that has the
wildcard in its cache can answer with the wildcard record, not with the
TXT record. If the ACME client can use a different resolver for the
propagation check, set it to public resolvers.

## 4. Configuration

Copy `bento.example.toml` to `/etc/bento/bento.toml`. You must set
`base_domain` and one `[[images]]` entry. You must also set the `[acme]`
Cloudflare token, unless a different server does the TLS (section 5).

```
mkdir -p /etc/bento /var/lib/bento/storage
install -m 0600 bento.example.toml /etc/bento/bento.toml
$EDITOR /etc/bento/bento.toml
bentod fetch-images
```

**Make the storage directory yourself.** Bento does a check that the
storage directory and the image directory exist. Bento does not make
them. If you make only `/var/lib/bento`, `serve` does not start:

```
bentod serve: host requirements not met (SPEC 4.2):
  storage directory: No such file or directory (os error 2)
```

`fetch-images` makes the image directory. Nothing makes the storage
directory.

`fetch-images` downloads each allowlist entry, verifies it, and keeps it
by checksum. You cannot make an instance before `fetch-images` runs. The
allowlist row is not sufficient: the image must have a downloaded
version. `bentod images` shows the stored images, the current checksum
of each entry, and the number of instances that use an older version.

### Bootc OCI images

A static allowlist entry uses `oci`, not `url`:

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

`bentod fetch-images` accepts registry references. It does not accept
local Podman transports. It pulls the source, finds the registry digest,
and changes the image to qcow2. Bento builds a moving tag again only
when the source digest changes. After the build, the image uses the same
content-addressed storage and overlay path as a downloaded qcow2 image.

Only one OCI build runs at a time, across all Bento processes, because
Podman and image-builder use the same rootful container storage.

A name in `operators` can add an OCI entry while Bento runs. It is not
necessary to edit the TOML file or to restart a process. Use the Images
dashboard or this command:

```
ssh bento.example.org images add fedora-bootc quay.io/fedora/fedora-bootc:latest
```

The command waits for the build. The build can take some minutes. If
you close the SSH session or the browser, the build continues on the
server. `bootc.build_timeout` sets the maximum time of each Podman step.

If the first build of a new name fails, Bento removes the new allowlist
row. Then you can correct the entry and try again with the same name.
Bento does not accept a different source for a name that has a
successful build.

Bento accepts only operating-system images that are compatible with
bootc. The image must contain:

- a kernel
- `cloud-init` with the NoCloud data source
- `qemu-guest-agent`

Bento cannot install packages into the immutable `/usr` at first boot.
Before the privileged build, Bento runs the source with no privileges
and no host mounts, and looks for these files. This check finds the
usual errors. It cannot prove that the guest boots correctly. Usual OCI
application images do not agree with this contract.

## 5. Behind a different TLS server

In SPEC 8, the proxy gets the wildcard certificate itself and uses port
443. On some hosts, a different server (for example Caddy or nginx) uses
port 443 for other domains. On such a host, set:

```toml
[listen]
https = "127.0.0.1:10443"   # private: these listeners have no TLS
tls   = "off"
```

Then the proxy does not use ACME and uses plain HTTP. The front proxy
has the certificate. The routing does not change. The proxy reads the
hostname from SNI if SNI is present. If SNI is not present, it reads
the `Host` header. A forwarded request has the `Host` header.

The port in `listen.https` becomes the main port of the proxy. Select a
port outside the high range. This is a Caddy site for this
configuration:

```caddy
*.bento.example.org, bento.example.org {
	tls {
		dns cloudflare {env.CF_API_TOKEN}
		resolvers 1.1.1.1 9.9.9.9
	}

	reverse_proxy 127.0.0.1:10443
}
```

Two notes about this block:

- A wildcard certificate covers **one** label. `*.example.org` does not
  cover `*.bento.example.org`. The site needs its own certificate.
- The `resolvers` line is the solution to the wildcard problem in
  section 3. If you remove it, the DNS-01 check can fail with "timed
  out waiting for record to fully propagate". This occurs although the
  TXT record is correct on the authoritative servers.

To find this problem, compare a public resolver with the local resolver.
If the results are different, the local resolver uses the wildcard:

```
dig +short TXT _acme-challenge.bento.example.org @1.1.1.1
dig +short TXT _acme-challenge.bento.example.org
```

The high ports stay on the address in `listen.https`. On loopback, they
are not available from the internet. A Caddyfile site address cannot
have a port range. To publish the high ports through Caddy, you need one
site block for each port (make the range small first). Or give Bento a
public address and its own certificate.

### Instances under a second domain

A front proxy can also publish instances under a domain that is not
`base_domain`. For example, a short alias zone, with
`<service>.example.net` as a CNAME to `<service>.bento.example.org`:

```
git.example.net   CNAME   git.bento.example.org
wiki.example.net  CNAME   outline.bento.example.org
```

Both names resolve to the host, thus the requests go to the front proxy.
You must do two more things:

1. Get a **separate certificate** for the alias domain.
   `*.bento.example.org` does not cover `*.example.net` (one label).
   If the alias zone is in a different DNS account, you need a second
   API token.
2. **Change the alias name to its `base_domain` name** before the front
   proxy forwards the request. The connection to `127.0.0.1:10443` is
   plain HTTP, thus it has no SNI. The proxy routes on `Host` only. If
   `Host` is outside `base_domain`, the proxy answers 404.

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

The regex row is for each alias that has the same label as the instance
name. Put the aliases with a different name above it, because `map`
uses the first row that matches. The empty `default` drops the apex
domain and all names with more than one label. The regex is `[^.]+`
because the proxy refuses a name with a dot before the instance lookup.

**Be careful with this 404.** It is the same 404 that the proxy gives
for a missing name, a released name, and an instance with visibility
`off` (SPEC 9.2). The bytes are the same, by design. Thus a missing
`Host` change looks like a missing instance, not like a routing error.

To find this problem, send a request to the alias name and to the
`base_domain` name. If the `base_domain` name answers and the alias
gives 404, the `Host` change is missing:

```
curl -sI --resolve <alias>:443:<host address> https://<alias>/
```

The proxy sends the changed name to the guest in `Host` and in
`X-Forwarded-Host`. Thus the application in the instance sees
`<service>.bento.example.org` and does not see the alias. An application
with a canonical URL sends the visitors from the alias to that name. The
proxy cannot correct this. Set the alias in the configuration of the
application in the guest.

## 6. Running Bento

Bento has four units, one for each process (SPEC 4). `bentod-serve` owns
the database. Start it first. The fourth unit, `bentod-runner`, is
necessary only when a deployment runs guests on more than one machine.

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

The `proxy` and `sshd` units are the same, with a different subcommand
and with `After=bentod-serve.service`. The proxy unit also needs this
line:

```ini
LimitNOFILE=65536
```

**If this line is missing, the proxy stops while it opens the high
range:**

```
bentod proxy: proxy: bind port 4011: Too many open files (os error 24)
```

Each port in the range uses one file descriptor, thus approximately
7000. systemd gives a service a soft `RLIMIT_NOFILE` of 1024. This
occurs also when the hard limit is 524288. The port number in the error
changes each time. Thus the error looks like the "port in use" problem
of section 2. The text `Too many open files` shows the difference.

To see the limit that a unit gets:

```
systemctl show bentod-proxy -p LimitNOFILESoft
```

The old Go build did not need this line. The Go runtime set its soft
limit to the hard limit at startup. Rust does not do this.

```
systemctl enable --now bentod-serve bentod-proxy bentod-sshd
```

### bentod-runner, on a machine with guests

`bentod-runner` is the service that the controller calls to operate the
libvirt of one machine (MULTI-NODE 11). A deployment on one host does
not need it. If it does not run, nothing fails.

**The units for each machine:**

- The controller machine runs all four units. It has the database, the
  proxy, and the SSH frontend. It also has guests, thus it runs its own
  runner service (MULTI-NODE 19).
- A machine with only guests runs `bentod-runner` **alone**. Do not
  enable `bentod-serve`, `bentod-proxy`, or `bentod-sshd` on it. One
  deployment has one control plane. A second `serve` with a second
  database is a second Bento.

`bento-monitor` reads the role from the machine. It writes only the unit
files for that role. Thus a runner-only machine gets only
`bentod-runner`.

The role comes from the configuration. Before the unit-file step, write
the `[runner]` section below. Set `listen` to the underlay address of
this machine. A machine with the loopback default is a controller, and
the step then writes all four units.

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

The configuration of the runner needs the `[runner]` section. The
`listen` address must be the underlay address of that machine. The
runner does not start with a wildcard address, because a wildcard puts
the management port on the user bridges.

### Adding a machine to the fleet

Do the steps in this sequence. Prepare the new machine first. Tell the
controller about it last. Then the controller never calls an address
that gives an unexpected answer.

**On the new machine**, run `bento-monitor`. Do the steps of the Install
tab, as for a first host:

1. Build and install the binaries.
2. Write the configuration. Set `[runner] listen` to the underlay
   address of this machine. Set `fence_db` to a path on a local disk.
   The header then shows `runner`. The Install and Services tabs then
   expect one unit, not four.
3. Make the directories.
4. Install the unit file.
5. Enable the unit at boot.

Then start `bentod-runner` on the Services tab. Do not start
`bentod-serve`, `bentod-proxy`, or `bentod-sshd`. One deployment has one
control plane. The Fleet tab shows `accepted 0` until the controller
connects to the machine for the first time.

Make sure that the log shows the correct values:

```
runner listening addr=10.0.0.97:10443 machine_id=167eeb68... accepted_epoch=0
```

The machine ID is the `/etc/machine-id` of the machine. Bento uses it as
the key of the host row. Thus a new hostname never makes a second row.

**On the controller**, add the machine to the configuration:

```toml
[[runners]]
name = "runner-a"
endpoint = "http://10.0.0.97:10443"
```

Restart `bentod-serve`. It makes the host row and calls the endpoint.
It gets the machine ID from the first answer. No enrollment secret is
necessary, because the network is the trust boundary (MULTI-NODE 11.1).

**Give a slot to the machine.** A machine with no slot has no addresses
and gets no instances (MULTI-NODE 7.2). Slot ownership applies to the
full deployment: the runner that owns slot 1 owns slot 1 of the `/24`
of each user.

```
bentod slots                     # show the current division
bentod slots set-prefix 25       # divide each user /24 into two halves
bentod slots give 1 runner-a     # give the second half to the new machine
bentod slots plan runner-a       # show what the machine gets, before it gets it
```

A subdivision moves nothing. Each old slot divides into child slots
that stay with the old owner. Each guest keeps its address and its
`/24` configuration (MULTI-NODE 17.1). A guest sees the full user network
as on-link and uses ARP for a remote address. The machine of the guest
answers and routes the packet (MULTI-NODE 8.1). Nothing in a guest
changes, and no restart is necessary.

`bentod slots plan` shows the routes, the proxy ARP settings, and the
firewall that a machine will get. It applies nothing. Read it before a
prefix change to see the effect of the change.

The controller installs the network at its next poll, not more than 30
seconds later. Do a check on both machines:

```
ip -4 route | grep 10.100        # one route for each slot of a different machine
```

**Wait for the image sync.** The new machine downloads the current
version of each allowlisted image. Bento refuses to make instances until
all machines have the same current version. Thus `base_checksum` has
one meaning across the deployment. `bentod images` shows the versions
on each machine. The refusal names the machine that is not ready:

```
the fleet is still fetching debian-13: runner-a is not ready
```

This stops automatically. A large image on a slow link can take a long
time.

> **A machine keeps each version that its guests need.** An image
> version is the qcow2 backing file of each overlay made from it. Thus
> Bento never replaces a version. A newer build goes next to it
> (SPEC 5.1). It is safe to get a new version with `bentod sync-images`
> while guests run.

> **The SSH frontend makes nothing.** If an unknown key connects to
> `bentod sshd`, it gets a link to sign in, valid for three minutes. It
> gets nothing more: no user row, no /24, no libvirt network (SPEC 13).
> The frontend is safe on the public internet.
>
> Your OIDC provider decides who gets an account. A verified login for
> an identity that Bento does not know makes a new account. To refuse
> these logins, set `allow_signup = false` under `[oidc]`. Then the user
> list stays as it is.

Verify the deployment:

```
bentod reconcile                       # "libvirt and the database agree"
curl -sI https://bento.example.org/    # dashboard
```

On a libvirt daemon that has other domains, `reconcile` shows each of
them under "domains without a database row". This is not an error.
`reconcile` changes nothing. The restore at startup starts only the
domains that have a database row.

### bento-monitor, the terminal screen

All steps of sections 4 and 6 are also available on a screen.
`make build` makes `target/release/bento-monitor` next to `bentod`, and
shows both names at the end. Run it on the host:

```
sudo bento-monitor                     # -config, -binary, -monitor, and -source override the paths
```

Run it as root, or as a user that can use `sudo`. It adds `sudo` itself,
for each action.

**Do not make it setuid.** The monitor runs a command that the operator
selects, in the terminal of the operator. With setuid root, `EDITOR`,
`-source`, and `-binary` give a local root shell. (`-source` builds, thus
`build.rs` runs.) Setuid also removes the authentication and the audit
line of `sudo`. To stop the password prompt, use a sudoers rule that
names the `systemctl` commands. Or use a polkit rule on
`org.freedesktop.systemd1.manage-units`. These give the same result
without the escalation.

The monitor has five screens:

- **Services** operates the units: start, stop, restart, enable,
  disable, and the journal.
- **Fleet** shows the deployment (refer to the subsection below).
- **Install** shows each step of sections 4 and 6 as done, missing, or
  waiting on an earlier step. It runs the missing steps.
- **Config** shows the parsed `/etc/bento/bento.toml`. It shows the ACME
  and OIDC secrets only as set or missing. It never shows their values.
  It runs `fetch-images`, `images`, and `reconcile`.
- **Host** shows the SPEC 4.2 checks, the processor, the memory, the
  swap, the free space of the image and storage directories, and the
  libvirt domains.

**The screen knows the role of the machine.** A controller runs `serve`,
the proxy, the SSH frontend, and its own runner. A machine with only
guests runs only the runner service (MULTI-NODE 19). The header shows
the role. The Services and Install screens count only the units of that
role. Thus a correct runner does not show as a controller with three
missing units.

The monitor reads the role from the machine. You do not set it:

- A configuration with `[[runners]]` is a controller.
- A machine with a `bentod-serve` unit file is a controller.
- A machine with neither, and with `[runner] listen` on an underlay
  address (not loopback), is a runner.

**The binary step installs both binaries.** `bentod` and `bento-monitor`
come from one build and you must install them together. If the two
binaries come from different commits, you cannot know how the
deployment operates. The step is done only when both binaries are
installed **and** neither is older than the copy in `target/release`.
The screen names a binary that you built again but did not install. A
version number cannot do this check, because all commits of one release
have the same version.

### The Fleet screen

**Fleet** shows the full deployment, not only this machine
(MULTI-NODE 20). On a controller, it shows each machine with its slots,
its health, and its number of instances. For the selected machine, it
shows:

- the machine ID, the endpoint, and the guest route
- the time of the last contact
- if it accepts new instances, and if not, why not
- the controller epoch that it accepted
- the provisioned vCPU, memory, and disk, and the size it reported
- the architecture and the image readiness
- its last error

Above the machines, the screen shows the slot prefix, the controller
lease, and the row counts. It shows no total memory or disk for the
deployment. Memory on different machines cannot be shared, thus a total
describes a machine that does not exist. A machine that stops answering
keeps its row and shows its last report.

Keys: `s` shows the slot table. `p` shows the slot plan for the selected
machine. `c` runs `reconcile`.

**The monitor never calls a runner.** A runner refuses each request that
has no controller epoch and no valid lease. A new lease increases the
epoch. That stops the running control plane from operating its own
deployment (MULTI-NODE 11.3). Thus the Fleet screen reads only what the
controller recorded. The controller polls each runner every 30 seconds
and writes the result. The monitor opens the database read-only. It
never migrates the database. You run a migration yourself, after you
make a copy.

A machine with only guests has no controller database. On such a
machine, Fleet shows the fence of that machine: the controller epoch
that it accepted, the number of objects that it must build, and the time
of its last completed change. If the accepted epoch is the same as the
lease epoch of the controller, the controller connected to this runner
after its last start.

The monitor is a shim, not a second control plane. It keeps no state
and starts no process of its own. It does not change things silently.
Each action first shows the exact command. Then the command runs in your
terminal. Thus `sudo` can ask for a password and `journalctl -f` scrolls
as usual. The monitor does what you would type.

The monitor is read-only at the start. With no configuration and no
installed units, all screens show. The Install tab then lists what is
missing.

After the binary step replaces `bento-monitor`, the screen on your
terminal is still the old copy. Quit and start it again.

## 7. Users, capacity, and the dashboard

A user signs in to the dashboard through OIDC. The first login makes the
account, its /24, and its libvirt network. To use the command line, the
user then does these steps:

1. Run `ssh bento.example.org`.
2. Open the link that it shows.
3. Confirm the fingerprint.

Use the same procedure to add more keys later, for example for a laptop
or a phone. Do it from a browser that is signed in.

**The SSH frontend uses the first key that the client offers.** If the
SSH agent has more than one key, the frontend can show a link for a
different key than you expect. To select one key, use
`-o IdentitiesOnly=yes -i <key>`.

**There is no quota for each user. There is nothing to grant.** An
account can use all the resources that the host has available. The only
limit is the host (SPEC 6.1). Bento refuses a create or a resize in two
conditions:

- The memory of all instances together is more than the host memory
  multiplied by `overcommit_ratio`.
- The virtual disk of all instances together is more than the size of
  the storage volume.

Bento sets no limit on vCPU.

`bentod serve` reads the two values one time at startup and writes them
to the log:

```
host capacity: the ceiling on create and resize (SPEC 6.1)
  memory_mib=65536 disk_gib=900 overcommit_ratio=1
```

Two results are not easy to see:

- The sums include **all** instances on the host. Thus the machines of
  one user decrease the space for a different user. A refusal names the
  resource, the limit, and the current use.
- The disk value is the size of the full filesystem that has
  `storage_dir`. If that directory is on the root filesystem, the limit
  is the full root filesystem, not a part of it. If this is a problem,
  put the storage on its own volume.

To put more memory on the host than it has, increase
`overcommit_ratio`. First read the two conditions in SPEC 5.3.

### The dashboard charts

The charts read each machine every 30 seconds:

- `/proc/stat` for processor time
- `/proc/meminfo` for memory
- the storage volume for disk

libvirt supplies the values for each instance. The disk value of an
instance is the real size of its overlay. The capacity check uses the
virtual size, not this value.

`bentod serve` reads its own machine through the local libvirt socket.
It gets the values of the other machines from their runner endpoints.
Thus each machine with guests must run `bentod-runner`, and the
controller must be able to get to its endpoint. If not, its guests have
no charts. The front page shows one card for each machine. Each card
counts only the instances on that machine. Capacity is a property of a
machine. Thus the tiles above the table show a limit only when there is
one machine.

The values are kept in memory. **A restart of `bentod-serve` clears all
charts.** They fill again during the next hour. Only the charts are
lost.

Three values can be missing (they are not incorrect):

- A machine that does not answer keeps its card, with the last size
  that it reported. Its charts stay flat until it answers again.
- An instance that is not running has no processor or memory value.
- A guest whose balloon driver never reported has no memory value. Its
  memory chart stays empty, but its processor chart fills. The host
  supplies the processor value, thus the guest is not necessary for it.

A chart with no values shows "No samples yet." A chart with generated
values has a "sample data" badge. A deployed `bentod` never generates
values.

### OIDC

OIDC makes the accounts. Thus `bentod serve` must have OIDC before a
user can sign in. This includes SSH, because the key-link page needs a
session. API tokens do not need OIDC after they exist.

If the provider does not answer at startup, `serve` continues. It tries
the discovery again, with a longer wait each time (maximum 60 seconds).
Until discovery succeeds, the dashboard login does not work.

With Pocket ID:

1. Make an OIDC client with the callback URL
   **`https://bento.example.org/callback`**. The URL must be exact.
2. Put the client ID and the client secret in `[oidc]`.
3. Restart `bentod serve`.
4. Sign in. The first login for an identity makes the account, keeps its
   subject, and gives it a /24.

Bento makes the account name from the first available value:

1. `preferred_username` from the provider
2. the local part of the email address
3. the display name

Bento changes the name to lowercase letters, digits, and hyphens. If the
name is already in use, Bento adds `-2`. To change the name, use a
direct database write. Do this before the user has instances. Nothing
changes the name of the libvirt network of the user.

With Pocket ID, the subject is the UUID of the user. It does not change
between clients if `subject_types_supported` is `["public"]`.

If a login fails, the log gives the cause. The possible causes are:

- missing state cookie
- state mismatch
- code exchange failed
- ID token invalid
- nonce mismatch
- unmatched subject (only with `allow_signup = false`)

If the log shows **nothing**, the request did not get to Bento. The
problem is at the provider. Look in the provider logs for a redirect to
its error page after a successful authentication. With Pocket ID, the
usual cause is a client that is limited to groups, with no groups in its
list. Such a client refuses all users.

Names in `operators` in the configuration get the operator controls on
the dashboard, for example the database download.

## 8. Backups

`bentod dump-db <file>` writes a consistent copy with the SQLite backup
API. **Do not copy the database file directly.** WAL makes a direct copy
unsafe. Back up the copy together with the image directory and the
storage directory (SPEC 12.1).

`bentod restore-db <copy>` puts a copy back. Stop the units first. A
restore replaces the full database and does not coordinate with a
running writer. Before the restore, Bento copies the current database to
`<db_path>.before-restore-<stamp>`. Thus you can recover from a restore
of the incorrect file. Then Bento applies the schema migrations that the
copy does not have. Thus an older backup can go back on a newer build.

`bento-monitor` has both on the Config tab. `b` makes a copy next to the
database. `r` offers the newest copy there. Make a copy before an upgrade
that has a schema migration.

## 9. x86_64 diagnostic run (2026-09-27)

This section records the first run of Bento on x86_64. It is also the
first run of Bento against a real libvirt daemon with a real guest. We
did the run on commit `bfbe081`.

### The environment

The "devbox" was a **rootless distrobox container** (Fedora 44) on a
NixOS host. It was not a Fedora host. These properties are important:

- The libvirt socket, `/dev/kvm`, and the home directory come from the
  NixOS host. The host runs the monolithic `libvirtd` (libvirt 12.0.0,
  QEMU 11.1.0), thus the default socket path is correct.
- The container uses the network namespace of the host. But container
  root is root only in a user namespace. It has no `CAP_NET_ADMIN` in
  the host network namespace.
- There is no systemd in the container.
- The libvirt daemon on the host also has other domains of the owner.

### Result

| Step | Result |
|------|--------|
| `make build` with the Fedora `rust` and `cargo` packages | Pass |
| `make check` (fmt, clippy, unit, end-to-end) | Pass after `dnf install clippy rustfmt`. 675 tests pass, 1 ignored. |
| `bentod fetch-images` (Debian 13 amd64) | Pass |
| Host checks of `serve` | Pass. Warnings for Podman and KSM only. |
| Connection to the host `libvirtd` | Pass |
| nftables load | **Fail**: `Operation not permitted`, also with `sudo` |
| Ports 22 and 443 | Not possible (`ip_unprivileged_port_start` is 1024) |

`serve` cannot start in this container, because the nftables load
fails.

### What the diagnostic run showed

To find more problems, we did one run with a stub `nft` on `PATH`. The
stub kept the ruleset in a file and loaded nothing. **This run has no
guest isolation and no guest egress. It is not a deployment.** All
listeners were on loopback, on unprivileged ports:

```toml
[listen]
http = "127.0.0.1:10080"
https = "127.0.0.1:10443"
ssh = "127.0.0.1:2222"
proxy_port_min = 13000
proxy_port_max = 13009
tls = "off"
```

No OIDC provider was available. Thus we wrote the account and an API
token into the database with `sqlite3`.

These items worked against the real libvirt daemon and KVM:

- `serve` made the user network `bento-user-0` (bridge `bento0`) and
  started it.
- `POST /api/instances` made a domain (`q35`, APIC, `host-model`,
  UEFI). libvirt selected Secure Boot firmware, and Debian booted with
  it. The guest answered on its static address some seconds later. Thus
  the NoCloud seed and the network configuration were correct.
- `serve` removed the seed ISO at the first `Running` event. The guest
  still read it, because QEMU keeps the file open. After a stop and a
  start, the persistent definition had no seed CD-ROM.
- `ssh bento@<address>` worked with the owner key.
- The proxy sent `Host: web.bento.example.org` to the HTTP server in the
  guest (200). The dashboard name gave 200. An unknown name gave 404.
- `ssh -p 2222 web@127.0.0.1` went through the frontend to the guest.
  `ssh -p 2222 127.0.0.1 ls` showed the instance.
- Stop, start, delete, `reconcile`, and `dump-db` worked.

These items did not work, because the stub loaded no rules:

- The guest had no internet access (refer to "Guest network").
- cloud-init did not complete, and `qemu-guest-agent` was not installed.

### Notes for a shared libvirt daemon

- **Paths.** The libvirt daemon opens the disk files, not Bento. Thus
  `image_dir` and `storage_dir` must have the same path in Bento and on
  the libvirt host. In a container, use a directory that the host also
  sees. The container `/var/lib` is not visible to the host.
- **File owner.** libvirt `dynamic_ownership` changes the owner of the
  base image and the overlay to the QEMU user of the host. Inside a
  rootless container, the files then show `nobody`. Delete still works,
  because Bento can write to the directory.
- **Capacity.** The disk limit was the full shared filesystem (465 GiB),
  not the free space (section 7).
- **reconcile.** It shows all other domains of the daemon (section 6).

### How to deploy on this type of machine

We did not test a full deployment. These are the possible procedures.
We did not test them.

1. **A dedicated virtual machine (recommended).** Install Fedora in a
   VM with nested virtualization, and deploy there with sections 1 to 8.
   The firewall table of Bento then affects only that VM.
2. **A rootful container.** Use `distrobox create --root` or rootful
   Podman with `--network host` and `CAP_NET_ADMIN`. Then `nft` and the
   ports work. But the table of Bento then affects all forwarded traffic
   of the NixOS host (section 2, "The host firewall").
3. **Directly on the NixOS host.** The binary uses the standard glibc
   loader path. NixOS does not have this path unless `nix-ld` is on.
   The firewall problem of item 2 also applies.

To clean up after a diagnostic run, stop the processes and remove the
network from the host:

```
virsh -c qemu:///system net-destroy bento-user-0
virsh -c qemu:///system net-undefine bento-user-0
```

## Known operator gaps

One task needs a direct database write, because there is no command:

- set `oidc_subject` on a user that exists

Use one `sqlite3` command on the database. Install `sqlite3` first if
the host does not have it. Without `sqlite3`, you must write a temporary
program for one `UPDATE`. Before the write, stop `bentod serve`. For one
small write, the WAL busy timeout is also sufficient.
