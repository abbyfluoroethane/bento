# Bento System Specification

Version 0.6. Draft. This version replaces Incus with libvirt.

Bento is a self-hosted platform for Linux virtual machines. A user creates an instance from the command line interface or from the dashboard. Bento publishes the instance on the internet as a subdomain of a domain that the operator owns. An instance exists until the user deletes it.

Bento talks to libvirt. Bento owns the parts that libvirt does not provide: tenancy, quota, addressing, firewall policy, and image supply. Appendix B lists what this decision moved into Bento.

## 1. Terms

| Term | Meaning |
| --- | --- |
| instance | One virtual machine. One instance is one libvirt domain. |
| the host | A machine that runs `libvirtd` and holds instances. Version 1 supports one host. |
| the operator | The person who installs and runs Bento. |
| the user | A person with a Bento account. A user creates instances. |
| the control plane | The Bento server process. The binary is `bentod`. |
| the dashboard | The web interface of Bento. The control plane serves the dashboard at the base domain. |
| the command line interface | The text interface of Bento. The SSH frontend serves the command line interface. |
| the HTTP proxy | The component that terminates TLS and forwards HTTP requests to an instance. |
| the SSH frontend | The component that accepts SSH connections and forwards them to an instance. |
| the base image | A distribution cloud image in `qcow2` format. |
| the base domain | The domain that the operator chooses. This document uses `bento.foid.space`. |

## 2. Scope

Bento version 1 does the following:

- Create, list, start, stop, resize, and delete instances on one host.
- Optionally publish each instance at `$NAME.bento.foid.space` over HTTPS.
- Accept SSH connections to each instance through one public port.
- Support more than one user.
- Give each user a private network, a fixed address range, and a quota.

Bento targets fewer than about 20 users. This number is a design target, not a measured limit.

Version 1 runs the control plane and `libvirtd` on the same machine. Section 16 designs for more hosts. Do not build section 16 in version 1.

## 3. Non-goals

Bento does not implement virtualization. QEMU and KVM do this. libvirt manages them.

Bento does not run containers.

Bento does not consume OCI images. Appendix A records the options for a later version.

Bento does not build images. Bento downloads distribution cloud images.

Bento does not place instances across hosts. Version 1 has one host. A later version needs a placement rule, which section 16 does not define.

Bento does not bill users.

Bento does not use a network database. SQLite is sufficient at the scale in section 2.

**Bento never deletes an instance.** Only the user deletes an instance. Bento has no expiry timer, no grace period, and no idle detection. A quota is the pressure on resource use, not a clock.

Idle detection is the reason for this rule. Idle detection needs a definition of activity, and every definition is wrong for some workload. A long build with no network traffic and no SSH session looks idle. A tool that deletes work is worse than a tool that runs out of disk.

## 4. Architecture

Three Bento processes and one libvirt daemon run on the host.

- **libvirtd** owns the domains, the storage, and the bridges. Bento connects to `qemu:///system` over the local socket.
- **The control plane** holds the database, applies policy, generates domain XML, and serves the dashboard. The control plane is the only component that writes to the database.
- **The HTTP proxy** listens on port 443 and on ports 3000 to 9999. The HTTP proxy terminates TLS. The HTTP proxy forwards a request for the base domain to the control plane. The HTTP proxy forwards every other request to an instance.
- **The SSH frontend** listens on port 22. The SSH frontend authenticates the client, then opens a second SSH connection to the instance. The SSH frontend also serves the command line interface.

Build all three Bento processes as one binary with subcommands.

### 4.1 The libvirt client

Use `github.com/digitalocean/go-libvirt`. This library speaks the libvirt RPC protocol in pure Go and needs no cgo. A static binary is easier to deploy.

Check the current state of both Go libraries before you commit. The official bindings at `libvirt.org/go/libvirt` cover more of the API but need cgo. This recommendation is a judgment, not a measurement.

Bento generates domain XML from a Go template. Keep one template. Do not build a general XML object model, because Bento uses a small and fixed subset of the schema.

### 4.2 Host requirements

1. Check for `/dev/kvm` at startup. Refuse to start without it.
2. Check that `libvirtd` answers on the local socket.
3. Check that the `qemu` and `swtpm` binaries exist.
4. Check that the storage directory exists and is writable.

