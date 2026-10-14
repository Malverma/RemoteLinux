# RemoteLinux

Low-latency remote desktop from an Arch laptop to an Arch + Hyprland desktop
on the same LAN. Built for responsiveness, not image quality. See
[spec.md](spec.md) for the design.

- `rl-host`: runs on the desktop. Captures the screen (wlr-screencopy),
  encodes H.264 with NVENC, streams over QUIC and injects input via uinput.
- `rl-client`: runs on the laptop. Decodes and displays the stream in an
  SDL3 window and forwards keyboard and mouse.

Measured on the desktop over loopback (1080p60, RTX 2080 SUPER):
**~6 ms from capture to decoded frame** (4.5 ms capture+encode, 1 ms decode).
Over a LAN, add the network time (usually 1–5 ms on Wi-Fi) and the laptop
display's refresh.

## Install

Runtime dependencies: `sudo pacman -S ffmpeg sdl3`. Install the same thing on both machines:

```sh
./install.sh          # from the release tarball (uses the prebuilt binaries)
scripts/install.sh    # from a source checkout (builds first; needs rust and clang)
```

This installs `rl-host`, `rl-client` and `rl-gui` to `~/.local/bin` and adds
**RemoteLinux** to your app launcher. You can also build a package:
`cd packaging && makepkg -si`.

## Set up and pair (in the app, once)

1. **Desktop:** open RemoteLinux → **Share this PC** → **Turn on**. If it shows
   *Finish setup*, click **Allow** (firewall / input access, asks for your
   password once). Then click **Show pairing code**.
2. **Laptop:** open RemoteLinux → **Pair a computer**. The desktop appears in
   the list automatically (mDNS). Pick it, type the code and press **Pair**.
3. **Laptop:** click **Connect**.

Sharing starts automatically at every login once it's turned on.

## Connect from the terminal (optional)

```sh
rl-client connect            # the only, or most recently used, desktop
rl-client connect BhiziePC --res 720p --fullscreen --stats
```

The toolbar at the top of the viewer has one button per desktop monitor
(numbered in reading order: top-left first) and a Fullscreen button. Toolbar
clicks stay on the laptop. Desktop audio plays on the laptop's default output.

Inside the viewer (the chord keys are never sent to the desktop):

| Keys | Action |
|---|---|
| `Ctrl+Alt+Shift+Q` | Release / recapture keyboard and mouse (or click the window) |
| `Ctrl+Alt+Shift+F` | Toggle fullscreen |
| `Ctrl+Alt+Shift+S` | Toggle the stats overlay |
| `Ctrl+Alt+Shift+R` | Relative mouse mode (pointer lock, for games) |
| `Ctrl+Alt+Shift+D` | Disconnect |
| `Ctrl+Alt+Shift+1`…`9` | Stream monitor 1…9 (same numbering as the toolbar) |

While the viewer has focus, every key goes to the desktop, including
Super-key Hyprland shortcuts.

## Other commands

```sh
rl-host status | list | unpair <name> | disconnect | monitors
rl-client list [--probe] | probe [<host>] | forget <host>
rl-client bench [<host>] --seconds 10    # headless latency measurement
rl-client connect --view-only             # watch without sending input
```

Every command accepts `--json` for scripting and the GUI.

## Configuration

`~/.config/remotelinux/host.toml` (desktop):

```toml
listen = "0.0.0.0:47800"
monitor = "auto"        # focused monitor, or e.g. "DP-1"
resolution = "native"   # or "720p", "900p", "1080p"
fps = 60
bitrate_kbps = 15000
encoder = "auto"        # auto | nvenc | x264
show_cursor_in_stream = false
notify_on_connect = true
audio = true            # forward desktop audio (needs parec: libpulse)
```

`~/.config/remotelinux/client.toml` (laptop):

```toml
default_host = ""       # name from `rl-client list`
fullscreen = false
show_stats = false
release_chord = "Ctrl+Alt+Shift+Q"
resolution = ""         # empty = host default
fps = 0
bitrate_kbps = 0
monitor = ""
```

Trust stores live in `~/.config/remotelinux/authorized_clients.toml` (desktop)
and `known_hosts.toml` (laptop). Identities live in `~/.local/share/remotelinux/identity/`.

## Integration (GUI / scripts)

- **Library.** `rl_common::client::{pair, probe}`, `rl_common::trust::KnownHosts`
  and `rl_common::config::ClientConfig` are plain public Rust APIs.
- **Viewer events.** `rl-client connect <name> --json` writes one JSON object
  per line to stdout. The `event` field is one of `connecting`, `connected`,
  `stream`, `stats`, `input`, `disconnected`, `reconnecting`, `error`, `notice`,
  `fullscreen`, `stats_visible`, `relative_mouse`, `closed`.
- **Viewer commands.** When stdin is not a terminal, the viewer accepts JSON
  lines: `{"cmd":"toggle_stats"|"fullscreen"|"release"|"resume"|"relative_mouse"|"disconnect"}`
  or `{"cmd":"settings","resolution":"720p","fps":60,"bitrate_kbps":10000,"monitor":"DP-1"}`.
  Settings are a partial update: only the keys present change, and `"native"`, `0` or `""`
  reset a value to the host default. Closing stdin closes the viewer.
- **Host control socket.** `$XDG_RUNTIME_DIR/remotelinux/host.sock` takes JSON
  lines: `{"cmd":"status"|"list"|"pair"|"cancel_pair"|"unpair","name":...|"disconnect"}`.

## Logs

`RL_LOG=debug rl-host run`, or `journalctl --user -u rl-host -f`.
