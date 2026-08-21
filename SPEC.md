# Bento System Specification

Version 0.9. Draft.

Bento is a self-hosted platform for Linux virtual machines. A user creates an instance from the command line interface or from the dashboard. Bento publishes the instance on the internet as a subdomain of a domain that the operator owns. An instance exists until the user deletes it.

Bento calls libvirt. Bento implements the parts that libvirt does not provide. These parts are tenancy, quota, addressing, firewall policy, and image supply. Appendix B lists what this decision moved into Bento.

## 1. Terms

| Term | Meaning |
| --- | --- |
| Bento | The system as a whole. The control plane performs any action that this document attributes to Bento, unless the text names another component. |
| instance | One virtual machine. One instance is one libvirt domain. |
| the host | A machine that runs `libvirtd` and holds instances. Version 1 supports one host. |
| the operator | The person who installs and runs Bento. |
| the user | A person with a Bento account. A user creates instances. |
| the control plane | The Bento server process. The binary is `bentod`. |
| the dashboard | The web interface of Bento. The control plane serves the dashboard at the base domain. |
| the command line interface | The text interface of Bento. The SSH frontend serves the command line interface. |
| the HTTP proxy | The component that terminates TLS and forwards HTTP requests to an instance. |
| the SSH frontend | The component that accepts SSH connections and forwards them to an instance. |
| an image | A named entry in the operator allowlist, such as `debian-13`. |
| an image version | One downloaded file for an image. A checksum identifies an image version. |
| the base domain | The domain that the operator chooses. This document uses `bento.foid.space`. |

## 2. Scope

Bento version 1 does the following:

- Create, list, start, stop, resize, and delete instances on one host.
- Publish an instance at `$NAME.bento.foid.space` over HTTPS when the user asks for it.
- Accept SSH connections to each instance through one public port.
- Support more than one user.
- Give each user a private network, a fixed address range, and a quota.
- Restore each instance to its last recorded state after a host reboot.

Bento targets fewer than about 20 users. This number is a design target, not a measured limit.

Version 1 runs the control plane and `libvirtd` on the same machine.

## 3. Non-goals

Bento does not implement virtualization. QEMU and KVM do this. libvirt manages them.

Bento does not run containers.

Bento does not consume OCI images in version 1. Section 18 makes this the first task after version 1.

Bento does not place instances across hosts. Version 1 has one host.

Bento does not bill users.

Bento does not use a network database. SQLite is sufficient at the scale in section 2.

**Bento never deletes an instance.** Only the user deletes an instance. Bento has no expiry timer, no grace period, and no idle detection. A quota is the limit on resource use. A clock is not.

Idle detection is the reason for this rule. Idle detection needs a definition of activity. Every such definition is wrong for some workload. A long build with no network traffic and no SSH session looks idle. A tool that deletes work is worse than a tool that runs out of disk.

## 4. Architecture

Three Bento processes and one libvirt daemon run on the host.

- **libvirtd** owns the domains, the storage, and the bridges. Bento calls the libvirt API at `qemu:///system` over the local socket.
- **The control plane** holds the database, applies policy, generates domain XML, and serves the dashboard. The control plane is the only component that writes to the database.
- **The HTTP proxy** listens on port 443 and on ports 3000 to 9999. The HTTP proxy terminates TLS. The HTTP proxy sends a request for the base domain to the control plane. The HTTP proxy sends every other request to an instance.
- **The SSH frontend** listens on port 22. The SSH frontend authenticates the client. The SSH frontend then opens a second SSH connection to the instance. The SSH frontend also serves the command line interface.

Build all three Bento processes as one binary with subcommands.

### 4.1 The libvirt client

Use `github.com/digitalocean/go-libvirt`. This library implements the libvirt RPC protocol in pure Go and needs no cgo. A static binary is easier to deploy.

Check the current state of both Go libraries before you commit. The official bindings at `libvirt.org/go/libvirt` cover more of the API. Those bindings need cgo. This recommendation is a judgment, not a measurement.

Bento generates domain XML from a Go template. Keep one template. Do not build a general XML object model. Bento uses a small and fixed subset of the schema.

### 4.2 Host requirements

1. Check for `/dev/kvm` at startup. Refuse to start without it.
2. Check that `libvirtd` answers on the local socket.
3. Check that the `qemu-img` and `xorriso` binaries exist.
4. Check that the image directory and the storage directory exist.
5. Check that both directories are writable.
6. Read `/sys/kernel/mm/ksm/run`. Warn if the value is 0. See section 5.4.
7. Read the nested virtualization module parameter. Warn if the parameter is off and any instance requests nested virtualization.

Run `bentod` as a user in the `libvirt` group. This group can create and control any domain on the host. Treat every string that reaches the domain XML as hostile. Escape every value that Bento writes into XML.

## 5. Hypervisor layer

One instance is one libvirt domain. Bento uses the `kvm` domain type.

