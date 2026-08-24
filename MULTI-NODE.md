# Bento Multi-Node Design

Status: proposed design for the post-version-1 multi-node work described in
SPEC section 17.

This document defines one Bento deployment spanning more than one machine that
runs libvirt. It replaces the sketch in SPEC section 17 when implementation
begins. Until then, the single-host behavior in SPEC remains authoritative.

## 1. Summary

Bento keeps one control plane, one SQLite database, one public HTTP proxy, and
one public SSH frontend. Each machine that runs guests runs a small Bento
runner service beside its local libvirt daemon. The control plane selects a
runner and sends host-local work to that service. A runner owns its libvirt
connection, image cache, instance storage, user bridges, routes, and nftables
policy.

Every user keeps one `/24`. Bento divides every user `/24` into the same fixed
number of runner slots:

| Anticipated runners | Slots | Slot prefix |
| ---: | ---: | ---: |
| 1 | 1 | `/24` |
| 2 | 2 | `/25` |
| 3-4 | 4 | `/26` |
| 5-8 | 8 | `/27` |

The setup question is "How many VM runners do you anticipate having?" Bento
rounds the answer up to the next supported slot count. The chosen slot prefix
is persisted in the database. Changing it later is an explicit maintenance
operation, not an ordinary configuration reload.

A runner may own more than one slot, but a slot has exactly one active owner.
An instance address must come from a slot owned by its `host_id`. This keeps
routes stable through ordinary create and delete operations and makes a slot,
rather than an individual address, the unit of runner evacuation.

The runner transport is any routed, mutually reachable IP network. A LAN,
WireGuard, Tailscale, and an equivalent routed VPN are all valid. Bento does
not depend on one VPN product and does not create the underlay.

## 2. Goals

The multi-node design must:

1. Run instances on as many as eight active runner slots.
2. Preserve each user's `/24` addresses, gateway, and unicast connectivity.
3. Keep names, authorization, shares, quota, and public routing global to the
   deployment.
4. Keep instance disks and image caches local to runners by default.
5. Let one unreachable runner degrade independently without taking down the
   control plane or healthy runners.
6. Preserve the rule that Bento never deletes an instance without a user
   request.
7. Keep libvirt and privileged host mutation behind narrow, testable seams.
8. Support the controller machine as a runner without giving it a special data
   model.
9. Work over a trusted LAN or a routed VPN.
10. Make runner-count changes possible, deliberate, and recoverable.

## 3. Non-goals

The first multi-node release does not provide:

- high availability for the control plane or SQLite database;
- live migration;
- automatic failover after runner loss;
- a distributed block store or shared filesystem;
- more than eight simultaneously owned network slots;
- transparent Ethernet broadcast or multicast across runners;
- automatic creation or administration of a LAN, WireGuard, or Tailscale
  network;
- placement across unlike CPU architectures;
- placement of a nested-virtualization instance on a runner with an
  incompatible CPU.

The control plane remains a deliberate single point of coordination. Existing
guests continue running when it is down, but lifecycle operations, the public
frontends, and state convergence are unavailable.

## 4. Terms

**Controller** is the machine running `bentod serve` and holding SQLite.

**Frontend** means the public HTTP proxy and SSH frontend. They normally run
on the controller machine but are logically separate from it.

**Runner** is a machine that runs `bentod runner`, libvirt, and zero or more
instances.

**Underlay** is the routed IP network joining the controller and runners. It
may be a LAN or a routed VPN.

**User network** is the existing libvirt bridge and `/24` assigned to one
user.

**Runner slot** is one equal subprefix of every user `/24`. Slot numbers are
deployment-wide and stable. If the deployment uses `/26` slots, slot 2 means
the third `/26` of every user network.

**Slot owner** is the runner currently responsible for all instance addresses
inside that slot.

## 5. Invariants

The implementation must preserve these invariants in transactions where the
database is involved:

1. One instance UUID identifies one database row and at most one libvirt
   domain across all runners.
