# RemoteLinux — Specification

**Status:** v0.1 implemented (see §16) · **Date:** 2026-10-09

## 1. Summary

RemoteLinux lets you control an Arch Linux **desktop** (the *host*) from an Arch Linux **laptop** (the *client*) over a local network. The client shows a live view of the host's screen, and keyboard and mouse input on the client goes to the host. You can launch apps, type in Firefox, use a terminal, and so on.

**Priorities, in order:**

1. **Low latency.** Input should feel immediate and the picture should update with as little delay as possible.
2. **Reliability.** No stuck keys, no runaway mouse, and it reconnects without fuss.
3. **Security.** Only a manually paired laptop can connect, and all traffic is encrypted.
4. **Image quality.** Lowest priority. Blur, compression artifacts, and lower resolution are fine.

## 2. Target Environment

| | Host (desktop) | Client (laptop) |
|---|---|---|
| OS | Arch Linux (rolling) | Arch Linux (rolling) |
| Session | Wayland, **Hyprland** | Wayland (Hyprland assumed, but any compositor should work) |
| GPU | NVIDIA RTX 2080 SUPER (NVENC) | Unknown. Use VA-API decode if available, otherwise software decode |
| Network | Same LAN as client (Ethernet preferred) | Same LAN (Wi-Fi OK) |

The host user must be logged into a Hyprland session. Remote login from a powered-off or logged-out state is not supported.

## 3. Goals and Non-Goals

### Goals (v1)
- Live view of one host monitor on the client, in a window or fullscreen.
- Full keyboard forwarding, including modifiers and Super-key Hyprland shortcuts, so the remote desktop can be driven entirely from the laptop.
- Mouse forwarding: motion, buttons, and scroll (including smooth/high-resolution scroll).
- One-time manual pairing between laptop and desktop. After that, connecting is a single command.
- Glass-to-glass latency of **≤ 50 ms** on wired/good Wi-Fi LAN (target ≤ 30 ms).
- Built-in latency/stats overlay.

### Non-Goals (v1)
- Connections over the internet / outside the LAN (NAT traversal, relays).
- Audio forwarding.
- File transfer.
- Multiple simultaneous clients.
- Wake-on-LAN, login screen access, X11 hosts.
- High-fidelity image quality.

### Nice-to-have (post-v1)
- Clipboard sync (text) in both directions.
- Monitor switching for multi-monitor hosts.
- Wake-on-LAN.
- Audio (Opus over the same connection).
- Internet use via Tailscale/WireGuard (should work with no code changes if the client can reach the host IP).

## 4. Prior Art (why build this)

- **Sunshine + Moonlight:** very low latency and works on Hyprland. It is the main benchmark, and if this project stalls, it is the fallback. It is game-oriented and heavier than needed.
- **wayvnc / VNC:** Wayland-native but the protocol is high-latency (no hardware video encode).
- **RustDesk:** general-purpose, and Wayland support relies on portal dialogs.

RemoteLinux aims to be a small, Hyprland-focused, LAN-only tool that is tuned purely for latency.

## 5. Architecture

```
┌──────────────────────── HOST (desktop) ────────────────────────┐
│                                                                │
│  Capture ──► Scale/convert ──► NVENC H.264 ──► Packetizer ──┐  │
│  (wlr-screencopy / ext-image-copy-capture)                  │  │
│                                                             ▼  │
│  uinput virtual kbd + mouse ◄── Input handler ◄──── QUIC endpoint
│                                                             ▲  │
└─────────────────────────────────────────────────────────────┼──┘
                                                    LAN (UDP) │
┌─────────────────────────────────────────────────────────────┼──┐
│                                                             ▼  │
│  Window (Wayland) ◄── Renderer ◄── Decoder ◄── Depacketizer    │
│        │                                                       │
│        └─► Input capture (keyboard-shortcuts-inhibit,          │
│            pointer events) ──► Input sender ──► QUIC endpoint  │
│                                                                │
└──────────────────────── CLIENT (laptop) ───────────────────────┘
```

### 5.1 Components

| Binary | Runs on | Role |
|---|---|---|
| `rl-host` | Desktop | Daemon (systemd user service). Captures the screen, encodes, streams, injects input. |
| `rl-client` | Laptop | Connects, decodes, displays, captures and forwards input. |

Both are built from one Rust workspace with a shared `rl-proto` crate.

### 5.2 Technology Choices

