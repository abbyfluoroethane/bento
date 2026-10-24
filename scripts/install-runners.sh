#!/usr/bin/env bash
# Sends this build to every other runner and installs it there
# (DEPLOYING.md section 6). Run it as the operator, not with sudo: it
# connects with the operator's SSH keys, and each runner runs
# install.sh under its own sudo.
#
# The runners are the [[runners]] of the controller configuration. The
# entry whose endpoint is this machine's own [runner] listen address is
# this machine, which `sudo make install` covers. A runner is reached
# by its name, so ~/.ssh/config needs a Host entry for each one. The
# SSH details stay out of bento.toml on purpose: the configuration
# refuses an unknown key, so a new key there would stop an older
# binary, and that is the binary a rollback needs to start.
#
# Every machine must run the same build (DEPLOYING.md section 6;
# MULTI-NODE 11.5 covers the protocol), so the check
# after each install compares checksums. It never trusts the file size.
#
# Usage: install-runners.sh [release-dir]
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
release=${1:-$root/target/release}
config=${BENTO_CONFIG:-/etc/bento/bento.toml}
bin_dir=/usr/local/bin
# Tests replace these.
ssh=${BENTO_SSH:-ssh}
scp=${BENTO_SCP:-scp}
binaries=(bentod bento-monitor)

die() {
    echo "install-runners: $*" >&2
    exit 1
}

for name in "${binaries[@]}" BUILD; do
    [ -f "$release/$name" ] || die "$release/$name is missing; run make build first"
done
[ -r "$config" ] || die "cannot read $config"
[ "$(id -u)" -ne 0 ] || die "run it as yourself, not with sudo; it uses your SSH keys"

# Prints "name endpoint" for each [[runners]] entry, then "listen
# address" for this machine's [runner] section.
entries=$(awk '
    function value(line) { sub(/^[^=]*=[ \t]*"/, "", line); sub(/".*$/, "", line); return line }
    /^\[\[runners\]\]/ { if (name != "") print name, endpoint; name = ""; endpoint = ""; section = "runners"; next }
    /^\[runner\]/      { if (name != "") print name, endpoint; name = ""; section = "runner"; next }
    /^\[/              { if (name != "") print name, endpoint; name = ""; section = ""; next }
    section == "runners" && /^name[ \t]*=/     { name = value($0) }
    section == "runners" && /^endpoint[ \t]*=/ { endpoint = value($0) }
    section == "runner"  && /^listen[ \t]*=/   { print "listen", value($0) }
    END { if (name != "") print name, endpoint }
' "$config")

listen=$(awk '$1 == "listen" { print $2 }' <<< "$entries")
targets=()
while read -r name endpoint; do
    [ "$name" = listen ] && continue
    # http://10.0.0.97:10443/ becomes 10.0.0.97:10443.
    address=${endpoint#*://}
    address=${address%%/*}
    [ "$address" = "$listen" ] && continue
    targets+=("$name")
done <<< "$entries"

if [ ${#targets[@]} -eq 0 ]; then
    echo "install-runners: $config names no other runner; nothing to do"
    exit 0
fi

commit=$(sed -n 's/^commit=//p' "$release/BUILD")
for name in "${binaries[@]}"; do
    if ! cmp -s "$release/$name" "$bin_dir/$name"; then
        echo "install-runners: note: this machine does not run $commit yet; run sudo make install too"
        break
    fi
done

tty=()
[ -t 0 ] && tty=(-t)
restarts=()
for target in "${targets[@]}"; do
    echo "== $target"
    "$ssh" -o BatchMode=yes "$target" true ||
        die "cannot reach $target over SSH; add a Host $target entry to ~/.ssh/config"
    remote=$("$ssh" "$target" mktemp -d)
    [[ $remote == /tmp/* ]] || die "mktemp on $target gave \"$remote\", not a directory under /tmp"
    "$scp" -q "$release/bentod" "$release/bento-monitor" "$release/BUILD" \
        "$root/scripts/install.sh" "$target:$remote/"
    # tee keeps the output on the screen and gives the restart line to
    # the summary below.
    if ! output=$("$ssh" "${tty[@]}" "$target" "sudo $remote/install.sh $remote" | tee /dev/stderr); then
        "$ssh" "$target" rm -rf "$remote"
        die "the install on $target failed; the runners after it were not touched"
    fi
    "$ssh" "$target" rm -rf "$remote"

    for name in "${binaries[@]}"; do
        want=$(sha256sum < "$release/$name" | awk '{print $1}')
        have=$("$ssh" "$target" sha256sum "$bin_dir/$name" | awk '{print $1}')
        [ "$want" = "$have" ] || die "$target has a different $name ($have, not $want)"
    done
    echo "  checked   sha256 of both binaries matches this build"

    line=$(sed -n 's/^  restart   \(sudo .*\)$/\1/p' <<< "${output//$'\r'/}")
    [ -n "$line" ] && restarts+=("ssh -t $target '$line'")
done

if [ ${#restarts[@]} -gt 0 ]; then
    echo
    echo "restart each runner after serve on this machine:"
    printf '  %s\n' "${restarts[@]}"
fi