2. One instance name is unique across the deployment.
3. One instance address is unique across the deployment.
4. Every instance address belongs to its owner's `/24`.
5. Every instance address belongs to exactly one configured runner slot.
6. The instance's `host_id` owns that runner slot.
7. One slot has at most one active owner.
8. A runner may own multiple slots.
9. A runner only defines domains whose rows name that runner.
10. A runner only stores overlays whose instance rows name that runner.
11. Global user quota is checked before runner mutation.
12. An unreachable runner never causes its instances to be reported as
    stopped merely because they could not be observed.
13. Route ownership never moves automatically after a health-check failure.
14. Cross-user traffic remains denied on every runner.

## 6. Topology

```text
                         public DNS
                              |
                    +---------+---------+
                    | HTTP proxy / SSH  |
                    | frontend          |
                    +---------+---------+
                              |
                    +---------+---------+
                    | control plane     |
                    | SQLite            |
                    +----+---------+----+
                         | underlay |
              +----------+         +----------+
              |                               |
       +------+-------+                +------+-------+
       | bentod runner|                | bentod runner|
       | local libvirt|                | local libvirt|
       | images/disks |                | images/disks |
       | routes / nft |                | routes / nft |
       +------+-------+                +------+-------+
              |                               |
         user bridges                    user bridges
```

The controller never assumes that its local filesystem, architecture, KVM
module settings, nftables namespace, or libvirt socket describe a remote
runner. Those checks and mutations execute on the runner that owns the work.

## 7. Runner-slot subdivision

### 7.1 Stable slot numbering

For a user network `10.100.7.0/24`, four `/26` slots are:

| Slot | Routed prefix | Normally assignable addresses |
| ---: | --- | --- |
| 0 | `10.100.7.0/26` | `10.100.7.2`-`10.100.7.62` |
| 1 | `10.100.7.64/26` | `10.100.7.65`-`10.100.7.126` |
| 2 | `10.100.7.128/26` | `10.100.7.129`-`10.100.7.190` |
| 3 | `10.100.7.192/26` | `10.100.7.193`-`10.100.7.254` |

The allocator excludes:

- the `/24` network address;
- the gateway `.1`;
- the `/24` broadcast address; and
- every runner subprefix's first and last address.

Avoiding subprefix boundary addresses costs at most two addresses per slot and
keeps routing, diagnostics, and future implementations from disagreeing about
whether a boundary address is assignable. The minimum capacity is therefore
253 addresses with `/24`, 125 per slot with `/25`, 61 per slot with `/26`, and
29 per slot with `/27`.

The gateway remains `.1` on every runner's copy of the user bridge, including
runners that do not own slot 0. Linux treats the local `.1/32` route as local
even when the runner has a more-specific underlay route covering slot 0.

### 7.2 Slot ownership

Slot ownership is global rather than per user. If runner `r2` owns slot 1, it
owns slot 1 of every user `/24`. This makes the address-to-runner mapping
deterministic without a per-instance route directory.

A runner with no instance for a user may still have that user's bridge and
routes. At Bento's target scale of fewer than about 20 users, eagerly ensuring
all user networks on every active runner is acceptable and makes later
placement predictable.

### 7.3 Why slots, not `/32` routes

Per-instance `/32` routes can preserve the same guest addresses, but every
create, delete, and move becomes a distributed route update. A failed update
can make the database, source runner, destination runner, and frontend
disagree about ownership.

Slots make routes independent of ordinary lifecycle actions. `/32` routes are
reserved for a future carefully bounded migration mechanism and are not part
of normal placement.

## 8. Network behavior

### 8.1 Guest configuration

Guests keep their existing configuration:

- address prefix: `/24`;
- gateway: the user's `.1` address;
- statically assigned MAC and address;
- no DHCP dependency.

Keeping `/24` means guests on the same local bridge communicate directly.
For an address on another runner, the source guest first attempts ARP because
the destination appears on-link. The source runner answers on behalf of the
remote address using Linux proxy ARP, then routes the packet through the
underlay.

### 8.2 Runner routes