| Concern | Choice | Rationale |
|---|---|---|
| Language | Rust | Performance, safety, good Wayland/QUIC/FFmpeg bindings. |
| Transport | QUIC via `quinn`. **Unreliable datagrams** for video, **reliable stream** for input/control | Built-in TLS 1.3 encryption, cert pinning, one UDP port, mixed reliability. |
| Screen capture | Wayland `ext-image-copy-capture-v1` (preferred) or `wlr-screencopy-unstable-v1` (fallback), via `wayland-client` | Direct compositor protocol. No portal permission dialog, lowest latency, frames delivered on damage. |
| Encoding | FFmpeg `h264_nvenc` via `ffmpeg-next` | Hardware encode in ~1–3 ms. H.264 decodes everywhere. |
| Decoding | FFmpeg: VA-API hwaccel if present, else software `h264` with `LOW_DELAY` flags | Software decode of 720p/1080p is cheap and predictable. |
| Input injection | Linux `uinput` (virtual keyboard + absolute pointer) | Works regardless of compositor, behaves like real hardware, supports every key. |
| Client window/render | `winit` + `wgpu` (or SDL3 if simpler), presented with vsync **off** (mailbox/immediate) | Avoid the extra frame of vsync latency. |
| Discovery | Manual IP/hostname in config. Optional mDNS (`_remotelinux._udp`) | Simple, and mDNS is convenient. |

## 6. Latency Design

Every stage is tuned for latency at the cost of quality or bandwidth.

### 6.1 Budget (target, LAN)

| Stage | Budget |
|---|---|
| Capture (after compositor commits frame) | ≤ 3 ms |
| Colour convert + downscale (GPU) | ≤ 2 ms |
| NVENC encode | ≤ 3 ms |
| Network (LAN) | ≤ 2 ms wired / ≤ 8 ms Wi-Fi |
| Decode | ≤ 4 ms |
| Render + present (vsync off) | ≤ 3 ms |
| Display scanout (client panel) | ~8 ms @ 60 Hz |
| **Total glass-to-glass** | **~25–35 ms** |
| Input path (client event → uinput) | ≤ 5 ms |

### 6.2 Encoder Settings (defaults)

- Codec: H.264, profile High (or Baseline if decode issues).
- NVENC preset `p1`, tune `ull` (ultra-low-latency), `rc=cbr`, `zerolatency=1`.
- **No B-frames**, `delay=0`, no lookahead.
- GOP: infinite, with **intra-refresh** so there are no large periodic I-frame spikes. IDR only on demand (client request after loss).
- Resolution: **native by default**. NVENC takes the captured BGRx frame directly, so there is no CPU scaling step. `720p`, `900p` and `1080p` downscale on the CPU (FFmpeg swscale).
- Frame rate: match the host monitor up to 60 fps (configurable to 120).
- Bitrate: 15 Mbps default (configurable).
- Frames are encoded **on damage**. When the screen is static, nothing is sent except a 1 fps keep-alive frame.

### 6.3 Network Behaviour

- One encoded frame is split into ≤ 1200-byte QUIC datagrams with a header: `frame_id`, `fragment_idx`, `fragment_count`, `capture_ts_us`, `flags (keyframe)`.
- **No jitter buffer.** The client decodes each frame as soon as all its fragments arrive.
- If a frame is incomplete when a newer frame finishes, the old one is dropped and the client sends a `RequestKeyframe` (rate-limited to 1 per 250 ms). Intra-refresh also recovers on its own within about 0.5 s.
- **Latest wins:** if the decoder is busy, queued older frames are discarded rather than decoded late.
- The host socket uses DSCP `EF` / `SO_PRIORITY` for lower Wi-Fi queueing where supported.

### 6.4 Client Rendering

- Decoded frame goes to a GPU texture and is drawn immediately. Present mode is `Mailbox` or `Immediate`, never `Fifo`.
- The render loop wakes when a frame is decoded, not on a timer.
- The cursor is drawn **locally on the client** (OS cursor visible over the window). This hides video latency for pointer movement. The host cursor is excluded from capture (`overlay_cursor = 0`) by default.

## 7. Input

### 7.1 Client-Side Capture

- **Keyboard:** physical **evdev keycodes** are captured (Wayland `wl_keyboard` keycodes, which are evdev + 8). They are sent unchanged, so keyboard layout is resolved on the host, just like a real keyboard.
- **Shortcut grabbing:** the client uses `zwp_keyboard_shortcuts_inhibit_manager_v1` while focused, so Super+key, Alt+Tab, and similar combos go to the host instead of the laptop's Hyprland.
- **Escape hatch:** a fixed client-local chord (default `Ctrl+Alt+Shift+Q`) releases the grab and is never forwarded. A second chord (`Ctrl+Alt+Shift+F`) toggles fullscreen. Configurable.
- **Pointer:** absolute coordinates normalised to `[0, 1]` against the video area (letterboxing handled), plus buttons and scroll (`axis_value120` for high-res wheels; discrete fallback).
- Optional **relative mode** (pointer lock) for apps or games that need it. Toggled by a chord.

