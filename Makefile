CARGO ?= cargo

# The two binaries a release build produces. `bentod` is the deployment;
# `bento-monitor` is the operator's terminal screen over it (DEPLOYING.md
# section 6). The build lists both, because a second binary is easy to
# miss in a tree that had one for a long time.
BINARIES = bentod bento-monitor

.PHONY: build install install-runners monitor test unit script-test e2e clippy fmt fmt-check check clean

build:
	$(CARGO) build --release
	@echo
	@echo "built:"
	@ls -lh $(addprefix target/release/,$(BINARIES)) | awk '{printf "  %-28s %s\n", $$9, $$5}'
	@echo
	@echo "  bentod          the three processes, one subcommand each"
	@echo "  bento-monitor   the terminal screen over the units; run it as root"
	@scripts/build-info.sh

# Installs the build on this machine: `sudo make install`. It keeps the
# old binaries and, where serve runs, a database copy, then prints the
# restart and the rollback (DEPLOYING.md section 6). It depends on no
# build on purpose: under sudo, root has no toolchain.
install:
	@scripts/install.sh $(CURDIR)/target/release

# Sends the build to every other runner over SSH and installs it there.
# Run it as yourself, not with sudo.
install-runners:
	@scripts/install-runners.sh $(CURDIR)/target/release

# Start the monitor against this host, without installing it first.
monitor:
	$(CARGO) run --release --bin bento-monitor

test:
	$(CARGO) test --workspace

# The two halves separately, matching how CI runs them.
unit:
	$(CARGO) test --workspace --lib --bins

# The install scripts, against fakes.
script-test:
	scripts/install_test.sh

# The end-to-end suite (TESTING.md). It runs the real bentod binary, so
# it needs qemu-img, xorriso, and a /dev/kvm the host check can stat.
e2e:
	$(CARGO) test --package bentod --test e2e -- --nocapture

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

fmt:
	$(CARGO) fmt --all

# Fails on unformatted code instead of rewriting it, matching CI. Run
# `make fmt` to fix what it reports.
fmt-check:
	$(CARGO) fmt --all --check

check: fmt-check clippy test script-test

clean:
	$(CARGO) clean
