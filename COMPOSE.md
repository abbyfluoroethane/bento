# Bento Compose Deployments

Status: proposed design. Not implemented. SPEC.md remains authoritative for
everything this document does not change.

This document defines how a Bento user gives Bento a git repository that holds
a `docker-compose.yml` and a `.env`, and gets a running, publicly addressed
application. Bento creates one instance for the repository, runs the compose
stack inside that instance with Podman, and publishes the ports the stack
declares.

## 1. Summary

The feature adds one idea to Bento: an instance can have a **deployment**. A
deployment is a repository reference plus the state of the last attempt to run
it. It is not a new kind of object. It is a property of an instance.

```
bento deploy new blog https://github.com/me/blog-stack
```

That command creates an ordinary instance named `blog`, with an ordinary
address on the owner's `/24`, an ordinary name in DNS, ordinary quota
accounting, and an ordinary `ssh blog@bento.example.org`. The only difference
is that Bento also puts the repository inside it and starts the stack.

Five decisions carry the design:

1. **The guest is a bootc image that Bento publishes, not Fedora CoreOS.**
   Section 3.
2. **A deployment is a facet of an instance, not a parallel object.** Section 4.
3. **Bento runs the user's compose file with `podman-compose`, and does not
   translate it.** Section 5.
4. **Podman runs as root inside the guest.** The virtual machine is the
   security boundary. Section 6.
5. **The seed ISO carries the first deployment. SSH carries every later one.**
   Section 7.

Most of the port work is already done. SPEC 9.1 maps
`https://NAME.example.org:3456` to port 3456 on the guest. A compose service
that declares `ports: ["3456:80"]` is therefore reachable the moment it
starts, with no change to the proxy. Section 8 covers what is left.

## 2. Goals and non-goals

The design must:

1. Accept a `docker-compose.yml` that the user did not write for Bento.
2. Add no second first-boot path. SPEC 5.2 has one, and it stays one.
3. Reuse instance identity, addressing, DNS, TLS, quota, shares, cooldown, and
   the reboot restore in SPEC 11.2 without change.
4. Bring the stack back after a guest reboot and after a host reboot.
5. Tell the user why a deployment failed, in words that name the cause.
6. Keep the compose file's secrets out of every place Bento does not need
   them.
7. Survive `bento cp`, `bento rename`, and `bento rm` with no special case.

The design does not:

1. Run one compose service per virtual machine. One repository is one virtual
   machine, whatever it contains.
2. Schedule, scale, or load balance. There is one replica of everything.
3. Promise complete Docker Compose semantics. Section 5 states what is given
   up.
4. Manage the application's data. Backup stays the operator's job (SPEC 12.1).
5. Build container images from a `build:` stanza in version 1. Section 12.

## 3. The guest operating system

### 3.1 Fedora CoreOS does not fit

The obvious guest is Fedora CoreOS. It is the container host, it ships Podman,
and it updates itself. It is the wrong choice for Bento, for one reason that is
not about CoreOS at all.

Fedora CoreOS provisions with Ignition. Bento provisions with cloud-init and
the NoCloud data source, and only that. SPEC 5.2 says it plainly: "Bento uses
the same cloud-init seed and instance lifecycle after conversion; there is no
separate boot path." Ignition is not cloud-init. It is delivered by a
platform-specific mechanism — on QEMU, the `fw_cfg` device — and it reads a
different configuration language.

An Ignition guest would need:

- a second seed renderer beside `crates/cloudinit/src/seed.rs`;
- a second network configuration format, because Ignition writes
  NetworkManager keyfiles, not cloud-init `network-config`;
- a second domain XML variant, because `fw_cfg` is not a CD-ROM
  (`crates/hypervisor/src/xml.rs:385`);
- a second first-boot completion rule, because there is no ISO to detach
  (`crates/lifecycle/src/poller.rs:29`);
- a second answer for `bento cp`, which today copies an overlay and re-seeds.

