//! H.264 decoding (software, low-delay, slice-threaded).

use anyhow::{Context, Result};
use ffmpeg_next as ff;
use ff::format::Pixel;

pub struct Decoder {
    dec: ff::decoder::Video,
    frame: ff::frame::Video,
    converted: Option<(ff::software::scaling::Context, ff::frame::Video)>,
}

/// A decoded picture in planar YUV 4:2:0.
pub struct Picture {
    pub frame: ff::frame::Video,
}

impl Picture {
    pub fn width(&self) -> u32 {
        self.frame.width()
    }
    pub fn height(&self) -> u32 {
        self.frame.height()
    }
}

impl Decoder {
    pub fn new() -> Result<Self> {
        let _ = ff::init();
        ff::util::log::set_level(ff::util::log::Level::Fatal);
        let codec = ff::decoder::find(ff::codec::Id::H264).context("FFmpeg has no H.264 decoder")?;
        let mut ctx = ff::codec::context::Context::new_with_codec(codec);
        unsafe {
            let c = ctx.as_mut_ptr();
            (*c).flags |= ff::ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*c).flags2 |= ff::ffi::AV_CODEC_FLAG2_FAST as i32;
            // Slice threads add no delay; frame threads add a frame per thread.
            (*c).thread_type = ff::ffi::FF_THREAD_SLICE as i32;
            (*c).thread_count = std::thread::available_parallelism().map_or(4, |n| n.get().min(4)) as i32;
        }
        let dec = ctx.decoder().video().context("opening H.264 decoder")?;
        Ok(Self { dec, frame: ff::frame::Video::empty(), converted: None })
    }

    /// Decode one access unit. Returns the picture if one was produced.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Picture>> {
        let pkt = ff::Packet::copy(data);
        self.dec.send_packet(&pkt).context("decoding")?;
        let mut latest = None;
        while self.dec.receive_frame(&mut self.frame).is_ok() {
            latest = Some(self.take()?);
        }
        Ok(latest)
    }

    fn take(&mut self) -> Result<Picture> {
        let f = &self.frame;
        if matches!(f.format(), Pixel::YUV420P | Pixel::YUVJ420P) {
            return Ok(Picture { frame: std::mem::replace(&mut self.frame, ff::frame::Video::empty()) });
        }
        // Unusual format: convert to YUV420P.
        let (w, h) = (f.width(), f.height());
        let ok = self.converted.as_ref().is_some_and(|(s, o)| s.input().format == f.format() && o.width() == w && o.height() == h);
        if !ok {
            let sws = ff::software::scaling::Context::get(f.format(), w, h, Pixel::YUV420P, w, h, ff::software::scaling::Flags::FAST_BILINEAR)?;
            self.converted = Some((sws, ff::frame::Video::new(Pixel::YUV420P, w, h)));
        }
        let (sws, out) = self.converted.as_mut().unwrap();
        sws.run(&self.frame, out)?;
        Ok(Picture { frame: out.clone() })
    }
}