Each runner installs, for every user, a route for every remotely owned slot.
It does not install a slot route for a slot it owns; the bridge's connected
`/24` route handles local destinations.

With four slots, runner 0 has routes conceptually equivalent to:

```text
10.100.7.64/26  via runner-1-underlay
10.100.7.128/26 via runner-2-underlay
10.100.7.192/26 via runner-3-underlay
```

The same pattern repeats for each allocated user `/24`. The implementation
must use the host routing API or a narrowly scoped command runner. nftables is
not the route store.

The frontend installs the route for every owned slot through its runner. It
must never accept two equal routes for the same slot from different runners.
When the frontend machine is also the slot's runner, its connected user bridge
handles the locally owned slot and only remote slots need underlay routes.
When the underlay has a separate router, Bento may instead render the desired
routes for that router and require the operator to apply them. The chosen route
backend is explicit deployment configuration.

### 8.3 Proxy ARP

IPv4 forwarding is enabled on runners. Proxy ARP is enabled on each Bento user
bridge, not indiscriminately on every host interface.

When a guest on runner 0 asks for the MAC of an address in runner 1's slot,
the kernel sees the more-specific route through the underlay and answers the
ARP request. The guest sends the Ethernet frame to runner 0, which then
forwards the IP packet normally.

Route convergence must precede enabling proxy ARP for a new user network. A
proxy response without a usable route creates a convincing black hole.

### 8.4 Packet paths

**Same user, same runner:** guests resolve each other directly on the local
bridge. nftables permits same-bridge traffic as it does in version 1.

**Same user, different runners:** the source bridge uses proxy ARP, the source
runner forwards through the underlay, and the destination runner forwards onto
its local user bridge. The source address is preserved.

**Frontend to instance:** the frontend route selects the owning runner. The
destination runner permits the trusted frontend source to the instance's SSH
and published HTTP ports, then forwards onto the user bridge.

**Instance to internet:** the runner routes the packet outward and
masquerades it only when the destination is outside Bento's entire configured
private range.

**Different users:** the source runner drops local cross-bridge traffic. A
destination runner also rejects underlay traffic whose source and destination
do not belong to the same user network, except for explicitly trusted frontend
traffic.

### 8.5 LAN and routed VPN underlays

Bento requires the same properties from either underlay:

1. Stable runner endpoint addresses.
2. Bidirectional unicast reachability between controller and runners.
3. Bidirectional unicast reachability among runners.
4. The ability to install or externally configure routes for runner slots.
5. Preservation of guest source addresses between runners.
6. An MTU and path-MTU behavior that carries guest traffic reliably.
7. A way to restrict controller-to-runner management traffic.

On a LAN, a slot route normally uses the runner's LAN address as its next hop.
The LAN router may hold the frontend routes instead of the frontend host.

On WireGuard, Tailscale, or another routed VPN, a slot route uses the runner's
tunnel endpoint or the VPN's route mechanism. A runner advertises only the
slots it owns. It never advertises a whole user `/24` when other runners own
parts of that `/24`.

The routed VPN must preserve original guest source addresses. Tailscale subnet
routers use SNAT by default, so a Tailscale deployment of this design must use
Linux subnet routers with subnet-route SNAT disabled and must provide the
corresponding return routes. Product defaults are not assumed to satisfy the
underlay requirements.

The data model records runner endpoints and slot ownership, not product-specific
VPN identifiers.

### 8.6 Layer-2 limitations

This design preserves `/24` addressing and unicast connectivity. It does not
create one Ethernet broadcast domain across runners. Broadcast, multicast,
mDNS, and protocols that require arbitrary Layer-2 flooding do not cross
runners unless a later feature provides them explicitly.

VXLAN or EVPN could provide a shared Layer-2 segment, but would introduce
per-user overlay state, broadcast replication, duplicate-gateway handling, and
additional failure modes. It is rejected for the initial multi-node design.

## 9. nftables policy

libvirt networks continue to use `<forward mode="open"/>`. libvirt creates the
bridge and gateway address but deliberately leaves forwarding policy to Bento.