That is a fork of the instance lifecycle, bought to get a package set. Do not
buy it.

### 3.2 Use a bootc image instead

Bento version 1.1 already converts a bootc OCI image to a content-addressed
qcow2 (SPEC 18.1, `crates/images/src/fetch.rs:268`). That pipeline gives every
property that made CoreOS attractive:

| Property wanted from CoreOS | Where it comes from with bootc |
| --- | --- |
| Immutable `/usr` | bootc deployment model |
| Podman present and current | the Containerfile |
| Atomic, reversible OS change | a new image version and a rebuilt instance |
| Reproducible guest | the content-addressed checksum in `image_versions` |
| No package installation at boot | packages are baked in |

And it costs nothing in new lifecycle code, because the disk that comes out is
an ordinary image version.

Bento therefore publishes one image, `bento-compose`, built on
`quay.io/fedora/fedora-bootc`. The Containerfile adds:

```
podman podman-compose containers-common container-selinux
cloud-init qemu-guest-agent
git-core curl tar jq
/usr/libexec/bento/compose        # the guest-side helper, section 9
/usr/lib/systemd/system/bento-compose.service
/usr/lib/systemd/system/bento-compose-reconcile.timer
```

The image satisfies the existing bootc contract without change: it has
`/usr/bin/cloud-init`, `/usr/bin/qemu-ga`, a kernel under `/usr/lib/modules`,
and the NoCloud data source (`crates/images/src/fetch.rs:586`).

**Extend the contract check for compose images only.** A `bento-compose` image
must also contain `/usr/bin/podman`, `/usr/bin/podman-compose`, and
`/usr/libexec/bento/compose`. Add these as a second, opt-in contract keyed on a
new `images.role` column, not as a requirement on every bootc image. An
operator who adds a plain Fedora bootc image must not be told it is missing
Podman.

**Pin the compose provider in the image.** A guest that installs
`podman-compose` from PyPI at first boot has a network dependency, a supply
chain, and a version that changes under the user. The provider version is part
of the image checksum. That is the whole point of content addressing.

## 4. The data model

A deployment is one row keyed on an instance UUID.

```sql
CREATE TABLE IF NOT EXISTS deployments (
    instance_uuid  TEXT PRIMARY KEY
                        REFERENCES instances(uuid) ON DELETE CASCADE,
    repo_url       TEXT NOT NULL,
    git_ref        TEXT NOT NULL DEFAULT 'HEAD',
    subdirectory   TEXT NOT NULL DEFAULT '',
    -- The commit the guest is running, not the commit the ref points at.
    revision       TEXT,
    -- Digest of the bundle the guest was last given. Section 7.
    bundle_sha256  TEXT,
    phase          TEXT NOT NULL DEFAULT 'pending'
                        CHECK (phase IN ('pending','bundling','delivering',
                                         'starting','running','failed')),
    -- A stable code from the table in section 11, never free text.
    error_code     TEXT,
    error_detail   TEXT,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);
```

`ON DELETE CASCADE` is the whole of the delete story. `bento rm blog` removes
the instance row and the deployment row goes with it.

There is no `owner_id`, no `name`, no `address`, and no `state` on this table.
Every one of those is already on `instances` and already correct. A deployments
table that carried its own name would need its own cooldown, its own uniqueness
rule, and its own share check — three chances to get authorization wrong.

The schema is idempotent and `crates/store/src/lib.rs:180` executes it on every
open, so an existing database gains the table with no migration function.

**Also add `instances.role`** — `'plain'` or `'compose'` — through a
`PRAGMA table_info` migration beside `migrate_image_sources`
(`crates/store/src/lib.rs:185`). This is what makes `ls` able to mark a
deployment and what stops `bento port` from fighting section 8.

