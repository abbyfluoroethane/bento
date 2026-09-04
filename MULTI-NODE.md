# Bento Multi-Node Design

Status: proposed design for the post-version-1 multi-node work described in
SPEC section 17.

This document defines one Bento deployment spanning more than one machine that
runs libvirt. It replaces the sketch in SPEC section 17 when implementation
begins. It also amends the SPEC section 4 statement that the control plane
generates domain XML. The runner generates domain XML in this design. Until
implementation begins, the single-host behavior in SPEC remains authoritative.

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

The runner transport is a routed, mutually reachable IP network that meets the
security requirements in section 11. A protected LAN, WireGuard, Tailscale,
and an equivalent routed VPN are valid. Bento does not depend on one VPN
product and does not create the underlay.

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
9. Work over an administratively isolated LAN or an authenticated routed VPN.
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

**Controller epoch** is the durable fencing number for one controller lease.

**Desired generation** is the durable version of one object's desired state.

**Request ID** identifies one protocol attempt and its recorded outcome.

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
10. A runner only stores overlays whose instance rows name that runner, except
    for source cleanup or destination staging named by one durable moving-slot
    operation.
11. Global user quota is checked before runner mutation.
12. An unreachable runner never causes its instances to be reported as
    stopped merely because they could not be observed.
13. Route ownership never moves automatically after a health-check failure.
14. Cross-user traffic remains denied on every runner.
15. Runner, controller, frontend, and route next-hop addresses are outside the
    whole configured Bento private range.
16. A mutating runner request comes only from the current controller epoch and
    never applies an older desired generation.
17. Provisioning, cleanup-pending, lost, moving, and removal-pending instances
    continue to reserve their names, addresses, quota, and runner capacity.

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

An address that was allocated before subdivision is grandfathered. This rule
includes `.63`, `.64`, `.127`, and `.128` when a `/24` becomes four `/26`
slots. Bento keeps the address and assigns its instance to the one slot whose
prefix contains it. The bridge and guest stay configured as `/24`, so the
address remains unicast and routes normally. Only a new allocation refuses a
subprefix's first or last address. An evacuation does not change a
grandfathered address.

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

Each user network has a durable desired generation. A change to its bridge,
slot routes, proxy ARP, firewall policy, or participant set increments this
generation. The controller stores the exact participant set for that
generation. The set
contains the destination runner, every healthy runner that already hosts an
instance for the user, every frontend, and the configured route backend. Each
participant stores or reports an acknowledgement only after it has applied and
verified the complete generation. Placement waits for every acknowledgement
in this set. A later health failure does not invalidate a completed placement,
but it prevents new placement that depends on a new unacknowledged generation.

A newly enrolled or reconnecting runner compares all applicable network
generations and converges them before it becomes healthy or
placement-eligible. It acknowledges the current generation before it hosts a
new instance for that user.

When the underlay has a separate router, the route backend may apply routes
through an authenticated API. A manual or external route backend renders the
desired routes and records no acknowledgement by itself. It cannot support
automatic placement. An operator must verify the routes and explicitly
acknowledge the exact user network generation before placement can continue.
The chosen route backend is explicit deployment configuration.

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
destination runner permits the authenticated frontend source to the
instance's SSH and published HTTP ports, then forwards onto the user bridge.

**Instance to internet:** the runner routes the packet outward and
masquerades it only when the destination is outside Bento's entire configured
private range.

**Different users:** the source runner drops local cross-bridge traffic. A
destination runner also rejects underlay traffic whose source and destination
do not belong to the same user network, except for authenticated frontend
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

Every configured runner endpoint, controller address, frontend address, and
route next-hop address must be outside the whole configured Bento private
range. This rule applies to current and future user `/24` allocations, not
only to allocated user networks. It prevents recursive routes and prevents a
guest route from naming an underlay management endpoint. Bento validates all
addresses before a host record or route backend becomes active. It refuses the
activation when an address overlaps the private range or cannot be validated.

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

