# Bento

One Rust workspace, one binary (`bentod`), one libvirt/KVM host. `SPEC.md` is the contract: when code and spec disagree, fix one in the same change. Runbook: `DEPLOYING.md`. Test tiers: `TESTING.md`.

## Commands

* `make check` runs fmt, clippy with `-D warnings`, and every test. It must pass before you report a task as complete.
* `make e2e` needs `qemu-img` and `xorriso`.
* Dashboard dev server: `BENTO_DEV_PORT=18080 cargo test -p bento-api dev_server -- --nocapture`. Stop it with `lsof -ti tcp:18080 -sTCP:LISTEN | xargs kill`. Never kill by bare port. That kills the user's browser too.

## Rules

* Toolchain comes from `rust-toolchain.toml`. Name no toolchain elsewhere.
* Versions live in `[workspace.dependencies]`. Every TLS user is `default-features = false` with the `ring` feature. `aws-lc-rs` does not build on the host.
* No Node, no JavaScript build, no frontend framework. Vendor finished files under `crates/dashboard/assets`.
* No C bindings beyond the bundled SQLite. libvirt speaks XDR over the socket.
* Each crate declares the narrow trait it needs. Only `bentod` knows concrete types (`bentod/src/adapters.rs`).
* Host-touching code sits behind a trait with an in-memory fake. A new trait comes with a fake. A behavior change comes with a test.
* Errors map to a status in one place, `error_parts` in `crates/api/src/lib.rs`. Never in a handler.
* Visibility and the HTTP port change through the lifecycle, never the store. Both reload the firewall.
* The UUID is the instance key. The name is a label. The same holds for a host: `/etc/machine-id` is the key, the hostname is a label. Never key on the kernel hostname; it is the transient one and it drifts.
* `schema.sql` is the baseline. A schema change after it ships as the next numbered migration in `crates/store/src/migrate.rs`. Never edit an applied migration; add the next number. A migration inspects before it mutates, so a database that already has the change only records the version.
* A migration is safe because there is a way back: `bentod dump-db` writes a copy, `bentod restore-db` reads one and then migrates it forward, and `bento-monitor` offers both on the Config tab. Take a copy before running a migration against real data.
* Read settings from the parsed `Config`. A missing required value stops `serve` at startup.
* A backend gap does not block frontend work. Put it behind a trait with a placeholder, badge placeholder data in the UI, and file a GitHub issue assigned to zackerthescar.
* There are no per-user quotas (issue #22). Capacity belongs to a host: SPEC 6.1 for one host, a per-runner cap for many (MULTI-NODE 12). The dashboard compares provisioned resources against the host, one host at a time.
* Names in `operators` are host root.

## Words

* The UI says "VM". Code and SPEC say "instance". No third name.
* Prose follows the `ste-writing` skill. Lint with `python3 ~/.claude/skills/ste-writing/ste-lint.py <file>`.
* Comments explain why and cite the SPEC section, for example `(SPEC 7.2)`.
* Use `example.org` in tests, seeds, docs, and screenshots. No real emails or hosts.

## Commits

* Subject names the area and the change: `proxy: answer 503 for a stopped instance`.
* No `Co-Authored-By` for an AI agent, no "generated with" footer.
* Commit only when asked. Push only when asked.