**Multi-node needs nothing.** A deployment follows its instance, and
MULTI-NODE.md already places instances. The one interaction is that the bundle
delivery in section 7 must reach the guest from wherever the instance runs;
because delivery is over the SSH frontend, which is already global to the
deployment, that is free.

## 5. The compose runtime

### 5.1 The choice

| Option | Compose coverage | Code Bento owns | Verdict |
| --- | --- | --- | --- |
| `podman-compose` | Common compose files work. Advanced keys vary. | None. Bento invokes it. | **Chosen.** |
| Docker Compose v2 on the Podman API socket | Best parser. Execution still goes through Podman's Docker API compatibility layer. | None, but a root-equivalent API socket must run. | Rejected, section 5.2. |
| `podman kube play` after conversion | Low. `depends_on`, profiles, restart rules, and network aliases do not survive. | A lossy converter. | Rejected. |
| Generated Quadlet units | Exactly as much as Bento's translator implements. | A compose implementation, forever. | Rejected. |

Quadlet is the better systemd citizen and it is tempting. It is also a trap.
Quadlet is an excellent target for a *Bento-native* deployment format, where
the user writes what Bento defined. It is a poor target for a file the user
wrote for Docker on their laptop, because Bento would then own the difference
between what compose means and what Quadlet does — permanently, and always
behind.

The product promise is "your compose file works." The runtime that consumes the
user's compose file unmodified is the one that keeps it.

**What this gives up:** exact Docker Compose semantics. `podman-compose` covers
services, ports, environment, volumes, networks, and `depends_on` well enough
for the applications people self-host. It differs on health-gated ordering,
profiles, and some build behavior. Say this in the documentation. Do not
promise compatibility Bento cannot test.

### 5.2 Why not the Docker Compose binary

Running Docker Compose v2 against `podman.socket` gets a better compose parser.
It also requires a long-lived API socket that grants complete control of the
Podman instance to anything that can reach it — including any container that is
given the socket. Bento would be adding an attack surface to buy parser
coverage it can mostly get for free. If `podman-compose` proves inadequate in
practice, revisit this with evidence, and mount the socket nowhere.

### 5.3 One project name, forever

Every invocation uses `--project-name bento`. Named volumes are keyed on the
project name; a project name that changes between deployments orphans the
user's data while appearing to work. Never run `down --volumes`, in any code
path, for any reason.

## 6. Rootless or rootful

This is the most consequential decision in the design, and the answer is not
the reflex.

**Run Podman as root inside the guest.**

The reflex is rootless: a dedicated `deploy` user, `loginctl enable-linger`,
subuid ranges, `net.ipv4.ip_unprivileged_port_start`, and user units. That is
correct advice for a shared container host. Bento is not a shared container
host. Bento chose virtual machines exactly so that an instance can run
untrusted code (SPEC 5.4). The virtual machine is the boundary. The guest is
one repository belonging to one user, who already has `sudo` on it through
`ssh blog@bento.example.org`.

Rootless inside a single-tenant VM defends the guest's root account from the
guest's own owner. That is not a threat Bento has.

What rootful buys:

| Rootless cost | Gone under rootful |
| --- | --- |
| subuid/subgid ranges must exist and not overlap | Not needed. |
| Ports under 1024 need `ip_unprivileged_port_start` | A container can bind 80 and 443. |
| `systemd --user` must be reachable from a root cloud-init context — `su`, `runuser`, `machinectl shell`, and `systemd-run --machine=` all behave differently and several fail quietly | Ordinary system units. |
| `loginctl enable-linger` must be set before anything starts | Not needed. |
| Volume ownership needs `:U` or an init step for images that run as a fixed UID | Ordinary. |
| `privileged: true`, `devices:`, and `cap_add:` in a user's compose file fail at runtime | They work, as they do on the user's laptop. |

That last row is the product argument. A compose file that runs on a laptop and
fails on Bento with a user-namespace error is a support burden that buys
nothing.

