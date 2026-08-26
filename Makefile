CARGO ?= cargo

.PHONY: build test unit e2e clippy fmt check dashboard clean

build:
	$(CARGO) build --release

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
