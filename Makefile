CARGO ?= cargo

# The two binaries a release build produces. `bentod` is the deployment;
# `bento-monitor` is the operator's terminal screen over it (DEPLOYING.md
# section 6). The build lists both, because a second binary is easy to
# miss in a tree that had one for a long time.
BINARIES = bentod bento-monitor

.PHONY: build monitor test unit e2e clippy fmt check dashboard clean

build:
	$(CARGO) build --release
	@echo
	@echo "built:"
	@ls -lh $(addprefix target/release/,$(BINARIES)) | awk '{printf "  %-28s %s\n", $$9, $$5}'
	@echo
	@echo "  bentod          the three processes, one subcommand each"
	@echo "  bento-monitor   the terminal screen over the units; run it as root"

# Start the monitor against this host, without installing it first.
monitor:
	$(CARGO) run --release --bin bento-monitor

test:
	$(CARGO) test --workspace

# The two halves separately, matching how CI runs them.
unit:
	$(CARGO) test --workspace --lib --bins

# The end-to-end suite (TESTING.md). It runs the real bentod binary, so
# it needs qemu-img, xorriso, and a /dev/kvm the host check can stat.
e2e:
	$(CARGO) test --package bentod --test e2e -- --nocapture

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

fmt:
	$(CARGO) fmt --all

check: clippy test

# Build the dashboard assets (SPEC 14.1). The output in web/dist is
# committed and embedded into the binary with rust-embed; the deployed
# artifact needs no Node runtime. Rerun this after changing web/src.
dashboard:
	cd web && npm ci && npm run build

clean:
	$(CARGO) clean
