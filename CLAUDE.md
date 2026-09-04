# Project rules

This file covers Bento: one Rust workspace that builds one binary. Read `SPEC.md` before you change behavior. It is authoritative. A change that does not match the specification changes the specification too.

* Project: Bento
* Purpose: Linux VMs on one libvirt/KVM host, published under a domain over HTTPS and SSH.
* Accounts: yes, through OIDC only
* Frontend: server-rendered pages inside this repository, in `crates/api`

---

## 1. Stack

| Concern | Choice | Note |
|---------|--------|------|
| Language | Rust, edition 2024, nightly | `rust-toolchain.toml` pins the channel. Do not name a toolchain anywhere else. |
| Build | cargo, `make` | `make build`, `make check`, `make unit`, `make e2e` |
| Lint, format | rustfmt, clippy | `cargo clippy --workspace --all-targets -- -D warnings` must pass |
| Async runtime | tokio | |
| HTTP | axum 0.8, hyper | The JSON API and the pages share one `Config` |
| Templates | minijinja | `crates/api/templates`. `include_str!` embeds them at build time. |
| Dashboard UI | Basecoat 1.0.2 (Lyra), HTMX 2, uPlot | Vendored under `crates/dashboard/assets`. No Node, no Tailwind build. |
| Static assets | rust-embed | The dashboard crate serves them under `/assets/` |
| Database | SQLite through rusqlite, bundled | WAL. The control plane is the only writer. |
| Schema | `crates/store/src/schema.sql` | `CREATE TABLE IF NOT EXISTS`. There is no migration tool. |
| TLS | rustls with the `ring` provider | Never `aws-lc-rs`. The host has no cmake and no clang. |
| HTTP client | reqwest with `rustls-no-provider` | |
| SSH | russh | |
| libvirt | `crates/hypervisor`, XDR RPC over the unix socket | No C library. It implements only the procedures Bento calls. |
| Auth | OIDC through `crates/auth`, a session cookie | The only thing that creates an account |
| Tests | `cargo test`, fakes behind traits | Plus an end-to-end suite that runs the real binary |
| Deployment | One binary, three systemd units | `bentod serve`, `bentod proxy`, `bentod sshd` |

## 2. Setup

You need Rust nightly and a C compiler for the bundled SQLite. You do not need Node, cmake, or clang. `rustup` reads `rust-toolchain.toml` on the first `cargo` call.

```bash
make build       # target/release/bentod and target/release/bento-monitor
make check       # fmt, clippy -D warnings, then every test
make unit        # the in-process tests only
make e2e         # the end-to-end suite, needs qemu-img and xorriso
```

Run the dashboard over the test fakes:

```bash
BENTO_DEV_PORT=18080 cargo test -p bento-api dev_server -- --nocapture
```

Stop it with `lsof -ti tcp:18080 -sTCP:LISTEN | xargs kill`. Do not kill by bare port. A bare port match also kills the browser that has the page open.

Ask me before you add a crate that the task does not need.

## 3. Cargo rules

* Declare every dependency version in `[workspace.dependencies]` in the root `Cargo.toml`. A crate refers to it with `workspace = true`.
* Keep `default-features = false` on every TLS user, and select the `ring` feature. A dependency that pulls `aws-lc-rs` breaks the build on the host.
* Commit `Cargo.lock`. Never edit it by hand.
* Update one crate with `cargo update -p <name>`. Do not update every crate to fix one problem.
* Keep every tool setting in `Cargo.toml` or `rust-toolchain.toml`. Add no `rustfmt.toml` and no `clippy.toml` without a reason in the commit.

## 4. Code style

**Format and lint.** Run `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` before you report a task as complete. CI runs both.

**Errors.** A crate defines its own error type with `thiserror`. A consumer-side trait returns `BoxError`. One function, `error_parts` in `crates/api/src/lib.rs`, maps an error to a status. Never map an error to a status in a handler.

**Async.** Every host-touching call is `async`. Never block a tokio worker. Use `spawn_blocking` for a synchronous library such as rusqlite. The store already does this.

