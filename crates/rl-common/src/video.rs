//! Splitting encoded frames into datagrams and reassembling them.
//!
//! Header (22 bytes, little endian):
//! `tag u8 | flags u8 | frame_id u32 | idx u16 | count u16 | capture_us u64 | host_us u32`
//! where `capture_us` is the host clock at capture and `host_us` is the time the
//! host spent between capture and sending (capture + encode).

use crate::proto::DG_VIDEO;

pub const HEADER_LEN: usize = 22;
pub const FLAG_KEYFRAME: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMeta {
    pub frame_id: u32,
    pub keyframe: bool,
    pub capture_us: u64,
    pub host_us: u32,
}

/// Split `data` into datagrams no larger than `max_dgram`.
pub fn fragment(meta: FrameMeta, data: &[u8], max_dgram: usize) -> Vec<Vec<u8>> {
    let chunk = max_dgram.saturating_sub(HEADER_LEN).max(256);
    let count = data.len().div_ceil(chunk).max(1);
    assert!(count <= u16::MAX as usize, "frame too large");
    let mut out = Vec::with_capacity(count);
    for idx in 0..count {
        let part = &data[(idx * chunk).min(data.len())..((idx + 1) * chunk).min(data.len())];
        let mut d = Vec::with_capacity(HEADER_LEN + part.len());
        d.push(DG_VIDEO);
        d.push(if meta.keyframe { FLAG_KEYFRAME } else { 0 });
        d.extend_from_slice(&meta.frame_id.to_le_bytes());
        d.extend_from_slice(&(idx as u16).to_le_bytes());
        d.extend_from_slice(&(count as u16).to_le_bytes());
        d.extend_from_slice(&meta.capture_us.to_le_bytes());
        d.extend_from_slice(&meta.host_us.to_le_bytes());
        d.extend_from_slice(part);
        out.push(d);
    }
    out
}

struct Partial {
    meta: FrameMeta,
    count: u16,
    received: u16,
    parts: Vec<Option<Vec<u8>>>,
}

/// Outcome of feeding a datagram to the reassembler.
#[derive(Debug, Default)]
pub struct Pushed {
    /// A frame completed.
    pub frame: Option<(FrameMeta, Vec<u8>)>,
    /// Number of older, incomplete frames that were abandoned.
    pub dropped: u32,
}

/// Reassembles frames. Frames complete in order; when a newer frame completes,
/// any older partial frames are dropped (no jitter buffer — latest wins).
#[derive(Default)]
pub struct Reassembler {
    partials: Vec<Partial>,
    last_completed: Option<u32>,
}

fn newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, d: &[u8]) -> Pushed {
        let mut res = Pushed::default();
        if d.len() < HEADER_LEN || d[0] != DG_VIDEO {
            return res;
        }
        let meta = FrameMeta {
            frame_id: u32::from_le_bytes(d[2..6].try_into().unwrap()),
            keyframe: d[1] & FLAG_KEYFRAME != 0,
            capture_us: u64::from_le_bytes(d[10..18].try_into().unwrap()),
            host_us: u32::from_le_bytes(d[18..22].try_into().unwrap()),
        };
        let idx = u16::from_le_bytes(d[6..8].try_into().unwrap());
        let count = u16::from_le_bytes(d[8..10].try_into().unwrap());
        if count == 0 || idx >= count {
            return res;
        }
        if let Some(last) = self.last_completed {
            if !newer(meta.frame_id, last) {
                return res; // stale or duplicate
            }
        }
        let pos = match self.partials.iter().position(|p| p.meta.frame_id == meta.frame_id) {
            Some(p) => p,
            None => {
                // Bound memory: keep only a handful of in-flight frames.
                if self.partials.len() >= 8 {
                    self.partials.remove(0);
                    res.dropped += 1;
                }
                self.partials.push(Partial { meta, count, received: 0, parts: vec![None; count as usize] });
                self.partials.len() - 1
            }
        };
        let p = &mut self.partials[pos];
        if p.count != count {
            return res;
        }
        let slot = &mut p.parts[idx as usize];
        if slot.is_none() {
            *slot = Some(d[HEADER_LEN..].to_vec());
            p.received += 1;
        }
        if p.received == p.count {
            let p = self.partials.remove(pos);
            let id = p.meta.frame_id;
            let before = self.partials.len();
            self.partials.retain(|o| newer(o.meta.frame_id, id));
            res.dropped += (before - self.partials.len()) as u32;
            self.last_completed = Some(id);
            let mut data = Vec::with_capacity(p.parts.iter().map(|x| x.as_ref().map_or(0, |v| v.len())).sum());
            for part in p.parts.into_iter().flatten() {
                data.extend_from_slice(&part);
            }
            res.frame = Some((p.meta, data));
        }
        res
    }

    /// Whether an incomplete frame is waiting for fragments.
    pub fn pending(&self) -> bool {
        !self.partials.is_empty()
    }

    /// Abandon incomplete frames (they are presumed lost).
    pub fn drop_pending(&mut self) {
        self.partials.clear();
    }

    /// Forget all state (e.g. after reconnect).
    pub fn reset(&mut self) {
        self.partials.clear();
        self.last_completed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: u32) -> FrameMeta {
        FrameMeta { frame_id: id, keyframe: id == 0, capture_us: 1000 + id as u64, host_us: 5 }
    }

    #[test]
    fn roundtrip_in_order() {
        let data: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let frags = fragment(meta(0), &data, 1200);
        assert!(frags.iter().all(|f| f.len() <= 1200));
        let mut r = Reassembler::new();
        let mut out = None;
        for f in &frags {
            if let Some(fr) = r.push(f).frame {
                out = Some(fr);
            }
        }
        let (m, d) = out.unwrap();
        assert_eq!(m, meta(0));
        assert_eq!(d, data);
    }

    #[test]
    fn reorder_and_duplicates() {
        let data = vec![7u8; 5000];
        let mut frags = fragment(meta(3), &data, 1000);
        frags.reverse();
        let dup = frags[1].clone();
        frags.insert(2, dup);
        let mut r = Reassembler::new();
        let done: Vec<_> = frags.iter().filter_map(|f| r.push(f).frame).collect();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].1, data);
    }

    #[test]
    fn loss_drops_older_frame() {
        let a = fragment(meta(1), &[1u8; 3000], 1000);
        let b = fragment(meta(2), &[2u8; 3000], 1000);
        let mut r = Reassembler::new();
        r.push(&a[0]); // frame 1 never completes
        let mut dropped = 0;
        let mut got = vec![];
        for f in &b {
            let p = r.push(f);
            dropped += p.dropped;
            got.extend(p.frame);
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.frame_id, 2);
        assert_eq!(dropped, 1);
        // Late fragments of frame 1 are now ignored.
        for f in &a[1..] {
            assert!(r.push(f).frame.is_none());
        }
    }

    #[test]
    fn wraparound() {
        let mut r = Reassembler::new();
        for id in [u32::MAX - 1, u32::MAX, 0, 1] {
            let fr = fragment(meta(id), &[0u8; 10], 1200);
            assert!(r.push(&fr[0]).frame.is_some(), "frame {id}");
        }
    }

    #[test]
    fn garbage_ignored() {
        let mut r = Reassembler::new();
        assert!(r.push(&[0u8; 5]).frame.is_none());
        let mut bad = fragment(meta(1), &[0u8; 10], 1200).remove(0);
        bad[8] = 0; // count = 0
        bad[9] = 0;
        assert!(r.push(&bad).frame.is_none());
    }
}