Every runner owns one atomic `inet bento` ruleset. The rendered policy extends
the version-1 rules with underlay forwarding. Its logical order is:

1. Accept established and related forwarding traffic.
2. Permit the trusted frontend sources to local instances on port 22 and on
   their currently published HTTP ports.
3. Permit a local user bridge to a remote slot only when source and destination
   belong to that same user's `/24`.
4. Permit the underlay to a local user bridge only when source and destination
   belong to that same user's `/24` and the destination is in a slot owned by
   this runner.
5. Drop traffic between different users, whether it arrived locally or over
   the underlay.
6. Permit instance egress.
7. Masquerade instance traffic only when its destination is outside the whole
   Bento private range.

Named nftables sets or maps should hold frontend addresses, local instance
addresses, published `(address, port)` pairs, user prefixes, and locally owned
slot prefixes. The exact rendering remains deterministic and is replaced as
one nft transaction, as in version 1.

The trust boundary is the runner, not an individual guest. All runners have
libvirt and storage privilege and are therefore administratively trusted.
Nevertheless, ingress rules validate guest source and destination prefixes so
a compromised guest cannot gain cross-user reach merely by choosing a source
address from another user network. Deployments must also prevent arbitrary
LAN or VPN peers from injecting packets on the runner underlay path.

Reverse-path filtering must be configured consistently with the underlay's
routing behavior. Strict reverse-path filtering is acceptable only when the
kernel's reverse lookup selects the same underlay interface. Policy-routed VPN
deployments may require loose mode. Bento's host check reports an incompatible
setting rather than silently changing unrelated interfaces.

Bento documents interactions with firewalld and other nftables managers. Its
table cannot guarantee forwarding if an earlier host firewall hook drops the
packet. A supported deployment either gives Bento's rules an agreed priority
or configures the host firewall to permit the Bento bridges and underlay.

## 10. Controller and runner responsibilities

### 10.1 Controller

The controller owns:

- SQLite and schema migrations;
- global names, users, shares, authentication, and quota;
- runner and slot records;
- placement and capacity reservations;
- desired instance state;
- the operator image allowlist;
- orchestration and unwind decisions;
- public HTTP and SSH routing data;
- runner health and observed-state freshness;
- reconciliation reports across all runners.

### 10.2 Runner

The runner owns host-local execution:

- the local Unix libvirt connection;
- KVM, architecture, nested-virtualization, KSM, and host requirement checks;
- local image versions;
- `qemu-img` overlay creation and resize;
- cloud-init seed creation and deletion;
- domain definition and lifecycle actions;
- per-user libvirt networks;
- proxy-ARP and route convergence;
- the local nftables table;
- domain inventory and observed state;
- local storage usage and capacity observations.

The runner does not open SQLite and does not make placement or authorization
decisions. It performs only authenticated controller requests and local
reconciliation for objects assigned to its host ID.

User registration commits the user and `/24` centrally. Ensuring the new user
network on runners is convergent work and does not make OIDC signup depend on
every runner being reachable. Placement only selects a runner after that
runner has successfully ensured the user's bridge, routes, and policy.

### 10.3 Why use a runner service

Remote libvirt alone is insufficient. Overlay creation, seed ISO creation,
image presence, nftables, routes, sysctls, architecture, and KVM feature probes
all describe the runner rather than the controller. Executing each of those
independently over SSH would create several transports and inconsistent error
handling.

A `bentod runner` subcommand reuses Bento's existing local implementations and
puts one authenticated, idempotent boundary around privileged host work. It
also avoids exposing libvirt's remote service on the underlay.

## 11. Runner protocol and security

The controller connects to a stable runner endpoint over the underlay. Mutual
TLS authenticates both sides even when the underlay is already encrypted. A
VPN ACL is useful defense in depth but is not the runner protocol's identity
mechanism.

Runner certificates identify one `hosts` row. The runner refuses a request for
another host ID. Requests that mutate an instance include its UUID and a
monotonic operation generation or idempotency key. Repeating a completed
request returns its recorded outcome or converges the same desired state.