The domain XML uses a fixed device set:

- One `virtio-blk` disk for the root volume.
- One `virtio` network interface on the network of the owner.
- One `virtio-rng` device backed by `/dev/urandom`.
- One `virtio` memory balloon with free page reporting on. See section 5.3.
- One `virtio` serial console.
- One `qemu-guest-agent` channel at `org.qemu.guest_agent.0`.
- UEFI firmware through OVMF.

Bento assigns the MAC address. Bento does not let libvirt generate the MAC address. Section 6.2 gives the reason.

Bento sets `<vcpu>` and the CPU model. The default CPU model is `host-model`. Section 5.5 changes the model for an instance that requests nested virtualization.

### 5.1 Images

**The image store is content addressed.** Bento stores an image version at a path derived from its checksum:

```
/var/lib/bento/images/sha256-<hex>.qcow2
```

This path never changes. This path never holds different content.

A `qcow2` overlay records the absolute path of its backing file inside the overlay. A backing file that moves destroys every overlay above it. A backing file that changes content does the same. Content addressing prevents both cases.

Never move the image directory after the first instance exists. Never write to a stored image version.

The durable operator allowlist is stored in SQLite. Configuration entries are merged into it, and an operator may append a bootc OCI entry at runtime from the dashboard or SSH CLI. A name cannot be reused for a different runtime source.

An image has one of two source kinds:

- `qcow2`: a download URL and an optional pinned checksum.
- `oci`: a bootc-compatible operating-system image reference. This is not an arbitrary OCI application container.

For a `qcow2` source, the `fetch-images` command does the following:

1. Download the file from the URL.
2. Compute the checksum.
3. Reject the file if the allowlist pins a checksum and the two do not match.
4. Return without action if a version with that checksum already exists.
5. Store the file at the content addressed path.
6. Insert a row in `image_versions`.
7. Mark the new row as the current version of that image.

An unpinned image is trusted on first use. Bento stores a later version under its own checksum and marks it current. Bento then logs a warning that names both checksums. A change is not an error, because a distribution republishes a cloud image as normal practice. Pin the checksum for any image where a change is not acceptable.

For an `oci` source, Bento pulls it with rootful Podman, records the resolved source digest, and runs the configured image-builder container privileged to produce a qcow2 artifact. It computes the output checksum and then uses steps 4-7 above. A previously successful build of the same image name and source digest is reused. Runtime addition performs this build immediately; `fetch-images` refreshes both source kinds.

**Never delete an image version while an overlay depends on it.** A new instance uses the current version. An existing instance keeps the version that Bento built it from. Delete an image version only when no row in `instances` carries its checksum. The `fetch-images` command runs this collection at the end. This deletion is safe because the condition is exact. Compare the reconciliation in section 6.1, where the condition is not exact.

The `images` command shows each image name and its current checksum. The command also shows the number of instances that hold an older version.

### 5.2 First boot

Create the root volume as a copy-on-write overlay:

1. Create a `qcow2` file with the current image version as its backing file.
2. Resize the overlay to the requested disk size.
3. Record the backing checksum in the `instances` table.

Bento configures the first boot with `cloud-init` and the NoCloud data source. Build an ISO with a `meta-data` file and a `user-data` file. Attach the ISO to the domain as a read-only CD-ROM. The `user-data` file does the following:

- Set the host name to the instance name.
- Create one user account.
- Install the public keys of the owner.
- Set the static address, the gateway, and the DNS server. See section 6.2.
- Install and start `qemu-guest-agent` for a traditional cloud image. A bootc image must already contain the package because its `/usr` is immutable; first boot only enables it.

A bootc OCI image must contain a kernel, `cloud-init` with the NoCloud data source, and `qemu-guest-agent`. This is the image author's contract. Bento uses the same cloud-init seed and instance lifecycle after conversion; there is no separate boot path.

Detach and delete the ISO after the first successful boot. The ISO holds the public keys of the owner. The ISO does not need to stay attached.

### 5.3 Memory

Set the memory balloon to report free pages:

```xml
<memballoon model='virtio' freePageReporting='on'/>
```

Free page reporting lets the guest return unused pages to the host. The host reclaims those pages. The host faults them back on next access.

Without this setting, a guest fills unused memory with page cache and never releases it. Host memory use then climbs to the configured limit and stays there.

**Do not inflate the balloon to change the memory of a running instance.** A balloon-driven decrease is slow and often does not reach the target. A memory change in version 1 edits the domain XML and restarts the instance. A restart is a clear event that a user can plan. A partial and silent decrease is not.

Overcommit is an operator setting. The default ratio is 1.0, which means no overcommit. Free page reporting and KSM both make a higher ratio workable. Two conditions apply before you raise the ratio:

1. Give the host swap or `zram`. Without either, the host out-of-memory killer terminates a QEMU process, and one user loses a machine with no warning.
2. Monitor the resident set size of every QEMU process. Alert on the total.