**Traits.** Each crate declares the narrow trait it needs. Only `bentod` knows every concrete type. `bentod/src/adapters.rs` wires them together.

**Comments.** Explain why. The code shows what. Cite the SPEC section when a rule comes from it, for example `(SPEC 7.2)`.

**Names.** Use one name for one thing. The dashboard says "VM". The specification and the code say "instance". Do not introduce a third name.

## 5. Layers

A call moves down, never up.

1. **HTTP surface.** `crates/api`. The JSON routes and the page handlers read the request and return a response. They hold no host logic.
2. **Lifecycle.** `crates/lifecycle`. It owns the order of operations for create, start, stop, rename, resize, and delete.
3. **Host machinery.** `crates/hypervisor`, `crates/images`, `crates/cloudinit`, `crates/network`. Each one talks to one thing on the host.
4. **Store.** `crates/store`. It maps tables. It holds no policy.

Rules:

* A page handler and a JSON handler call the same adapter. Never duplicate logic between them.
* A page handler renders a template. It builds no HTML in Rust beyond the two fallback strings in `pages/mod.rs`.
* Visibility and the HTTP port change through the lifecycle, never through the store. Both reload the firewall.
* A backend gap does not block frontend work. Put the gap behind a trait with a placeholder. Mark placeholder data in the UI. File a GitHub issue and assign it to zackerthescar.

## 6. Database

One SQLite file. `bentod serve` owns it. The SSH frontend also writes today. SPEC 4 forbids that, and the README lists it as a known gap.

* `schema.sql` runs at every open with `IF NOT EXISTS`. To add a table or a column, add it there and handle the old shape in code. There is no migration tool.
* The UUID is the instance key. The name is a label. Never join on the name.
* Times are RFC 3339 UTC text. Memory is MiB. Disk is GiB.
* Back up with `bentod dump-db`. Never copy the file while `serve` runs.
* Put no database file in the repository. The end-to-end suite creates its own in a temporary directory.

## 7. Authentication

* OIDC creates accounts. Nothing else does. Without a working provider nobody can sign up, over the web or over SSH.
* Read the user from the request extension that the `authenticate` middleware inserts. Never from a body or a query parameter.
* Scope every query by the user. A missing owner check leaks the machines of another person.
* A sharer can read a shared machine. Only the owner can change it. Return 404 to a stranger and 403 to a sharer who tries to write.
* Names in `operators` are host root. An operator chooses the input to a privileged build.

## 8. Configuration

One file, `/etc/bento/bento.toml`. `crates/config` parses it and fills every default. `bento.example.toml` documents every setting.

* Add a new setting to `bento.example.toml`, with its default in a comment, in the same commit that reads it.
* Read a setting from the parsed `Config`. Never read the environment in application code.
* A wrong or missing required value stops `serve` at startup. It must not fail at the first request.

## 9. File layout

```
bentod/                 the binary: subcommands, adapters, wiring
bentod/tests/e2e/       the end-to-end suite
bento-monitor/          the operator's terminal screen
crates/types            shared domain types
crates/config           TOML configuration
crates/store            SQLite
crates/hypervisor       libvirt XDR client
crates/images           the image store and the bootc builder
crates/cloudinit        NoCloud seed ISOs
crates/network          nftables and addressing
crates/lifecycle        orchestration
crates/tlscert          the wildcard certificate over ACME DNS-01
crates/proxy            the HTTPS proxy and the 503 page
crates/auth             OIDC, sessions, the sign-in page
crates/api              the JSON API, the page handlers, and templates/
crates/dashboard        static assets and their router; dev/check.js
crates/sshfront         the SSH frontend
crates/cli              the command line served over SSH
SPEC.md                 the specification
DEPLOYING.md            the runbook
TESTING.md              what the tests cover
```

## 10. The dashboard

The dashboard is server-rendered. Handlers live in `crates/api/src/pages`, templates in `crates/api/templates`, assets in `crates/dashboard/assets`. `crates/dashboard/assets/README.md` records the vendored versions.

Rules:

