//! The viewer window (SDL3): shows the remote screen and forwards input.
//!
//! The main thread runs the SDL event loop. Network and decoding run on their
//! own threads and wake the loop with an SDL user event, so a decoded frame is
//! drawn the moment it is ready (no vsync, no timer).

use std::collections::{HashSet, VecDeque};
use std::ffi::{CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use rl_common::proto::{ClientMsg, Motion, StreamSettings};
use rl_common::trust::KnownHost;
use rl_common::video::FrameMeta;
use serde_json::json;
use sdl3_sys::blendmode::SDL_BLENDMODE_BLEND;
use sdl3_sys::error::SDL_GetError;
use sdl3_sys::events::*;
use sdl3_sys::hints::*;
use sdl3_sys::init::*;
use sdl3_sys::keyboard::SDL_GetScancodeFromName;
use sdl3_sys::keycode::*;
use sdl3_sys::mouse::SDL_SetWindowRelativeMouseMode;
use sdl3_sys::pixels::SDL_PIXELFORMAT_IYUV;
use sdl3_sys::rect::SDL_FRect;
use sdl3_sys::render::*;
use sdl3_sys::scancode::SDL_Scancode;
use sdl3_sys::surface::SDL_SCALEMODE_LINEAR;
use sdl3_sys::video::*;

use crate::decode::{Decoder, Picture};
use crate::events::emit;
use crate::keymap;
use crate::net::{self, NetEvent, Outgoing};

pub struct Options {
    pub host: KnownHost,
    pub settings: StreamSettings,
    pub client_name: String,
    pub fullscreen: bool,
    pub show_stats: bool,
    /// Never forward input.
    pub view_only: bool,
    /// e.g. "Ctrl+Alt+Shift+Q"
    pub release_chord: String,
}

/// Commands from stdin (UI integration) or other threads.
#[derive(Debug)]
pub enum Command {
    ToggleStats,
    Fullscreen,
    Release,
    Resume,
    Relative,
    Disconnect,
    Settings(SettingsPatch),
}

/// A partial settings change: `None` = leave as is, `Some(None)` = host default.
#[derive(Debug, Default)]
pub struct SettingsPatch {
    pub max_height: Option<Option<u32>>,
    pub fps: Option<Option<u32>>,
    pub bitrate_kbps: Option<Option<u32>>,
    pub monitor: Option<Option<String>>,
}

struct Decoded {
    pic: Picture,
    meta: FrameMeta,
    decode_us: u64,
}

enum UiMsg {
    Net(NetEvent),
    Cmd(Command),
}

struct Shared {
    wake_type: u32,
    wake_pending: AtomicBool,
    queue: Mutex<VecDeque<UiMsg>>,
    decoded: Mutex<Option<Decoded>>,
    skipped: AtomicU32,
    bytes: std::sync::atomic::AtomicU64,
}

impl Shared {
    fn wake(&self) {
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            unsafe {
                let mut ev: SDL_Event = std::mem::zeroed();
                ev.r#type = self.wake_type;
                SDL_PushEvent(&mut ev);
            }
        }
    }
    fn post(&self, m: UiMsg) {
        self.queue.lock().unwrap().push_back(m);
        self.wake();
    }
}

fn sdl_err() -> String {
    unsafe { CStr::from_ptr(SDL_GetError()).to_string_lossy().into_owned() }
}

#[derive(Default)]
struct Stats {
    // Accumulated over the current 1 s window.
    frames: u32,
    bytes: u64,
    dropped: u32,
    decode_us: u64,
    latency_us: u64,
    latency_n: u32,
    host_us: u64,
    // Running values.
    rtt_us: u64,
    offset_us: Option<i64>,
    dropped_total: u64,
    // Last report.
    r_fps: f64,
    r_kbps: f64,
    r_decode_ms: f64,
    r_latency_ms: Option<f64>,
    r_host_ms: f64,
    r_dropped: u32,
}

struct Chord {
    ctrl: bool,
    alt: bool,
    shift: bool,
    gui: bool,
    release: SDL_Scancode,
}