The protocol exposes typed operations, not arbitrary paths, XML, shell
commands, or nftables text from an untrusted caller. The controller may send a
domain specification and a desired network-policy snapshot; the runner
validates it and renders local XML and rules. Paths derive from trusted runner
configuration and instance UUIDs.

At minimum the protocol supports:

- health and capabilities;
- local domain and image inventory;
- ensure/remove image version;
- ensure user network, routes, and firewall policy;
- provision, start, stop, reboot, redefine, and remove instance;
- resize overlay;
- finish first boot;
- stream a stopped overlay for an operator-directed move or copy.

Protocol compatibility is negotiated. A controller does not schedule onto a
runner whose protocol version or capabilities cannot satisfy the request.

## 12. Placement and capacity

Placement occurs before address allocation because the selected slot bounds
the address search.

A runner is eligible when:

1. it is enabled and healthy;
2. it owns at least one active slot;
3. it supports the requested architecture and nested-virtualization setting;
4. it has the selected image version ready locally;
5. configured or observed capacity can accept the requested vCPU, memory, and
   virtual disk reservation; and
6. one of its slots has a free address in the user's `/24`.

The initial placement policy chooses the eligible runner with the lowest
reserved-memory ratio, then lowest reserved-vCPU ratio, then stable host ID.
This is deterministic and understandable rather than predictive. Global user
quota and per-runner capacity reservations are checked in the same SQLite
transaction that inserts the provisioning instance row.

Configured memory capacity is multiplied by the runner's overcommit ratio.
Disk placement uses a configured allocatable limit or a conservative observed
free-space threshold. Virtual disk quota remains based on virtual size as in
version 1.

An operator can disable placement on a runner without stopping its instances.
Draining is an explicit stronger state that also prepares its slots for
evacuation.

## 13. Storage and images

### 13.1 Local storage

Each runner has its own image and storage directories. Paths may be identical
across runners but are not assumed to name shared storage. Domain XML is
rendered on the runner with runner-local absolute paths.

The default design does not require NFS or another shared filesystem. An
operator may use shared storage, but Bento treats it as an implementation of
runner storage rather than changing instance identity or placement rules.

### 13.2 Image distribution

The image allowlist and current checksum remain global. Presence is tracked per
runner. A runner becomes eligible for an image only after it has independently
verified that exact content-addressed version.

The preferred distribution path is for each runner to fetch the allowlisted
URL and verify the configured or controller-provided checksum. A controller
streaming fallback may be added for sources that cannot be reached from
runners. Neither path permits a runner to substitute a different checksum.

Collection is local: a runner deletes an image version only when the controller
confirms that no instance assigned to that runner depends on it and no local
operation holds the image-store lock.

### 13.3 Copy and move

The first multi-node release keeps `cp` on the source runner unless the user or
placement policy explicitly requests a destination. A same-runner copy remains
a local stopped-overlay copy.

A cross-runner copy requires the source to be stopped, streams a consistent
overlay to the destination, verifies its digest and virtual size, builds a new
seed, and only then defines the new domain. Failure removes destination partial
work and leaves the source unchanged.

Moving an existing instance is an operator maintenance action. Because routes
belong to slots, the normal evacuation unit is an entire slot. Individual
movement using a temporary `/32` exception is deferred.

### 13.4 Backup

The controller database backup remains one consistent SQLite backup. The
operator must also back up every runner's storage and image directories. The
backup report lists runners and content-addressed image versions required by
their overlays so an incomplete backup is visible.

## 14. Lifecycle orchestration

### 14.1 Create

Create is a controller-owned saga:

1. Select an eligible runner and one of its slots.
2. Allocate an address from that slot.
3. In one database transaction, enforce quota and runner capacity, claim the
   name, and insert a provisioning row with `host_id` and address.
4. Ensure the runner has the image, user network, routes, and current policy.
5. Ask the runner to create the overlay and seed, define the domain, clear
   autostart, and start it.
