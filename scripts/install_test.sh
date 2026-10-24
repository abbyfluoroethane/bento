#!/usr/bin/env bash
# Tests install.sh and install-runners.sh against fakes in a temporary
# directory. `make check` runs it. Nothing here touches the real host.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}

expect() { # expect <text> <haystack> <what>
    grep -qF -- "$1" <<< "$2" || fail "$3: no \"$1\" in:
$2"
}

refuse() { # refuse <text> <haystack> <what>
    if grep -qF -- "$1" <<< "$2"; then fail "$3: unexpected \"$1\""; fi
}

# A fake bentod. It names itself in what it writes, so a test can tell
# which binary made the database copy.
fake_bentod() { # fake_bentod <path> <label>
    cat > "$1" <<EOF
#!/usr/bin/env bash
echo "\$*" >> "$work/bentod.log"
echo "INFO database open" >&2
[ "\${FAIL_DUMP:-}" = 1 ] && exit 1
if [ "\$3" = dump-db ]; then echo "copy by $2 from \$2" > "\$4"; fi
EOF
    chmod +x "$1"
}

# A fake systemctl that knows the units in $UNITS.
cat > "$work/systemctl" <<'EOF'
#!/usr/bin/env bash
case $1 in
list-unit-files) for unit in $UNITS; do echo "$unit.service enabled disabled"; done ;;
show) echo "{ path=/usr/local/bin/bentod ; argv[]=/usr/local/bin/bentod -config /srv/bento.toml serve ; ignore_errors=no }" ;;
esac
EOF
chmod +x "$work/systemctl"

# fresh <name> <protocol> <schema>: a release directory and an empty
# machine with an old build installed.
fresh() {
    rm -rf "$work/release" "$work/bin" "$work/state" "$work/bentod.log"
    mkdir -p "$work/release" "$work/bin" "$work/state"
    fake_bentod "$work/release/bentod" new
    printf '#!/bin/sh\necho new monitor\n' > "$work/release/bento-monitor"
    chmod +x "$work/release/bento-monitor"
    printf 'commit=%s\nprotocol=%s\nschema=%s\n' "$1" "$2" "$3" > "$work/release/BUILD"
    fake_bentod "$work/bin/bentod" old
    printf '#!/bin/sh\necho old monitor\n' > "$work/bin/bento-monitor"
    chmod +x "$work/bin/bento-monitor"
    mkdir -p "$work/state/rollback"
    printf 'commit=aaa1111\nprotocol=1\nschema=7\n' > "$work/state/rollback/installed"
}

install() {
    BENTO_BIN_DIR=$work/bin BENTO_STATE_DIR=$work/state BENTO_SYSTEMCTL=$work/systemctl \
        "$here/install.sh" "$work/release" 2>&1
}

controller="bentod-proxy bentod-runner bentod-serve bentod-sshd"