### 5.4 Kernel same-page merging

Turn KSM on. Every instance boots from one of a small set of image versions. Two instances that run the same distribution hold many identical pages. KSM merges those pages and returns the memory to the host.

KSM needs two things:

1. The host must run KSM. Set `/sys/kernel/mm/ksm/run` to 1. Run `ksmtuned`, or set `pages_to_scan` and `sleep_millisecs` by hand.
2. The guest memory must be marked mergeable. QEMU marks it by default. libvirt suppresses the mark only when the domain XML contains `<memoryBacking><nosharepages/></memoryBacking>`. Bento does not emit that element by default.

KSM and free page reporting solve different halves of the same problem. Free page reporting returns memory that the guest is not using. KSM merges memory that two guests use for the same content.

KSM costs CPU time in the `ksmd` kernel thread. Measure that cost. Lower `pages_to_scan` if the cost is high.

**This is an accepted security trade.** Page deduplication across a security boundary has known side channel results. Section 5 chose virtual machines so that an instance can run untrusted code. KSM weakens that boundary. Bento accepts the trade at the scale in section 2, where the users know each other. Record the trade in the operator documentation.

Give a per-instance opt-out. An instance with `ksm=false` gets `<nosharepages/>` in its domain XML.

Revisit this decision if Bento serves users who do not trust each other.

### 5.5 Nested virtualization

Nested virtualization is a per-instance setting. The default value is off.

An instance with `nested=true` gets `<cpu mode='host-passthrough'/>` instead of `host-model`. The host must also load the KVM module with nesting on. The parameter is `kvm_intel.nested=1` or `kvm_amd.nested=1`.

Reject a `new` or `resize` command that requests nested virtualization when the host has nesting off. Give the module parameter in the error message.

Two costs come with this setting:

1. The `host-passthrough` model ties the domain to the exact host CPU. This prevents migration to a host with a different CPU. Section 17 needs that migration.
2. Nested virtualization adds attack surface in KVM. An instance that does not need it should not have it. This is the reason for the default.

## 6. Multi-user model

libvirt has no tenancy concept. Bento implements tenancy.

### 6.1 Quota

Each user has four limits. These limits are the instance count, the total vCPU count, the total memory, and the total virtual disk size.

Enforce a limit in the control plane. Run the check and the insert in one SQLite transaction. Two concurrent `new` commands must not both pass a check when only one instance fits.

Bento accounts against the database. libvirt is authoritative for what exists. The two records can disagree after a crash. The `reconcile` command reports the disagreement:

1. List every domain on the host.
2. Report every domain with no matching row in the `instances` table.
3. Report every row with no matching domain.

**The `reconcile` command reports and never deletes.** A reconciliation bug that deletes a domain is worse than a row that is wrong. The operator reads the report and corrects the disagreement by hand. At the scale in section 2 this report is short. An automatic cleanup can come later.

Report quota use in the `ls` command and on the dashboard.

### 6.2 Networks and addressing

Give each user one libvirt network. Use `<forward mode='open'/>`. This mode creates the bridge and installs no firewall rules, so Bento owns the whole policy. A NAT network installs libvirt rules that Bento must then work around.

Assign each user a `/24` from a private range. Bento is the address manager. Bento selects the address for an instance at creation time. Bento writes that address into the `cloud-init` network configuration.

Static assignment is better than address discovery here. There is no DHCP server, no lease file, and no wait for the guest agent. Bento knows the address of an instance before the instance boots. The HTTP proxy and the SSH frontend can therefore reach an instance that has never started.

Bento also assigns the MAC address from the locally administered range. A fixed MAC keeps the interface name stable in the guest across a restart.

### 6.3 Firewall

Bento writes one nftables table. Bento owns every rule in that table. Do not edit these rules by hand.

The rules do the following:

1. Permit traffic from the host to any instance on port 22 and on the published HTTP ports.
2. Permit egress from any instance to the internet.
3. Drop traffic between the bridges of two different users.
4. Permit traffic between two instances of the same user.
5. Masquerade instance egress behind the address of the host.

Rule 4 is a decision, not an oversight. A user who runs a database instance and a web instance expects the two to communicate. The isolation boundary in version 1 is the user, not the instance.

Reload the whole table on every change. A partial rule update leaves a period with the wrong policy.

## 7. Names and DNS

### 7.1 DNS records

Create these DNS records:

1. Create an `A` record for `bento.foid.space` that points to the host.
2. Create an `A` record for `*.bento.foid.space` that points to the host.

IPv6 is not in version 1.

Every name resolves to the same address, so a stale DNS cache never sends a request to the wrong machine. The HTTP proxy resolves the name to an instance on every request.

### 7.2 The name lifecycle

An instance name is unique across the deployment, not per user. The name appears in the URL and in the SSH user name. The name is therefore public.

**A name is not an identifier.** Bento identifies an instance by the libvirt domain UUID. The name is a label. A user can change a name and can release a name. Every table that refers to an instance uses the UUID.

