CARGO ?= cargo

.PHONY: build test clippy fmt check dashboard clean

build:
	$(CARGO) build --release

test:
	$(CARGO) test --workspace

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