impl Chord {
    fn parse(s: &str) -> Self {
        let mut c = Chord { ctrl: false, alt: false, shift: false, gui: false, release: SDL_Scancode(20) /* Q */ };
        for part in s.split('+').map(|p| p.trim()) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => c.ctrl = true,
                "alt" => c.alt = true,
                "shift" => c.shift = true,
                "super" | "meta" | "gui" | "win" => c.gui = true,
                key => {
                    let name = CString::new(key).unwrap_or_default();
                    let sc = unsafe { SDL_GetScancodeFromName(name.as_ptr()) };
                    if sc.0 != 0 {
                        c.release = sc;
                    }
                }
            }
        }
        if !(c.ctrl || c.alt || c.shift || c.gui) {
            c.ctrl = true;
            c.alt = true;
            c.shift = true;
        }
        c
    }

    fn mods_held(&self, m: SDL_Keymod) -> bool {
        let has = |k: SDL_Keymod| m.0 & k.0 != 0;
        (!self.ctrl || has(SDL_KMOD_CTRL)) && (!self.alt || has(SDL_KMOD_ALT)) && (!self.shift || has(SDL_KMOD_SHIFT)) && (!self.gui || has(SDL_KMOD_GUI))
    }
}

// SDL scancodes for the fixed chord keys.
const SC_F: i32 = 9;
const SC_S: i32 = 22;
const SC_R: i32 = 21;
const SC_D: i32 = 7;

struct Viewer {
    opts: Options,
    shared: Arc<Shared>,
    window: *mut SDL_Window,
    renderer: *mut SDL_Renderer,
    texture: *mut SDL_Texture,
    tex: (u32, u32),
    wayland: bool,
    x11: bool,
    net: net::NetHandle,
    settings: StreamSettings,
    connected: bool,
    status: String,
    status_detail: String,
    notice: Option<(String, Instant)>,
    monitor: String,
    forwarding: bool,
    focused: bool,
    relative: bool,
    fullscreen: bool,
    show_stats: bool,
    fatal: bool,
    motion_seq: u32,
    wheel_rem: (f32, f32),
    swallowed_keys: HashSet<i32>,
    swallowed_buttons: HashSet<u8>,
    chord: Chord,
    stats: Stats,
    stats_at: Instant,
    quit: bool,
}