### 7.2 Wire Delivery

| Event | Channel | Notes |
|---|---|---|
| Key down/up | Reliable stream | Must never be lost or reordered. |
| Mouse buttons | Reliable stream | Same. |
| Scroll | Reliable stream | |
| Pointer motion | Datagram | Sent on every client event, with sequence numbers. The host ignores out-of-order (stale) packets. |

### 7.3 Host-Side Injection (uinput)

- On start, `rl-host` creates two uinput devices:
  - `RemoteLinux Keyboard`: all `KEY_*` codes.
  - `RemoteLinux Pointer`: `ABS_X/ABS_Y` (range = captured output's logical size), `BTN_LEFT/RIGHT/MIDDLE/SIDE/EXTRA`, `REL_WHEEL`, `REL_HWHEEL`, `REL_WHEEL_HI_RES`, `REL_HWHEEL_HI_RES`.
- Absolute coordinates are mapped to the selected monitor's position within Hyprland's layout (queried via `hyprctl monitors -j` / Hyprland IPC socket).
- **Stuck-key protection:** the host tracks pressed keys and buttons and **releases all of them** when any of these happen:
  - client disconnect or timeout (no heartbeat for 1 s),
  - client sends `FocusLost` (client window loses focus or grab is released),
  - session ends.
- Permissions: the host user needs write access to `/dev/uinput`. The package ships a udev rule (`/usr/lib/udev/rules.d/70-remotelinux.rules`) that grants it to the `input` group (or `uaccess`), and documents `usermod -aG input <user>`.

## 8. Pairing and Security

### 8.1 Identity

- On first run, each device generates a long-term **Ed25519 key pair** and self-signed X.509 certificate, stored in `~/.local/share/remotelinux/identity/` (mode `0600`).
- A device's identity is the SHA-256 fingerprint of its certificate.

### 8.2 Pairing Flow (one-time, manual)

1. On the desktop, run `rl-host pair`. It prints a **6-digit PIN** valid for 2 minutes and listens for one pairing attempt.
2. On the laptop, run `rl-client pair <host-ip-or-name>` and enter the PIN.
3. The devices run **SPAKE2** (`spake2` crate) over a temporary QUIC connection using the PIN as the password. The shared key authenticates an exchange of the two certificate fingerprints, so a MITM without the PIN cannot get in.
4. Each side stores the other's fingerprint:
   - Host: `~/.config/remotelinux/authorized_clients.toml` (fingerprint, name, paired date).
   - Client: `~/.config/remotelinux/known_hosts.toml` (name, address, fingerprint).
5. Pairing mode exits automatically. Three wrong PINs abort pairing.

### 8.3 Connection

- Mutual TLS 1.3 inside QUIC, with both sides **pinning** the stored fingerprint. Unknown peers are rejected before any frame or input is processed.
- The host accepts **one client at a time**. A new connection from a paired client replaces the old one, so a laptop that drops off Wi-Fi reconnects instantly instead of waiting for the old session to time out. The replaced client is told why and does not auto-reconnect, which prevents two laptops from fighting over the host.
- `rl-host unpair <name>` / `rl-client forget <name>` revoke trust.
- Optional: the host shows a desktop notification (`notify-send`) when a client connects or disconnects.

### 8.4 Threat Model Notes

- Anyone with a paired laptop has full control of the desktop session, the same as sitting at it. Protect the laptop.
- By default the host listens on LAN interfaces only. A config option sets the bind address/interface.

## 9. Protocol (`rl-proto`)

All control messages are serialised with `postcard` (serde, compact binary) and length-prefixed on the reliable stream.

```text
// Client → Host (reliable stream)
Hello        { version, client_name, max_width, max_height, decoder_caps }
Key          { code: u16, pressed: bool }
Button       { code: u16, pressed: bool }
Scroll       { v120_x: i32, v120_y: i32 }
FocusLost
RequestKeyframe
SetConfig    { resolution, fps, bitrate_kbps }
Ping         { t_client_us }
Bye

// Host → Client (reliable stream)
Welcome      { version, host_name, stream_width, stream_height, fps, monitor_name }
Pong         { t_client_us, t_host_us }
StreamReset  { stream_width, stream_height }   // monitor/res change
Notice       { text }

// Client → Host (datagram)
Motion       { seq: u32, x: f32, y: f32 }        // normalised 0..1
MotionRel    { seq: u32, dx: f32, dy: f32 }

// Host → Client (datagram)
VideoFragment{ frame_id: u32, idx: u16, count: u16, capture_ts_us: u64, flags: u8, payload: [u8] }
```

- `Ping/Pong` every 500 ms gives RTT and clock offset (used for the latency overlay). Missing 2 consecutive pongs (≥ 1 s) counts as disconnected.
- Protocol `version` mismatch → connection refused with a clear error.

## 10. User Interface

### 10.1 CLI

```text
rl-host                      # run daemon in foreground
rl-host pair                 # enter pairing mode, show PIN
rl-host list | unpair <name>
rl-host status               # connected client, stream stats

rl-client pair <host>
rl-client connect [<name>]   # default: last/only known host
rl-client list | forget <name>
  --res 720p|900p|1080p|native  --fps 60  --bitrate 10M
  --fullscreen  --monitor <name>
```

### 10.2 Client Window

- Title `RemoteLinux — <host>`. Resizable, aspect-correct letterboxing.
- Overlay toggled by `Ctrl+Alt+Shift+S`: RTT, estimated glass-to-glass latency, fps, bitrate, dropped frames, decode time.
- Clear status screens: "Connecting…", "Host unreachable", "Not paired", "Reconnecting…".
- **Auto-reconnect** with backoff (0.5 s → 5 s) if the connection drops.
- Optional `.desktop` file so the client appears in the app launcher (e.g. walker).

### 10.3 Configuration

`~/.config/remotelinux/host.toml`
```toml
listen = "0.0.0.0:47800"
monitor = "auto"          # or e.g. "DP-1"
resolution = "720p"
fps = 60
bitrate_kbps = 10000
show_cursor_in_stream = false
notify_on_connect = true
```

`~/.config/remotelinux/client.toml`
```toml
default_host = "desktop"
fullscreen = false
release_chord = "Ctrl+Alt+Shift+Q"
decoder = "auto"           # auto | vaapi | software
```

## 11. Packaging and Deployment

- Ship one PKGBUILD (`remotelinux-git`) that installs both binaries. Dependencies: `ffmpeg`, `wayland`, `libva` (optional), `nvidia-utils` (host, for NVENC).
- systemd **user** unit `rl-host.service`:
  - `After=graphical-session.target`, `PartOf=graphical-session.target`
  - Imports `WAYLAND_DISPLAY`/`HYPRLAND_INSTANCE_SIGNATURE` (Hyprland: `exec-once = systemctl --user import-environment ...` or `uwsm`).
  - `Restart=on-failure`.
- udev rule for `/dev/uinput` (see §7.3).
- Firewall: document opening UDP 47800 (e.g. `ufw allow 47800/udp` on the host).

## 12. Error Handling and Edge Cases

| Situation | Behaviour |
|---|---|
| Host monitor resolution/refresh changes | Host restarts the capture and encoder, sends `StreamReset`, and the client resizes. |
| Selected monitor unplugged | Fall back to the primary monitor and send a `Notice`. |
| Screen locked (hyprlock) | The lock screen is captured and shown like anything else, so you can type the password remotely. |
| Monitor off via DPMS | Capture may stall. Host sends `Notice`. Forwarded input wakes the display. |
| NVENC unavailable | Fall back to `libx264 -preset ultrafast -tune zerolatency` with a warning. |
| Client window loses focus | Send `FocusLost` → host releases all keys. Stop forwarding input. |
| Packet loss | Drop the frame, request a keyframe (rate-limited), and let intra-refresh recover. |
| Hyprland restarts | Host daemon reconnects to the new Wayland socket or exits, and systemd restarts it. |

## 13. Milestones

1. **M0 — Spike:** host captures a screen via screencopy and saves PNGs. Separately, uinput types "hello" into a focused terminal.
2. **M1 — Input only:** client window (blank) captures keyboard and mouse and forwards them over QUIC to uinput. Verify there are no stuck keys and Super shortcuts work.
3. **M2 — Video:** screencopy (SHM buffers) → NVENC → datagrams → software decode → wgpu render. Measure latency.
4. **M3 — Pairing and security:** identities, SPAKE2 pairing, pinned mutual TLS.
5. **M4 — Polish:** overlay, auto-reconnect, config files, systemd unit, udev rule, PKGBUILD.
6. **M5 — Latency optimisation:** DMA-BUF zero-copy capture → CUDA/NVENC import, VA-API decode on client, tuning.
7. **Post-v1:** clipboard, monitor switching, audio, Wake-on-LAN.

## 14. Testing and Acceptance Criteria

**Latency measurement:**
- Built-in: `capture_ts_us` + clock offset → overlay shows host-capture-to-client-present time.
- End-to-end: point a 240 fps phone camera at both screens (or use a host-side on-screen millisecond clock) and compare. Optionally, an `rl-client --latency-test` mode sends a key press and times how long a host test window takes to flash on screen.

**v1 is accepted when:**
- [ ] Pairing works once and never needs to be repeated across reboots.
- [ ] Unpaired devices cannot connect (verified with a fresh client identity).
- [ ] Typing in a host terminal and Firefox from the laptop works with no dropped, doubled, or stuck keys over a 10-minute session.
- [ ] Hyprland Super shortcuts (launcher, workspace switching, closing windows) work on the host from the laptop.
- [ ] Mouse clicking, dragging, and scrolling work accurately on the selected monitor.
- [ ] Median glass-to-glass latency ≤ 50 ms on Wi-Fi and ≤ 35 ms wired, at 720p60.
- [ ] Disconnecting Wi-Fi mid-session releases all held keys on the host, and the client auto-reconnects when the network returns.
- [ ] `rl-host` survives Hyprland monitor changes without manual restart.

**Automated tests:**
- `rl-proto` serialisation round-trip and fuzz tests.
- Packetizer/depacketizer with simulated loss, reordering, and duplication.
- Pairing: correct PIN succeeds, wrong PIN fails, PIN expiry, MITM simulation.
- Input state machine: release-all on every disconnect path.

## 15. Open Questions

1. Does the host have multiple monitors? If yes, which should be the default, and is switching needed in v1?
2. What GPU does the laptop have? This decides whether VA-API decode is worth prioritising.
3. Is the laptop's Hyprland using a non-default keyboard layout? (Evdev forwarding assumes the host layout is the one you want.)
4. Is 720p acceptable as the default, or should it match the laptop's native resolution?
5. Does clipboard sync belong in v1? Copying text between the two machines is a common need.

## 16. Implementation Notes (v0.1)

Built as a Cargo workspace: `rl-common` (protocol, transport, identity, pairing, trust stores, config, client helpers), `rl-host` (library + binary) and `rl-client` (binary).

**Where v0.1 differs from the sections above:**

| Area | Spec | v0.1 |
|---|---|---|
| Capture | ext-image-copy-capture, falling back to wlr-screencopy | wlr-screencopy v3 with SHM buffers, double-buffered, `copy_with_damage` |
| Zero-copy | DMA-BUF → CUDA (M5) | Not yet. The frame is copied to system memory, then uploaded by NVENC. About 4.5 ms total at 1080p |
| Client window | winit + wgpu | SDL3 renderer (IYUV texture, vsync off, letterboxed logical presentation) |
| Keyboard | `wl_keyboard` evdev codes | SDL3 `raw` keycode, which is the evdev code on Wayland (X11: −8). A HID→evdev table is the fallback |
| Shortcut grab | keyboard-shortcuts-inhibit | `SDL_SetWindowKeyboardGrab`, which uses that protocol on Wayland |
| Decode | VA-API, else software | Software only, slice-threaded, low-delay (~1 ms at 1080p on the desktop CPU) |
| Congestion control | (unspecified) | Fixed 16 MiB QUIC window. The stream is CBR and LAN-only, so the controller never delays a frame |
| Relative mouse | ABS + REL on one device | Separate uinput device ("RemoteLinux Relative Mouse"), because libinput rejects mixed ABS/REL pointers |
| Liveness | — | `remotelinux-probe/1` ALPN: TLS handshake + fingerprint check, then close. Used by `rl-client list --probe` |
| Stuck keys | Release on disconnect / focus loss | Same, plus a 1 s watchdog: the client pings every 250 ms, and if it goes quiet the host releases all keys and buttons without ending the session |
| Discovery | mDNS optional | Not implemented. Use an IP or hostname |
| Close codes | — | 1 not paired, 2 replaced, 3 version, 4 stream error, 5 host shutdown |

**Measured** (desktop loopback, 1080p60, RTX 2080 SUPER, `rl-client bench`): capture→decoded p50 **6.0 ms**, p95 6.6 ms, 58–60 fps, 0 drops. At 720p: p50 4.6 ms.

**Milestones:** M0–M4 done. M5 (DMA-BUF zero-copy, VA-API) and the post-v1 items are still open.
