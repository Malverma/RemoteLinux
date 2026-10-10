//! Screen capture with `wlr-screencopy-unstable-v1` into shared-memory buffers.
//!
//! Two buffer slots are used so the compositor can fill one while the encoder
//! reads the other. `copy_with_damage` makes the compositor deliver a frame only
//! once something on screen changed, so a static screen costs nothing.

use std::fs::File;
use std::os::fd::AsFd;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use memmap2::MmapMut;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::{zwlr_screencopy_frame_v1 as frame_v1, zwlr_screencopy_manager_v1 as manager_v1};

use ffmpeg_next::format::Pixel;

pub const SLOTS: usize = 2;

/// Pixel layout of a captured buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferInfo {
    pub format: wl_shm::Format,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl BufferInfo {
    pub fn pixel(&self) -> Pixel {
        shm_to_pixel(self.format).expect("unsupported format accepted")
    }
}

fn shm_to_pixel(f: wl_shm::Format) -> Option<Pixel> {
    // wl_shm formats are little-endian packed: XRGB8888 is B,G,R,X in memory.
    Some(match f {
        wl_shm::Format::Xrgb8888 => Pixel::BGRZ,
        wl_shm::Format::Argb8888 => Pixel::BGRA,
        wl_shm::Format::Xbgr8888 => Pixel::RGBZ,
        wl_shm::Format::Abgr8888 => Pixel::RGBA,
        _ => return None,
    })
}

struct ShmBuffer {
    info: BufferInfo,
    _file: File,
    map: MmapMut,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Idle,
    /// Waiting for the compositor to describe buffer requirements.
    Negotiating,
    /// Buffer format known; copy not yet issued.
    NeedCopy,
    /// Copy requested; waiting for damage + ready.
    Copying,
    Ready,
    Failed,
}

struct Slot {
    state: SlotState,
    frame: Option<frame_v1::ZwlrScreencopyFrameV1>,
    offered: Vec<BufferInfo>,
    buf: Option<ShmBuffer>,
    y_invert: bool,
    ready_us: u64,
}

impl Default for Slot {
    fn default() -> Self {
        Self { state: SlotState::Idle, frame: None, offered: vec![], buf: None, y_invert: false, ready_us: 0 }
    }
}

struct Output {
    output: wl_output::WlOutput,
    name: Option<String>,
}

struct State {
    shm: wl_shm::WlShm,
    manager: manager_v1::ZwlrScreencopyManagerV1,
    outputs: Vec<Output>,
    slots: [Slot; SLOTS],
    error: Option<String>,
}

pub struct Capturer {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    /// Copy every frame instead of waiting for damage (benchmarking).
    pub continuous: bool,
}

/// A captured frame, borrowed from its slot.
pub struct Captured<'a> {
    pub info: BufferInfo,
    pub data: &'a [u8],
    pub y_invert: bool,
    pub ready_us: u64,
}