The data plane between runners does not authenticate IP packets. A supported
LAN underlay is an administratively isolated segment. Its switches, routers,
and host filters enforce anti-spoofing for runner endpoint addresses and the
guest prefixes routed through each runner. An unisolated or shared network
must use an authenticated tunnel whose peer identities and route ACLs enforce
the same source ownership. This protection is required configuration, not an
assumption that a LAN is trusted.

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
the version-1 rules with underlay forwarding. The rules use a map from each
user bridge name to that user's `/24`. No rule accepts a guest source from a
user prefix unless the ingress bridge maps to that same prefix. After
established and related traffic, the five accept predicates are exact:

1. **Local forward.** `iifname` maps to user U, the source address is in U,
   `oifname` is U's same bridge, and the destination address is in U.
2. **Cross-runner forward.** `iifname` maps to user U, the source address is in
   U, `oifname` is the configured underlay interface, and the destination is in
   one of U's remotely owned slots.
3. **Underlay ingress.** `iifname` is the configured underlay interface, the
   source and destination are in the same user U, the destination is in a slot
   of U owned by this runner, and `oifname` is U's bridge.
4. **Frontend ingress.** `iifname` is the configured underlay interface, the
   source is in the authenticated frontend-source set, the destination is a
   local instance, `oifname` is that instance's user bridge, and the TCP port
   is 22 or a published `(address, port)` pair.
5. **Internet egress.** `iifname` maps to user U, the source address is in U,
   `oifname` is a configured external interface, and the destination is
   outside the whole Bento private range and outside the management-address
   set. The same source, interface, and destination predicate controls
   masquerading in `postrouting`.

All other new forwarding is dropped. In particular, an ingress bridge alone
never authorizes egress, an underlay interface alone never authorizes delivery
to a bridge, and a claimed source address never selects another user's rules.
The authenticated frontend-source set contains only sources whose identity is
enforced by the isolated underlay or authenticated tunnel in section 8.5.

Named nftables sets or maps hold the bridge-to-prefix binding, authenticated
frontend sources, management addresses, local instance addresses, published
`(address, port)` pairs, remote slots by user, and locally owned slots by user.
The exact rendering is deterministic and is replaced as one nft transaction,
as in version 1.

The trust boundary is the runner, not an individual guest. All runners have
libvirt and storage privilege and are therefore administratively trusted.
Nevertheless, ingress rules validate the bridge, guest source, destination,
slot ownership, underlay interface, and frontend source. A compromised guest
cannot gain cross-user reach by choosing a source address from another user
network. The required underlay controls in section 8.5 prevent arbitrary LAN
or VPN peers from injecting packets on the runner underlay path.

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
- desired instance state and per-object generations;
- the controller lease and epoch;
- user network generations, participant sets, and acknowledgements;
- the operator image allowlist;
- global content-addressed image-version metadata;
- orchestration and unwind decisions;
- public HTTP and SSH routing data;
- runner health and observed-state freshness;
- reconciliation reports across all runners.

### 10.2 Runner

The runner owns host-local execution:

- the local Unix libvirt connection;
- KVM, architecture, nested-virtualization, KSM, and host requirement checks;
- per-runner image paths, readiness, verification times, and errors;
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
every runner being reachable. The current network generation records each
required participant. Placement starts only after the complete acknowledgement
set in section 8.2 is durable.

### 10.3 Why use a runner service

Remote libvirt alone is insufficient. Overlay creation, seed ISO creation,
image presence, nftables, routes, sysctls, architecture, and KVM feature probes
all describe the runner rather than the controller. Executing each of those
independently over SSH would create several transports and inconsistent error
handling.

A `bentod runner` subcommand reuses Bento's existing local implementations and
puts one authenticated, idempotent boundary around privileged host work. It
also avoids exposing libvirt's remote service on the underlay.

The existing `bento_lifecycle::Runner` trait is the local command executor. It
is renamed to `CommandRunner`. The word "runner" is reserved for the new
service and host role.

## 11. Runner protocol and security

The controller opens every runner protocol connection to a stable runner
endpoint over the underlay. A runner never opens a control connection to the
controller. Mutual TLS authenticates both sides even when the underlay is
already encrypted. A VPN ACL is defense in depth and is not the runner
protocol's identity mechanism.

The deployment CA issues certificates with distinct roles. A runner-server
certificate has server authentication usage and maps to exactly one immutable
`host_id`. A controller-client certificate has client authentication usage and
the controller role. Only a controller-role client may query or mutate a
runner. A runner certificate is never accepted as a controller credential,
even when both certificates chain to the same CA. The runner also rejects a
request whose target host ID differs from its certificate identity.

