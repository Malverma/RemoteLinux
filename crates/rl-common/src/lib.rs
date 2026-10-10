//! Shared code for RemoteLinux: protocol, transport, identity, pairing, config.

pub mod client;
pub mod clock;
pub mod config;
pub mod discovery;
pub mod hostctl;
pub mod identity;
pub mod pairing;
pub mod paths;
pub mod proto;
pub mod tls;
pub mod transport;
pub mod trust;
pub mod video;

/// Wire protocol version. Bumped on any incompatible change.
pub const PROTOCOL_VERSION: u32 = 1;
/// Default UDP port the host listens on.
pub const DEFAULT_PORT: u16 = 47800;

/// This machine's hostname, used as the default device name.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