* Use Basecoat components. Use its `select` for a pick list and its `combobox` for a typed lookup. Never a native `<select>` or `<datalist>`.
* Every form works without JavaScript. HTMX adds polling and boosted navigation on top.
* A polled fragment sets `hx-target="this"`. HTMX inherits `hx-target` down the tree.
* A destructive button uses the destructive variant.
* Color never carries state alone. Pair it with a label.
* Placeholder data shows a "sample data" badge, driven by `placeholder: true` from the `Metrics` trait.
* Tokens live in `assets/css/app.css`. The accent is blue. Change it there and nowhere else.
* Keep the copy short. Say what the user can do, not why. Write "You cannot share this VM."
* Add a page test in `crates/api/src/pages/tests.rs` for every new route.

Check a change in a browser with `crates/dashboard/dev/check.js` (see `TESTING.md`). Look at the screenshots. A passing script is not a passing page.

## 11. Deployment

One binary, three systemd units, one host. Every host-touching step is in `DEPLOYING.md`.

* `bentod serve` refuses to start when the host lacks a requirement. Add a new requirement to the host check, not to a later error.
* The proxy binds every port from 3000 to 9999. A new listener must stay outside that range.
* A schema change ships with code that reads both shapes. There is no migration step to rely on.
* `bento-monitor` shows each command before it runs it. Keep that true.

## 12. Tests

* Everything that touches the host sits behind a trait with an in-memory fake. Do not mock what you own. Fake the host, the network, and the clock.
* A change to behavior comes with a test. A new trait comes with a fake.
* The end-to-end suite runs the real binary against a fake libvirtd. It needs `qemu-img` and `xorriso`. It runs in CI on x86_64 and arm64.
* Golden files in `crates/cloudinit/testdata` and `crates/hypervisor/testdata` change only on purpose. State the change in the commit.
* Tell me when a test fails. Show the output.

## 13. Writing

This applies to documentation, comments, error messages, and commit text. It does not apply to code, identifiers, or command syntax.

Load the `ste-writing` skill before you write prose in this repository. It lives at `~/.claude/skills/ste-writing`. Lint every prose file you wrote or changed, then fix what it reports:

```bash
python3 ~/.claude/vendor/ste-kit/videos/ep01-the-cure-for-ai-slop/ste-writing/ste-lint.py README.md
```

The rules in short:

* Use short common words. Use the active voice.
* Hold a sentence to 20 words. Use no contractions and no semicolons.
* Use one name for one thing across the whole project.
* Use no marketing words.

### Commits

* The subject line names the area and the change: `proxy: answer 503 for a stopped instance`.
* Write what changed and why in the body.
* Add no `Co-Authored-By` line for an AI agent and no "generated with" footer. The history names the people who own the work.
* Commit only when I ask. Push only when I ask.

## 14. Public repository

* Put no real name, email address, internal URL, or token in the code, the tests, or a screenshot. Use `example.org`.
* Keep secrets in `bento.toml` on the host. Only `bento.example.toml` is in Git.
* Put no database file, log file, or disk image in the repository.

## 15. Do not add

Ask me before you add anything outside the table in section 1.

Do not add these at all:

* A JavaScript build. No Node, no npm, no Tailwind compiler. Vendor a finished file.
* A frontend framework. No React, no Vue, no Svelte, no Alpine.
* A second template engine.
* A C-library binding for libvirt, TLS, or SQLite beyond the bundled SQLite.
* `aws-lc-rs` in any form.
* A migration framework. The schema is one SQL file.
* Per-user quotas. Issue #22 removes them.

## 16. Principles

**The specification is the contract.** When the code and `SPEC.md` disagree, one of them is wrong. Fix it in the same change.

**One binary.** Every feature must ship inside `bentod`. If it needs a runtime the host does not have, it does not ship.

**Fakes make the tests run anywhere.** A test that needs a real host is an end-to-end test. There are few of those.

Then the working rules:

* Read the file before you change it.
* Make the smallest change that solves the task.
* Do not add an abstraction that has one caller.
* Do not build a feature I did not ask for.
* Match the style of the code around you.
* Delete code that nothing calls. Do not comment it out.
* Run fmt, clippy, and the tests before you report a task as complete.
* Stop and ask when the task has two reasonable readings.