Run `bentod` as a user in the `libvirt` group. This group can create and control any domain on the host. Treat every string that reaches the domain XML as hostile. Escape every value that Bento writes into XML.

## 5. Hypervisor layer

One instance is one libvirt domain. Bento uses the `kvm` domain type.

The domain XML uses a fixed device set:

- One `virtio-blk` or `virtio-scsi` disk for the root volume.
- One `virtio` network interface on the network of the owner.
- One `virtio-rng` device backed by `/dev/urandom`.
- One `virtio` memory balloon with free page reporting on. See section 5.2.
- One `virtio` serial console.
- One `qemu-guest-agent` channel at `org.qemu.guest_agent.0`.
- UEFI firmware through OVMF.

Bento assigns the MAC address. Bento does not let libvirt generate it. Section 6.2 explains why.

### 5.1 Images and first boot

The operator sets an allowlist of base images in the configuration file. Each entry gives a name, a download URL, and a checksum. Use distribution cloud images in `qcow2` format. These images carry a kernel, an init system, `sshd`, `cloud-init`, and the guest agent.

Bento downloads a base image once and stores it under the image directory. Verify the checksum after the download. Never download a base image while a user waits, because the download takes minutes. Download at startup or from an operator command.

Create the root volume as a copy-on-write overlay:

1. Create a `qcow2` file with the base image as its backing file.
2. Resize the overlay to the requested disk size.
3. Record the base image checksum in the `instances` table.

Never write to a base image after the first instance uses it. An overlay depends on the exact contents of its backing file.

Bento configures the first boot with `cloud-init` and the NoCloud data source. Build an ISO with a `meta-data` file and a `user-data` file. Attach the ISO to the domain as a read-only CD-ROM. The `user-data` file does the following:

- Set the host name to the instance name.
- Create one user account and install the public keys of the owner.
- Set the static address, the gateway, and the DNS server. See section 6.2.
- Install and start `qemu-guest-agent`.

Detach and delete the ISO after the first successful boot. The ISO holds the public keys of the owner, and it does not need to stay attached.

### 5.2 Memory

Set the memory balloon to report free pages:

```xml
<memballoon model='virtio' freePageReporting='on'/>
```

Free page reporting lets the guest return unused pages to the host. The host reclaims those pages and faults them back on next access. Without this setting, a guest fills unused memory with page cache and never releases it, and host memory use climbs to the configured limit and stays there.

**Do not inflate the balloon to change the memory of a running instance.** A balloon-driven decrease is slow and often does not reach the target. A memory change in version 1 edits the domain XML and restarts the instance. A restart is a clear event that a user can plan. A partial and silent decrease is not.

Section 17 covers virtio-mem, which gives a reliable live change. Note that virtio-mem and balloon inflation do not combine.

Overcommit is an operator setting. The default ratio is 1.0, which means no overcommit. Free page reporting makes a higher ratio workable, because host memory use tracks real use. Two conditions apply before you raise it:

1. Give the host swap or `zram`. Without either, the host out-of-memory killer terminates a QEMU process, and one user loses a machine with no warning.
2. Monitor the resident set size of every QEMU process. Alert on the total.

### 5.3 Storage

Version 1 uses a directory of `qcow2` files. Do not use a libvirt storage pool. A directory is simpler, and Bento already tracks every file in the database.

The disk quota counts the virtual size of each overlay. The virtual size bounds the worst case. The file on disk starts small and grows toward that bound.

Bento takes no automatic snapshot. The `cp` command creates a new overlay from a copy of the source overlay. The source instance must be stopped, because copying a running disk gives a crash-consistent image at best.

## 6. Multi-user model

libvirt has no tenancy concept. Bento implements tenancy. This section replaces what Incus projects gave the previous version.

### 6.1 Quota

Each user has four limits: the instance count, the total vCPU count, the total memory, and the total virtual disk size.

Enforce a limit in the control plane. The check and the insert run in one SQLite transaction. Two concurrent `new` commands must not both pass a check that only one of them fits.

Bento accounts against the database, and libvirt is authoritative for what exists. These two can disagree after a crash. Reconcile at startup:

