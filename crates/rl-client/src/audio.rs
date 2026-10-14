//! Desktop audio playback: Opus packets → SDL audio stream.
//!
//! No jitter buffer beyond a small cushion: the queue is kept short so audio
//! stays in step with the video, and a lost packet is concealed by Opus.

use std::ptr;
use std::sync::mpsc;

use anyhow::{bail, Result};
use rl_common::proto::{AUDIO_CHANNELS, AUDIO_FRAME, AUDIO_RATE};
use sdl3_sys::audio::*;

const BYTES_PER_MS: usize = AUDIO_RATE as usize / 1000 * AUDIO_CHANNELS * 2;
/// Silence queued after an underrun, to absorb network jitter.
const CUSHION_MS: usize = 30;
/// Drop queued audio past this, so latency can't creep up.
const MAX_QUEUED_MS: usize = 120;
/// Conceal at most this many lost packets in a row.
const MAX_CONCEAL: u32 = 5;

struct Stream(*mut SDL_AudioStream);
// SDL audio streams are thread-safe.
unsafe impl Send for Stream {}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe { SDL_DestroyAudioStream(self.0) };
    }
}

/// Open the default playback device. SDL's audio subsystem must be initialised.
pub fn spawn() -> Result<mpsc::Sender<(u32, Vec<u8>)>> {
    let spec = SDL_AudioSpec { format: SDL_AUDIO_S16, channels: AUDIO_CHANNELS as i32, freq: AUDIO_RATE as i32 };
    let stream = unsafe { SDL_OpenAudioDeviceStream(SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK, &spec, None, ptr::null_mut()) };
    if stream.is_null() {
        bail!("opening audio output: {}", crate::viewer::sdl_err());
    }
    let stream = Stream(stream);
    unsafe { SDL_ResumeAudioStreamDevice(stream.0) };
    let mut dec = opus::Decoder::new(AUDIO_RATE, opus::Channels::Stereo)?;
    let (tx, rx) = mpsc::channel::<(u32, Vec<u8>)>();
    std::thread::Builder::new().name("rl-audio".into()).spawn(move || {
        let stream = stream;
        let mut pcm = vec![0i16; AUDIO_FRAME * AUDIO_CHANNELS * 6];
        let mut next: Option<u32> = None;
        for (seq, packet) in rx {
            match next.map(|n| seq.wrapping_sub(n) as i32) {
                Some(g) if (-50..0).contains(&g) => continue, // late or duplicate
                Some(g) if (1..50).contains(&g) => {
                    for _ in 0..(g as u32).min(MAX_CONCEAL) {
                        // Loss concealment: the buffer length sets the frame size.
                        if let Ok(k) = dec.decode(&[], &mut pcm[..AUDIO_FRAME * AUDIO_CHANNELS], false) {
                            put(&stream, &pcm[..k * AUDIO_CHANNELS]);
                        }
                    }
                }
                _ => {} // first packet, or the host restarted the stream
            }
            next = Some(seq.wrapping_add(1));
            match dec.decode(&packet, &mut pcm, false) {
                Ok(k) => put(&stream, &pcm[..k * AUDIO_CHANNELS]),
                Err(e) => tracing::debug!("audio decode: {e}"),
            }
        }
    })?;
    Ok(tx)
}

fn put(stream: &Stream, pcm: &[i16]) {
    unsafe {
        let queued = SDL_GetAudioStreamQueued(stream.0).max(0) as usize;
        if queued > MAX_QUEUED_MS * BYTES_PER_MS {
            SDL_ClearAudioStream(stream.0);
        }
        if queued == 0 || queued > MAX_QUEUED_MS * BYTES_PER_MS {
            let silence = vec![0u8; CUSHION_MS * BYTES_PER_MS];
            SDL_PutAudioStreamData(stream.0, silence.as_ptr().cast(), silence.len() as i32);
        }
        SDL_PutAudioStreamData(stream.0, pcm.as_ptr().cast(), (pcm.len() * 2) as i32);
    }
}