**What rootful costs:** a container escape owns the guest. The guest is the
user's own machine, which the user already controls, so the escape gains
nothing it did not already have. It does not reach the host — that is KVM's
job — and it does not reach another user, because SPEC 6.3 puts an nftables
policy between the networks.

**Do keep SELinux enforcing.** Never generate `label=disable`. Named volumes
are labeled correctly by default; a bind mount inside the release directory
needs `:Z`.

**Revisit this if** Bento ever runs more than one user's deployment in one
guest. It should not. One repository, one machine, is the design.

## 7. Getting the repository in

### 7.1 Bento clones. The guest never does.

Three options exist. Only one keeps repository credentials out of the guest.

| Method | Where the git credential lives | Redeploy channel | Verdict |
| --- | --- | --- | --- |
| Guest clones at first boot | In the guest, permanently, next to the application that may be compromised | Guest-side polling, which drifts from Bento's database | Rejected |
| Bento clones, ships a tarball on the seed ISO | On the control plane only | None — the ISO is deleted after first boot (SPEC 5.2) | First boot only |
| Bento clones, ships a tarball over SSH | On the control plane only | The same channel every time | **Chosen** |

Bento clones the repository host-side into a `0700` temporary directory,
checks out the ref, records the resolved commit, **discards `.git`**, and
produces a tarball. Discarding `.git` removes the history, the embedded remote
URL, and any credential helper configuration from everything that follows.

Impose a size cap. 64 MiB compressed is generous for a compose repository and
small enough that no delivery path has to stream. A repository over the cap is
rejected with its measured size, because the alternative is a mysterious
timeout.

### 7.2 The channel already exists

Bento installs its own SSH frontend key into every instance, as the `bento`
user, which has `NOPASSWD` sudo (`bentod/src/adapters.rs:302`,
`crates/cloudinit/src/seed.rs:110`). Bento therefore already holds an
authenticated, root-capable channel into every guest, and already has an SSH
client — `russh`, in `crates/sshfront`.

Use it. Delivery is:

```
ssh -i <frontend key> bento@<instance address> \
    sudo /usr/libexec/bento/compose install
```

with the tarball on stdin, and a JSON status document on stdout.

This is worth stating clearly because the alternative looks natural and is not.
The domain XML has a `qemu-guest-agent` channel (`crates/hypervisor/src/xml.rs:385`)
and every image installs the agent — but **no code in Bento talks to it
today**. Choosing the guest agent means writing a QMP client, a `guest-exec`
wrapper, base64 chunking for file transfer, and an exit-status poll, to obtain
a channel Bento already has. Choose the guest agent later, if ever, for the
case where the guest network is broken — which is the case where a compose
deployment cannot work anyway.

### 7.3 First boot

First boot has no SSH yet, so the first bundle rides the seed ISO. This needs
two changes, and they are the only changes to the cloud-init crate:

1. `Seed` gains `files: Vec<SeedFile>` — path, mode, and contents — rendered
   into `write_files`. `Seed::validate` rejects a path outside a fixed
   allowlist of prefixes.
2. `Builder::build` stages the bundle as a fourth file on the ISO beside
   `meta-data`, `user-data`, and `network-config`
   (`crates/cloudinit/src/builder.rs:180`). NoCloud ignores files it does not
   recognize.

The rendered `user-data` adds:

```yaml
write_files:
  - path: /etc/bento/deployment.json
    owner: root:root
    permissions: '0400'
    content: |
      {"deployment_id":"...","revision":"...","bundle_sha256":"..."}

runcmd:
  - [systemctl, enable, --now, qemu-guest-agent]
  - [systemctl, enable, bento-compose.service, bento-compose-reconcile.timer]
  - [sh, -c, '/usr/libexec/bento/compose install < /run/media/cidata/bundle.tar.gz']
```

Note `enable` without `--now` for `bento-compose.service`, then an explicit
`install` that starts it. `runcmd` runs inside `cloud-final.service`; a unit
ordered after `cloud-final.service` cannot be started synchronously from
`runcmd` without deadlocking.

