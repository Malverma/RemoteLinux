//! Process-local monotonic microsecond clock. The two machines' clocks have
//! different bases; the client estimates the offset from Ping/Pong.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

pub fn now_us() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}
