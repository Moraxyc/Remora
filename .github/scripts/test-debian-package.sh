#!/usr/bin/env bash
# Installs, runs, upgrades, removes and purges a Remora .deb on this machine with sudo. Only for a
# throwaway Debian or Ubuntu with systemd, such as a CI runner: it adds a system user, starts a
# service on port 8080 and leaves /etc/remora behind.
set -Eeuo pipefail
trap 'echo "FAILED at line $LINENO: $BASH_COMMAND" >&2' ERR

deb=$(realpath "$1")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
url=http://127.0.0.1:8080/
wants=/etc/systemd/system/multi-user.target.wants/remora.service
socket_wants=/etc/systemd/system/sockets.target.wants/remora.socket

step() { printf '\n== %s\n' "$*"; }
fail() {
    echo "FAILED: $*" >&2
    sudo journalctl -u remora.service --no-pager -o cat | tail -n 50 >&2 || true
    exit 1
}
serving() {
    for _ in $(seq 30); do
        curl -fsS -o /dev/null "$url" && return
        sleep 1
    done
    fail "nothing serves $url"
}
main_pid() { systemctl show -P MainPID remora.service; }

step "a first install adds the user and /etc/remora, and starts nothing"
sudo apt-get install --yes "$deb"
id remora
[ "$(stat -c %U:%G:%a /etc/remora)" = root:remora:750 ] || fail "/etc/remora is $(stat -c %U:%G:%a /etc/remora)"
! systemctl is-active --quiet remora.service || fail "the service started without settings"
! systemctl is-enabled --quiet remora.service || fail "the service was enabled without settings"

step "the service runs as remora and reads root's settings through its group"
sudo install -m 0640 -g remora /dev/stdin /etc/remora/remora.toml << 'EOF'
listen = "127.0.0.1:8080"
database = "/var/lib/remora/remora.db"
sign_key_file = "sign.key"

[login]
auto_confirm = true
EOF
head -c 32 /dev/urandom | base64 | sudo install -m 0640 -g remora /dev/stdin /etc/remora/sign.key
sudo systemctl enable --now remora.service
serving
[ "$(systemctl show -P UID remora.service)" = "$(id -u remora)" ] || fail "the service does not run as remora"
sudo test -s /var/lib/private/remora/remora.db || fail "no database"

step "an upgrade restarts the service and keeps it enabled"
# The same package under a later version, so that dpkg takes its upgrade path.
dpkg-deb -R "$deb" "$work/root"
sed -i 's/^Version: .*/&+upgrade/' "$work/root/DEBIAN/control"
dpkg-deb --root-owner-group -b "$work/root" "$work/upgrade.deb" > /dev/null
before=$(main_pid)
sudo apt-get install --yes "$work/upgrade.deb"
serving
after=$(main_pid)
[ "$after" != 0 ] && [ "$after" != "$before" ] || fail "the service was not restarted ($before, $after)"
systemctl is-enabled --quiet remora.service || fail "the upgrade disabled the service"

step "the socket starts the service on the first connection"
sudo systemctl stop remora.service
sudo systemctl enable --now remora.socket
! systemctl is-active --quiet remora.service || fail "the service started before a connection"
serving
systemctl is-active --quiet remora.service || fail "a connection did not start the service"

step "removing stops the service and its socket, and keeps its settings and database"
sudo apt-get remove --yes remora
! systemctl is-active --quiet remora.socket || fail "the socket still listens"
! systemctl is-active --quiet remora.service || fail "the service still runs"
sudo test -s /var/lib/private/remora/remora.db || fail "removing deleted the database"
sudo test -e /etc/remora/remora.toml || fail "removing deleted the settings"

step "purging drops the database and the enablement, and keeps the settings"
sudo apt-get purge --yes remora
sudo test ! -e /var/lib/private/remora || fail "the database is still there"
sudo test ! -e /var/lib/remora -a ! -L /var/lib/remora || fail "/var/lib/remora is still there"
[ ! -L "$wants" ] || fail "the service is still enabled"
[ ! -L "$socket_wants" ] || fail "the socket is still enabled"
[ ! -L /etc/systemd/system/remora.service ] || fail "the service is still masked"
[ ! -L /etc/systemd/system/remora.socket ] || fail "the socket is still masked"
sudo test -e /etc/remora/remora.toml || fail "purging deleted the settings"

step "passed"
