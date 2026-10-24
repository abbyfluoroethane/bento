#!/usr/bin/env bash
# Writes the BUILD file beside the release binaries (DEPLOYING.md
# section 6). `make build` runs it as the operator, so the install that
# runs later as root does not need git: git refuses a tree that root
# does not own.
#
# protocol and schema let the install compare two builds. A change of
# PROTOCOL_VERSION means the machines must restart together, and a
# change of schema means serve migrates the database at its next start
# (MULTI-NODE 16).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
release=${1:-$root/target/release}

commit=$(git -C "$root" describe --always --dirty 2>/dev/null || echo unknown)
protocol=$(sed -n 's/^pub const PROTOCOL_VERSION: u32 = \([0-9]*\);$/\1/p' \
    "$root/crates/runner/src/protocol.rs")
# Only the entries of MIGRATIONS sit at this indent.
schema=$(sed -n 's/^        version: \([0-9]*\),$/\1/p' \
    "$root/crates/store/src/migrate.rs" | sort -n | tail -n 1)

[ -n "$protocol" ] || { echo "build-info: no PROTOCOL_VERSION found" >&2; exit 1; }
[ -n "$schema" ] || { echo "build-info: no migration version found" >&2; exit 1; }

cat > "$release/BUILD" <<EOF
commit=$commit
protocol=$protocol
schema=$schema
EOF
echo "  BUILD           commit $commit, protocol $protocol, schema $schema"