6. Record the observed running state.
7. Reload published-port policy where necessary.

Every runner step is idempotent by UUID. Failure after step 3 invokes ordered
runner cleanup, then removes the row and releases the reservation and name. If
cleanup cannot reach the runner, the row remains in an explicit failed or
cleanup-pending state; Bento must not erase the only record of possibly
existing storage or a domain.

### 14.2 Existing-instance actions

Start, stop, restart, resize, rename, remove, first-boot cleanup, and SSH
auto-start resolve the row first and dispatch to its `host_id`. No hypervisor
method may select a runner from a domain name alone.

Delete retains the existing conservative ordering: remove the domain, remove
the overlay and seed, then delete the database row and release the name. An
unreachable runner makes delete retryable; it does not skip host cleanup.

### 14.3 Restore after reboot

Runner reboot and controller reboot are independent events. A runner reconnects
and sends its inventory. The controller compares only rows assigned to that
runner, clears libvirt autostart, and restores desired-running instances in
runner-local batches.

The controller also performs this handshake when its own process restarts. A
runner that never reconnects does not block restore on healthy runners.

## 15. State, health, and partial failure

Observed instance state needs freshness. Store a separate observation time or
runner observation generation; do not overload `last_seen_at`, which records
user traffic.

When a runner becomes unreachable:

- its last observed instance states remain recorded;
- API, CLI, and dashboard output mark them stale or host-unreachable;
- lifecycle requests fail with a runner-specific retryable error;
- healthy runners continue polling and accepting work;
- no slot is reassigned automatically;
- no domain or database row is deleted automatically.

The host record tracks at least enabled state, placement state, endpoint,
protocol version, architecture, last successful contact, and last error.
Ephemeral health may live in control-plane memory, but enough durable identity
and configuration must survive restart.

State polling and reconciliation are host-scoped. A domain seen on runner A is
compared only with rows whose `host_id` is A. Reports identify the runner for
both domains without rows and rows without domains. Failure to list runner B
does not turn all of B's rows into discrepancies.

Split-brain avoidance is more important than automatic availability. An
operator may reassign a failed runner's slot only after fencing the old runner
or otherwise proving it cannot resume its domains on the old network path.

## 16. Data-model changes

The exact migration may evolve during implementation, but the model needs:

- a persisted deployment runner prefix (`24` through `27`);
- host endpoint, enablement, placement state, and capability observations;
- runner-slot rows with stable slot numbers and one active owner;
- per-runner image-version presence and status;
- per-instance provisioning/cleanup status;
- per-instance observed-state freshness;
- per-runner resource reservations or queries that calculate them by
  `host_id`.

`instances.host_id` remains the placement key. Existing single-host databases
migrate to `/24`, slot 0, owned by the existing host. Existing instance
addresses remain valid.

Bento must introduce ordered schema migrations before adding these fields.
Reapplying only `CREATE TABLE IF NOT EXISTS` cannot safely evolve an existing
database.

## 17. Changing the runner-slot prefix

Changing `/24`-`/27` is an operator maintenance workflow. Bento refuses to
start with a TOML value that silently disagrees with the persisted deployment
prefix.

### 17.1 Increasing the number of slots

Subdivision does not change guest addresses or `/24` configuration. Each old
slot splits into two children. Initially both children remain owned by the old
runner, so the database can adopt the finer prefix without moving a domain.

For example, changing `/25` to `/26` maps:

```text
old slot 0 -> new slots 0 and 1, both initially on old owner 0
old slot 1 -> new slots 2 and 3, both initially on old owner 1
```

Adding capacity then requires freeing and reassigning a child slot. If the
child contains instances, Bento drains the slot:

1. Disable placement into the child.
2. Stop every instance in the child.
3. Transfer and verify their disks and required images.
4. Fence or undefine the source domains.
5. Install destination bridge, routes, proxy ARP, and firewall policy.
6. Atomically change slot ownership and affected `host_id` values.
7. Change frontend and peer routes.
8. Define and restore instances on the destination.
9. Remove source storage only after explicit verification or operator choice.

