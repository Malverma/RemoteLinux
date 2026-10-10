//! H.264 encoding with FFmpeg: NVENC when available, x264 as a fallback.
//! Every setting favours latency over quality.

use anyhow::{anyhow, bail, Context, Result};
use ffmpeg_next as ff;
use ff::format::Pixel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Nvenc,
    X264,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    pub src_w: u32,
    pub src_h: u32,
    pub src_fmt: Pixel,
    pub out_w: u32,
    pub out_h: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

pub struct Encoded {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

pub struct Encoder {
    pub params: Params,
    pub kind: Kind,
    enc: ff::encoder::Video,
    frame: ff::frame::Video,
    /// Present when converting/scaling on the CPU.
    scaler: Option<ff::software::scaling::Context>,
    packet: ff::Packet,
    next_pts: i64,
}

/// Output size for a given source and max height, keeping aspect and even dims.
pub fn output_size(src_w: u32, src_h: u32, max_h: Option<u32>) -> (u32, u32) {
    let h = max_h.map_or(src_h, |m| m.min(src_h));
    let w = ((src_w as u64 * h as u64) / src_h.max(1) as u64) as u32;
    (w & !1, h & !1)
}

pub fn init() {
    let _ = ff::init();
    ff::util::log::set_level(ff::util::log::Level::Error);
}

impl Encoder {
    /// `preference`: "auto", "nvenc" or "x264".
    pub fn new(params: Params, preference: &str) -> Result<Self> {
        let order: &[Kind] = match preference {
            "nvenc" => &[Kind::Nvenc],
            "x264" => &[Kind::X264],
            _ => &[Kind::Nvenc, Kind::X264],
        };
        let mut last = None;
        for &kind in order {
            match Self::open(params.clone(), kind) {
                Ok(e) => return Ok(e),
                Err(e) => {
                    tracing::warn!("{kind:?} encoder unavailable: {e:#}");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("no encoder")))
    }

    fn open(p: Params, kind: Kind) -> Result<Self> {
        let name = match kind {
            Kind::Nvenc => "h264_nvenc",
            Kind::X264 => "libx264",
        };
        let codec = ff::encoder::find_by_name(name).ok_or_else(|| anyhow!("FFmpeg has no {name}"))?;
        let scaled = p.out_w != p.src_w || p.out_h != p.src_h;
        // NVENC converts packed RGB on the GPU, so unscaled frames skip the CPU.
        let enc_fmt = match kind {
            Kind::Nvenc if !scaled => p.src_fmt,
            Kind::Nvenc => Pixel::NV12,
            Kind::X264 => Pixel::YUV420P,
        };
        let mut enc = ff::codec::context::Context::new_with_codec(codec).encoder().video()?;
        enc.set_width(p.out_w);
        enc.set_height(p.out_h);
        enc.set_format(enc_fmt);
        enc.set_time_base((1, 1_000_000));
        enc.set_frame_rate(Some((p.fps as i32, 1)));
        enc.set_bit_rate(p.bitrate_kbps as usize * 1000);
        enc.set_max_bit_rate(p.bitrate_kbps as usize * 1000);
        enc.set_max_b_frames(0);
        // Long GOP; NVENC uses it as the intra-refresh period.
        enc.set_gop(p.fps.max(1) * 2);
        unsafe {
            let ctx = enc.as_mut_ptr();
            // VBV of ~2 frames: no frame can take much longer than a frame
            // interval to transmit.
            (*ctx).rc_buffer_size = ((p.bitrate_kbps as i64 * 1000 * 2) / p.fps.max(1) as i64) as i32;
            (*ctx).flags |= ff::ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).color_range = ff::ffi::AVColorRange::AVCOL_RANGE_MPEG;
            (*ctx).colorspace = ff::ffi::AVColorSpace::AVCOL_SPC_BT709;
            (*ctx).color_primaries = ff::ffi::AVColorPrimaries::AVCOL_PRI_BT709;
            (*ctx).color_trc = ff::ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        }
        let mut opts = ff::Dictionary::new();
        match kind {
            Kind::Nvenc => {
                opts.set("preset", "p1");
                opts.set("tune", "ull");
                opts.set("zerolatency", "1");
                opts.set("rc", "cbr");
                opts.set("delay", "0");
                opts.set("rc-lookahead", "0");
                opts.set("no-scenecut", "1");
                opts.set("intra-refresh", "1");
                opts.set("forced-idr", "1");
                opts.set("profile", "high");
                // Multiple slices let the decoder use slice threads.
                opts.set("slices", "4");
            }
            Kind::X264 => {
                opts.set("preset", "ultrafast");
                opts.set("tune", "zerolatency");
                opts.set("x264-params", "intra-refresh=1:sliced-threads=1:sync-lookahead=0:rc-lookahead=0");
            }
        }
        let enc = enc.open_with(opts).with_context(|| format!("opening {name}"))?;
        let frame = ff::frame::Video::new(enc_fmt, p.out_w, p.out_h);
        let scaler = if enc_fmt != p.src_fmt || scaled {
            Some(ff::software::scaling::Context::get(
                p.src_fmt,
                p.src_w,
                p.src_h,
                enc_fmt,
                p.out_w,
                p.out_h,
                ff::software::scaling::Flags::FAST_BILINEAR,
            )?)
        } else {
            None
        };
        tracing::info!(
            "encoder {name}: {}x{} -> {}x{} {:?}->{:?} @{}fps {}kbps",
            p.src_w, p.src_h, p.out_w, p.out_h, p.src_fmt, enc_fmt, p.fps, p.bitrate_kbps
        );
        Ok(Self { params: p, kind, enc, frame, scaler, packet: ff::Packet::empty(), next_pts: 0 })
    }

