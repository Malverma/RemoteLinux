//! LAN discovery over mDNS (`_remotelinux._udp.local.`). The host advertises
//! its name, QUIC port, fingerprint and whether it is showing a pairing code;
//! the laptop browses for it. mDNS (224.0.0.251:5353) is allowed by ufw's
//! default rules, so discovery works without opening extra ports.
//! Discovery only finds hosts; trust still comes from PIN pairing.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::Serialize;

pub const SERVICE_TYPE: &str = "_remotelinux._udp.local.";

/// A host found on the network.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Discovered {
    pub name: String,
    /// "ip:port" to connect or pair with.
    pub address: String,
    pub fingerprint: String,
    /// Whether the host is currently showing a pairing code.
    pub pairing: bool,
}

/// Host side: keeps the mDNS advertisement up to date.
pub struct Advertiser {
    daemon: ServiceDaemon,
    name: String,
    port: u16,
    fingerprint: String,
    pairing: bool,
    fullname: Option<String>,
}

impl Advertiser {
    pub fn new(name: &str, port: u16, fingerprint: &str) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("starting mDNS")?;
        let mut a = Self { daemon, name: name.into(), port, fingerprint: fingerprint.into(), pairing: false, fullname: None };
        a.publish()?;
        Ok(a)
    }

    fn publish(&mut self) -> Result<()> {
        let instance = format!("{}-{}", self.name, &self.fingerprint[..8.min(self.fingerprint.len())]);
        let host = format!("{}.local.", crate::hostname().replace(['.', ' '], "-"));
        let pairing = if self.pairing { "1" } else { "0" };
        let props = [("name", self.name.as_str()), ("fp", self.fingerprint.as_str()), ("pairing", pairing), ("v", "1")];
        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", self.port, &props[..])?.enable_addr_auto();
        self.fullname = Some(info.get_fullname().to_string());
        self.daemon.register(info)?;
        Ok(())
    }

    /// Re-announce when the pairing state changes.
    pub fn set_pairing(&mut self, on: bool) -> Result<()> {
        if on != self.pairing {
            self.pairing = on;
            self.publish()?;
        }
        Ok(())
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        if let Some(f) = &self.fullname {
            let _ = self.daemon.unregister(f);
        }
        let _ = self.daemon.shutdown();
    }
}

/// Browse for hosts for `wait` (blocking; call from a blocking thread).
pub fn discover_blocking(wait: Duration) -> Result<Vec<Discovered>> {
    let daemon = ServiceDaemon::new().context("starting mDNS")?;
    let rx = daemon.browse(SERVICE_TYPE)?;
    let deadline = Instant::now() + wait;
    let mut found: HashMap<String, Discovered> = HashMap::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(ev) = rx.recv_timeout(left) else { break };
        if let ServiceEvent::ServiceResolved(s) = ev {
            let Some(fp) = s.get_property_val_str("fp").map(str::to_string) else { continue };
            let mut v4: Vec<Ipv4Addr> = s.get_addresses_v4().into_iter().collect();
            v4.sort_by_key(|ip| (ip.is_loopback(), !ip.is_private())); // prefer LAN addresses
            let Some(ip) = v4.first() else { continue };
            found.insert(
                fp.clone(),
                Discovered {
                    name: s.get_property_val_str("name").unwrap_or("desktop").to_string(),
                    address: SocketAddr::from((*ip, s.get_port())).to_string(),
                    fingerprint: fp,
                    pairing: s.get_property_val_str("pairing") == Some("1"),
                },
            );
        }
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    let mut v: Vec<_> = found.into_values().collect();
    v.sort_by(|a, b| b.pairing.cmp(&a.pairing).then(a.name.cmp(&b.name)));
    Ok(v)
}

/// Async wrapper around [`discover_blocking`].
pub async fn discover(wait: Duration) -> Result<Vec<Discovered>> {
    tokio::task::spawn_blocking(move || discover_blocking(wait)).await?
}

/// This machine's main LAN IPv4 address (no packets are sent).
pub fn local_ip() -> Option<Ipv4Addr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    match s.local_addr().ok()? {
        SocketAddr::V4(a) if !a.ip().is_loopback() && !a.ip().is_unspecified() => Some(*a.ip()),
        _ => None,
    }
}