This rule matters most for `shares`. A share keyed on a name remains after a delete. That share then grants the old holders access to a later instance with the same name. A share keyed on the UUID cannot do this.

Deleting an instance releases its name into a cooldown. Renaming an instance releases the old name into the same cooldown.

The cooldown rules are:

1. The owner who released a name may take that name again at once.
2. Any other user must wait for the cooldown to expire.
3. The default cooldown is 24 hours. Make the value an operator setting.

Rule 1 covers the normal case. A user rebuilds a machine and wants the same URL.

Rules 2 and 3 address a different case. One user watches for a released name. That user then claims the name to receive traffic that was meant for someone else.

The cooldown is not a security control. The cooldown reduces an opportunity. A user who wants a name that another user held can still take it after a day.

### 7.3 Rename

The `rename` command is allowed on an instance with any visibility value. The command asks for confirmation when the visibility is `public`. State two facts in the prompt. Every existing link to the old URL stops working. The SSH user name also changes.

**Bento does not redirect the old name.** An alias table keeps the old name resolvable forever. Section 9.2 requires that an `off` instance and a name that does not exist give the same response. An alias table defeats that requirement. A visitor could then probe old names and map which names once existed.

The old name enters the cooldown in section 7.2.

## 8. TLS

Get one wildcard certificate for `*.bento.foid.space` and `bento.foid.space`. Use the ACME DNS-01 challenge. A wildcard certificate requires the DNS-01 challenge.

The first reason is the Certificate Transparency logs. A per-instance certificate publishes the name of every instance to a public log. A wildcard certificate publishes only the base domain.

The second reason is the Let's Encrypt rate limit. Let's Encrypt limits new certificates to 50 per registered domain per 7 days. The limit refills at one certificate every 202 minutes. Bento does not delete instances automatically, so normal use stays under this limit. The limit applies during development. Check the current limit before you build.

A wildcard certificate also makes `rename` cheap. A per-instance certificate needs a new issuance on every rename.

The control plane needs write access to the DNS zone. Use an API token that is limited to the `_acme-challenge` records, if the DNS provider supports such a limit.

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

The HTTP 404 response for `off` matches the response for a name that does not exist. The response also matches the response for a name in the cooldown from section 7.2. This hides the existence of the instance.

Ports 3000 to 9999 are always private. The `public` value applies only to the default HTTP port.

### 9.3 Unavailable instance

The HTTP proxy returns HTTP 503 when the instance is not ready. This covers a `starting` state, a restart, and a refused connection on the target port.

Do not hold the request until the instance answers. A held request gives the user no information and no error.

Serve a Bento error page with the 503 response. Name the instance and the state on that page. Set the `Retry-After` header to 5 seconds. Section 14.5 defines the appearance of this page.

## 10. SSH frontend

The user connects with `ssh $NAME@bento.foid.space`.

SSH has no Server Name Indication field. The instance name must therefore travel in the SSH user name field.

The SSH frontend does the following:

1. Accept the connection and read the client public key.
2. Look up the public key in the database to find the user.
3. Offer an unknown key a link instead of a session. See section 13.
4. Read the SSH user name and treat it as the instance name.
5. Resolve the name to an instance UUID.
6. Check that the user owns the instance or has a share for the UUID.
7. Start the instance if the observed state is `stopped`.
8. Wait for `sshd` in the instance to accept a connection.
9. Open a new SSH connection to the instance on the internal address.
10. Join the two connections.

Write the SSH frontend in Go with `gliderlabs/ssh`.

Steps 7 and 8 let a user connect to a stopped instance without a separate command. Write a line such as `bento: starting $NAME` to the SSH session before the wait. Set a timeout of 120 seconds. Report a clear failure after the timeout.

Step 7 does not change the desired state. See section 11.2. A user who stopped an instance and then connects to it gets a running instance. A later host reboot returns that instance to `stopped`.

The SSH frontend presents the same host key for every instance. A rename or a name reuse therefore produces no `known_hosts` warning. This is a consequence of the design in section 10, not a defect. The authorization check in step 6 protects the instance.

The SSH frontend also answers `ssh bento.foid.space` with no user name. This session runs the command line interface.

## 11. Instance lifecycle

### 11.1 States and actions

An instance has an observed state and a desired state. Both hold one of these values: `running` or `stopped`. The observed state also holds `starting`.

The observed state comes from libvirt. The desired state comes from the last user action. The two differ during a change, after a host reboot, or after a guest shuts itself down.

| Action | libvirt call | Desired state after |
| --- | --- | --- |
| `new` | `virDomainDefineXML`, then `virDomainCreate` | `running` |
| `start` | `virDomainCreate` | `running` |
| `stop` | `virDomainShutdown`, then `virDomainDestroy` | `stopped` |
| `restart` | `virDomainReboot` | `running` |
| `rm` | `virDomainDestroy`, then `virDomainUndefine` | Bento deletes the row. |