The runner listener binds only to an explicitly configured underlay or tunnel
address. It does not bind to a wildcard address, a public address, or a user
bridge. The host firewall permits the runner management port only from the
configured controller addresses. It also denies guest prefixes access to all
management addresses. Host-address validation in section 8.5 completes before
the listener starts.

Enrollment uses a one-time bootstrap token bound to one pending `host_id` and
the expected endpoint. The controller stores only its hash. The token has a
mandatory expiry, defaults to 10 minutes, and can be used once. The runner
generates its private key locally and sends a certificate request to the
authenticated enrollment endpoint. Normal renewal requires the current valid
runner certificate and keeps the same `host_id`. Rotation allows a short,
recorded overlap between old and new serial numbers. Private keys never leave
the machine that generated them.

The controller stores certificate serial numbers, validity, role, and
revocation state. Revocation is checked on every new connection and existing
connections are closed when the revocation generation changes. CA rollover is
two-phase. First, all peers install and acknowledge a trust bundle containing
the old and new CA. Next, Bento rotates all certificates and records their
acknowledgements. Bento removes the old CA only after every enabled peer uses
the new CA. A failed acknowledgement stops the rollover.

Revoking a runner certificate immediately disables its health, placement, and
lifecycle RPCs. Its existing guests and data-plane routes continue unchanged.
Bento does not stop them, delete them, or reassign their slots. An operator
must re-enroll the same fenced host or fence it and use the lost-host workflow
before ownership can move.

The controller lease is stored in SQLite. Acquisition uses an immediate write
transaction, refuses an unexpired lease held by another process, and increments
the durable controller epoch. The lease records a random holder ID and an
expiry. Only that holder may dispatch. Each request carries the epoch, holder
ID, and lease expiry. A runner rejects an expired lease and durably records the
highest accepted controller epoch. After it accepts a higher epoch, it rejects
every request from a lower epoch, including an in-flight request from an old
controller process. Runners validate lease expiry with the configured maximum
clock skew and fail closed when their clock is not trustworthy.

Every mutable object has a desired generation that is separate from protocol
replay. A database transaction changes desired state and increments that
object's generation. Each protocol attempt also has a unique request ID. The
runner durably stores, across process and host reboots, the highest accepted
generation for each object and the completed outcome for each request ID. It
rejects a lower generation. It accepts an equal generation only when the
desired-state digest is equal. It then returns the recorded result or
continues convergence. It rejects an equal generation with a different
digest. A higher generation supersedes unfinished older work only at an
operation-defined safe point.

Restoring an older controller database is a fenced recovery operation. The
operator first proves that no controller from the newer database can run. The
restored controller enters recovery mode. A runner permits a controller-role
certificate to read fencing generations and inventory through a read-only
recovery operation even when the database epoch is stale. This operation does
not change runner state or its accepted epoch. The controller reads the
highest epoch and per-object generations from every reachable runner. It
allocates an epoch greater than every reported epoch and advances each
retained object generation before it sends a mutation. It reconciles runner
inventory with the restored rows. Objects on an unreachable runner remain
blocked. When that runner returns, the controller reads it before mutation and
allocates another higher epoch if necessary. The controller cannot lower a
runner's durable epoch or generation, and it cannot treat a stale backup as
authority to start, remove, or overwrite a newer object.

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
3. it supports the requested architecture, nested-virtualization setting, and
   CPU compatibility class;
4. it has the selected image version ready locally;
5. configured or observed capacity can accept the requested vCPU, memory, and
   virtual disk reservation; and
6. one of its slots has a free address in the user's `/24`; and
7. the complete participant set has acknowledged the user's current network
   generation.

The initial placement policy chooses the eligible runner with the lowest
reserved-memory ratio, then lowest reserved-vCPU ratio, then stable host ID.
This is deterministic and understandable rather than predictive. Address and
placement selection may occur before the write transaction only as a plan.
The transaction that inserts the provisioning row revalidates global quota,
runner capacity, address availability, active slot ownership, the slot state,
the `(slot_id, host_id)` owner relation, image readiness, CPU compatibility,
and the network acknowledgement set. It stores `slot_id` on the instance.
Failure of any check retries placement from a new snapshot.

