//! Headless benchmark: receive and decode the stream, report latency.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use rl_common::proto::StreamSettings;
use rl_common::trust::KnownHost;
use serde_json::json;

use crate::decode::Decoder;
use crate::net::{self, NetEvent, Outgoing};

#[derive(Default)]
struct Acc {
    frames: u64,
    bytes: u64,
    dropped: u64,
    decode_us: Vec<u64>,
    latency_us: Vec<u64>,
    host_us: Vec<u64>,
    rtt_us: Vec<u64>,
    offset: Option<i64>,
    connected: bool,
    fatal: Option<String>,
    size: (u32, u32),
    audio_packets: u64,
}

fn pct(v: &mut [u64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p).round() as usize] as f64 / 1000.0
}

pub fn run(host: KnownHost, settings: StreamSettings, name: String, dur: Duration) -> Result<()> {
    let acc = Arc::new(Mutex::new(Acc::default()));
    let (ftx, frx) = std::sync::mpsc::channel();
    let a = acc.clone();
    let handle = net::spawn(host.clone(), settings, name, move |ev| {
        let mut a = a.lock().unwrap();
        match ev {
            NetEvent::Frame { meta, data } => {
                a.bytes += data.len() as u64;
                let _ = ftx.send((meta, data));
            }
            NetEvent::Connected { .. } => a.connected = true,
            NetEvent::Clock { rtt_us, offset_us } => {
                a.rtt_us.push(rtt_us);
                if offset_us != i64::MIN {
                    a.offset = Some(offset_us);
                }
            }
            NetEvent::FramesDropped(n) => a.dropped += n as u64,
            NetEvent::Audio { .. } => a.audio_packets += 1,
            NetEvent::Fatal(e) => a.fatal = Some(e),
            NetEvent::Reconnecting { reason, .. } => eprintln!("reconnecting: {reason}"),
            _ => {}
        }
    })?;
    let a = acc.clone();
    let tx = handle.tx.clone();
    std::thread::spawn(move || {
        let mut dec = Decoder::new().expect("decoder");
        for (meta, data) in frx {
            let t0 = rl_common::clock::now_us();
            let r = dec.decode(&data);
            let now = rl_common::clock::now_us();
            let mut a = a.lock().unwrap();
            match r {
                Ok(Some(p)) => {
                    a.frames += 1;
                    a.size = (p.width(), p.height());
                    a.decode_us.push(now - t0);
                    a.host_us.push(meta.host_us as u64);
                    if let Some(off) = a.offset {
                        let lat = now as i64 - (meta.capture_us as i64 - off);
                        if (0..5_000_000).contains(&lat) {
                            a.latency_us.push(lat as u64);
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    let _ = tx.send(Outgoing::Msg(rl_common::proto::ClientMsg::RequestKeyframe));
                }
            }
        }
    });
    let start = Instant::now();
    while start.elapsed() < dur {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(e) = acc.lock().unwrap().fatal.clone() {
            bail!(e);
        }
    }
    let _ = handle.tx.send(Outgoing::Quit);
    std::thread::sleep(Duration::from_millis(200));
    let mut a = acc.lock().unwrap();
    if !a.connected {
        bail!("never connected to {}", host.address);
    }
    let secs = dur.as_secs_f64();
    let v = json!({
        "host": host.name,
        "seconds": secs,
        "width": a.size.0, "height": a.size.1,
        "frames": a.frames,
        "fps": a.frames as f64 / secs,
        "mbps": a.bytes as f64 * 8.0 / 1e6 / secs,
        "dropped": a.dropped,
        "audio_packets": a.audio_packets,
        "rtt_ms_p50": pct(&mut a.rtt_us, 0.5),
        "host_ms_p50": pct(&mut a.host_us, 0.5),
        "decode_ms_p50": pct(&mut a.decode_us, 0.5),
        "latency_ms_p50": pct(&mut a.latency_us, 0.5),
        "latency_ms_p95": pct(&mut a.latency_us, 0.95),
    });
    if crate::events::json() {
        println!("{v}");
    } else {
        println!(
            "{} frames ({:.1} fps, {}x{}), {:.1} Mbps, {} dropped\n\
             capture->decoded latency: p50 {:.1} ms, p95 {:.1} ms (excludes display)\n\
             host capture+encode p50 {:.1} ms, decode p50 {:.1} ms, rtt p50 {:.2} ms\n\
             {} audio packets",
            v["frames"], v["fps"].as_f64().unwrap_or(0.0), a.size.0, a.size.1, v["mbps"].as_f64().unwrap_or(0.0), a.dropped,
            v["latency_ms_p50"].as_f64().unwrap_or(f64::NAN), v["latency_ms_p95"].as_f64().unwrap_or(f64::NAN),
            v["host_ms_p50"].as_f64().unwrap_or(f64::NAN), v["decode_ms_p50"].as_f64().unwrap_or(f64::NAN), v["rtt_ms_p50"].as_f64().unwrap_or(f64::NAN),
            a.audio_packets,
        );
        if a.frames == 0 {
            println!("(no frames: the desktop screen may be static — move something on it)");
        }
    }
    Ok(())
}