### 7.4 First boot is not "running"

Bento currently treats "libvirt reports Running" as first boot complete, and
detaches and deletes the seed ISO at that moment
(`crates/lifecycle/src/poller.rs:29`). libvirt reports `Running` when the
firmware starts, long before cloud-init has read the ISO.

This is already true for plain instances and is already survivable, because
QEMU holds the CD-ROM open for the life of that boot and cloud-init reads it
early. It becomes load-bearing here, because the bundle is on that ISO.

Do not depend on it. The guest helper copies the bundle out of `/run/media`
into `/var/lib/bento/releases/<commit>` as its first action, before anything
else can fail. After that the ISO is disposable, which is what SPEC 5.2 wants
anyway.

## 8. Ports

### 8.1 What already works

SPEC 9.1 forwards `https://NAME.example.org:3456` to `<address>:3456` in the
guest, for every port in 3000-9999 (`crates/proxy/src/lib.rs:342`). Firewall
rules for the whole range already exist for a visible instance
(`bentod/src/firewall.rs:77`).

So a compose file with:

```yaml
services:
  web:
    ports: ["3000:8080"]
```

is reachable at `https://blog.example.org:3000/` as soon as it starts, with no
change to the proxy, the firewall, or the database. This is a genuine accident
of the SPEC 9.1 design and the design should lean on it rather than build a
port-mapping table beside it.

### 8.2 What does not work, and what to do

Four cases need Bento to act. All four are detected by parsing the compose
file host-side, before the instance is created.

**The default HTTP port.** `instances.http_port` holds one port and defaults to
80 (`crates/proxy/src/lib.rs:343`). A compose file usually publishes 8080 or
3000, not 80. Bento sets `http_port` from the compose file at creation:

1. If exactly one service publishes a port whose container side is 80, 8080,
   3000, or 8000, use its host port.
2. Otherwise, if exactly one port is published at all, use it.
3. Otherwise leave `http_port` at 0 and tell the user to run
   `bento port blog <n>`.

Never guess between two candidates. A wrong guess sends the base name to the
database instead of the web front end.

**Host port 22.** A compose file that publishes host port 22 breaks
`ssh blog@bento.example.org`, which is how the user reaches the machine to fix
it. Reject at parse time, naming the service.

**Loopback binds.** `ports: ["127.0.0.1:8080:80"]` binds the guest's loopback.
The proxy connects from the gateway address and is refused, and SPEC 9.3 turns
a refused connection into a 503 with no explanation. Reject at parse time and
say why — this is the single most confusing failure the feature can produce.

**Ports outside the range.** A stack that publishes 5432 for Postgres is
correct and should not be rejected; that port is meant to be internal. Report
it, do not reject it:

```
blog: published ports
  3000 -> https://blog.example.org:3000/   (default)
  5432 -> not reachable from outside; inside the instance only
```

The user learns the rule from the output of their own deployment, which is
the only documentation anybody reads.

### 8.3 Do not build a named-port table

It is tempting to store a per-instance port map so the dashboard can label
ports. Resist it in version 1. It duplicates information that lives in the
compose file, it goes stale the moment the file changes, and section 8.1 means
the routing works without it. Render the list from the parsed compose file at
deploy time and store it as opaque JSON on the deployment row if the dashboard
needs it.

## 9. The guest helper

One program, `/usr/libexec/bento/compose`, baked into the image. It has four
subcommands and no network access to anything but container registries.

| Subcommand | Action |
| --- | --- |
| `install` | Read a tarball on stdin, verify its SHA-256 against `/etc/bento/deployment.json`, extract to `releases/<commit>`, run `up`, swap `current` on success. |
| `up` | `podman-compose --project-name bento up -d --remove-orphans` in `current`. |
| `status` | Write the JSON status document in section 11. |
| `reconcile` | If the project's containers are absent or exited, run `up`. |