1. List every domain on the host.
2. Find every domain with no matching row in the `instances` table. Log it and leave it alone.
3. Find every row with no matching domain. Mark the row and report it to the operator.

Never delete a domain during reconciliation. A reconciliation bug that deletes a domain is worse than a row that is wrong.

Report quota use in the `ls` command and on the dashboard.

### 6.2 Networks and addressing

Give each user one libvirt network. Use `<forward mode='open'/>`. This mode creates the bridge and installs no firewall rules, so Bento owns the whole policy. A NAT network would install libvirt rules that Bento then has to work around.

Assign each user a `/24` from a private range. Bento is the address manager. Bento picks the address for an instance at creation time and writes it into the `cloud-init` network configuration.

Static assignment is better than address discovery here. There is no DHCP server, no lease file, and no wait for the guest agent. Bento knows the address of an instance before the instance boots. The HTTP proxy and the SSH frontend can therefore route to an instance that has never started.

Bento also assigns the MAC address from the locally administered range. A fixed MAC keeps the interface name stable in the guest across a restart.

### 6.3 Firewall

Bento writes one nftables table. Bento owns every rule in that table. Do not edit rules by hand.

The rules do the following:

1. Permit traffic from the host to any instance on ports 22 and on the published HTTP ports.
2. Permit egress from any instance to the internet.
3. Drop traffic between the bridges of two different users.
4. Drop traffic between two instances of the same user by default. See the note below.
5. Masquerade instance egress behind the address of the host.

Rule 4 is a judgment call. A user who runs a database instance and a web instance expects them to talk. Version 1 permits traffic inside one user network and drops traffic across user networks. Record this as a decision so that a later version can tighten it.

Reload the whole table on every change. A partial rule update leaves a window with the wrong policy.

## 7. Naming and DNS

Each instance has a name. The name is unique across the deployment, not per user.

Create these DNS records:

1. Create an `A` record for `bento.foid.space` that points to the host.
2. Create an `A` record for `*.bento.foid.space` that points to the host.

IPv6 is not in version 1.

## 8. TLS

Get one wildcard certificate for `*.bento.foid.space` and `bento.foid.space`. Use the ACME DNS-01 challenge. The DNS-01 challenge is required for a wildcard certificate.

The first reason is the Certificate Transparency logs. A per-instance certificate publishes the name of every instance to a public log. A wildcard certificate publishes only the base domain.

The second reason is the Let's Encrypt rate limit. Let's Encrypt limits new certificates to 50 per registered domain per 7 days, and the limit refills at one certificate every 202 minutes. Bento does not delete instances automatically, so normal use stays under this limit. The limit matters during development. Check the current limit before you build.

The control plane needs write access to the DNS zone. Use an API token that is limited to the `_acme-challenge` records if the DNS provider supports this limit.

## 9. HTTP proxy

The HTTP proxy reads the TLS Server Name Indication field and extracts the instance name. The HTTP proxy then reads the address from the `instances` table. Section 6.2 makes this address known before the instance boots.

Write the proxy in Go with `net/http/httputil.ReverseProxy`.

A request for the base domain goes to the control plane. The control plane serves the dashboard and the OIDC login flow.

Set these headers on each forwarded request:

- `X-Forwarded-Proto`
- `X-Forwarded-Host`
- `X-Forwarded-For`

### 9.1 Port selection

Each instance has one default HTTP port. The default value is 80. A user changes the port with the `port` command.

The HTTP proxy also listens on ports 3000 to 9999. A request to `https://$NAME.bento.foid.space:3456/` goes to port 3456 on the instance. The wildcard certificate covers these ports, because the host name does not change.

Do not use a name prefix such as `3456-$NAME.bento.foid.space`. This form needs a certificate for `*.*.bento.foid.space`. Such a certificate does not exist.

### 9.2 Visibility

An instance has one of three visibility values. The default value is `off`.

| Value | Behavior of the HTTP proxy |
| --- | --- |
| `off` | Return HTTP 404 for the name. |
| `private` | Redirect an unauthenticated request to the login page of the dashboard. |
| `public` | Forward the request without authentication. |

The `off` value binds nothing to the name. A user who runs a database in an instance needs this value. A login page in front of the port is not the same protection.