    /// Encode one captured frame. `data` is the source image with row stride
    /// `stride`; `y_invert` flips it vertically.
    pub fn encode(&mut self, data: &[u8], stride: usize, y_invert: bool, pts_us: i64, force_idr: bool) -> Result<Vec<Encoded>> {
        let p = &self.params;
        let row = p.src_w as usize * 4;
        if stride < row || data.len() < stride * p.src_h as usize {
            bail!("capture buffer smaller than expected");
        }
        match &mut self.scaler {
            None => {
                let dst_stride = self.frame.stride(0);
                let dst = self.frame.data_mut(0);
                for y in 0..p.src_h as usize {
                    let sy = if y_invert { p.src_h as usize - 1 - y } else { y };
                    dst[y * dst_stride..y * dst_stride + row].copy_from_slice(&data[sy * stride..sy * stride + row]);
                }
            }
            Some(sws) => unsafe {
                let (src_ptr, src_stride) = if y_invert {
                    (data.as_ptr().add(stride * (p.src_h as usize - 1)), -(stride as i32))
                } else {
                    (data.as_ptr(), stride as i32)
                };
                let srcs = [src_ptr, std::ptr::null(), std::ptr::null(), std::ptr::null()];
                let src_strides = [src_stride, 0, 0, 0];
                let f = self.frame.as_mut_ptr();
                let r = ff::ffi::sws_scale(
                    sws.as_mut_ptr(),
                    srcs.as_ptr(),
                    src_strides.as_ptr(),
                    0,
                    p.src_h as i32,
                    (*f).data.as_ptr(),
                    (*f).linesize.as_ptr(),
                );
                if r <= 0 {
                    bail!("sws_scale failed ({r})");
                }
            },
        }
        // Timestamps must strictly increase even if the clock is coarse.
        let pts = pts_us.max(self.next_pts);
        self.next_pts = pts + 1;
        self.frame.set_pts(Some(pts));
        self.frame.set_kind(if force_idr { ff::picture::Type::I } else { ff::picture::Type::None });
        unsafe {
            let f = self.frame.as_mut_ptr();
            if force_idr {
                (*f).flags |= ff::ffi::AV_FRAME_FLAG_KEY as i32;
            } else {
                (*f).flags &= !(ff::ffi::AV_FRAME_FLAG_KEY as i32);
            }
        }
        self.enc.send_frame(&self.frame).context("sending frame to encoder")?;
        let mut out = Vec::new();
        loop {
            match self.enc.receive_packet(&mut self.packet) {
                Ok(()) => {
                    if let Some(d) = self.packet.data() {
                        out.push(Encoded { data: d.to_vec(), keyframe: self.packet.is_key() });
                    }
                }
                Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => break,
                Err(ff::Error::Eof) => break,
                Err(e) => return Err(e).context("receiving encoded packet"),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(output_size(1920, 1080, None), (1920, 1080));
        assert_eq!(output_size(1920, 1080, Some(720)), (1280, 720));
        assert_eq!(output_size(1920, 1080, Some(2160)), (1920, 1080));
        assert_eq!(output_size(2560, 1080, Some(901)), (2134, 900));
    }

    /// Encodes a synthetic gradient with whichever encoder is available.
    #[test]
    fn encodes_frames() {
        init();
        let (w, h) = (640u32, 360u32);
        let p = Params { src_w: w, src_h: h, src_fmt: Pixel::BGRZ, out_w: w, out_h: h, fps: 60, bitrate_kbps: 4000 };
        let mut enc = Encoder::new(p, "auto").expect("encoder");
        let stride = w as usize * 4;
        let mut img = vec![0u8; stride * h as usize];
        let mut total = 0;
        let mut first_key = None;
        for i in 0..10 {
            for (j, px) in img.chunks_mut(4).enumerate() {
                px[0] = (j % w as usize) as u8;
                px[1] = (i * 20) as u8;
                px[2] = (j / w as usize) as u8;
            }
            let out = enc.encode(&img, stride, false, i * 16_666, i == 0).unwrap();
            if first_key.is_none() {
                first_key = out.first().map(|e| e.keyframe);
            }
            total += out.len();
        }
        assert_eq!(first_key, Some(true));
        assert!(total >= 9, "zero-latency encoder should emit ~1 packet per frame, got {total}");
    }
}