Directory layout inside the guest:

```
/var/lib/bento/
  releases/<commit>/        # docker-compose.yml, .env, the repository
  current -> releases/<commit>
  state/                    # bind-mount target for application data
  status.json
```

`state/` is outside `releases/` on purpose. A relative bind mount inside a
release directory points at the *new* release after an update, and the
application's data appears to vanish. Rewrite relative bind mounts to `state/`
at parse time, or reject them, but do not leave them pointing into a release.

Three systemd units, all system units:

```ini
# bento-compose.service
[Unit]
Description=Bento compose application
Wants=network-online.target
After=network-online.target
ConditionPathExists=/var/lib/bento/current/docker-compose.yml

[Service]
Type=oneshot
RemainAfterExit=yes
WorkingDirectory=/var/lib/bento/current
ExecStart=/usr/libexec/bento/compose up
ExecStop=/usr/libexec/bento/compose stop
TimeoutStartSec=15min

[Install]
WantedBy=multi-user.target
```

```ini
# bento-compose-reconcile.timer
[Timer]
OnBootSec=2min
OnUnitActiveSec=2min
Persistent=true
```

The timer is not decoration. It covers the registry that was down at boot, the
network that came up late, the container someone stopped by hand, and the
guest that rebooted while the host was still bringing up the bridge.
`network-online.target` is only a synchronization point when a wait-online
service is enabled; enable `NetworkManager-wait-online.service` in the image,
and still retry, because reachable is not the same as resolvable.

## 10. Redeploy

```
bento deploy update blog          # re-read the ref, deliver, restart
bento deploy update blog --ref v2 # change the ref, then the same
```

The sequence:

1. Bento clones and resolves the ref to a commit. If the commit equals
   `deployments.revision`, stop and say so.
2. Bento parses the compose file and runs the checks in section 8.2 and 11.
3. Bento builds the bundle and delivers it over SSH (section 7.2).
4. The guest extracts to `releases/<new commit>`, pulls images, and only then
   swaps `current` and runs `up`.
5. On failure the guest restores the previous `current` and runs `up` again,
   then reports.

**Rollback is best effort, and say so.** `podman-compose up` recreates services
one at a time; a failure partway leaves some new and some old. Restoring the
symlink and running `up` converges, but a mutable image tag may no longer
resolve to what the old release ran. Resolve image references to digests at
deploy time if reliable rollback matters.

**Named volumes survive** because the project name is fixed (section 5.3) and
no path runs `down --volumes`.

**Do not poll git inside the guest.** It puts a repository credential in the
workload, it lets the running revision drift from `deployments.revision`, and
it makes a force-push into a silent production change. A webhook that calls
`deploy update` is the right shape and can come later; it needs a token and an
endpoint, not a new mechanism.

## 11. Validation and reporting

### 11.1 What to reject, and what not to

The instinct is to reject `privileged: true`, `network_mode: host`, `devices:`,
and `cap_add:`. That instinct comes from Docker-based hosting, where those
options cross a security boundary. **Here they do not.** The boundary is the
virtual machine, and a compose file that needs a device inside its own machine
is doing something ordinary.

Reject only what breaks *Bento's* contract with the user:

| Rejected | Why |
| --- | --- |
| host port 22 | Breaks `ssh NAME@domain`, the way to fix everything else (SPEC 10). |
| `127.0.0.1:` host bind | The proxy cannot reach it; produces an unexplained 503 (SPEC 9.3). |
| a bind mount of `/`, `/etc`, `/var/lib/bento`, or the guest agent socket | Breaks Bento's own management of the guest. |
| a relative bind mount into the release directory | Silently loses data on update (section 9). |
| duplicate host ports | Fails at run time with a worse message. |
| a `.env` or compose path outside the repository root | Path traversal in Bento's own extractor. |

Warn, do not reject:

