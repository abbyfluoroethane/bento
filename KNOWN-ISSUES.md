# Known issues

Things found on the running deployment that are not yet fixed. Each entry
says what happens, what causes it, and what the fix is. Remove an entry
when its fix lands.

## 1. A disk smaller than the base image fails with a message nobody sees

**What happens.** A create with a disk smaller than the image's virtual
size fails. The dashboard reports a failure. The reason appears only in
the journal:

```
WARN new: failed, partial work unwound: images: qemu-img resize:
command exited with exit status: 1:
qemu-img: Use the --shrink option to perform a shrink operation.
```

**Cause.** `create_overlay` makes the overlay, then runs
`qemu-img resize <overlay> <disk_gib>G` (`crates/images/src/overlay.rs`).
`qemu-img` refuses to shrink below the backing file. Nothing compares the
requested disk against the image first:

* `crates/api/src/pages/home.rs` refuses only `disk_gib < 1`;
* `crates/api/src/instances.rs` refuses only `disk_gib < 0`;
* `create_overlay` stats the backing file, but never reads its virtual
  size.

Observed on 2026-09-05 with `debian-13`, whose virtual size is 3 GiB. No
instance on the deployment is smaller than 4 GiB, which is why this went
unseen until somebody asked for less.

**Fix.** Read the image version's virtual size and refuse a smaller disk
before any work starts. The message names the number the user needs, for
example "disk must be at least 3 GiB for debian-13". Map it through
`error_parts`, never in a handler. Store the virtual size on
`image_versions` when the image is fetched, so the check is a column read
rather than a `qemu-img info` call on every create.

**Do not pass `--shrink`.** It truncates the guest filesystem.

**Meanwhile.** Ask for a disk at least as large as the image.

## 2. A create that reaches the host ceiling only says so at the end

**What happens.** A create returns 409 with
`the host has no room: the disk limit is 160, 152 provisioned, N
requested`. The user learns the limit only after filling in the form.

**Cause.** This is the SPEC 6.1 ceiling working as designed, not a defect.
There is no per-user quota (issue #22), so the host is the only limit. The
create form does not show what is left, and does not cap its own inputs.

**Fix.** Show remaining headroom in the create form and bound the vCPU,
memory, and disk inputs by it. The front page already draws provisioned
against the host; the form should use the same numbers.

**The real fix is more than one runner.** One host has one ceiling. This
is the pressure that MULTI-NODE.md answers, and it is why the multi-node
work matters more than the form change.

## 3. Duplicate host rows on the running deployment

**What happens.** The `hosts` table holds four rows for one machine:
`linux`, `localhost.localdomain`, `Mac-mini`, and `konata`. The 13
instances are spread across three of them.

**Cause.** Version 1 read `/proc/sys/kernel/hostname`, the transient
hostname, and keyed the row on that string. NetworkManager renamed the
machine whenever no static hostname was set, and each rename made a row.
A static hostname was set on 2026-09-05, so no new row will appear.

**Fix.** Written, not yet deployed. `/etc/machine-id` is the key and the
hostname is a label; migration 2 collapses the rows and moves the
instances onto the one that remains. It is verified against copies of the
live database. It ships with the multi-node work, not before it.

## 4. A runner cannot yet be told which image version to fetch

**What happens.** A runner asked to fetch a named image version from the
allowlist URL can get different bytes and refuses them:

```
fetched image is sha256-f580e185..., not the requested sha256-3ddb4bb4...
```

**Cause.** This is not a defect in the check, and not a defect in the
allowlist. The allowlist says where a known-good image comes from. It
does not say the bytes never change, and for a distribution that is
correct: `debian.org` publishes a new build of trixie whenever it fixes
something, and the newer build is the one an operator wants. The URL
names a stream, and the checksum names one file from it.

The gap is that `EnsureImage` asks for one exact checksum. A runner
fetching the same URL an hour later gets the next build and refuses it,
even though that build is fine.

**Fix.** A runner fetches what the URL serves now and reports the
checksum it got. The controller records what each runner holds, and a new
instance uses the version its own runner has. Fetching becomes a sync,
not a match.

Two rules keep that safe:

* A runner keeps every version its own overlays are backed by. A
  `base_checksum` is a qcow2 backing file, so deleting the version an
  instance was built from breaks that instance (SPEC 5.1).
* A new machine only needs the current version. It holds no instances, so
  it backs nothing.

**Still to do.** Per-runner image readiness in the database, a sync when
a machine joins, and an operator action to pull a new version of an
image on every machine at once.

## 5. A copy of an instance on another machine is refused

**What happens.** `bento cp` refuses when the source runs on a machine
other than the controller's:

```
lifecycle: cp web: it runs on another machine, and copying between
machines is not implemented
```

**Cause.** A copy is a local file copy of the source's overlay, and a
copy stays on its source's machine (MULTI-NODE 13.3). Copying an overlay
between machines is a transfer with its own verification and fencing,
which is the slot-move workflow of section 17 and not this. The refusal
is deliberate: building the copy locally from a source that is elsewhere
would silently produce an empty disk.

**Fix.** Either send the copy to the machine that holds the source, so
the file copy stays local there, or add a verified transfer. The first is
the smaller change and matches section 13.3.

**Meanwhile.** Create a new instance rather than copying one that runs on
another machine.

## 6. Bento cannot tell a good host firewall from a bad one

**What happens.** `bentod runner` logs this on a machine that routes to
another machine, whether or not anything is wrong:

```
another nftables table filters the forward hook; Bento cannot accept
what it rejects  tables="inet firewalld filter_FORWARD"
```

**Cause.** nftables runs every base chain at a hook, and a `drop` or
`reject` in any of them is final, so Bento's table cannot override a host
firewall (MULTI-NODE 8.6). Bento names the tables it finds but does not
read their rules, so it cannot say whether they would reject guest
traffic. Deciding that needs the whole rule set evaluated against a
packet, and the answer changes with the packet.

**Fix.** Send a probe. Once a second machine holds a slot, the controller
could ask one machine to send a packet from a guest address to a guest
address on the other and report whether it arrived. That answers the real
question instead of guessing from rules. It needs a protocol operation
and somewhere on each machine to send from.

**Meanwhile.** Follow "the host firewall" in `DEPLOYING.md` on every
machine, and read the message as a reminder rather than a fault.

## 7. A restore only starts instances on the controller's own machine

**What happens.** After a machine reboots, `bentod serve` starts the
instances that are desired-running on the controller's machine. An
instance on another machine stays stopped until something else starts it.

**Cause.** Restore reads local libvirt and acts on this machine's rows
only (SPEC 11.2, MULTI-NODE 21). That was right when a deployment had one
machine. With more, each machine's runner sees its own domains, but
nothing tells it to start them: a runner starts a domain when the
controller asks, and the controller only asks for its own.

The observed state is correct in the meantime. The runner poll reads each
machine's inventory and records what it finds, so the dashboard shows a
stopped instance as stopped rather than as running (MULTI-NODE 20).

**Fix.** Give restore the same treatment placement got: read the desired
state of every machine's rows, and send each machine the starts it owes
through its runner. The runner already has `StartInstance`.

**Meanwhile.** Start such an instance from the dashboard or the CLI after
a reboot; that path already reaches the right machine.