Configured memory capacity is multiplied by the runner's overcommit ratio.
Disk placement uses a configured allocatable limit or a conservative observed
free-space threshold. Virtual disk quota remains based on virtual size as in
version 1.

Each runner reports a CPU compatibility class derived from its architecture
and the host CPU features exposed by `host-passthrough`. A nested instance
stores the exact class used at creation. It can copy or move only to a runner
with the same class. Before a slot transfer starts, Bento reserves compatible
CPU, memory, disk, address, and image capacity for every instance in that slot.
One failure rejects the complete transfer plan.

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

The image allowlist and immutable version metadata are global. One global
version row records the source kind, source reference, resolved OCI digest when
applicable, content-addressed QCOW2 checksum, virtual size, and artifact
locator. It does not record a runner filesystem path.

The deployment configures one designated OCI builder. Only this builder pulls
and converts a bootc image. It holds the global OCI build lock, produces one
QCOW2 artifact, computes its content checksum, and makes the content-addressed
artifact durable. The controller commits the global image-version row only
after that step. Independent runner builds are forbidden because equal OCI
source digests do not guarantee byte-identical QCOW2 output.

Every runner fetches or receives that exact committed artifact. It computes
the checksum and refuses a mismatch before the version becomes ready. A
controller streaming path may serve the same bytes when a runner cannot reach
the artifact store. Neither path permits a runner to build or substitute a
different artifact.

Presence is separate per-runner state. It records `host_id`, image version,
runner-local path, readiness state, verified time, and last error. A runner is
eligible for an image only after this row says that it verified the global
checksum. A local error does not modify global image metadata or another
runner's readiness.

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

A consistent deployment backup uses a quiesce protocol. The controller takes
a deployment backup lock that blocks placement and every mutating lifecycle,
image, network, and maintenance operation. Each enabled runner finishes its
current durable step, flushes local files, enters read-only quiescence, and
returns a storage snapshot generation and manifest. The operator backup
backend snapshots each runner directory and returns the snapshot reference.
The controller records every runner generation, manifest digest, and snapshot
reference in the SQLite backup checkpoint. It then creates the SQLite backup
while the lock remains held. Only this complete set is one recovery point.

If any required runner or snapshot backend cannot acknowledge, Bento does not
label the result consistent. An explicit best-effort backup may still copy the
SQLite database and runner directories. It is crash-consistent and may combine
different times. Its report marks every missing or unmatched runner generation
and lists the content-addressed image versions required by the overlays.

## 14. Lifecycle orchestration

### 14.1 Create

Create is a controller-owned saga:

1. Select an eligible runner and one of its slots.
2. Allocate an address from that slot.
3. In the placement transaction described in section 12, claim the name and
   address and insert a provisioning row with `host_id`, `slot_id`, and desired
   generation.
4. Ensure the exact image version and wait for the complete user network
   acknowledgement set.
5. Ask the runner to create the overlay and seed, define the domain, clear
   autostart, and start it.
6. Record the observed running state.
7. Reload published-port policy where necessary.

Every runner step uses the controller epoch, object generation, desired-state
digest, and request ID from section 11. Failure after step 3 changes the
desired state to cleanup-pending and invokes ordered runner cleanup. Bento
deletes the live row and releases its resources only after the runner confirms
that no domain, overlay, or seed remains. An unreachable runner leaves the row
and all reservations intact. Bento must not erase the only record of possibly
existing storage or a domain.

### 14.2 Existing-instance actions

Start, stop, restart, resize, rename, remove, first-boot cleanup, and SSH
auto-start resolve the row first and dispatch to its `host_id`. No hypervisor
method may select a runner from a domain name alone.

Every mutating action first commits a new desired generation and then
converges the runner to it. A timeout leaves the action pending. A retry uses a
new request ID with the same generation and digest until the outcome is known.
Reconciliation repeats the same convergence contract after either process
restarts. It does not infer success from a transport timeout.

