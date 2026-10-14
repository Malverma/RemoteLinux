#!/usr/bin/env bash
# Install RemoteLinux for the current user (same on desktop and laptop).
#
# Uses the prebuilt rl-host / rl-client / rl-gui next to this script if they
# are there (release tarball), otherwise builds them from source.
# Everything else (turning on sharing, firewall, pairing) is done in the app.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
bin="$HOME/.local/bin"
apps="$HOME/.local/share/applications"
mkdir -p "$bin" "$apps"

if [[ -x "$here/rl-gui" && -x "$here/rl-host" && -x "$here/rl-client" ]]; then
  src="$here"
else
  root="$(cd "$here/.." && pwd)"
  [[ -f "$root/Cargo.toml" ]] || { echo "No prebuilt binaries next to this script and no source tree found." >&2; exit 1; }
  echo "Building from source…"
  (cd "$root" && cargo build --release -p rl-host -p rl-client -p rl-gui)
  src="$root/target/release"
fi

for b in rl-host rl-client rl-gui; do
  install -m755 "$src/$b" "$bin/$b"
done
desktop="$here/remotelinux.desktop"
[[ -f "$desktop" ]] || desktop="$here/../packaging/remotelinux.desktop"
sed "s|^Exec=rl-gui|Exec=$bin/rl-gui|" "$desktop" > "$apps/remotelinux.desktop"

# If sharing was already turned on, restart it so the new rl-host is used.
if systemctl --user is-enabled -q rl-host.service 2>/dev/null; then
  systemctl --user restart rl-host.service || true
fi

missing=()
for lib in libavcodec.so libSDL3.so libopus.so; do
  ldconfig -p | grep -q "$lib" || missing+=("$lib")
done
if ((${#missing[@]})); then
  echo "! Missing runtime libraries (${missing[*]}). Install them with:  sudo pacman -S ffmpeg sdl3 opus"
fi

echo "Installed rl-host, rl-client and rl-gui to $bin."
echo "Open \"RemoteLinux\" from your app launcher (or run rl-gui)."
echo "  On the desktop: Share this PC → Turn on → Show pairing code."
echo "  On the laptop:  Pair new computer → pick the desktop → type the code."
