#!/usr/bin/env bash
# Installs one build of bentod and bento-monitor on this machine
# (DEPLOYING.md section 6). `sudo make install` runs it here, and
# `make install-runners` copies it to each runner and runs it there.
#
# It never builds. Under sudo, root has no toolchain, and a build as
# root would leave target/ owned by root.
#
# Before it replaces anything, it keeps a way back: the old binaries,
# and on the machine that runs serve, a database copy. It restarts
# nothing. It prints the restart and the rollback instead.
#
# Usage: install.sh [release-dir]
# The release directory holds bentod, bento-monitor, and BUILD.
set -euo pipefail

release=${1:-$(cd "$(dirname "$0")/.." && pwd)/target/release}
# The defaults are the runbook paths. Tests move them.
bin_dir=${BENTO_BIN_DIR:-/usr/local/bin}
state_dir=${BENTO_STATE_DIR:-/var/lib/bento}
systemctl=${BENTO_SYSTEMCTL:-systemctl}
keep=${BENTO_KEEP:-5}
rollback_root=$state_dir/rollback
# The BUILD of the binaries that are installed now.
record=$rollback_root/installed
binaries=(bentod bento-monitor)

die() {
    echo "install: $*" >&2
    exit 1
}

field() {
    [ -f "$2" ] && sed -n "s/^$1=//p" "$2" | head -n 1
    return 0
}

for name in "${binaries[@]}"; do
    [ -x "$release/$name" ] || die "$release/$name is missing; run make build first"
done
[ -f "$release/BUILD" ] || die "$release/BUILD is missing; run make build first"
[ -w "$bin_dir" ] || die "cannot write $bin_dir; run it with sudo"

# A build is its commit and the checksum of its bentod. Two builds of
# one dirty tree share the commit, and only the checksum tells them
# apart.
short_sum() { sha256sum < "$1" | cut -c1-8; }
new_build="$(field commit "$release/BUILD") sha-$(short_sum "$release/bentod")"
old_build=
if [ -f "$bin_dir/bentod" ]; then
    # A machine installed before this script has no record, but the
    # checksum still names its build.
    old_build="$(field commit "$record") sha-$(short_sum "$bin_dir/bentod")"
    old_build=${old_build# }
fi

same=yes
for name in "${binaries[@]}"; do
    if ! [ -f "$bin_dir/$name" ] ||
        [ "$(sha256sum < "$release/$name")" != "$(sha256sum < "$bin_dir/$name")" ]; then
        same=no
    fi
done
if [ "$same" = yes ]; then
    echo "install: $new_build is already installed; nothing to do"
    exit 0
fi

# Only the bentod units that are on this machine. serve owns the
# database (SPEC 4), so the machine that runs it is the one to copy.
units=$("$systemctl" list-unit-files 'bentod-*' --no-legend | awk '{print $1}' | sed 's/\.service$//')
has_unit() { grep -qx "$1" <<< "$units"; }

# The configuration serve reads, from its own unit, so the copy comes
# from the database serve uses.
serve_config() {
    local argv
    argv=$("$systemctl" show bentod-serve -p ExecStart --value |
        sed -n 's/.*argv\[\]=\([^;]*\);.*/\1/p')
    # shellcheck disable=SC2086 # split argv on purpose
    set -- $argv
    while [ $# -gt 0 ]; do
        case $1 in
        -config | --config) echo "$2"; return ;;
        -config=* | --config=*) echo "${1#*=}"; return ;;
        esac
        shift
    done
    echo /etc/bento/bento.toml
}

stamp=$(date +%Y%m%d-%H%M%S)
dir=$rollback_root/$stamp-$(tr ' ' - <<< "${old_build:-none}")
mkdir -p "$rollback_root"
# The database holds sessions and keys.
chmod 700 "$rollback_root"
mkdir "$dir"

if has_unit bentod-serve; then
    if [ -x "$bin_dir/bentod" ]; then
        # The installed binary makes the copy, never the new one.
        # dump-db opens the store, and opening it runs the migrations:
        # the new binary would migrate the live database under the
        # running serve before it copied it.
        config=$(serve_config)
        # Its log goes to the screen only when the copy fails.
        if ! log=$("$bin_dir/bentod" -config "$config" dump-db "$dir/bento.db" 2>&1); then
            rm -rf "$dir"
            echo "$log" >&2
            die "the database copy failed; nothing was installed"
        fi
    else
        echo "install: no bentod is installed yet, so there is no database to copy"
    fi
fi

had_old=no
for name in "${binaries[@]}"; do
    if [ -f "$bin_dir/$name" ]; then
        cp -p "$bin_dir/$name" "$dir/$name"
        had_old=yes
    fi
done
[ -f "$record" ] && cp -p "$record" "$dir/BUILD"

for name in "${binaries[@]}"; do
    install -m 0755 "$release/$name" "$bin_dir/$name"
done
cp "$release/BUILD" "$record"

# Keep the newest sets. A name starts with its time stamp, so the sort
# order is the age order.
find "$rollback_root" -mindepth 1 -maxdepth 1 -type d -name '[0-9]*-[0-9]*-*' |
    sort | head -n "-$keep" | xargs -r rm -rf

restart=
if has_unit bentod-serve; then
    rest=$(grep -vx bentod-serve <<< "$units" | tr '\n' ' ' | sed 's/ $//')
    restart="sudo systemctl restart bentod-serve"
    [ -n "$rest" ] && restart="$restart && sudo systemctl restart $rest"
elif [ -n "$units" ]; then
    restart="sudo systemctl restart $(tr '\n' ' ' <<< "$units" | sed 's/ $//')"
fi

echo "installed $new_build (was ${old_build:-nothing})"
echo "  backup    $dir"
if [ -n "$restart" ]; then
    echo "  restart   $restart"
else
    echo "  restart   no bentod units here yet; bento-monitor writes them"
fi

old_protocol=$(field protocol "$dir/BUILD")
new_protocol=$(field protocol "$release/BUILD")
if [ -n "$old_protocol" ] && [ "$old_protocol" != "$new_protocol" ]; then
    echo "  WARNING   the runner protocol changes from $old_protocol to $new_protocol:"
    echo "            install every machine, then restart them all together"
fi

if [ "$had_old" = yes ]; then
    echo "  rollback  sudo install -m 0755 $dir/bentod $dir/bento-monitor $bin_dir/"
    echo "            then restart as above"
    if [ -f "$dir/bento.db" ]; then
        old_schema=$(field schema "$dir/BUILD")
        new_schema=$(field schema "$release/BUILD")
        if [ -n "$old_schema" ] && [ "$old_schema" = "$new_schema" ]; then
            echo "            the schema stays at $new_schema, so keep the database"
        else
            echo "            if serve migrated (schema ${old_schema:-unknown} to $new_schema), stop the units"
            echo "            and run: sudo $bin_dir/bentod -config $config restore-db $dir/bento.db"
        fi
    fi
fi