Start, stop, and SSH auto-start set the desired power state. Restart records a
new restart generation. Resize stores desired resources but keeps separate
observed resources until disk growth, domain redefinition, and any required
restart complete. Rename claims the new name and reserves both old and new
names until the runner redefines the UUID under the new name. First-boot
cleanup keeps the seed required until the runner confirms its removal.

Delete retains the existing conservative ordering: remove the domain, remove
the overlay and seed, then replace the live row with its audit tombstone and
release resources. An unreachable runner makes delete retryable; it does not
skip host cleanup.

### 14.3 Restore after reboot

Runner reboot and controller reboot are independent events. The controller
opens the connection and requests the runner's durable inventory. It compares
only rows assigned to that runner, asks the runner to clear libvirt autostart,
and restores desired-running instances in runner-local batches.

The controller performs this handshake after either side restarts. Before a
restarted runner is healthy, it also converges current user network, route,
firewall, certificate, and image state. An unreachable runner does not block
restore on healthy runners.

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

The public frontends use the same reachability state. HTTP returns a generic
503 response when the target runner is unreachable. The response does not name
the runner or reveal placement. SSH fails promptly with a generic temporary
unavailable error and does not wait for the former 120-second auto-start
timeout. HTTP and the SSH data connection continue to dial the guest address
through the routing plane. They do not tunnel through a runner RPC. Only SSH
auto-start uses the runner lifecycle protocol.

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
Certificate revocation alone is not a domain or data-plane fence.

## 16. Data-model changes

The exact migration may evolve during implementation, but the model needs:

- a persisted deployment runner prefix (`24` through `27`);
- the controller epoch, lease holder, and lease expiry;
- host endpoint, enablement, placement state, certificate identity, and
  capability observations, including CPU compatibility class;
- runner-slot rows with stable slot numbers, `active`, `draining`, or `moving`
  state, active owner, ownership epoch, source, destination, and operation ID;
- user network desired generations, participant snapshots, desired-state
  digests, and participant acknowledgements;
- global image-version source and checksum metadata without local paths;
- per-runner image-version path, readiness, verified time, and error;
- per-instance `slot_id`, desired generation and digest, provisioning and
  cleanup status, and observed-state freshness;
- pending operation request IDs and durable audit tombstones;
- per-runner resource reservations or queries that calculate them by
  `host_id`;
- prefix-change and slot-move phase, source, destination, participant list,
  transfer digest, route acknowledgements, and last error; and
- backup checkpoints and their runner snapshot generations.

`instances.host_id` remains the placement key and `instances.slot_id` records
the bounded allocator that supplied the address. A schema constraint permits
only one active ownership row for a slot. Insert and update triggers enforce
that an instance's `(slot_id, host_id)` names the active owner. The controller
also revalidates these facts under the same immediate write transaction that
claims an address and capacity. A unique live-address constraint covers all
states that still reserve an address.

Provisioning, cleanup-pending, lost, moving, and removal-pending rows consume
an address, a name, user quota, and runner capacity. A transition releases
them only after confirmed cleanup or the explicit abandon workflow in section
18. Existing single-host databases migrate to `/24`, slot 0, owned by the
existing host. Existing instance addresses remain valid, including the
grandfathered boundaries in section 7.1.

Bento must introduce ordered schema migrations before adding these fields.
Reapplying only `CREATE TABLE IF NOT EXISTS` cannot safely evolve an existing
database.

## 17. Changing the runner-slot prefix

Changing `/24`-`/27` is an operator maintenance workflow. Bento refuses to
start with a TOML value that silently disagrees with the persisted deployment
prefix.

One deployment-wide prefix-change lock serializes the complete workflow. It is
also exclusive with a backup and every slot move. The first immediate write
transaction acquires the lock, stores the target prefix and operation
generation, snapshots every participant and affected instance, and changes
each affected slot from `active` to `draining`. The same transaction reserves
destination capacity for every affected instance. While the lock is held,
Bento rejects allocation, create, start, restart, resize, rename, remove,
first-boot cleanup, automatic convergence mutations, and a second maintenance
operation for an affected slot. Stop is allowed only as a recorded phase of
the maintenance plan.