The HTTP 404 response for `off` matches the response for a name that does not exist. This hides the existence of the instance.

Ports 3000 to 9999 are always private. The `public` value applies only to the default HTTP port.

### 9.3 Unavailable instance

The HTTP proxy returns HTTP 503 when the instance is not ready. This covers a `starting` state, a restart, and a refused connection on the target port.

Do not hold the request until the instance answers. A held request looks like a hung browser tab and gives the user no information.

Serve a Bento error page with the 503 response. Name the instance and the state on that page. Set the `Retry-After` header to 5 seconds.

## 10. SSH frontend

The user connects with `ssh $NAME@bento.foid.space`.

SSH has no Server Name Indication field. The instance name must therefore travel in the SSH user name field.

The SSH frontend does the following:

1. Accept the connection and read the client public key.
2. Look up the public key in the database to find the user.
3. Reject the connection if the key is unknown.
4. Read the SSH user name and treat it as the instance name.
5. Check that the user owns the instance or has a share for the instance.
6. Start the instance if the state is `stopped`.
7. Wait for `sshd` in the instance to accept a connection.
8. Open a new SSH connection to the instance on the internal address.
9. Join the two connections.

Write the SSH frontend in Go with `gliderlabs/ssh`.

Steps 6 and 7 make a stopped instance feel persistent. Write a line such as `bento: starting $NAME` to the SSH session before the wait. Set a timeout of 120 seconds and report a clear failure.

The SSH frontend also answers `ssh bento.foid.space` with no user name. This session runs the command line interface.

## 11. Instance lifecycle

An instance has one of these states: `starting`, `running`, `stopped`.

The user controls every state change. Bento changes the state only in response to a user action.

| Action | libvirt call | Result |
| --- | --- | --- |
| `new` | `virDomainDefineXML`, then `virDomainCreate` | Create the domain and set the state to `starting`. |
| `start` | `virDomainCreate` | Start the domain. |
| `stop` | `virDomainShutdown`, then `virDomainDestroy` | Stop the domain. Keep the disk. |
| `restart` | `virDomainReboot` | Restart the guest. |
| `rm` | `virDomainDestroy`, then `virDomainUndefine` | Delete the domain and the disk. |

The `stop` action sends an ACPI shutdown request first. Wait 60 seconds. Call `virDomainDestroy` only after the timeout. Report which path the stop took.

A `resize` action that changes memory or vCPU count edits the XML and needs a restart. Tell the user this before the change. A `resize` action that grows the disk edits the overlay and the guest sees the change after a restart.

The `rm` command asks for confirmation. Accept a `--force` flag for scripts.

## 12. Data model

Use SQLite with write-ahead logging.

| Table | Columns |
| --- | --- |
| `users` | `id`, `name`, `email`, `oidc_subject`, `subnet`, `created_at` |
| `quotas` | `user_id`, `max_instances`, `max_vcpu`, `max_memory`, `max_disk` |
| `ssh_keys` | `id`, `user_id`, `public_key`, `fingerprint`, `comment`, `created_at` |
| `hosts` | `id`, `name`, `libvirt_uri`, `created_at` |
| `instances` | `name`, `owner_id`, `host_id`, `uuid`, `base_image`, `base_checksum`, `state`, `address`, `mac`, `vcpu`, `memory`, `disk`, `http_port`, `visibility`, `created_at`, `last_seen_at` |
| `shares` | `instance_name`, `user_id`, `created_at` |
| `tokens` | `id`, `user_id`, `hash`, `expires_at` |

The `uuid` column holds the libvirt domain UUID. Address a domain by UUID, not by name. A rename then does not break the link.

The `host_id` column exists in version 1 and always points to the one host. Section 16 needs this column. Adding it now costs nothing.

The `address` and `mac` columns hold values that Bento assigned. See section 6.2.

Index `ssh_keys.fingerprint`, because the SSH frontend reads this column on every connection.

The `last_seen_at` column records the last SSH connection or HTTP request. Bento does not act on this column. The column lets a user find a forgotten instance in the `ls` output.

libvirt is authoritative for the run state. Poll `virConnectListAllDomains` every 30 seconds and update the `state` column. Subscribe to libvirt lifecycle events as well, because a poll misses a short transition.

