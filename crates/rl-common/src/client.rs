//! High-level client operations for frontends (CLI, GUI): pairing with a host
//! and checking whether a paired host is online. The viewer itself runs as the
//! `rl-client connect` process.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::identity::{Identity, Role};
use crate::transport::{self, ALPN_PAIR, ALPN_PROBE};
use crate::trust::{KnownHost, KnownHosts};

/// Pair with the host at `address` ("ip", "host", "ip:port") using the PIN it
/// shows, and save it to `known_hosts.toml`. `name` is how this laptop is
/// shown on the host.
pub async fn pair(address: &str, pin: &str, name: &str) -> Result<KnownHost> {
    let pin: String = pin.chars().filter(|c| c.is_ascii_digit()).collect();
    if pin.len() != 6 {
        bail!("the PIN is 6 digits");
    }
    let addr = transport::resolve(address)?;
    let identity = Identity::load_or_create(Role::Client)?;
    let ep = transport::client_endpoint()?;
    let conn = tokio::time::timeout(Duration::from_secs(5), ep.connect_with(transport::client_config(&identity, ALPN_PAIR)?, addr, "remotelinux")?)
        .await
        .map_err(|_| anyhow!("{address} did not respond; is rl-host running there and UDP port {} open?", addr.port()))?
        .with_context(|| format!("connecting to {addr}"))?;
    let res = crate::pairing::pair_as_client(&conn, &identity.fingerprint, name, &pin).await;
    conn.close(0u32.into(), b"");
    ep.wait_idle().await;
    let paired = res?;
    // Keep the address as typed (a hostname survives DHCP changes), with an explicit port.
    let has_port = address.parse::<std::net::SocketAddr>().is_ok()
        || matches!(address.rsplit_once(':'), Some((h, p)) if !h.contains(':') && p.parse::<u16>().is_ok());
    let address = if has_port { address.to_string() } else { format!("{address}:{}", addr.port()) };
    let now = crate::unix_now();
    let host = KnownHost { name: paired.peer_name, address, fingerprint: paired.peer_fingerprint, paired_at: now, last_seen: Some(now) };
    let mut kh = KnownHosts::load()?;
    kh.upsert(host.clone());
    kh.save()?;
    Ok(host)
}

/// Result of probing a host.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProbeResult {
    pub name: String,
    pub address: String,
    pub online: bool,
    pub rtt_ms: Option<f64>,
    pub error: Option<String>,
}

/// Check which hosts are online (and still have their paired identity).
/// All hosts are probed concurrently.
pub async fn probe(hosts: &[KnownHost], timeout: Duration) -> Vec<ProbeResult> {
    let fail = |h: &KnownHost, e: String| ProbeResult { name: h.name.clone(), address: h.address.clone(), online: false, rtt_ms: None, error: Some(e) };
    let identity = match Identity::load_or_create(Role::Client) {
        Ok(i) => std::sync::Arc::new(i),
        Err(e) => return hosts.iter().map(|h| fail(h, format!("{e:#}"))).collect(),
    };
    let ep = match transport::client_endpoint() {
        Ok(e) => e,
        Err(e) => return hosts.iter().map(|h| fail(h, format!("{e:#}"))).collect(),
    };
    let tasks: Vec<_> = hosts
        .iter()
        .cloned()
        .map(|h| {
            let (ep, id) = (ep.clone(), identity.clone());
            tokio::spawn(async move { probe_one(&ep, &id, &h, timeout).await })
        })
        .collect();
    let mut out = Vec::with_capacity(tasks.len());
    for (t, h) in tasks.into_iter().zip(hosts) {
        out.push(match t.await {
            Ok(Ok(rtt)) => ProbeResult {
                name: h.name.clone(),
                address: h.address.clone(),
                online: true,
                rtt_ms: Some((rtt.as_secs_f64() * 10_000.0).round() / 10.0),
                error: None,
            },
            Ok(Err(e)) => fail(h, format!("{e:#}")),
            Err(e) => fail(h, e.to_string()),
        });
    }
    ep.close(0u32.into(), b"");
    out
}

async fn probe_one(endpoint: &quinn::Endpoint, identity: &Identity, host: &KnownHost, timeout: Duration) -> Result<Duration> {
    let addr = transport::resolve(&host.address)?;
    let cfg = transport::client_config(identity, ALPN_PROBE)?;
    let t = Instant::now();
    let conn = tokio::time::timeout(timeout, endpoint.connect_with(cfg, addr, "remotelinux")?)
        .await
        .map_err(|_| anyhow!("timed out"))??;
    let rtt = t.elapsed();
    let fp = transport::peer_fingerprint(&conn)?;
    conn.close(0u32.into(), b"probe");
    if fp != host.fingerprint {
        bail!("a different machine answered at {} (fingerprint mismatch)", host.address);
    }
    Ok(rtt)
}