A slot has one of three durable states. `active` permits normal placement and
lifecycle work. `draining` keeps the source owner authoritative but permits no
new allocation. `moving` permits only the persisted source, destination, and
operation generation to act. The operation row persists its phase, source,
destination, old and new ownership epochs, participant list, reservations,
per-instance transfer digest, route generation and acknowledgements, retry
count, and last error.

### 17.1 Increasing the number of slots

Subdivision does not change guest addresses or `/24` configuration. Each old
slot splits into two children. Initially both children remain owned by the old
runner, so the database can adopt the finer prefix without moving a domain.
An existing subprefix-boundary address stays grandfathered under section 7.1.

For example, changing `/25` to `/26` maps:

```text
old slot 0 -> new slots 0 and 1, both initially on old owner 0
old slot 1 -> new slots 2 and 3, both initially on old owner 1
```

While the prefix-change lock remains held, Bento renders the finer routes and
increments each affected user network generation. It commits the new
deployment prefix as active and releases the lock only after the complete
participant sets acknowledge those generations. Failure leaves the durable
operation pending and ordinary work blocked. It does not expose a mixture of
allocator prefixes.

Adding capacity then requires freeing and reassigning a child slot. If the
child contains instances, the move acquires the same deployment-wide lock and
Bento runs these durable phases:

1. Change the child to `draining`, block conflicting work, and verify all
   destination reservations and CPU compatibility classes.
2. Stop every instance on the source and record its stopped generation.
3. Change the child to `moving`. Transfer and verify every disk and required
   image. Store each digest before continuing.
4. Undefine every source domain. The source durably fences the old slot
   ownership epoch and rejects every later request that names it. If the source
   cannot acknowledge, require an external power, network, and storage fence.
   Certificate revocation is not sufficient.
5. In one transaction, assign a higher ownership epoch to the destination,
   change affected `host_id` values, and create a new user network generation
   with its exact participant set.
6. Apply and acknowledge the destination bridge, proxy ARP, firewall policy,
   frontend routes, peer routes, and route backend for that generation.
7. Define and restore instances on the destination according to their desired
   power state.
8. Change the slot to `active`, release the maintenance reservations, and
   remove source storage only after explicit verification or operator choice.

The destination runner may stage transferred storage before phase 4. It must
not define or start a domain until the old ownership epoch is durably fenced,
the source runner is fenced, the new ownership transaction is committed, and
every network participant acknowledges the new generation. This order also
rejects a late start from the old controller epoch.

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

Before the source fence, rollback restarts the source. After the source fence,
every rollback uses a higher ownership epoch. After the ownership commit,
rollback is another fenced slot move back to the source. The operation stores
a durable phase so controller restart cannot forget which side is
authoritative.

Each phase is idempotent and has a phase-specific retry and rollback. Failure
during drain or transfer removes verified destination staging and returns the
slot to `active` on the source. Failure after the source fence never clears an
old fence. Rollback assigns a higher ownership epoch to the source, reconverges
and acknowledges that route generation, and then restores source domains.
Failure after the ownership commit remains `moving` with all instances down
until route convergence can retry. Rollback after that commit is a new fenced
move with another higher ownership epoch. Failure of final source cleanup is
retryable and does not change destination authority.

## 18. Runner addition, drain, and removal

Adding a runner performs host checks, enrolls its certificate, synchronizes
required images, converges every applicable user network generation, and then
assigns it an unowned or evacuated slot. A runner does not become healthy or
placement-eligible before this convergence. A runner with no slot may be
healthy but receives no placements.

Disabling placement is immediate and non-disruptive. Draining operates on all
slots owned by the runner. Removing the host record is allowed only when it
owns no slots, has no instance rows, and reconciliation finds no Bento domains.

If a runner is permanently lost, recovery requires restored storage or an
operator decision about each unrecoverable instance. Bento never equates
"unreachable" with "empty."

The operator-only `declare-lost` action requires a recorded external fence that
prevents the host from running domains or reaching the data plane. It marks the
host lost, revokes its certificate, and preserves every instance, reservation,
and slot until a later decision. Moving a slot from that host still uses the
fenced move protocol in section 17.

The operator may then restore an instance from known storage or explicitly
abandon it. Abandon writes an immutable audit tombstone with the UUID, former
name, address, host, slot, last desired generation, fence evidence, operator,
reason, and time. It deliberately releases the live name, address, quota, and
runner reservation. It does not assert that any domain or storage was cleaned.
The abandoned UUID is permanently retired. A disk found or restored later may
be imported only as a new instance with a new UUID and currently available
name and address.

