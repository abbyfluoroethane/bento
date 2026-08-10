GO ?= go
BIN := bin/bentod

.PHONY: build test vet check dashboard clean

build:
	$(GO) build -o $(BIN) ./cmd/bentod

test:
	$(GO) test ./...

vet:
	$(GO) vet ./...

check: vet test

# Build the dashboard assets (SPEC 14.1). The output in web/dist is
# committed and embedded into the Go binary via go:embed; the deployed
# artifact needs no Node runtime. Rerun this after changing web/src.
dashboard:
	cd web && npm ci && npm run build

clean:
	rm -rf bin