The `stop` action sends an ACPI shutdown request first. Wait 60 seconds. Call `virDomainDestroy` only after the timeout. Report which path the stop took.

A `resize` action that changes memory, vCPU count, or the nested virtualization setting edits the XML. That change needs a restart. Tell the user before the change. A `resize` action that grows the disk edits the overlay. The guest sees that change after a restart.

The `rm` command does four things in order:

1. Destroy and undefine the domain.
2. Delete the overlay file.
3. Delete every share for the UUID.
4. Insert the name into `released_names`.

Step 3 is required. Section 7.2 gives the reason.

The `rm` command asks for confirmation. Accept a `--force` flag for scripts.

### 11.2 Host reboot

**Bento restores each instance to its last recorded desired state.** An instance that a user left running comes back running. An instance that a user stopped stays stopped.

Bento performs this restore. libvirt does not. Clear the libvirt autostart flag on every domain that Bento creates. One component decides, and that component is the control plane. Two components that both start domains produce a race that is hard to observe and harder to reproduce.

At startup the control plane does the following:

1. Read the observed state of every domain from libvirt.
2. Select every instance with desired state `running` and observed state `stopped`.
3. Start those instances in batches.
4. Wait for each batch to reach `running` before the next batch.

Start the instances in batches for a reason. A host with 20 instances that start at once produces a large memory demand and a large number of disk reads at the same moment. The default batch size is 4. Make the value an operator setting.

Report progress in the startup log and on the dashboard. A user who connects during the restore sees `starting`, not an error.

## 12. Data model

Use SQLite with write-ahead logging.

| Table | Columns |
| --- | --- |
| `users` | `id`, `name`, `email`, `oidc_subject`, `subnet`, `created_at` |
| `quotas` | `user_id`, `max_instances`, `max_vcpu`, `max_memory`, `max_disk` |
| `ssh_keys` | `id`, `user_id`, `public_key`, `fingerprint`, `comment`, `created_at` |
| `pairings` | `id`, `token_hash`, `public_key`, `fingerprint`, `comment`, `created_at`, `expires_at`, `linked_user_id` |
| `hosts` | `id`, `name`, `libvirt_uri`, `created_at` |
| `images` | `name`, `url`, `pinned_checksum`, `current_checksum` |
| `image_versions` | `checksum`, `image_name`, `path`, `size`, `fetched_at` |
| `instances` | `uuid`, `name`, `owner_id`, `host_id`, `image_name`, `base_checksum`, `state`, `desired_state`, `address`, `mac`, `vcpu`, `memory`, `disk`, `nested`, `ksm`, `http_port`, `visibility`, `created_at`, `last_seen_at` |
| `shares` | `instance_uuid`, `user_id`, `created_at` |
| `released_names` | `name`, `previous_owner_id`, `released_at` |
| `tokens` | `id`, `user_id`, `hash`, `expires_at` |

The `uuid` column is the primary key of `instances`. The `name` column has a unique index. Section 7.2 gives the reason for the split.

**The `shares` table keys on a UUID, not on a name.** A share keyed on a name remains after a delete and grants access to the next instance with that name.

The `base_checksum` column links an instance to the exact image version that Bento built it from. Section 5.1 uses this column to decide which image versions it can delete.

The `state` column holds the observed state. libvirt is authoritative for that column. The `desired_state` column holds the last user action. Bento is authoritative for that column. Section 11.2 uses both.

The `host_id` column exists in version 1 and always points to the one host. Section 17 needs this column.

The `released_names` table enforces the cooldown in section 7.2. The table also records what a name used to be. Keep a row after the cooldown expires and compare the timestamp. A kept row is more useful and costs nothing at this scale.

Index `ssh_keys.fingerprint`. The SSH frontend reads this column on every connection.

The `pairings` table holds pending key links from section 13. Only the hash of the link token is stored, as for `tokens.hash`. A row with `linked_user_id` set is spent. Unused rows are swept once they expire, so the table stays the size of the links in flight.

The `last_seen_at` column records the last SSH connection or HTTP request. Bento does not act on this column. The column lets a user find a forgotten instance in the `ls` output.

Poll `virConnectListAllDomains` every 30 seconds and update the `state` column. Subscribe to libvirt lifecycle events as well. A poll misses a short transition.

### 12.1 Backup

Backup is the responsibility of the operator. Bento does not schedule, rotate, or copy a backup.

Bento does these three things instead:

1. Store the database at one documented path. The default path is `/var/lib/bento/bento.db`. Print this path in the startup log and show it on the dashboard.
2. Give the operator a "Download database" control on the dashboard and a `dump-db` subcommand. Both write a consistent copy with the SQLite backup API. Do not copy the file directly. A write-ahead log makes a direct copy unsafe.
3. Document that the instance disks live in the storage directory. The operator backs up that directory. An overlay is useless without its backing file, so the backup must also cover the image directory.