## 19. Configuration and setup

Initial setup asks for anticipated VM runners and persists the corresponding
slot prefix. Advanced configuration may specify the prefix directly:

```toml
# One of 24, 25, 26, or 27. Persisted on first initialization.
runner_prefix = 25

# These addresses must be outside the complete Bento private range.
controller_address = "10.0.0.10"
frontend_addresses = ["10.0.0.11"]
oci_builder = "runner-a"

[[runners]]
name = "runner-a"
endpoint = "https://10.0.0.21:10443"

[[runners]]
name = "runner-b"
endpoint = "https://10.0.0.22:10443"
```

This shape is illustrative rather than a committed parser interface. Private
keys and enrollment secrets do not belong inline in the main TOML file.
Setup validates every controller, frontend, runner endpoint, listener, and
route next-hop address against the whole private range. A host or route backend
remains inactive until this validation succeeds. Setup also requires the
underlay isolation mode, underlay interface, authenticated frontend sources,
runner listener address, host-firewall policy, route backend, and designated
OCI builder.

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
- user network generation and missing participant acknowledgements;
- controller lease, certificate expiry, revocation, and CA rollover state;
- stale instance counts;
- route, bridge, proxy-ARP, firewall, domain, and database reconciliation;
- slot-drain and prefix-change plans.

User-facing `ls` and the dashboard need not expose placement by default, but
must distinguish a stopped instance from one whose runner is unreachable.

Logs attach runner ID, slot, instance UUID, controller epoch, desired
generation, and request ID to every distributed lifecycle action.

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
9. User network participant acknowledgements against the desired generation.
10. Runner durable generations and outcomes against pending controller work.
11. Global image checksums against per-runner readiness records.

An unreachable runner produces one reachability finding and stale dependent
checks, not a false list of missing domains and files.

## 22. Implementation sequence

1. Add schema migrations, controller fencing, and the runner/slot data model;
   migrate one-host deployments to `/24` slot 0.
2. Make store queries, restore, polling, and reconcile explicitly host-scoped.
3. Rename `bento_lifecycle::Runner` to `CommandRunner`, add a host registry,
   and dispatch existing lifecycle actions by `host_id`.
4. Implement `bentod runner` with certificate roles, mutual TLS, durable
   generation and outcome storage, and local host checks.
5. Move overlay, seed, domain XML, image presence, and libvirt operations
   behind the runner boundary.
6. Add slot-aware address allocation and placement reservations.
7. Implement durable network generations, route convergence, proxy ARP, and
   the multi-node nftables policy; validate with two real runners.
8. Validate the same route abstraction over one supported routed VPN setup.
9. Keep HTTP and SSH data connections on routed guest addresses. Dispatch only
   SSH auto-start and CLI/API lifecycle actions by `host_id`.
10. Add the designated OCI builder, exact artifact distribution, per-runner
    image synchronization, and same-runner copy.
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
- grandfathered boundary addresses after subdivision;
- address exhaustion per slot;
- placement transaction races;
- network-generation acknowledgement barriers and manual route backends;
- private-range overlap rejection for every infrastructure address;
- host-scoped polling, restore, and reconcile;
- nftables rendering for local, remote-same-user, frontend, cross-user, and
  internet traffic;
- route snapshots for LAN and routed-VPN backends;
- controller epoch fencing, generation ordering, outcome replay, and old
  database restore;
- certificate role, expiry, rotation, revocation, and CA rollover handling;
- idempotent runner retries and every cleanup-pending mutation;
- runner loss without false stopped states;
- HTTP 503 and prompt SSH failure without placement disclosure;
- designated-builder artifact verification and per-runner image state;
- slot subdivision and merge plans;
- prefix-lock races and retry or rollback from every slot-move phase;
- consistent and explicitly best-effort backup reports;
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
13. A spoofed guest source, spoofed underlay source, and unapproved frontend
    source are denied.
14. Verification that broadcast-dependent discovery does not accidentally
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

The design relies on four documented Linux/libvirt behaviors:

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
