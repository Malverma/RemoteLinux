//! Capture + encode the focused monitor for a few seconds and print timings.
use std::time::{Duration, Instant};

use rl_host::capture::{Capturer, SlotState};
use rl_host::encoder::{self, Encoder, Params};

fn main() -> anyhow::Result<()> {
    encoder::init();
    let mons = rl_host::monitors::list()?;
    let m = rl_host::monitors::select(&mons, None).unwrap().info.name.clone();
    let mut cap = Capturer::connect()?;
    cap.continuous = std::env::var_os("CONTINUOUS").is_some();
    println!("outputs: {:?}, capturing {m}", cap.output_names());
    let mut enc: Option<Encoder> = None;
    let mut cur = 0;
    cap.request(cur, &m, false)?;
    let start = Instant::now();
    let mut n = 0;
    let mut last_req = Instant::now();
    let (mut cap_ms, mut enc_ms, mut bytes) = (0.0, 0.0, 0usize);
    while start.elapsed() < Duration::from_secs(4) {
        cap.poll(Duration::from_millis(100))?;
        match cap.slot_state(cur) {
            SlotState::Ready => {
                let done = cur;
                cap_ms += last_req.elapsed().as_secs_f64() * 1e3;
                cur = 1 - cur;
                cap.request(cur, &m, false)?;
                last_req = Instant::now();
                let f = cap.ready(done).unwrap();
                let e = enc.get_or_insert_with(|| {
                    let (w, h) = encoder::output_size(f.info.width, f.info.height, None);
                    Encoder::new(Params { src_w: f.info.width, src_h: f.info.height, src_fmt: f.info.pixel(), out_w: w, out_h: h, fps: 60, bitrate_kbps: 15000 }, "auto").unwrap()
                });
                let t = Instant::now();
                let out = e.encode(f.data, f.info.stride as usize, f.y_invert, rl_common::clock::now_us() as i64, n == 0)?;
                enc_ms += t.elapsed().as_secs_f64() * 1e3;
                bytes += out.iter().map(|o| o.data.len()).sum::<usize>();
                n += 1;
            }
            SlotState::Failed => anyhow::bail!("capture failed"),
            _ => {}
        }
    }
    println!("{n} frames in 4s; avg wait-for-frame {:.2} ms, avg encode {:.2} ms, avg {:.1} KB/frame, kind {:?}",
        cap_ms / n.max(1) as f64, enc_ms / n.max(1) as f64, bytes as f64 / n.max(1) as f64 / 1024.0, enc.map(|e| e.kind));
    Ok(())
}