## 13. Identity

The dashboard uses OIDC. Pocket ID is a suitable provider.

OIDC is the only way an account comes into existence. A verified login for a subject no `users` row carries creates that row, deriving the account name from `preferred_username`, then the local part of the email, then the display name, reduced to lowercase letters, digits, and inner hyphens; a taken name is suffixed `-2`, `-3`. Account creation also allocates the subnet and the libvirt network of the user. The identity provider therefore decides who has an account. Setting `allow_signup = false` under `[oidc]` refuses logins from identities that have no row yet, which freezes the user list.

The command line interface uses SSH public key authentication. A key is attached to an account by linking, not by registering:

1. A key the `ssh_keys` table does not know connects to `ssh bento.foid.space`.
2. The frontend records the key in `pairings` with a random link token, stores only the hash of that token, and prints the URL and the key's fingerprint. Nothing else is created: no user, no subnet, no network. The frontend can therefore answer the public internet without registration being open.
3. The link expires in three minutes and works once. The session waits for it, so the terminal reports the result.
4. Opening the link requires a session, so a first-time user goes through OIDC and gets an account on the way past.
5. The page shows the fingerprint and the account name. Only a `POST` from that page attaches the key. A link that attached a key by being visited would let one user's link, sent to another, take over that account, and would fire on anything that follows links; the session cookie is `SameSite=Lax`, so a cross-site submission arrives without it and is refused.

An account with no keys is normal — it is what a dashboard-only user has. Keys are added by linking again from an already signed-in browser.

The HTTP proxy needs a session for private instances. Issue a cookie for the base domain after an OIDC login. The cookie is valid for every subdomain.

The cookie identifies the user and nothing else. Authorization runs on every request against the owner and the shares of the instance. A cookie held from before a name changed hands therefore grants nothing.

A token in the `tokens` table gives programmatic access.

## 14. Dashboard

The dashboard exposes every operation in section 15. The dashboard is not a separate product. A user who prefers a browser must not lose a capability.

The bundle assumes a session and has no sign-in of its own, so a request without one is answered before it reaches the bundle. A visitor who already has a session at the provider should not have to click anything: answer the first such request with a `prompt=none` authorization request, which returns either a code or `error=login_required` without showing the visitor anything. A refusal renders a sign-in page rather than an error, and sets a short-lived cookie so the next request goes straight there instead of to the provider again. Logging out sets the same cookie: the provider's session outlives Bento's, and without it the next request would silently sign the user back in and the logout would appear to do nothing. Built assets are served without any of this.

### 14.1 Stack

Use shadcn/ui. Apply the preset `b3DooLR16I`. Record this identifier in the repository, because the preset is the source of the component tokens.

shadcn/ui needs React, Tailwind CSS, and Radix primitives. This adds a Node build step to a project that is otherwise one Go binary. Resolve the conflict at build time, not at run time:

1. Build the dashboard assets in continuous integration.
2. Embed the built assets with `go:embed`.
3. Serve them from the control plane.

The deployed artifact stays one binary with no Node runtime.

Radix supplies keyboard navigation and focus management. Do not replace a Radix primitive with a plain `div`. An infrastructure tool gets used by keyboard.

### 14.2 Color

Use Catppuccin. Latte is the light palette. Mocha is the dark palette.

1. Follow the operating system preference on first load.
2. Give the user a manual override.
3. Store the override in `localStorage`.

The accent color is Mauve. This is a choice, not a requirement of the palette. Change it in one place.

Map instance state to a named Catppuccin color. Use the same name in both palettes.

| State | Color |
| --- | --- |
| `running` | Green |
| `starting` | Yellow |
| `stopped` | Overlay1 |
| error | Red |

Never use color alone to carry state. Pair each color with a text label. A user with a color vision deficiency reads the label.

Check the contrast ratio of Yellow and Peach on the Latte base. Both are low contrast against a light background. Use a darker text color for a warning in Latte, or move the color to a border instead of the text.

### 14.3 Typography

Headings use IBM Plex Mono. All other text uses IBM Plex Sans.

Also use IBM Plex Mono for any value that a user can type, copy, or compare. These values include instance names, addresses, MAC addresses, checksums, image names, and command examples. A monospace font makes a transposed character visible.

Self-host both fonts. Do not load a font from a third-party CDN. A dashboard that reaches an external host on every load contradicts the purpose of a self-hosted tool. Subset the fonts to the Latin range and serve `woff2`.

Declare `font-display: swap`. The dashboard must render before the fonts arrive.

### 14.4 Layout

A table is the primary view. This tool manages a list of machines, and a user compares rows.

The instance table shows the name, the observed state, the address, the image, the visibility, and the last use time. Sort by name by default.

Show the quota of the user above the table. Show the used amount and the limit for all four limits in section 6.1.

Every view needs three states beyond the normal one:

1. A loading state.
2. An empty state that says what to do next.
3. An error state that names the failure.