pub fn run(opts: Options, commands: Option<mpsc::Receiver<Command>>) -> Result<()> {
    unsafe {
        SDL_SetHint(SDL_HINT_APP_ID, c"remotelinux".as_ptr());
        SDL_SetHint(SDL_HINT_RENDER_VSYNC, c"0".as_ptr());
        SDL_SetHint(SDL_HINT_MOUSE_FOCUS_CLICKTHROUGH, c"1".as_ptr());
        if std::env::var_os("SDL_VIDEO_DRIVER").is_none() {
            SDL_SetHint(SDL_HINT_VIDEO_DRIVER, c"wayland,x11".as_ptr());
        }
        if !SDL_Init(SDL_INIT_VIDEO | SDL_INIT_EVENTS) {
            bail!("SDL_Init failed: {}", sdl_err());
        }
    }
    let wake_type = unsafe { SDL_RegisterEvents(1) };
    if wake_type == 0 {
        bail!("SDL_RegisterEvents failed");
    }
    let shared = Arc::new(Shared {
        wake_type,
        wake_pending: AtomicBool::new(false),
        queue: Mutex::new(VecDeque::new()),
        decoded: Mutex::new(None),
        skipped: AtomicU32::new(0),
        bytes: Default::default(),
    });

    let title = CString::new(format!("RemoteLinux — {}", opts.host.name)).unwrap_or_default();
    let mut window = ptr::null_mut();
    let mut renderer = ptr::null_mut();
    unsafe {
        let flags = SDL_WINDOW_RESIZABLE | SDL_WINDOW_HIGH_PIXEL_DENSITY;
        if !SDL_CreateWindowAndRenderer(title.as_ptr(), 1280, 720, flags, &mut window, &mut renderer) {
            bail!("creating window: {}", sdl_err());
        }
        SDL_SetRenderVSync(renderer, 0);
        SDL_SetRenderDrawBlendMode(renderer, SDL_BLENDMODE_BLEND);
    }
    let driver = unsafe {
        let d = SDL_GetCurrentVideoDriver();
        if d.is_null() { String::new() } else { CStr::from_ptr(d).to_string_lossy().into_owned() }
    };
    tracing::info!("video driver: {driver}");

    // Decoder thread.
    let (frame_tx, frame_rx) = mpsc::channel::<(FrameMeta, Vec<u8>)>();
    let net_events = {
        let shared = shared.clone();
        move |ev: NetEvent| match ev {
            NetEvent::Frame { meta, data, .. } => {
                shared.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                let _ = frame_tx.send((meta, data));
            }
            other => shared.post(UiMsg::Net(other)),
        }
    };
    let net = net::spawn(opts.host.clone(), opts.settings.clone(), opts.client_name.clone(), net_events)?;
    {
        let shared = shared.clone();
        let tx = net.tx.clone();
        std::thread::Builder::new().name("rl-decode".into()).spawn(move || decode_loop(frame_rx, shared, tx))?;
    }
    if let Some(rx) = commands {
        let shared = shared.clone();
        std::thread::spawn(move || {
            for c in rx {
                shared.post(UiMsg::Cmd(c));
            }
        });
    }

    let mut v = Viewer {
        chord: Chord::parse(&opts.release_chord),
        settings: opts.settings.clone(),
        fullscreen: opts.fullscreen,
        show_stats: opts.show_stats,
        status: format!("Connecting to {}…", opts.host.name),
        opts,
        shared,
        window,
        renderer,
        texture: ptr::null_mut(),
        tex: (0, 0),
        wayland: driver == "wayland",
        x11: driver == "x11",
        net,
        connected: false,
        status_detail: String::new(),
        notice: None,
        monitor: String::new(),
        forwarding: true,
        focused: true,
        relative: false,
        fatal: false,
        motion_seq: 0,
        wheel_rem: (0.0, 0.0),
        swallowed_keys: HashSet::new(),
        swallowed_buttons: HashSet::new(),
        stats: Stats::default(),
        stats_at: Instant::now(),
        quit: false,
    };
    if v.fullscreen {
        unsafe { SDL_SetWindowFullscreen(v.window, true) };
    }
    v.render();
    v.main_loop();
    v.shutdown();
    Ok(())
}

fn decode_loop(rx: mpsc::Receiver<(FrameMeta, Vec<u8>)>, shared: Arc<Shared>, net_tx: tokio::sync::mpsc::UnboundedSender<Outgoing>) {
    let mut dec = match Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            shared.post(UiMsg::Net(NetEvent::Fatal(format!("{e:#}"))));
            return;
        }
    };
    let mut last_err = Instant::now() - Duration::from_secs(1);
    for (meta, data) in rx {
        let t0 = rl_common::clock::now_us();
        match dec.decode(&data) {
            Ok(Some(pic)) => {
                let d = Decoded { pic, meta, decode_us: rl_common::clock::now_us() - t0 };
                if shared.decoded.lock().unwrap().replace(d).is_some() {
                    shared.skipped.fetch_add(1, Ordering::Relaxed);
                }
                shared.wake();
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!("decode error: {e:#}");
                if last_err.elapsed() > Duration::from_millis(250) {
                    last_err = Instant::now();
                    let _ = net_tx.send(Outgoing::Msg(ClientMsg::RequestKeyframe));
                }
            }
        }
    }
}

impl Viewer {
    fn active(&self) -> bool {
        self.connected && self.forwarding && self.focused && !self.opts.view_only
    }

    fn send(&self, m: ClientMsg) {
        let _ = self.net.tx.send(Outgoing::Msg(m));
    }

    fn update_grab(&self) {
        unsafe {
            SDL_SetWindowKeyboardGrab(self.window, self.active());
            SDL_SetWindowRelativeMouseMode(self.window, self.relative && self.active());
        }
    }

