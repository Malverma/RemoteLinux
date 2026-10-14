//! Desktop audio: record the default output's monitor with `parec` and encode
//! it as Opus, one 10 ms packet at a time.

use std::io::Read;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};
use rl_common::proto::{AUDIO_CHANNELS, AUDIO_FRAME, AUDIO_RATE};
use tokio::sync::mpsc;

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start capturing on a thread. Packets go to `tx`; the thread exits when `tx`
/// is closed. A failure is sent once on `err` (audio is optional, so it never
/// ends the session).
pub fn spawn(tx: mpsc::UnboundedSender<Vec<u8>>, err: mpsc::UnboundedSender<String>) {
    std::thread::Builder::new()
        .name("rl-audio".into())
        .spawn(move || {
            if let Err(e) = run(&tx) {
                if !tx.is_closed() {
                    tracing::warn!("audio: {e:#}");
                    let _ = err.send(format!("{e:#}"));
                }
            }
        })
        .expect("spawning audio thread");
}

fn run(tx: &mpsc::UnboundedSender<Vec<u8>>) -> Result<()> {
    let mut child = KillOnDrop(
        Command::new("parec")
            .args([
                "--device=@DEFAULT_MONITOR@",
                "--raw",
                "--format=s16le",
                &format!("--rate={AUDIO_RATE}"),
                &format!("--channels={AUDIO_CHANNELS}"),
                "--latency-msec=10",
                "--client-name=RemoteLinux",
                "--stream-name=Remote desktop audio",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting parec (install libpulse / pipewire-pulse for audio)")?,
    );
    let mut out = child.0.stdout.take().context("parec stdout")?;
    let mut enc = opus::Encoder::new(AUDIO_RATE, opus::Channels::Stereo, opus::Application::LowDelay)?;
    enc.set_bitrate(opus::Bitrate::Bits(128_000))?;

    let mut raw = vec![0u8; AUDIO_FRAME * AUDIO_CHANNELS * 2];
    let mut pcm = vec![0i16; AUDIO_FRAME * AUDIO_CHANNELS];
    let mut packet = [0u8; 1500];
    tracing::info!("audio capture started");
    while !tx.is_closed() {
        if out.read_exact(&mut raw).is_err() {
            let _ = child.0.kill();
            let mut msg = String::new();
            if let Some(mut e) = child.0.stderr.take() {
                let _ = e.read_to_string(&mut msg);
            }
            anyhow::bail!("parec stopped: {}", msg.lines().last().unwrap_or("no output"));
        }
        for (s, b) in pcm.iter_mut().zip(raw.chunks_exact(2)) {
            *s = i16::from_le_bytes([b[0], b[1]]);
        }
        let n = enc.encode(&pcm, &mut packet)?;
        if tx.send(packet[..n].to_vec()).is_err() {
            break;
        }
    }
    Ok(())
}