The maintenance plan names every affected instance and estimates bytes to
transfer before it changes anything.

### 17.2 Decreasing the number of slots

Merging slots is harder because two child slots may have different owners.
Bento first evacuates one child so both children have the same owner, then
merges them. It does not install equal-cost ownership or leave two active
runners for one merged prefix.

### 17.3 Downtime and rollback

Route ownership and domain ownership must never disagree while both source and
destination can run. Slot evacuation therefore requires downtime for affected
instances unless a future live-migration design adds storage and fencing
guarantees.

Before the ownership commit, rollback restarts the source. After the ownership
commit, rollback is another fenced slot move back to the source. The operation
stores a durable phase so controller restart cannot forget which side is
authoritative.

## 18. Runner addition, drain, and removal

Adding a runner performs host checks, enrolls its certificate, synchronizes
required images, and assigns it an unowned or evacuated slot. A runner with no
slot may be healthy but receives no placements.

Disabling placement is immediate and non-disruptive. Draining operates on all
slots owned by the runner. Removing the host record is allowed only when it
owns no slots, has no instance rows, and reconciliation finds no Bento domains.

If a runner is permanently lost, recovery requires restored storage or an
operator decision about each unrecoverable instance. Bento never equates
"unreachable" with "empty."

## 19. Configuration and setup

Initial setup asks for anticipated VM runners and persists the corresponding
slot prefix. Advanced configuration may specify the prefix directly:

```toml
# One of 24, 25, 26, or 27. Persisted on first initialization.
runner_prefix = 25

[[runners]]
name = "runner-a"
endpoint = "https://10.0.0.21:10443"

[[runners]]
name = "runner-b"
endpoint = "https://10.0.0.22:10443"
```

This shape is illustrative rather than a committed parser interface. Private
keys and enrollment secrets do not belong inline in the main TOML file.

The runbook distinguishes:

- direct LAN routing;
- routes managed on each host;
- routes managed by an external LAN router;
- WireGuard/Tailscale-style routed VPNs; and
- unsupported overlapping whole-`/24` advertisements.

## 20. Observability and operator interface

Operator output includes:

- runner name, endpoint, slot ownership, enabled/draining state, architecture,
  protocol version, and last contact;
- reserved and configured vCPU, memory, and disk capacity;
- image readiness by runner;
- stale instance counts;
- route, bridge, proxy-ARP, firewall, domain, and database reconciliation;
- slot-drain and prefix-change plans.

User-facing `ls` and the dashboard need not expose placement by default, but
must distinguish a stopped instance from one whose runner is unreachable.

Logs attach runner ID, slot, instance UUID, and operation generation to every
distributed lifecycle action.

## 21. Reconciliation

Reconciliation remains report-only unless an operator invokes a narrowly
defined repair action. It checks, per runner:

1. Database rows against libvirt domains by UUID.
2. Assigned overlays and seeds against instance rows.
3. Required image checksums against local files.
4. User networks against assigned users.
5. Remote slot routes against persisted ownership.
6. Proxy-ARP settings on Bento bridges.
7. nftables policy against the rendered desired snapshot.
8. Frontend routes against slot ownership.

An unreachable runner produces one reachability finding and stale dependent
checks, not a false list of missing domains and files.

## 22. Implementation sequence

1. Add schema migrations and the runner/slot data model; migrate one-host
   deployments to `/24` slot 0.
2. Make store queries, restore, polling, and reconcile explicitly host-scoped.
3. Add a host registry and dispatch existing lifecycle actions by `host_id`.
4. Implement `bentod runner` with mutual TLS and local host checks.
5. Move overlay, seed, domain XML, image presence, and libvirt operations
   behind the runner boundary.
6. Add slot-aware address allocation and placement reservations.
7. Implement LAN route convergence, proxy ARP, and the multi-node nftables
   policy; validate with two real runners.