- a published port outside `{http_port} ∪ [3000, 9999]` — internal by design;
- a UDP publish — the proxy is HTTP over TCP, so it is not reachable;
- `network_mode: host` — it works, and it means Bento's port reasoning does not
  apply, so the user should know.

Parse the **fully interpolated** compose model, because `${VAR}` from `.env`
changes the ports. Interpolation means the parsed model contains secrets: keep
it in a `0600` file, never log it, and delete it when the checks finish.

### 11.2 The status document

The guest writes `/var/lib/bento/status.json` atomically. Bento reads it over
the same SSH channel and stores the phase and error code on the deployment row.

```json
{
  "schema": 1,
  "revision": "9f2c1ab",
  "phase": "running",
  "updated_at": "2026-09-03T20:00:00Z",
  "services": {
    "web": {"state": "running", "health": "healthy", "restarts": 0},
    "db":  {"state": "running", "health": "healthy", "restarts": 0}
  },
  "error": null
}
```

`error` holds a code from the table below and a redacted summary. Never a raw
log line — compose output contains environment values.

**A stale document is not a healthy one.** If the SSH probe fails, report
`GUEST_UNREACHABLE` and mark every service state unknown. Do not show the last
good status as if it were current.

### 11.3 Failure codes

| Code | Detected by | What the user is told |
| --- | --- | --- |
| `REPO_UNREACHABLE` | host-side clone | The URL and the git error, credentials stripped. |
| `REPO_TOO_LARGE` | size cap | Measured size and the cap. |
| `COMPOSE_MISSING` | file check | No `docker-compose.yml` at the given subdirectory. |
| `COMPOSE_PARSE_FAILED` | interpolated parse | File and line. Never the interpolated content. |
| `COMPOSE_POLICY_REJECTED` | section 11.1 | The exact field and why. |
| `PORT_CONFLICT_SSH` | port check | The service that publishes 22. |
| `PORT_LOOPBACK_BIND` | port check | The service, and that the proxy connects from outside. |
| `DELIVERY_FAILED` | SSH install | Whether the guest was reachable at all. |
| `BUNDLE_DIGEST_MISMATCH` | guest helper | Corrupt transfer. Nothing was extracted. |
| `IMAGE_PULL_FAILED` | `podman-compose up` | Image name, and auth or rate limit if known. |
| `IMAGE_ARCH_UNSUPPORTED` | pull, no matching manifest | The image has no build for the host architecture. |
| `CONTAINER_EXITED` | container state | Service, exit code, journal cursor. |
| `CONTAINER_RESTART_LOOP` | restart delta over a window | Service and restart count. |
| `CONTAINER_UNHEALTHY` | health state | Service and the redacted health output. |
| `CONTAINER_OOM` | cgroup event | Service, and the instance's configured memory. |
| `APPLICATION_NOT_LISTENING` | proxy probe to the published port | The port is published but nothing answers. |
| `DISK_FULL` | `df`, `df -i` | Usage, and that image layers are the usual cause. |
| `SELINUX_DENIED` | AVC correlation | Path and type. Never advise disabling SELinux. |
| `GUEST_UNREACHABLE` | SSH probe | The instance state, and that status is stale. |

`APPLICATION_NOT_LISTENING` deserves its own probe. A running container is not
evidence that the proxy can reach it, and SPEC 9.3 turns the difference into a
bare 503.

**Never run `podman system reset` as an automatic repair.** It deletes named
volumes. The stack can be recreated; the user's data cannot.

## 12. Deferred

1. **`build:` support.** Building an image in the guest needs disk headroom,
   a much longer first-boot timeout, and a story for build secrets. Version 1
   rejects `build:` with `COMPOSE_UNSUPPORTED_FEATURE` and names it. Most
   self-hosted stacks use published images.
2. **Webhook redeploy.** Section 10 gives the mechanism; the endpoint and token
   are separate work.