A destructive action needs a confirmation dialog. The dialog names the instance. The `rm` and `rename` commands are destructive. Section 7.3 lists what the `rename` dialog must state.

### 14.5 The error page

The HTTP proxy serves the 503 page in section 9.3. The dashboard does not serve it.

This page must render without JavaScript. Write it as static HTML with inline CSS. Use the same palette and the same self-hosted fonts. The page must work when the dashboard bundle is unavailable.

The page names the instance and the state. The page does not name the owner. A visitor to a `public` instance must not learn who owns it.

## 15. Command line interface

The interface runs over SSH. The form is `ssh bento.foid.space <command> [arguments]`.

| Command | Action |
| --- | --- |
| `ls` | List the instances of the user. Show the state, the address, the quota use, and the last use time. |
| `new <name>` | Create an instance. |
| `rm <name>` | Delete an instance. Ask for confirmation. |
| `start <name>` | Start a stopped instance. |
| `stop <name>` | Stop a running instance. |
| `restart <name>` | Restart an instance. |
| `rename <old> <new>` | Rename an instance. Ask for confirmation when the visibility is `public`. |
| `cp <source> <target>` | Copy a stopped instance. |
| `resize <name>` | Change the memory, the vCPU count, or the disk size. Warn that a restart is needed. |
| `console <name>` | Attach to the serial console. |
| `port <name> <port>` | Set the default HTTP port. |
| `visibility <name> <off\|private\|public>` | Set the visibility value. |
| `share <name> <user>` | Grant or revoke access for a second user. |
| `images` | List the images, their source kinds, current checksums, and how many instances hold an older version. |
| `images add <name> <oci-reference>` | Operator only. Append and immediately build a bootc OCI image. |
| `ssh-key` | Add, list, or remove an SSH key. |
| `whoami` | Show the account and the quota of the user. |

The `new` command accepts `--image`, `--memory`, `--cpu`, `--disk`, `--nested`, and `--no-ksm`.

The operator runs `bentod dump-db`, `bentod fetch-images`, and `bentod reconcile`.

A `new` command fails when it names a released name that belongs to another user. Report the remaining cooldown in the error message.

## 16. Build order

1. Write the domain XML template and the libvirt client. Create, start, stop, and delete one hard-coded instance. Do not use the database yet.
2. Add the content addressed image store, the overlay creation, and the `cloud-init` ISO. An instance now boots with a known address and accepts an SSH key.
3. Add the database, the users, and the quota check.
4. Add the per-user network, the address manager, and the nftables table.
5. Add the desired state column and the host reboot restore in section 11.2.
6. Write the SSH frontend and the command line interface. The system is usable at this point without HTTP.
7. Get the wildcard certificate. Write the HTTP proxy with `off` and `public` instances.
8. Add OIDC and sessions. Add `private` instances.
9. Write the dashboard.

Step 4 comes before any second user. Until step 4, every instance shares one network.

Steps 1 and 2 are the new work in this version. The previous version obtained both from one Incus API call. Expect them to take longer than the rest of the list.

Turn KSM on during step 1 and leave it on. A memory setting that arrives late gives misleading measurements early.

Use the UUID as the instance key from step 3. A change after `shares` exists needs a data migration. Such a migration can leave one lookup on the name by mistake.

## 17. More than one host, deferred

Version 1 runs one host. This section records the design so that version 1 does not prevent it. Do not build this section in version 1.

libvirt reaches a remote host with a `qemu+ssh://host/system` connection URI. There is no cluster to form and no shared database. Each host is independent. This model suits a set of machines that one operator owns.

The control plane changes little. The `hosts` table already exists. Each instance already carries a `host_id`.

The data plane is the work. The HTTP proxy and the SSH frontend run on one machine. Both must reach an instance on any host. There are two options:

1. Route a distinct subnet to each host. Run a mesh between the hosts. WireGuard and Tailscale both do this.
2. Run a proxy on each host. Forward from the front machine to that proxy.

Option 1 is simpler. The proxy keeps one routing table and adds no second hop.

Four more items need answers before this ships:

- **Placement.** Bento must select a host for a new instance. A quota that is global and enforced per host needs a placement rule.
- **Copy across hosts.** Storage is local to a host. The `cp` command becomes a transfer.
- **Image distribution.** The content addressed store in section 5.1 is per host. Each host fetches the same versions, or one host serves the others.
- **Partial failure.** One unreachable host must not stop the whole deployment. The `ls` command must show a stale state rather than fail.

## 18. After version 1

This list is ordered. Do the first item first.

### 18.1 bootc images, version 1.1 (implemented)

Support a base image published as a bootc-compatible registry image.

A bootc image is an OCI image that holds a full operating system and a kernel. The image-builder tool converts it to a `qcow2` file. Section 5.1 already stores a `qcow2` file by checksum. Section 5.2 already creates an overlay from such a file. The work is a build pipeline, not a new instance path.