8. Validate the same route abstraction over one supported routed VPN setup.
9. Route HTTP, SSH, SSH auto-start, and CLI/API lifecycle actions through the
   host dispatcher.
10. Add per-runner image synchronization and same-runner copy.
11. Add stale health, reconnect restore, and host-scoped reconciliation.
12. Add runner drain and prefix-subdivision planning.
13. Add stopped-overlay transfer and slot evacuation.

Each step keeps the one-host configuration working. The controller machine may
run a loopback runner during the transition so single-host and multi-host use
the same orchestration path.

## 23. Test and acceptance plan

Unit tests continue using fake hypervisor, filesystem, command, network, and
clock seams. Add deterministic tests for:

- prefix selection and every `/24`-`/27` slot boundary;
- address exhaustion per slot;
- placement transaction races;
- host-scoped polling, restore, and reconcile;
- nftables rendering for local, remote-same-user, frontend, cross-user, and
  internet traffic;
- route snapshots for LAN and routed-VPN backends;
- idempotent runner retries and cleanup-pending creates;
- runner loss without false stopped states;
- slot subdivision and merge plans;
- old single-host database migration.

Live acceptance requires at least two libvirt runners and covers:

1. Same-runner and cross-runner unicast between one user's guests.
2. Cross-user denial locally and across the underlay.
3. HTTP and SSH frontend access to both runners.
4. Internet egress and absence of NAT for Bento-private traffic.
5. LAN underlay routing.
6. One routed VPN underlay.
7. Runner reboot while the controller remains up.
8. Controller reboot while runners remain up.
9. Underlay interruption and recovery.
10. Failed create at every runner-side phase.
11. Image mismatch on one runner.
12. Slot evacuation with controller restart during each durable phase.
13. Verification that broadcast-dependent discovery does not accidentally
    appear supported.

Multi-node support does not ship based solely on fakes. The current project has
not yet exercised real guest boots and nftables policy comprehensively; this
feature makes a repeatable live network test environment mandatory.

## 24. Alternatives considered

### Pin one user to one runner

This makes routing simple but creates coarse placement, strands capacity, and
prevents a user's instances from using multiple runners. It remains a useful
temporary implementation constraint but is not the target design.

### Per-instance `/32` routes

This gives flexible placement but turns every lifecycle action into distributed
route convergence. Stable slots are simpler and safer at Bento's scale.

### Proxy on every runner

A runner-side TCP proxy avoids guest routes for frontend traffic but does not
preserve direct same-user unicast across runners and adds a second hop and
another protocol to both HTTP and SSH paths.

### Shared storage

Shared storage can simplify copy and move but introduces an operational and
availability dependency that Bento does not otherwise need. The architecture
allows it without requiring it.

### Direct remote libvirt plus remote shell commands

Remote libvirt handles domains but not all host-local files, programs, routes,
firewall state, and feature probes. A typed runner service gives those actions
one security and idempotency model.

### VXLAN or another Layer-2 overlay

This preserves broadcast semantics but is disproportionate to the expected one
or two runners and at most eight slots. Routed prefixes plus proxy ARP preserve
the unicast behavior Bento needs with less state.

## 25. External behavior relied upon

The design relies on three documented Linux/libvirt behaviors:

1. libvirt `forward mode="open"` sends guest traffic through the host routing
   stack without adding libvirt firewall rules, leaving policy to Bento:
   <https://libvirt.org/formatnetwork.html>.
2. Linux proxy ARP answers based on whether the kernel would route the
   requested address through another interface:
   <https://docs.kernel.org/networking/ip-sysctl.html>.
3. nftables `forward` filters packets routed to another host and `postrouting`
   applies after the route decision; nftables sets and maps can express the
   desired address and port policy:
   <https://www.netfilter.org/projects/nftables/manpage.html>.
4. Tailscale subnet routers use SNAT by default and support preserving source
   addresses on Linux by disabling subnet-route SNAT:
   <https://tailscale.com/docs/features/subnet-routers>.

These are implementation dependencies and must be revalidated in the live
acceptance environment, especially when the underlay uses policy routing.