### 12.1 Backup

Backup is the responsibility of the operator. Bento does not schedule, rotate, or copy a backup.

Bento does these three things instead:

1. Store the database at one documented path. The default path is `/var/lib/bento/bento.db`. Print this path in the startup log and show it on the dashboard.
2. Give the operator a "Download database" control on the dashboard and a `dump-db` subcommand. Both write a consistent copy with the SQLite backup API. Do not copy the file directly, because a write-ahead log makes a direct copy unsafe.
3. Document that the instance disks live in the storage directory, and that the operator backs up that directory.

## 13. Identity

The dashboard uses OIDC. Pocket ID is a suitable provider.

The command line interface uses SSH public key authentication. A new user registers by connecting to `ssh bento.foid.space`. The registration flow records the presented public key and asks for a name and an email address. Registration also allocates the subnet and the libvirt network of the user.

The HTTP proxy needs a session for private instances. Issue a cookie for the base domain after an OIDC login. The cookie is valid for every subdomain.

A token in the `tokens` table gives programmatic access.

## 14. Command line interface

The interface runs over SSH. The form is `ssh bento.foid.space <command> [arguments]`. The dashboard exposes the same operations.

| Command | Action |
| --- | --- |
| `ls` | List the instances of the user. Show the state, the address, the quota use, and the last use time. |
| `new <name>` | Create an instance. |
| `rm <name>` | Delete an instance. Ask for confirmation. |
| `start <name>` | Start a stopped instance. |
| `stop <name>` | Stop a running instance. |
| `restart <name>` | Restart an instance. |
| `rename <old> <new>` | Rename an instance. |
| `cp <source> <target>` | Copy a stopped instance. |
| `resize <name>` | Change the memory, the vCPU count, or the disk size. Warn that a restart is needed. |
| `console <name>` | Attach to the serial console. |
| `port <name> <port>` | Set the default HTTP port. |
| `visibility <name> <off\|private\|public>` | Set the visibility value. |
| `share <name> <user>` | Grant or revoke access for a second user. |
| `images` | List the allowlisted base images. |
| `ssh-key` | Add, list, or remove an SSH key. |
| `whoami` | Show the account and the quota of the user. |

The `new` command accepts `--image`, `--memory`, `--cpu`, and `--disk`.

The operator runs `bentod dump-db`, `bentod fetch-images`, and `bentod reconcile`.

The `console` command is new in this version. libvirt gives a serial console, and the previous version had no equivalent. A console is the only way into an instance with a broken network or a broken `sshd`.

## 15. Build order

1. Write the domain XML template and the libvirt client. Create, start, stop, and delete one hard-coded instance. Do not touch the database yet.
2. Add the image fetch, the overlay creation, and the `cloud-init` ISO. An instance now boots with a known address and accepts an SSH key.
3. Add the database, the users, and the quota check.
4. Add the per-user network, the address manager, and the nftables table.
5. Write the SSH frontend and the command line interface. The system is usable at this point without HTTP.
6. Get the wildcard certificate. Write the HTTP proxy with `off` and `public` instances.
7. Add OIDC, sessions, the dashboard, and `private` instances.

Step 4 comes before any second user. Until step 4, every instance shares one network.

Steps 1 and 2 are the new work in this version. The previous version got both from one Incus API call. Expect them to take longer than the rest of the list.

## 16. More than one host, deferred

Version 1 runs one host. This section records the design so that version 1 does not block it. Do not build this section in version 1.

libvirt reaches a remote host with a `qemu+ssh://host/system` connection URI. There is no cluster to form and no shared database. Each host is independent. This suits a set of machines that one operator owns.

The control plane changes little. The `hosts` table already exists. Each instance already carries a `host_id`.

The data plane is the work. The HTTP proxy and the SSH frontend run on one machine and must reach an instance on any host. Two options:

1. Route a distinct subnet to each host, then run a mesh between the hosts. WireGuard or Tailscale both do this.
2. Run a proxy on each host and forward from the front machine.

Option 1 is simpler, because the proxy keeps one routing table and no second hop.

Three more items need answers before this ships:

