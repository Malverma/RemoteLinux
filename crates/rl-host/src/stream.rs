//! The capture → encode thread for one client session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use rl_common::config::HostConfig;
use rl_common::proto::StreamSettings;

use crate::capture::{Capturer, SlotState};
use crate::encoder::{self, Encoder, Params};
use crate::input::Target;
use crate::monitors;

/// Re-send the last frame this often when the screen is static, so a lost
/// final frame can't leave the client stale.
const KEEPALIVE: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct StreamControl {
    pub stop: AtomicBool,
    pub keyframe: AtomicBool,
    pub settings: Mutex<Option<StreamSettings>>,
}

pub enum StreamEvent {
    /// Stream (re)started with this geometry.
    Info { monitor: String, width: u32, height: u32, fps: u32, target: Target },
    Frame { data: Vec<u8>, keyframe: bool, capture_us: u64, host_us: u32 },
    Error(String),
}

/// Effective settings: client request over host config.
#[derive(Debug, Clone, PartialEq)]
struct Effective {
    monitor: Option<String>,
    max_height: Option<u32>,
    fps: u32,
    bitrate_kbps: u32,
    show_cursor: bool,
}

fn effective(cfg: &HostConfig, s: &StreamSettings) -> Effective {
    Effective {
        monitor: s.monitor.clone().or_else(|| Some(cfg.monitor.clone())),
        max_height: s.max_height.or_else(|| rl_common::config::parse_resolution(&cfg.resolution).ok().flatten()),
        fps: s.fps.unwrap_or(cfg.fps).clamp(10, 240),
        bitrate_kbps: s.bitrate_kbps.unwrap_or(cfg.bitrate_kbps).clamp(500, 200_000),
        show_cursor: s.show_cursor.unwrap_or(cfg.show_cursor_in_stream),
    }
}

pub fn spawn(cfg: HostConfig, settings: StreamSettings, ctrl: Arc<StreamControl>, tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>) {
    std::thread::Builder::new()
        .name("rl-stream".into())
        .spawn(move || {
            if let Err(e) = run(&cfg, settings, &ctrl, &tx) {
                tracing::error!("stream: {e:#}");
                let _ = tx.send(StreamEvent::Error(format!("{e:#}")));
            }
        })
        .expect("spawning stream thread");
}

fn run(cfg: &HostConfig, mut settings: StreamSettings, ctrl: &StreamControl, tx: &tokio::sync::mpsc::UnboundedSender<StreamEvent>) -> Result<()> {
    encoder::init();
    let continuous = std::env::var_os("RL_CONTINUOUS_CAPTURE").is_some();
    let mut cap = Capturer::connect()?;
    cap.continuous = continuous;
    let mut failures = 0u32;
    'outer: loop {
        let eff = effective(cfg, &settings);
        let mons = monitors::list()?;
        let Some(mon) = monitors::select(&mons, eff.monitor.as_deref()) else { bail!("no monitors found") };
        if eff.monitor.as_deref().is_some_and(|w| w != "auto" && !w.is_empty() && w != mon.info.name) {
            tracing::warn!("monitor {:?} not found, streaming {}", eff.monitor, mon.info.name);
        }
        let target = monitors::target(&mons, mon);
        let name = mon.info.name.clone();
        if !cap.output_names().contains(&name) {
            cap = Capturer::connect()?; // output list changed
            cap.continuous = continuous;
        }
        let interval = Duration::from_secs_f64(1.0 / eff.fps as f64);
        let mut enc: Option<Encoder> = None;
        let mut cur = 0usize;
        let mut last_ready: Option<usize> = None;
        let mut dirty = false;
        let mut last_encode = Instant::now() - KEEPALIVE;
        cap.request(cur, &name, eff.show_cursor)?;
        tracing::info!("streaming {name} (max {:?}p, {} fps, {} kbps)", eff.max_height, eff.fps, eff.bitrate_kbps);

        loop {
            if ctrl.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if let Some(s) = ctrl.settings.lock().unwrap().take() {
                if effective(cfg, &s) != eff {
                    settings = s;
                    continue 'outer;
                }
                settings = s;
            }
            // Frame-rate cap with slack: a frame is encoded as soon as it
            // arrives unless it comes much sooner than the target interval, so
            // a 60 Hz monitor at 60 fps never waits.
            let min_gap = interval.mul_f32(0.8);
            let wait = if dirty { min_gap.saturating_sub(last_encode.elapsed()) } else { Duration::from_millis(50) };
            cap.poll(wait)?;

            match cap.slot_state(cur) {
                SlotState::Ready => {
                    failures = 0;
                    last_ready = Some(cur);
                    dirty = true;
                    cur = 1 - cur;
                    cap.request(cur, &name, eff.show_cursor)?;
                }
                SlotState::Failed => {
                    failures += 1;
                    if failures > 50 {
                        bail!("screen capture of {name} keeps failing");
                    }
                    if failures % 10 == 0 {
                        // Output may have been reconfigured; re-read layout.
                        std::thread::sleep(Duration::from_millis(100));
                        cap = Capturer::connect()?;
                        cap.continuous = continuous;
                        continue 'outer;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                    cap.request(cur, &name, eff.show_cursor)?;
                }
                _ => {}
            }

            let want_key = ctrl.keyframe.load(Ordering::Relaxed);
            let due = dirty && last_encode.elapsed() >= min_gap;
            let idle = last_encode.elapsed() >= KEEPALIVE || (want_key && last_encode.elapsed() >= Duration::from_millis(30));
            let Some(slot) = last_ready else { continue };
            if !(due || idle) {
                continue;
            }
            let Some(f) = cap.ready(slot) else { continue };
            let (ow, oh) = encoder::output_size(f.info.width, f.info.height, eff.max_height);
            let params = Params {
                src_w: f.info.width,
                src_h: f.info.height,
                src_fmt: f.info.pixel(),
                out_w: ow,
                out_h: oh,
                fps: eff.fps,
                bitrate_kbps: eff.bitrate_kbps,
            };
            let mut fresh = false;
            if enc.as_ref().map(|e| &e.params) != Some(&params) {
                enc = Some(Encoder::new(params, &cfg.encoder)?);
                fresh = true;
                let _ = tx.send(StreamEvent::Info { monitor: name.clone(), width: ow, height: oh, fps: eff.fps, target });
            }
            let force_idr = ctrl.keyframe.swap(false, Ordering::Relaxed) || fresh;
            // A keepalive re-sends an old image: stamp it now so latency stats stay honest.
            let capture_us = if due { f.ready_us } else { rl_common::clock::now_us() };
            // Measured from encode start so encode time doesn't eat into the interval.
            last_encode = Instant::now();
            let packets = enc.as_mut().unwrap().encode(f.data, f.info.stride as usize, f.y_invert, capture_us as i64, force_idr)?;
            dirty = false;
            let host_us = (rl_common::clock::now_us().saturating_sub(capture_us)) as u32;
            for p in packets {
                if tx.send(StreamEvent::Frame { data: p.data, keyframe: p.keyframe, capture_us, host_us }).is_err() {
                    return Ok(()); // session gone
                }
            }
        }
    }
}