# The controller: the old binary copies the database, then the new
# binaries go in.
fresh bbb2222 1 7
new_sum=$(sha256sum < "$work/release/bentod" | cut -c1-8)
old_sum=$(sha256sum < "$work/bin/bentod" | cut -c1-8)
out=$(UNITS=$controller install)
backup=$(ls -d "$work"/state/rollback/*-aaa1111-sha-"$old_sum")
expect "installed bbb2222 sha-$new_sum (was aaa1111 sha-$old_sum)" "$out" "controller"
expect "sudo systemctl restart bentod-serve && sudo systemctl restart bentod-proxy bentod-runner bentod-sshd" "$out" "serve restarts first"
expect "the schema stays at 7, so keep the database" "$out" "same schema"
refuse "INFO database open" "$out" "the copy's log stays off the screen"
expect "rollback  sudo install -m 0755 $backup/bentod $backup/bento-monitor $work/bin/" "$out" "rollback line"
refuse "WARNING" "$out" "same protocol"
grep -q "copy by old" "$backup/bento.db" || fail "the installed binary must make the copy"
grep -q -- "-config /srv/bento.toml dump-db" "$work/bentod.log" || fail "the copy must use serve's configuration"
grep -q "old monitor" "$backup/bento-monitor" || fail "the old monitor is kept"
grep -q "new monitor" "$work/bin/bento-monitor" || fail "the new monitor is installed"
grep -q "commit=bbb2222" "$work/state/rollback/installed" || fail "the record follows the install"

# The same build again changes nothing.
out=$(UNITS=$controller install)
expect "bbb2222 sha-$new_sum is already installed; nothing to do" "$out" "second run"
[ "$(ls -d "$work"/state/rollback/*-* | wc -l)" -eq 1 ] || fail "a second run must not add a backup"

# A runner: no database copy, and only its own unit restarts.
fresh bbb2222 1 7
out=$(UNITS=bentod-runner install)
backup=$(ls -d "$work"/state/rollback/*-aaa1111-sha-*)
expect "restart   sudo systemctl restart bentod-runner" "$out" "runner restart"
[ -e "$backup/bento.db" ] && fail "a runner holds no database to copy"
[ -e "$work/bentod.log" ] && fail "a runner must not run dump-db"

# A schema and a protocol change are both called out.
fresh ccc3333 2 8
out=$(UNITS=$controller install)
expect "the runner protocol changes from 1 to 2" "$out" "protocol change"
expect "if serve migrated (schema 7 to 8)" "$out" "schema change"
expect "-config /srv/bento.toml restore-db" "$out" "restore uses serve's configuration"

# A failed copy installs nothing and leaves no backup behind.
fresh ddd4444 1 7
out=$(UNITS=$controller FAIL_DUMP=1 install) && fail "a failed copy must fail the install"
expect "the database copy failed; nothing was installed" "$out" "failed copy"
expect "INFO database open" "$out" "a failed copy shows its log"
grep -q "old monitor" "$work/bin/bento-monitor" || fail "a failed copy must keep the old monitor"
[ -z "$(ls -d "$work"/state/rollback/*-* 2> /dev/null)" ] || fail "a failed copy must remove its backup"

# A machine with no record names its old build by checksum.
fresh ggg7777 1 7
rm "$work/state/rollback/installed"
sum=$(sha256sum < "$work/bin/bentod" | cut -c1-8)
out=$(UNITS=bentod-runner install)
expect "(was sha-$sum)" "$out" "no record"
ls -d "$work"/state/rollback/*[0-9]-sha-"$sum" > /dev/null || fail "the backup is named by checksum"

# Without BUILD, nothing is installed.
fresh eee5555 1 7
rm "$work/release/BUILD"
out=$(UNITS=$controller install) && fail "a missing BUILD must fail the install"
expect "BUILD is missing; run make build first" "$out" "missing BUILD"

# Only the newest BENTO_KEEP backups stay.
fresh fff6666 1 7
for n in 1 2 3; do
    printf 'build %s\n' "$n" >> "$work/release/bento-monitor"
    UNITS=bentod-runner BENTO_KEEP=2 install > /dev/null
    sleep 1
done
[ "$(ls -d "$work"/state/rollback/*-* | wc -l)" -eq 2 ] || fail "only two backups must stay"

# install-runners: the entry at this machine's listen address is this
# machine, and every other runner gets the build by its name.
fresh abc1234 1 7
cat > "$work/bento.toml" <<'EOF'
[runner]
listen = "10.0.0.188:10443"

[[runners]]
name = "konata"
endpoint = "http://10.0.0.188:10443"

[[runners]]
name = "tsukasa"
endpoint = "http://10.0.0.97:10443/"
underlay = "10.0.0.97"
EOF
want=$(sha256sum < "$work/release/bentod" | awk '{print $1}')
wantm=$(sha256sum < "$work/release/bento-monitor" | awk '{print $1}')
cat > "$work/ssh" <<EOF
#!/usr/bin/env bash
echo "ssh \$*" >> "$work/ssh.log"
case "\$*" in
*mktemp*) echo /tmp/bento-remote ;;
*install.sh*) echo "installed abc1234 (was aaa1111)"; echo "  restart   sudo systemctl restart bentod-runner" ;;
*bento-monitor) echo "$wantm  /usr/local/bin/bento-monitor" ;;
*bentod) echo "\${REMOTE_SHA:-$want}  /usr/local/bin/bentod" ;;
esac
EOF
printf '#!/bin/sh\necho "scp $*" >> %s/ssh.log\n' "$work" > "$work/scp"
chmod +x "$work/ssh" "$work/scp"
runners() {
    rm -f "$work/ssh.log"
    BENTO_CONFIG=$work/bento.toml BENTO_SSH=$work/ssh BENTO_SCP=$work/scp \
        "$here/install-runners.sh" "$work/release" 2>&1 < /dev/null
}
out=$(runners)
log=$(cat "$work/ssh.log")
expect "== tsukasa" "$out" "the other runner"
refuse "konata" "$log" "this machine is not a target"
expect "scp -q $work/release/bentod $work/release/bento-monitor $work/release/BUILD $here/install.sh tsukasa:/tmp/bento-remote/" "$log" "the build goes over"
expect "tsukasa sudo /tmp/bento-remote/install.sh /tmp/bento-remote" "$log" "the install runs there"
expect "sha256 of both binaries matches this build" "$out" "checksum check"
expect "ssh -t tsukasa 'sudo systemctl restart bentod-runner'" "$out" "restart summary"

out=$(REMOTE_SHA=0000 runners) && fail "a checksum mismatch must fail"
expect "tsukasa has a different bentod" "$out" "checksum mismatch"

if [ "$failures" -gt 0 ]; then
    echo "install_test: $failures failed" >&2
    exit 1
fi
echo "install_test: ok"