    fn main_loop(&mut self) {
        let mut ev: SDL_Event = unsafe { std::mem::zeroed() };
        while !self.quit {
            let got = unsafe { SDL_WaitEventTimeout(&mut ev, 250) };
            if got {
                self.handle_event(&mut ev);
                while unsafe { SDL_PollEvent(&mut ev) } {
                    self.handle_event(&mut ev);
                    if self.quit {
                        return;
                    }
                }
            }
            self.shared.wake_pending.store(false, Ordering::Release);
            let msgs: Vec<UiMsg> = self.shared.queue.lock().unwrap().drain(..).collect();
            let mut redraw = false;
            for m in msgs {
                redraw = true;
                match m {
                    UiMsg::Net(e) => self.on_net(e),
                    UiMsg::Cmd(c) => self.on_command(c),
                }
            }
            let decoded = self.shared.decoded.lock().unwrap().take();
            if let Some(d) = decoded {
                self.show_frame(d);
            } else if redraw {
                self.render();
            }
            if self.stats_at.elapsed() >= Duration::from_secs(1) {
                self.report_stats();
                if self.show_stats || !self.connected {
                    self.render();
                }
            }
            if let Some((_, t)) = &self.notice {
                if t.elapsed() > Duration::from_secs(4) {
                    self.notice = None;
                    self.render();
                }
            }
        }
    }