- **Placement.** Bento must pick a host for a new instance. A quota that is global and enforced per host needs a placement rule.
- **Copy across hosts.** Storage is local to a host. The `cp` command becomes a transfer.
- **Partial failure.** One host that is unreachable must not stop the whole deployment. The `ls` command must show a stale state rather than fail.

## 17. Open items

**virtio-mem for live memory change.** libvirt supports a `<memory model='virtio-mem'>` device with a `requested` size that changes while the guest runs, and a separate `current` size that reports what the guest accepted. This gives a reliable live change and reports partial application instead of hiding it. virtio-mem does not combine with balloon inflation, so adopting it means dropping the balloon path in section 5.2. Consider this after version 1 works.

**KSM is not configured.** Every instance boots from a small set of base images, so identical page rates should be high. KSM is a host setting rather than a libvirt setting. It costs CPU time and it has known side channel results, which matters more here because instances run untrusted code.

**IPv6 is not in version 1.**

**No automatic stop, ever.** An instance is a machine that a user depends on. An automatic stop breaks that. The reliability of the host still bounds this promise. Say so in the user documentation.

**Host reboot behavior is not specified.** Decide between three options. Start every instance that was running. Restore each instance to its last recorded state. Leave all instances stopped and let the start-on-connect flow handle the rest. libvirt has an autostart flag, and using it means libvirt rather than Bento decides.

**Nested virtualization is not specified.** A user who wants to run a hypervisor inside an instance needs the host CPU feature exposed. Decide whether this is a per-instance option or always on.

**Secure boot and TPM are not specified.** OVMF supports secure boot, and `swtpm` gives a virtual TPM. Some guest operating systems want both.

## Appendix A. OCI images, deferred

Bento does not consume OCI images. This appendix records the research.

The requirement splits in two:

- **Requirement A: run a registry image as a virtual machine.** A user names `docker.io/library/nginx` and gets a virtual machine.
- **Requirement B: define an instance base image as a Containerfile.** The operator builds a base image and users start instances from it.

**Option 1. Unpack the OCI image onto a partition and boot a prebuilt kernel.** This answers requirement A. Cost: Bento maintains a kernel and an initrd for each architecture. An OCI application image has no `sshd`, no user accounts, and often no init system. Bento must inject all three.

**Option 2. Use a container runtime conversion.** Incus converts an OCI image to an application container with `umoci` and `skopeo`, and the result has no kernel. This path does not produce a virtual machine. It is listed only to record that it was checked.

**Option 3. Use bootc.** This answers requirement B. A bootc image is an OCI image that contains a full operating system and a kernel. The `bootc-image-builder` tool converts it to a `qcow2` image, which drops straight into section 5.1. Cost: this is a build step, not a pull. The ecosystem is centered on Fedora, CentOS, and RHEL.

Option 3 fits this architecture better than it fit the previous one. Section 5.1 already consumes a `qcow2` file with a backing-file overlay. A bootc output is such a file. The work is a build pipeline, not a new instance path.

## Appendix B. What libvirt does not give you

The previous version used Incus. This table records what moved into Bento. Read it before you start, because it is the cost of the decision.

| Capability | Previous source | Now |
| --- | --- | --- |
| Tenancy | Incus projects | Section 6. Bento owns users and isolation. |
| Quota accounting and enforcement | Incus project limits | Section 6.1. Bento counts and enforces. |
| Network isolation | Incus network ACLs | Section 6.3. Bento writes nftables rules. |
| Address assignment | Incus bridge with DHCP | Section 6.2. Bento is the address manager. |
| Image supply and caching | Incus image server | Section 5.1. Bento downloads and verifies. |
| Copy-on-write instance creation | Incus storage driver | Section 5.1. Bento manages `qcow2` overlays. |
| Guest agent | Incus agent | `qemu-guest-agent`, which does less. |
| Instance copy | `incus copy` | Section 5.3. Bento copies an overlay. |
| A typed API | Incus Go client | Section 4.1. Bento generates and parses XML. |

What Bento gains in return:

- Direct control of the memory configuration. Section 5.2 is possible only here.
- Direct control of the firewall, with no second rule writer.
- A known address before boot, instead of address discovery.
- A serial console.
- A remote host model that needs no cluster. Section 16.
- No opinions from a layer above QEMU that Bento then has to work around.
