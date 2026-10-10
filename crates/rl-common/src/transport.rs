//! QUIC endpoint configuration tuned for a LAN, low-latency stream.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use quinn::congestion::{Controller, ControllerFactory};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};

use crate::identity::Identity;
use crate::tls::{provider, AcceptAnyCert};

pub const ALPN_SESSION: &[u8] = b"remotelinux/1";
pub const ALPN_PAIR: &[u8] = b"remotelinux-pair/1";
/// Liveness/identity check: the host closes immediately after the handshake.
pub const ALPN_PROBE: &[u8] = b"remotelinux-probe/1";

/// Congestion "controller" with a large fixed window. The video stream is
/// constant-bitrate and self-limited, and on a LAN we never want the window to
/// delay a frame — standard controllers start small and back off on Wi-Fi loss,
/// which shows up as latency spikes.
#[derive(Clone)]
struct FixedWindow(u64);

impl Controller for FixedWindow {
    fn on_congestion_event(&mut self, _now: std::time::Instant, _sent: std::time::Instant, _persistent: bool, _lost: u64) {}
    fn on_mtu_update(&mut self, _new_mtu: u16) {}
    fn window(&self) -> u64 {
        self.0
    }
    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }
    fn initial_window(&self) -> u64 {
        self.0
    }
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

struct FixedWindowFactory;

impl ControllerFactory for FixedWindowFactory {
    fn build(self: Arc<Self>, _now: std::time::Instant, _mtu: u16) -> Box<dyn Controller> {
        Box::new(FixedWindow(16 * 1024 * 1024))
    }
}

fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.congestion_controller_factory(Arc::new(FixedWindowFactory));
    t.keep_alive_interval(Some(Duration::from_millis(500)));
    t.max_idle_timeout(Some(Duration::from_secs(3).try_into().unwrap()));
    t.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
    t.datagram_send_buffer_size(4 * 1024 * 1024);
    t.initial_rtt(Duration::from_millis(5));
    // Allow MTU discovery to grow datagrams past 1200 bytes on Ethernet.
    t.mtu_discovery_config(Some(quinn::MtuDiscoveryConfig::default()));
    Arc::new(t)
}

pub fn server_config(id: &Identity) -> Result<quinn::ServerConfig> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(AcceptAnyCert::new())
        .with_single_cert(vec![id.cert.clone()], id.key_der())?;
    tls.alpn_protocols = vec![ALPN_SESSION.to_vec(), ALPN_PAIR.to_vec(), ALPN_PROBE.to_vec()];
    tls.max_early_data_size = 0;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    cfg.transport_config(transport_config());
    Ok(cfg)
}

pub fn client_config(id: &Identity, alpn: &[u8]) -> Result<quinn::ClientConfig> {
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(AcceptAnyCert::new())
        .with_client_auth_cert(vec![id.cert.clone()], id.key_der())?;
    tls.alpn_protocols = vec![alpn.to_vec()];
    let mut cfg = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    cfg.transport_config(transport_config());
    Ok(cfg)
}

/// Bind a UDP socket and set low-latency socket options.
pub fn bind_socket(addr: SocketAddr) -> Result<std::net::UdpSocket> {
    let sock = std::net::UdpSocket::bind(addr).with_context(|| format!("binding UDP {addr}"))?;
    set_low_latency(&sock);
    Ok(sock)
}

fn set_low_latency(sock: &std::net::UdpSocket) {
    use std::os::fd::AsRawFd;
    // DSCP EF (46) << 2 = 0xb8, and SO_PRIORITY 6 for local qdisc / Wi-Fi WMM voice/video queues.
    let fd = sock.as_raw_fd();
    unsafe {
        let tos: i32 = 0xb8;
        libc_setsockopt(fd, 0 /* IPPROTO_IP */, 1 /* IP_TOS */, &tos);
        let prio: i32 = 6;
        libc_setsockopt(fd, 1 /* SOL_SOCKET */, 12 /* SO_PRIORITY */, &prio);
    }
}

unsafe fn libc_setsockopt(fd: i32, level: i32, name: i32, val: &i32) {
    extern "C" {
        fn setsockopt(fd: i32, level: i32, name: i32, val: *const core::ffi::c_void, len: u32) -> i32;
    }
    setsockopt(fd, level, name, val as *const i32 as *const _, 4);
}

pub fn server_endpoint(id: &Identity, addr: SocketAddr) -> Result<quinn::Endpoint> {
    let sock = bind_socket(addr)?;
    let ep = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_config(id)?),
        sock,
        Arc::new(quinn::TokioRuntime),
    )?;
    Ok(ep)
}

pub fn client_endpoint() -> Result<quinn::Endpoint> {
    let sock = bind_socket("0.0.0.0:0".parse().unwrap())?;
    let ep = quinn::Endpoint::new(quinn::EndpointConfig::default(), None, sock, Arc::new(quinn::TokioRuntime))?;
    Ok(ep)
}

/// Fingerprint of the peer's certificate on an established connection.
pub fn peer_fingerprint(conn: &quinn::Connection) -> Result<String> {
    let any = conn.peer_identity().ok_or_else(|| anyhow!("peer presented no certificate"))?;
    let certs = any
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .map_err(|_| anyhow!("unexpected peer identity type"))?;
    let cert = certs.first().ok_or_else(|| anyhow!("empty peer certificate chain"))?;
    Ok(crate::identity::fingerprint(cert))
}

/// ALPN negotiated on a connection.
pub fn alpn(conn: &quinn::Connection) -> Option<Vec<u8>> {
    conn.handshake_data()?
        .downcast::<quinn::crypto::rustls::HandshakeData>()
        .ok()?
        .protocol
}

/// Resolve "host", "host:port", "ip" or "ip:port" to a socket address (IPv4 preferred).
pub fn resolve(addr: &str) -> Result<SocketAddr> {
    use std::net::ToSocketAddrs;
    if let Ok(a) = addr.parse::<SocketAddr>() {
        return Ok(a);
    }
    let has_port = matches!(addr.rsplit_once(':'), Some((h, p)) if !h.contains(':') && p.parse::<u16>().is_ok());
    let target = if has_port { addr.to_string() } else { format!("{addr}:{}", crate::DEFAULT_PORT) };
    let addrs: Vec<SocketAddr> = target.to_socket_addrs().with_context(|| format!("resolving {addr}"))?.collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
        .ok_or_else(|| anyhow!("could not resolve {addr}"))
}