3. **A separate data disk.** Section 10 makes a full rebuild lossy for stateful
   stacks. A second qcow2 mounted at `/var/lib/bento/state`, kept across a
   rebuild, fixes that and is the right shape for `bento cp` too.
4. **Compose `secrets:`.** File-mounted secrets are better than environment
   variables. Support depends on the pinned `podman-compose` version; test
   before promising it.
5. **Dashboard log view.** The status document gives phase and service state.
   Streaming logs needs the console work in SPEC 18.4.

## 13. Secrets

Trace where the `.env` comes to rest and decide each one.

| Resting place | Acceptable | Mitigation |
| --- | --- | --- |
| The user's git repository | Their decision | Document putting `.env` outside git and uploading it separately. |
| Bento's clone directory | Briefly | `0700` directory, `0600` files, deleted when the bundle is built. |
| The bundle in flight | Yes | It travels inside the SSH session, already encrypted. |
| The seed ISO, first boot only | Yes | The ISO already carries the owner's SSH keys and is already deleted after first boot (SPEC 5.2). |
| cloud-init's cached user-data in the guest | Yes | Same trust level as the guest disk itself. |
| The guest disk | Unavoidable | `0600`, owned by root, under `/var/lib/bento/current`. |
| `podman inspect` and the container environment | Unavoidable | Any environment variable is visible to guest root. This is what `.env` means. |
| Bento's database | **No** | Store `bundle_sha256`, never the bundle and never the `.env`. A redeploy re-clones. |
| The journal, on the host or in the guest | **No** | Never log the interpolated model, compose stderr unredacted, or a repository URL with credentials. |

The rule that does the work: **Bento's database stores no secret.** A redeploy
fetches the repository again. That costs one clone and removes the entire
question of encrypting a secret at rest in `bento.db`, which SPEC 12.1 hands to
the operator as a plain file to back up.

## 14. Build order

1. Publish the `bento-compose` bootc image and the guest helper. Verify by
   hand: `bentod images add`, `bento new`, then install a bundle over SSH.
   Nothing in Bento changes yet.
2. Add `deployments`, `instances.role`, and the store module.
3. Add the host-side clone, the size cap, and the bundle builder.
4. Add the compose parser and the checks in sections 8.2 and 11.1. This is
   pure logic and it takes the most tests.
5. Extend `Seed` with `files` and `Builder::build` with the fourth ISO file.
   Wire first-boot delivery.
6. Add `bento deploy` to the SSH CLI, following the `images add` pattern
   (`crates/cli/src/info.rs:236`).
7. Add SSH delivery and the status read. Redeploy works.
8. Add the API resource and the dashboard view.

Step 1 first, and by hand, because every later step assumes a guest that works.
Step 4 before step 5, because the parser decides `http_port`, and `http_port`
is set at creation.

## 15. Open items

**Does `podman-compose` cover the applications people actually self-host?**
Assemble ten real compose files — Immich, Paperless, Nextcloud, Gitea,
Home Assistant, and so on — and run each one on the image from step 1 before
writing any Bento code. If three of ten fail, section 5.1 must be reopened.
This is the cheapest test in the plan and it invalidates the most work.

**Is one virtual machine per repository too expensive?** A 2 vCPU, 2 GiB,
20 GiB default against a quota built for whole machines (SPEC 6.1) may mean a
user can host two applications. Measure the resident set of a real stack with
KSM and free page reporting on before setting the defaults.

**How does `bento cp` behave on a deployment?** Copying the overlay copies the
`.env`, the volumes, and the running state, into an instance with a different
address. The copy's `deployments` row must be created fresh, and the copy must
re-run `up` rather than inherit a status document that describes the original.
Decide whether `cp` on a compose instance is allowed at all in version 1.

**Disk sizing.** Image layers are the usual cause of a full guest and the
default disk quota counts virtual size, not real size (SPEC 19). A compose
stack with four images can want 10 GiB before it starts.