impl Capturer {
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland compositor (WAYLAND_DISPLAY)")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).context("reading Wayland globals")?;
        let qh = queue.handle();
        let shm = globals.bind::<wl_shm::WlShm, _, _>(&qh, 1..=1, ()).context("compositor lacks wl_shm")?;
        let manager = globals
            .bind::<manager_v1::ZwlrScreencopyManagerV1, _, _>(&qh, 1..=3, ())
            .context("compositor lacks wlr-screencopy (Hyprland, sway and other wlroots compositors support it)")?;
        let outputs = globals.contents().with_list(|list| {
            list.iter()
                .filter(|g| g.interface == "wl_output")
                .enumerate()
                .map(|(i, g)| Output {
                    output: globals.registry().bind::<wl_output::WlOutput, _, _>(g.name, g.version.min(4), &qh, i),
                    name: None,
                })
                .collect::<Vec<_>>()
        });
        let mut state = State { shm, manager, outputs, slots: Default::default(), error: None };
        queue.roundtrip(&mut state).context("Wayland roundtrip")?;
        Ok(Self { conn, queue, qh, state, continuous: false })
    }

    pub fn output_names(&self) -> Vec<String> {
        self.state.outputs.iter().filter_map(|o| o.name.clone()).collect()
    }

    pub fn slot_state(&self, slot: usize) -> SlotState {
        self.state.slots[slot].state
    }

    /// Start capturing the next (damaged) frame of `output` into `slot`.
    pub fn request(&mut self, slot: usize, output: &str, overlay_cursor: bool) -> Result<()> {
        let out = self
            .state
            .outputs
            .iter()
            .find(|o| o.name.as_deref() == Some(output))
            .ok_or_else(|| anyhow!("no Wayland output named {output}"))?;
        let s = &mut self.state.slots[slot];
        if let Some(f) = s.frame.take() {
            f.destroy();
        }
        s.offered.clear();
        s.state = SlotState::Negotiating;
        s.frame = Some(self.state.manager.capture_output(overlay_cursor as i32, &out.output, &self.qh, slot));
        self.conn.flush().context("Wayland flush")?;
        Ok(())
    }

    /// Process compositor events, waiting up to `timeout` for some to arrive.
    pub fn poll(&mut self, timeout: Duration) -> Result<()> {
        self.queue.dispatch_pending(&mut self.state)?;
        self.queue.flush()?;
        if let Some(guard) = self.queue.prepare_read() {
            let fd = guard.connection_fd();
            let mut fds = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
            let ts = rustix::event::Timespec { tv_sec: timeout.as_secs() as _, tv_nsec: timeout.subsec_nanos() as _ };
            let n = match rustix::event::poll(&mut fds, Some(&ts)) {
                Ok(n) => n,
                Err(rustix::io::Errno::INTR) => 0,
                Err(e) => return Err(e.into()),
            };
            if n > 0 {
                guard.read().context("reading Wayland events")?;
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;
        // Negotiation may have completed; allocate buffers and start copies.
        self.start_copies()?;
        if let Some(e) = self.state.error.take() {
            bail!(e);
        }
        Ok(())
    }

    fn start_copies(&mut self) -> Result<()> {
        let version = self.state.manager.version();
        for s in self.state.slots.iter_mut() {
            // Before v3 there is no buffer_done: the first offer is all we get.
            if s.state == SlotState::Negotiating && version < 3 && !s.offered.is_empty() {
                s.state = SlotState::NeedCopy;
            }
            if s.state != SlotState::NeedCopy {
                continue;
            }
            let info = s.offered[0];
            if s.buf.as_ref().map(|b| b.info) != Some(info) {
                s.buf = Some(alloc(&self.state.shm, &self.qh, info)?);
            }
            let (Some(frame), Some(buf)) = (s.frame.as_ref(), s.buf.as_ref()) else { continue };
            if version >= 2 && !self.continuous {
                frame.copy_with_damage(&buf.buffer);
            } else {
                frame.copy(&buf.buffer);
            }
            s.state = SlotState::Copying;
        }
        self.conn.flush()?;
        Ok(())
    }

    /// The captured frame in `slot`, if it is ready.
    pub fn ready(&self, slot: usize) -> Option<Captured<'_>> {
        let s = &self.state.slots[slot];
        if s.state != SlotState::Ready {
            return None;
        }
        let b = s.buf.as_ref()?;
        let len = (b.info.stride * b.info.height) as usize;
        Some(Captured { info: b.info, data: &b.map[..len], y_invert: s.y_invert, ready_us: s.ready_us })
    }
}

fn alloc(shm: &wl_shm::WlShm, qh: &QueueHandle<State>, info: BufferInfo) -> Result<ShmBuffer> {
    let size = (info.stride * info.height) as usize;
    let fd = rustix::fs::memfd_create("remotelinux-capture", rustix::fs::MemfdFlags::CLOEXEC)?;
    let file = File::from(fd);
    file.set_len(size as u64)?;
    let map = unsafe { MmapMut::map_mut(&file)? };
    let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(0, info.width as i32, info.height as i32, info.stride as i32, info.format, qh, ());
    Ok(ShmBuffer { info, _file: file, map, pool, buffer })
}

impl Dispatch<frame_v1::ZwlrScreencopyFrameV1, usize> for State {
    fn event(state: &mut Self, frame: &frame_v1::ZwlrScreencopyFrameV1, event: frame_v1::Event, slot: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let s = &mut state.slots[*slot];
        if s.frame.as_ref() != Some(frame) {
            return; // stale frame object
        }
        match event {
            frame_v1::Event::Buffer { format, width, height, stride } => {
                if s.state != SlotState::Negotiating {
                    return;
                }
                if let WEnum::Value(f) = format {
                    if shm_to_pixel(f).is_some() {
                        let info = BufferInfo { format: f, width, height, stride };
                        // Prefer opaque XRGB; otherwise keep compositor order.
                        if f == wl_shm::Format::Xrgb8888 {
                            s.offered.insert(0, info);
                        } else {
                            s.offered.push(info);
                        }
                    }
                }
            }
            frame_v1::Event::BufferDone => {
                if s.state == SlotState::Negotiating {
                    if s.offered.is_empty() {
                        state.error = Some("compositor offered no supported shm buffer format".into());
                        s.state = SlotState::Failed;
                    } else {
                        s.state = SlotState::NeedCopy;
                    }
                }
            }
            frame_v1::Event::Flags { flags } => {
                s.y_invert = matches!(flags, WEnum::Value(f) if f.contains(frame_v1::Flags::YInvert));
            }
            frame_v1::Event::Ready { .. } => {
                s.state = SlotState::Ready;
                s.ready_us = rl_common::clock::now_us();
                if let Some(f) = s.frame.take() {
                    f.destroy();
                }
            }
            frame_v1::Event::Failed => {
                s.state = SlotState::Failed;
                if let Some(f) = s.frame.take() {
                    f.destroy();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event, idx: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = event {
            if let Some(o) = state.outputs.get_mut(*idx) {
                o.name = Some(name);
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {
        // Hotplugged outputs are handled by reconnecting (the stream restarts
        // when the monitor layout changes).
    }
}

macro_rules! ignore_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, manager_v1::ZwlrScreencopyManagerV1);