The steps are:

1. Let the operator name a registry image in the configuration or append one at runtime.
2. Run image-builder for `qcow2`. This tool runs privileged under `podman`.
3. Compute the checksum of the output and store it as an image version.
4. Record the source image digest next to the checksum.

The implementation answers the two design questions as follows:

- **First boot.** The image contract requires `cloud-init`, NoCloud, and `qemu-guest-agent` in the OCI image.
- **Size and time.** Builds happen in `fetch-images` or synchronously when an operator appends a runtime entry, never in `new`.

The bootc update model applies a new image in place from a registry. That model is out of scope. Bento treats a bootc output as a static image version.

### 18.2 virtio-mem for live memory change

libvirt supports a `<memory model='virtio-mem'>` device. This device has a `requested` size that changes while the guest runs. The device also has a `current` size that reports what the guest accepted. This gives a reliable live change and reports a partial application.

virtio-mem does not combine with balloon inflation. Section 5.3 does not inflate the balloon, so the conflict is small. Confirm that free page reporting still works with virtio-mem before you switch.

### 18.3 More than one host

See section 17.

### 18.4 A serial console in the dashboard

Section 15 gives a serial console over SSH. The dashboard has no equivalent. A console in the browser needs `xterm.js` and a WebSocket. This matters most when the network of an instance is broken, which is the case where the SSH frontend cannot help either.

### 18.5 Secure boot and a virtual TPM

OVMF supports secure boot. The `swtpm` package gives a virtual TPM. Some guest operating systems want both. No user has asked. Build this when one does.

### 18.6 IPv6

A separate public address for each instance removes the user name method in section 10. This costs one address for each instance. It also removes the single point of control for SSH access.

## 19. Open items

**The Go libvirt library choice is not settled.** Section 4.1 recommends `go-libvirt` because it builds without cgo. Confirm that it covers every call in section 11 before you commit.

**The cooldown period is a guess.** Section 7.2 sets 24 hours with no evidence. Pick a number and watch for complaints.

**The image store has no lock.** A `fetch-images` collection can delete a version while a `new` command reads it. Take a lock around image version creation and deletion. An alternative is to run the collection only when no create is in progress.

**The disk quota counts virtual size, not real size.** A user with a 100 GiB quota can create ten 10 GiB instances that together use 4 GiB on disk. The quota is a worst case bound. Decide whether that is the number to show a user, or whether `ls` shows both numbers.

**The shadcn/ui preset contents are not recorded here.** Section 14.1 names the identifier `b3DooLR16I`. Commit the generated token file to the repository. A preset that lives only in a hosted tool is a dependency that can disappear.

## Appendix A. Not in version 1

**Running a registry image as a virtual machine.** A user names `docker.io/library/nginx` and gets a virtual machine. This needs an unpacked image on a partition. It also needs a kernel and an initrd that Bento supplies. The initrd mounts the root filesystem, pivots, and runs the entry point. An OCI application image has no `sshd`, no user accounts, and often no init system. Bento must inject all three.

This is a different requirement from section 18.1. Section 18.1 defines a base image with a Containerfile. This item runs an arbitrary registry image. Do not confuse the two. Do not build this one without a user who wants it.

**Converting an OCI image with a container runtime.** Incus converts an OCI image to an application container with `umoci` and `skopeo`. The result has no kernel. This path does not produce a virtual machine. This entry exists so that nobody checks the same path twice.

## Appendix B. What libvirt does not give you

The previous version used Incus. This table records what moved into Bento. Read it before you start. It is the cost of the decision.

| Capability | Previous source | Now |
| --- | --- | --- |
| Tenancy | Incus projects | Section 6. Bento owns users and isolation. |
| Quota accounting and enforcement | Incus project limits | Section 6.1. Bento counts and enforces. |
| Network isolation | Incus network ACLs | Section 6.3. Bento writes nftables rules. |
| Address assignment | Incus bridge with DHCP | Section 6.2. Bento is the address manager. |
| Image supply, versioning, and caching | Incus image server | Section 5.1. Bento downloads, verifies, and stores by checksum. |
| Copy-on-write instance creation | Incus storage driver | Section 5.2. Bento manages `qcow2` overlays. |
| Guest agent | Incus agent | `qemu-guest-agent`, which does less. |
| Instance copy | `incus copy` | Bento copies an overlay. |
| Restart after a host reboot | Incus instance startup | Section 11.2. Bento holds the desired state. |
| A typed API | Incus Go client | Section 4.1. Bento generates and parses XML. |

Bento gains these things in return:

- Direct control of the memory configuration. Sections 5.3 and 5.4 need it.
- Per-instance nested virtualization. Section 5.5.
- Direct control of the firewall, with no second component writing rules.
- A known address before boot, instead of address discovery.
- A serial console.
- A remote host model that needs no cluster. Section 17.
- No decisions from a layer above QEMU that Bento must then work around.
