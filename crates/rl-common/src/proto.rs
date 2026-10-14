//! Control-channel messages (reliable QUIC stream, postcard + u32 length
//! prefix) and small input datagrams.

use anyhow::{bail, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Requested stream parameters. `None` means "use the host's default".
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StreamSettings {
    /// Maximum encoded height (e.g. 720). `None` = native resolution.
    pub max_height: Option<u32>,
    pub fps: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    /// Monitor name (e.g. "DP-1"). `None` = host default.
    pub monitor: Option<String>,
    /// Draw the host cursor into the video (needed in relative-mouse mode).
    pub show_cursor: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MonitorInfo {
    pub name: String,
    pub description: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f32,
    pub focused: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ClientMsg {
    Hello { version: u32, client_name: String, settings: StreamSettings },
    /// Linux evdev key code (`KEY_*`).
    Key { code: u16, pressed: bool },
    /// Linux evdev button code (`BTN_LEFT` = 0x110 …).
    Button { code: u16, pressed: bool },
    /// Scroll in 1/120ths of a wheel notch (positive y = down, positive x = right).
    Scroll { v120_x: i32, v120_y: i32 },
    /// Release every pressed key/button (client lost focus or released grab).
    ReleaseAll,
    RequestKeyframe,
    UpdateSettings(StreamSettings),
    Ping { t_client_us: u64 },
    Bye,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum HostMsg {
    Welcome { version: u32, host_name: String, monitors: Vec<MonitorInfo> },
    /// Sent whenever the stream starts or its geometry/monitor changes.
    StreamInfo { monitor: String, width: u32, height: u32, fps: u32 },
    Pong { t_client_us: u64, t_host_us: u64 },
    Notice { text: String },
    Error { text: String },
}

/// Maximum control message size; anything larger is a protocol error.
const MAX_MSG: usize = 1 << 20;

pub async fn write_msg<T: Serialize>(w: &mut quinn::SendStream, msg: &T) -> Result<()> {
    let body = postcard::to_stdvec(msg)?;
    let mut buf = Vec::with_capacity(body.len() + 4);
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf).await?;
    Ok(())
}

/// Read one message; `Ok(None)` on clean end of stream.
pub async fn read_msg<T: DeserializeOwned>(r: &mut quinn::RecvStream) -> Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MSG {
        bail!("control message too large ({len} bytes)");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(postcard::from_bytes(&body)?))
}

// ---- Datagrams --------------------------------------------------------------

pub const DG_VIDEO: u8 = 0;
pub const DG_MOTION_ABS: u8 = 1;
pub const DG_MOTION_REL: u8 = 2;
pub const DG_AUDIO: u8 = 3;

/// Desktop audio: 48 kHz stereo Opus, one 10 ms frame per datagram.
pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: usize = 2;
pub const AUDIO_FRAME: usize = 480;

/// `tag u8 | seq u32 | opus packet`
pub fn encode_audio(seq: u32, packet: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + packet.len());
    v.push(DG_AUDIO);
    v.extend_from_slice(&seq.to_le_bytes());
    v.extend_from_slice(packet);
    v
}

pub fn decode_audio(d: &[u8]) -> Option<(u32, &[u8])> {
    if d.len() < 5 || d[0] != DG_AUDIO {
        return None;
    }
    Some((u32::from_le_bytes(d[1..5].try_into().ok()?), &d[5..]))
}

/// Pointer motion, sent as an unreliable datagram (latest wins).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Motion {
    /// Normalised position in the streamed monitor, 0..1.
    Abs { seq: u32, x: f32, y: f32 },
    /// Relative motion in host pixels.
    Rel { seq: u32, dx: f32, dy: f32 },
}

impl Motion {
    pub fn encode(&self) -> Vec<u8> {
        let (tag, seq, a, b) = match *self {
            Motion::Abs { seq, x, y } => (DG_MOTION_ABS, seq, x, y),
            Motion::Rel { seq, dx, dy } => (DG_MOTION_REL, seq, dx, dy),
        };
        let mut v = Vec::with_capacity(13);
        v.push(tag);
        v.extend_from_slice(&seq.to_le_bytes());
        v.extend_from_slice(&a.to_le_bytes());
        v.extend_from_slice(&b.to_le_bytes());
        v
    }

    pub fn decode(d: &[u8]) -> Option<Self> {
        if d.len() != 13 {
            return None;
        }
        let seq = u32::from_le_bytes(d[1..5].try_into().ok()?);
        let a = f32::from_le_bytes(d[5..9].try_into().ok()?);
        let b = f32::from_le_bytes(d[9..13].try_into().ok()?);
        if !a.is_finite() || !b.is_finite() {
            return None;
        }
        match d[0] {
            DG_MOTION_ABS => Some(Motion::Abs { seq, x: a, y: b }),
            DG_MOTION_REL => Some(Motion::Rel { seq, dx: a, dy: b }),
            _ => None,
        }
    }

    pub fn seq(&self) -> u32 {
        match *self {
            Motion::Abs { seq, .. } | Motion::Rel { seq, .. } => seq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_roundtrip() {
        for m in [Motion::Abs { seq: 7, x: 0.25, y: 0.75 }, Motion::Rel { seq: u32::MAX, dx: -3.5, dy: 12.0 }] {
            assert_eq!(Motion::decode(&m.encode()), Some(m));
        }
        assert_eq!(Motion::decode(&[1, 2, 3]), None);
    }

    #[test]
    fn audio_roundtrip() {
        let d = encode_audio(42, &[9, 8, 7]);
        assert_eq!(decode_audio(&d), Some((42, &[9u8, 8, 7][..])));
        assert_eq!(Motion::decode(&d), None);
    }

    #[test]
    fn msg_roundtrip() {
        let m = ClientMsg::Key { code: 30, pressed: true };
        let b = postcard::to_stdvec(&m).unwrap();
        match postcard::from_bytes::<ClientMsg>(&b).unwrap() {
            ClientMsg::Key { code: 30, pressed: true } => {}
            other => panic!("{other:?}"),
        }
    }
}
