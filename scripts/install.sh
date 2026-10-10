#!/usr/bin/env bash
# Build and install RemoteLinux for the current user (no root needed except
# for the optional udev rule / firewall steps it prints).
#   scripts/install.sh host     # on the desktop: rl-host + systemd user service
#   scripts/install.sh client   # on the laptop:  rl-client
set -euo pipefail
role=${1:-}
[[ $role == host || $role == client ]] || { echo "usage: $0 host|client" >&2; exit 2; }
cd "$(dirname "$0")/.."
bin="$HOME/.local/bin"
mkdir -p "$bin"
cargo build --release -p "rl-$role"
install -m755 "target/release/rl-$role" "$bin/rl-$role"
echo "Installed $bin/rl-$role"

if [[ $role == host ]]; then
  unit_dir="$HOME/.config/systemd/user"
  mkdir -p "$unit_dir"
  sed "s|/usr/bin/rl-host|$bin/rl-host|" packaging/rl-host.service > "$unit_dir/rl-host.service"
  systemctl --user daemon-reload
  systemctl --user enable --now rl-host.service
  echo "rl-host service enabled (logs: journalctl --user -u rl-host -f)"
  if [[ ! -w /dev/uinput ]]; then
    echo
    echo "! /dev/uinput is not writable. Install the udev rule:"
    echo "    sudo install -Dm644 packaging/70-remotelinux.rules /etc/udev/rules.d/70-remotelinux.rules"
    echo "    sudo udevadm control --reload && sudo udevadm trigger --name-match=uinput"
  fi
  if systemctl is-active -q ufw 2>/dev/null; then
    echo
    echo "! ufw is active. Allow RemoteLinux from your LAN, e.g.:"
    echo "    sudo ufw allow from 192.168.0.0/16 to any port 47800 proto udp"
  fi
  echo
  echo "Next: run 'rl-host pair' here, then 'rl-client pair <this-ip>' on the laptop."
  ip -4 -o addr show scope global | awk '{print "  this desktop: " $4}'
fi
