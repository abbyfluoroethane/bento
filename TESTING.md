# Testing Bento

Bento has two test tiers. Both run on every push, on x86_64 and on arm64.

| Tier | Command | What it drives |
| --- | --- | --- |
| Unit | `make unit` | Each crate against fake seams, in one process |
| End-to-end | `make e2e` | The shipped `bentod` binary, over real sockets |

`make test` runs both. `make check` adds the lint.

## The unit tier

Every crate keeps its tests beside the code, in `#[cfg(test)]` modules.
The host-touching seams have fakes: `bento_hypervisor::Fake` for libvirt,
injected command runners for `qemu-img` and `xorriso`, and injected clocks
for anything that expires. Two crates keep golden files —
`crates/cloudinit/testdata` and `crates/hypervisor/testdata` — so a change
to generated cloud-init data or domain XML has to be stated, not slipped
in.

## The end-to-end tier

The suite lives in `bentod/tests/e2e/`. Each test builds a whole
deployment in a temporary directory and runs the real binary against it.

Real, in every test:

* the `bentod` binary itself, started as a child process, one `serve` per
  test, plus `fetch-images` and `reconcile` as separate invocations;
* configuration parsing, from a generated `bento.toml`;
* the SQLite database, including its schema and the account, quota, and
  token rows the test seeds before startup;
* `qemu-img` and `xorriso`, which really do create the overlay disk and
  the seed ISO;
* the HTTP listener, reached over loopback with `reqwest`;
* bearer-token authentication, against the stored SHA-256 hash;
* the whole lifecycle path: quota check, address allocation, overlay,
  seed, domain definition, the state poller, rename, and delete;
* SIGTERM shutdown, which the suite asserts exits cleanly.

Substituted, because a CI runner cannot supply them:

| Substitute | File | Why |
| --- | --- | --- |
| libvirtd | `libvirtd.rs` | Speaks the libvirt RPC protocol on a Unix socket. `bentod` connects to it unmodified and cannot tell the difference. It records every procedure it answers, and the assertions read that recording. |
| The image mirror | `imageserver.rs` | Serves one small qcow2 on loopback, so `fetch-images` runs its real download, checksum, and pin-verification path without the network. |
| `nft` | a stub on `PATH` | Loading a real ruleset needs `CAP_NET_ADMIN` and would rewrite the runner's own firewall. The stub records the ruleset instead, and the assertions read what would have been loaded. |

The fake libvirtd writes out the wire encoding again rather than sharing
the client's codec. A fake built on the same encoder could not catch an
encoding mistake.

### What this tier does not cover

No guest ever boots. Nothing verifies that a Debian image really comes up,
that cloud-init really applies the seed, that the SSH frontend really
reaches a guest, or that the nftables ruleset really isolates one user
from another. Those need real hardware virtualization and real network
namespaces.

`MULTI-NODE.md` section 23 lists the live acceptance items and says
plainly that multi-node support cannot ship on fakes alone. This tier is
not that. It is the layer below: everything up to the hypervisor and the
kernel, on any machine, in under a second.

## Running the end-to-end suite locally

It needs three things on the host:

```sh
qemu-img --version     # from qemu-utils
xorriso --version
ls -l /dev/kvm         # only stat'ed, never opened
```

Then:

```sh
make e2e
```

A failing test prints the daemon's own stderr with the assertion, because
a failure here is usually explained by one log line.

Each test gets its own temporary directory, its own loopback port, and its
own daemon, so the suite runs in parallel and leaves nothing behind.

## Continuous integration

`.github/workflows/ci.yml` runs format, lint, unit, and end-to-end on two
runners: `ubuntu-24.04` for x86_64 and `ubuntu-24.04-arm` for arm64. The
matrix does not fail fast, so one architecture failing still reports the
other.

One thing about the arm64 runner is worth knowing. GitHub's hosted arm64
runners have no nested virtualization and therefore no `/dev/kvm`, while
the x86_64 ones do. `bentod serve` treats a missing `/dev/kvm` as a fatal
host requirement (SPEC 4.2 item 1), so the workflow creates a stub device
node when the real one is absent. Nothing in this tier opens the device —
the fake libvirtd stands in for everything behind it — so the stub only
satisfies the check that the host claims a KVM device.

That stub is also the boundary of what CI can do. A tier that boots real
guests can never run on the hosted arm64 runners, and would need a
self-hosted runner or a machine with working KVM.