    fn handle_event(&mut self, ev: &mut SDL_Event) {
        let t = unsafe { ev.r#type };
        if t == self.shared.wake_type {
            return; // handled after the event batch
        }
        match SDL_EventType(t) {
            SDL_EVENT_QUIT | SDL_EVENT_WINDOW_CLOSE_REQUESTED => self.quit = true,
            SDL_EVENT_KEY_DOWN | SDL_EVENT_KEY_UP => self.on_key(unsafe { ev.key }),
            SDL_EVENT_MOUSE_MOTION => {
                if !self.active() || self.tex.0 == 0 {
                    return;
                }
                unsafe { SDL_ConvertEventToRenderCoordinates(self.renderer, ev) };
                let m = unsafe { ev.motion };
                self.motion_seq = self.motion_seq.wrapping_add(1);
                let motion = if self.relative {
                    Motion::Rel { seq: self.motion_seq, dx: m.xrel, dy: m.yrel }
                } else {
                    Motion::Abs { seq: self.motion_seq, x: m.x / self.tex.0 as f32, y: m.y / self.tex.1 as f32 }
                };
                let _ = self.net.tx.send(Outgoing::Motion(motion));
            }
            SDL_EVENT_MOUSE_BUTTON_DOWN | SDL_EVENT_MOUSE_BUTTON_UP => {
                let b = unsafe { ev.button };
                let down = SDL_EventType(t) == SDL_EVENT_MOUSE_BUTTON_DOWN;
                if down && !self.forwarding && self.connected {
                    // Click to resume; the click itself isn't forwarded.
                    self.swallowed_buttons.insert(b.button);
                    self.set_forwarding(true);
                    return;
                }
                if !down && self.swallowed_buttons.remove(&b.button) {
                    return;
                }
                if !self.active() {
                    return;
                }
                // Make sure the host pointer is where the click happens.
                if down && !self.relative && self.tex.0 > 0 {
                    unsafe { SDL_ConvertEventToRenderCoordinates(self.renderer, ev) };
                    let b2 = unsafe { ev.button };
                    self.motion_seq = self.motion_seq.wrapping_add(1);
                    let _ = self.net.tx.send(Outgoing::Motion(Motion::Abs { seq: self.motion_seq, x: b2.x / self.tex.0 as f32, y: b2.y / self.tex.1 as f32 }));
                }
                if let Some(code) = keymap::mouse_button(b.button) {
                    self.send(ClientMsg::Button { code, pressed: down });
                }
            }
            SDL_EVENT_MOUSE_WHEEL => {
                if !self.active() {
                    return;
                }
                let w = unsafe { ev.wheel };
                // SDL: +y = scroll up. Protocol: +y = scroll down.
                self.wheel_rem.0 += w.x * 120.0;
                self.wheel_rem.1 += -w.y * 120.0;
                let (ix, iy) = (self.wheel_rem.0.trunc() as i32, self.wheel_rem.1.trunc() as i32);
                self.wheel_rem.0 -= ix as f32;
                self.wheel_rem.1 -= iy as f32;
                if ix != 0 || iy != 0 {
                    self.send(ClientMsg::Scroll { v120_x: ix, v120_y: iy });
                }
            }
            SDL_EVENT_WINDOW_FOCUS_GAINED => {
                self.focused = true;
                self.update_grab();
                self.render();
            }
            SDL_EVENT_WINDOW_FOCUS_LOST => {
                if self.active() {
                    self.send(ClientMsg::ReleaseAll);
                }
                self.focused = false;
                self.swallowed_keys.clear();
                self.update_grab();
                self.render();
            }
            SDL_EVENT_WINDOW_RESIZED | SDL_EVENT_WINDOW_PIXEL_SIZE_CHANGED | SDL_EVENT_WINDOW_EXPOSED => self.render(),
            SDL_EVENT_WINDOW_ENTER_FULLSCREEN => self.fullscreen = true,
            SDL_EVENT_WINDOW_LEAVE_FULLSCREEN => self.fullscreen = false,
            _ => {}
        }
    }

    fn evdev_code(&self, k: &SDL_KeyboardEvent) -> Option<u16> {
        if self.wayland && k.raw != 0 {
            return Some(k.raw);
        }
        if self.x11 && k.raw > 8 {
            return Some(k.raw - 8);
        }
        keymap::hid_to_evdev(k.scancode.0)
    }

    fn on_key(&mut self, k: SDL_KeyboardEvent) {
        let sc = k.scancode.0;
        if std::env::var_os("RL_DEBUG_KEYS").is_some() {
            tracing::info!("key sc={sc} raw={} down={} repeat={} mod={:#x}", k.raw, k.down, k.repeat, k.r#mod.0);
        }
        if k.down && !k.repeat && self.chord.mods_held(k.r#mod) {
            let handled = match sc {
                s if s == self.chord.release.0 => {
                    let f = !self.forwarding;
                    self.set_forwarding(f);
                    true
                }
                SC_F => {
                    self.on_command(Command::Fullscreen);
                    true
                }
                SC_S => {
                    self.on_command(Command::ToggleStats);
                    true
                }
                SC_R => {
                    self.on_command(Command::Relative);
                    true
                }
                SC_D => {
                    self.on_command(Command::Disconnect);
                    true
                }
                _ => false,
            };
            if handled {
                // The modifiers already reached the host; let go of them there.
                if self.connected {
                    self.send(ClientMsg::ReleaseAll);
                }
                self.swallowed_keys.insert(sc);
                return;
            }
        }
        if !k.down && self.swallowed_keys.remove(&sc) {
            return;
        }
        if k.repeat || !self.active() {
            return; // the host compositor generates its own key repeat
        }
        if let Some(code) = self.evdev_code(&k) {
            self.send(ClientMsg::Key { code, pressed: k.down });
        }
    }

    fn set_forwarding(&mut self, on: bool) {
        if !on && self.active() {
            self.send(ClientMsg::ReleaseAll);
        }
        self.forwarding = on;
        self.update_grab();
        emit("input", json!({ "forwarding": on }));
        self.render();
    }

    fn on_command(&mut self, c: Command) {
        match c {
            Command::ToggleStats => {
                self.show_stats = !self.show_stats;
                emit("stats_visible", json!({ "visible": self.show_stats }));
            }
            Command::Fullscreen => unsafe {
                self.fullscreen = !self.fullscreen;
                SDL_SetWindowFullscreen(self.window, self.fullscreen);
                emit("fullscreen", json!({ "fullscreen": self.fullscreen }));
            },
            Command::Release => self.set_forwarding(false),
            Command::Resume => self.set_forwarding(true),
            Command::Relative => {
                self.relative = !self.relative;
                self.settings.show_cursor = Some(self.relative);
                self.send(ClientMsg::UpdateSettings(self.settings.clone()));
                self.update_grab();
                emit("relative_mouse", json!({ "relative": self.relative }));
            }
            Command::Disconnect => self.quit = true,
            Command::Settings(p) => {
                let s = &mut self.settings;
                if let Some(v) = p.max_height {
                    s.max_height = v;
                }
                if let Some(v) = p.fps {
                    s.fps = v;
                }
                if let Some(v) = p.bitrate_kbps {
                    s.bitrate_kbps = v;
                }
                if let Some(v) = p.monitor {
                    s.monitor = v;
                }
                self.send(ClientMsg::UpdateSettings(self.settings.clone()));
            }
        }
        self.render();
    }

    fn on_net(&mut self, e: NetEvent) {
        let host = &self.opts.host;
        match e {
            NetEvent::Connecting { attempt } => {
                if !self.connected {
                    self.status = format!("Connecting to {}…", host.name);
                }
                emit("connecting", json!({ "host": host.name, "address": host.address, "attempt": attempt }));
            }
            NetEvent::Connected { host_name, monitors } => {
                self.connected = true;
                self.status.clear();
                self.status_detail.clear();
                self.update_grab();
                emit("connected", json!({ "host": host.name, "host_name": host_name, "address": host.address, "monitors": monitors }));
            }
            NetEvent::Stream { monitor, width, height, fps } => {
                self.monitor = monitor.clone();
                emit("stream", json!({ "monitor": monitor, "width": width, "height": height, "fps": fps }));
            }
            NetEvent::Clock { rtt_us, offset_us } => {
                self.stats.rtt_us = rtt_us;
                if offset_us != i64::MIN {
                    self.stats.offset_us = Some(offset_us);
                }
            }
            NetEvent::FramesDropped(n) => {
                self.stats.dropped += n;
                self.stats.dropped_total += n as u64;
            }
            NetEvent::Notice(text) => {
                emit("notice", json!({ "text": text }));
                self.notice = Some((text, Instant::now()));
            }
            NetEvent::Disconnected { reason } => {
                self.connected = false;
                self.update_grab();
                self.status = "Disconnected".into();
                self.status_detail = reason.clone();
                // The host releases every held key when a client goes away.
                emit("disconnected", json!({ "reason": reason, "released_keys": true }));
            }
            NetEvent::Reconnecting { retry_in, attempt, reason } => {
                self.connected = false;
                self.status = format!("Reconnecting in {:.1}s…", retry_in.as_secs_f32());
                self.status_detail = reason.clone();
                emit("reconnecting", json!({ "retry_in_ms": retry_in.as_millis() as u64, "attempt": attempt, "reason": reason }));
            }
            NetEvent::Fatal(msg) => {
                self.connected = false;
                self.fatal = true;
                self.update_grab();
                self.status = "Cannot connect".into();
                self.status_detail = msg.clone();
                tracing::error!("{msg}");
                emit("error", json!({ "message": msg, "fatal": true }));
            }
            NetEvent::Frame { .. } => {}
        }
    }

    fn show_frame(&mut self, d: Decoded) {
        let (w, h) = (d.pic.width(), d.pic.height());
        unsafe {
            if self.texture.is_null() || self.tex != (w, h) {
                if !self.texture.is_null() {
                    SDL_DestroyTexture(self.texture);
                }
                self.texture = SDL_CreateTexture(self.renderer, SDL_PIXELFORMAT_IYUV, SDL_TEXTUREACCESS_STREAMING, w as i32, h as i32);
                if self.texture.is_null() {
                    tracing::error!("creating texture: {}", sdl_err());
                    return;
                }
                SDL_SetTextureScaleMode(self.texture, SDL_SCALEMODE_LINEAR);
                self.tex = (w, h);
            }
            let f = &d.pic.frame;
            SDL_UpdateYUVTexture(
                self.texture,
                ptr::null(),
                f.data(0).as_ptr(),
                f.stride(0) as i32,
                f.data(1).as_ptr(),
                f.stride(1) as i32,
                f.data(2).as_ptr(),
                f.stride(2) as i32,
            );
        }
        self.render();
        let now = rl_common::clock::now_us();
        let s = &mut self.stats;
        s.frames += 1;
        s.decode_us += d.decode_us;
        s.host_us += d.meta.host_us as u64;
        if let Some(off) = s.offset_us {
            let capture_local = d.meta.capture_us as i64 - off;
            let lat = now as i64 - capture_local;
            if (0..5_000_000).contains(&lat) {
                s.latency_us += lat as u64;
                s.latency_n += 1;
            }
        }
    }

    fn report_stats(&mut self) {
        let dt = self.stats_at.elapsed().as_secs_f64();
        self.stats_at = Instant::now();
        let s = &mut self.stats;
        s.bytes = self.shared.bytes.swap(0, Ordering::Relaxed);
        let n = s.frames.max(1) as f64;
        s.r_fps = s.frames as f64 / dt;
        s.r_kbps = s.bytes as f64 * 8.0 / 1000.0 / dt;
        s.r_decode_ms = s.decode_us as f64 / n / 1000.0;
        s.r_host_ms = s.host_us as f64 / n / 1000.0;
        s.r_latency_ms = (s.latency_n > 0).then(|| s.latency_us as f64 / s.latency_n as f64 / 1000.0);
        s.r_dropped = s.dropped;
        if self.connected {
            emit(
                "stats",
                json!({
                    "rtt_ms": s.rtt_us as f64 / 1000.0,
                    "latency_ms": s.r_latency_ms,
                    "fps": (s.r_fps * 10.0).round() / 10.0,
                    "kbps": s.r_kbps.round(),
                    "dropped": s.r_dropped,
                    "dropped_total": s.dropped_total,
                    "decode_ms": (s.r_decode_ms * 100.0).round() / 100.0,
                    "host_ms": (s.r_host_ms * 100.0).round() / 100.0,
                    "skipped": self.shared.skipped.swap(0, Ordering::Relaxed),
                    "width": self.tex.0,
                    "height": self.tex.1,
                    "monitor": self.monitor,
                }),
            );
        }
        s.frames = 0;
        s.bytes = 0;
        s.dropped = 0;
        s.decode_us = 0;
        s.host_us = 0;
        s.latency_us = 0;
        s.latency_n = 0;
    }

    fn text(&self, x: f32, y: f32, s: &str) {
        let c = CString::new(s.replace('\0', "")).unwrap_or_default();
        unsafe { SDL_RenderDebugText(self.renderer, x, y, c.as_ptr()) };
    }

    fn panel(&self, x: f32, y: f32, lines: &[String]) {
        let w = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f32 * 8.0 + 12.0;
        let h = lines.len() as f32 * 11.0 + 8.0;
        unsafe {
            SDL_SetRenderDrawColor(self.renderer, 0, 0, 0, 170);
            SDL_RenderFillRect(self.renderer, &SDL_FRect { x, y, w, h });
            SDL_SetRenderDrawColor(self.renderer, 235, 235, 235, 255);
        }
        for (i, l) in lines.iter().enumerate() {
            self.text(x + 6.0, y + 5.0 + i as f32 * 11.0, l);
        }
    }

    fn render(&mut self) {
        let r = self.renderer;
        unsafe {
            SDL_SetRenderDrawColor(r, 0, 0, 0, 255);
            SDL_SetRenderLogicalPresentation(r, 0, 0, SDL_LOGICAL_PRESENTATION_DISABLED);
            SDL_RenderClear(r);
            if !self.texture.is_null() {
                SDL_SetRenderLogicalPresentation(r, self.tex.0 as i32, self.tex.1 as i32, SDL_LOGICAL_PRESENTATION_LETTERBOX);
                SDL_RenderTexture(r, self.texture, ptr::null(), ptr::null());
                SDL_SetRenderLogicalPresentation(r, 0, 0, SDL_LOGICAL_PRESENTATION_DISABLED);
            }
            // Overlays in window pixels, scaled for readability.
            let (mut ow, mut oh) = (0, 0);
            SDL_GetRenderOutputSize(r, &mut ow, &mut oh);
            let scale = ((oh as f32 / 540.0).floor()).clamp(1.0, 4.0);
            SDL_SetRenderScale(r, scale, scale);
            let (vw, vh) = (ow as f32 / scale, oh as f32 / scale);

            if !self.connected || self.texture.is_null() {
                let mut lines = vec![if self.status.is_empty() { "Waiting for video…".to_string() } else { self.status.clone() }];
                if !self.status_detail.is_empty() {
                    lines.extend(wrap(&self.status_detail, ((vw - 40.0) / 8.0).max(20.0) as usize));
                }
                if self.fatal {
                    lines.push(String::new());
                    lines.push("Close this window to exit.".into());
                }
                let w = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f32 * 8.0 + 12.0;
                let h = lines.len() as f32 * 11.0 + 8.0;
                self.panel(((vw - w) / 2.0).max(0.0), ((vh - h) / 2.0).max(0.0), &lines);
            }
            if self.connected && !self.forwarding {
                let l = format!("Input released - click to resume ({})", self.opts.release_chord);
                let w = l.len() as f32 * 8.0 + 12.0;
                self.panel(((vw - w) / 2.0).max(0.0), 6.0, &[l]);
            } else if self.connected && !self.focused {
                let l = "Window not focused - input paused".to_string();
                let w = l.len() as f32 * 8.0 + 12.0;
                self.panel(((vw - w) / 2.0).max(0.0), 6.0, &[l]);
            }
            if let Some((n, _)) = &self.notice {
                let w = n.len() as f32 * 8.0 + 12.0;
                self.panel(((vw - w) / 2.0).max(0.0), vh - 30.0, &[n.clone()]);
            }
            if self.show_stats {
                let s = &self.stats;
                let lat = s.r_latency_ms.map_or("--".to_string(), |l| format!("{l:.1}"));
                let lines = vec![
                    format!("{} {}x{}  {}", self.opts.host.name, self.tex.0, self.tex.1, self.monitor),
                    format!("latency {lat} ms   rtt {:.1} ms", s.rtt_us as f64 / 1000.0),
                    format!("host {:.1} ms   decode {:.1} ms", s.r_host_ms, s.r_decode_ms),
                    format!("{:.0} fps   {:.1} Mbps   dropped {}", s.r_fps, s.r_kbps / 1000.0, s.r_dropped),
                    format!("{}  mouse {}", if self.active() { "input: on" } else { "input: off" }, if self.relative { "relative" } else { "absolute" }),
                ];
                self.panel(6.0, 6.0, &lines);
            }
            SDL_SetRenderScale(r, 1.0, 1.0);
            if !self.texture.is_null() {
                // Keep mouse-coordinate conversion in stream pixels.
                SDL_SetRenderLogicalPresentation(r, self.tex.0 as i32, self.tex.1 as i32, SDL_LOGICAL_PRESENTATION_LETTERBOX);
            }
            SDL_RenderPresent(r);
        }
    }

    fn shutdown(&mut self) {
        if self.active() {
            self.send(ClientMsg::ReleaseAll);
        }
        let _ = self.net.tx.send(Outgoing::Quit);
        self.net.quit.store(true, Ordering::Relaxed);
        // Give the Bye a moment to go out.
        std::thread::sleep(Duration::from_millis(150));
        unsafe {
            if !self.texture.is_null() {
                SDL_DestroyTexture(self.texture);
            }
            SDL_DestroyRenderer(self.renderer);
            SDL_DestroyWindow(self.window);
            SDL_Quit();
        }
        emit("closed", json!({}));
    }
}

fn wrap(s: &str, width: usize) -> Vec<String> {
    let mut out = vec![];
    let mut line = String::new();
    for word in s.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}
