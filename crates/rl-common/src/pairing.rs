//! One-time pairing with a 6-digit PIN.
//!
//! Both sides run SPAKE2 with the PIN as password, giving a shared key `K` only
//! if the PINs match. Each side then sends `HMAC(K, role || client_fp || host_fp)`,
//! where the fingerprints are those *observed in the TLS handshake*. A
//! man-in-the-middle terminates TLS separately on each leg, so the fingerprints
//! differ between legs and the MACs fail — the PIN authenticates the
//! certificates.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};

use crate::proto::{read_msg, write_msg};

pub const PIN_LIFETIME: Duration = Duration::from_secs(120);
pub const MAX_ATTEMPTS: u32 = 3;

#[derive(Serialize, Deserialize, Debug)]
enum PairMsg {
    Hello { version: u32, name: String, spake: Vec<u8> },
    Reply { name: String, spake: Vec<u8> },
    Confirm { mac: Vec<u8> },
    Accepted { mac: Vec<u8> },
    Refused { reason: String },
}

pub fn generate_pin() -> String {
    use rand::Rng;
    format!("{:06}", rand::rngs::OsRng.gen_range(0..1_000_000u32))
}

fn spake_start(pin: &str) -> (Spake2<Ed25519Group>, Vec<u8>) {
    Spake2::<Ed25519Group>::start_symmetric(&Password::new(pin.as_bytes()), &SpakeId::new(b"remotelinux-pair"))
}

fn mac(key: &[u8], role: &[u8], client_fp: &str, host_fp: &str) -> Vec<u8> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac key");
    m.update(role);
    m.update(client_fp.as_bytes());
    m.update(host_fp.as_bytes());
    m.finalize().into_bytes().to_vec()
}

fn verify(key: &[u8], role: &[u8], client_fp: &str, host_fp: &str, tag: &[u8]) -> bool {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac key");
    m.update(role);
    m.update(client_fp.as_bytes());
    m.update(host_fp.as_bytes());
    m.verify_slice(tag).is_ok()
}

/// Result of a successful pairing, from either side's point of view.
#[derive(Debug, Clone)]
pub struct Paired {
    pub peer_name: String,
    pub peer_fingerprint: String,
}

/// Client side. `own_fp` is this client's certificate fingerprint.
pub async fn pair_as_client(conn: &quinn::Connection, own_fp: &str, name: &str, pin: &str) -> Result<Paired> {
    let host_fp = crate::transport::peer_fingerprint(conn)?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let (state, msg) = spake_start(pin);
    write_msg(&mut send, &PairMsg::Hello { version: crate::PROTOCOL_VERSION, name: name.into(), spake: msg }).await?;
    let (host_name, their) = match read_msg::<PairMsg>(&mut recv).await? {
        Some(PairMsg::Reply { name, spake }) => (name, spake),
        Some(PairMsg::Refused { reason }) => bail!("host refused pairing: {reason}"),
        other => bail!("unexpected pairing message: {other:?}"),
    };
    let key = state.finish(&their).map_err(|e| anyhow!("SPAKE2 failed: {e:?}"))?;
    write_msg(&mut send, &PairMsg::Confirm { mac: mac(&key, b"client", own_fp, &host_fp) }).await?;
    match read_msg::<PairMsg>(&mut recv).await? {
        Some(PairMsg::Accepted { mac: tag }) => {
            if !verify(&key, b"host", own_fp, &host_fp, &tag) {
                bail!("host failed to prove the PIN (possible man-in-the-middle)");
            }
        }
        Some(PairMsg::Refused { reason }) => bail!("pairing failed: {reason}"),
        other => bail!("unexpected pairing message: {other:?}"),
    }
    let _ = send.finish();
    Ok(Paired { peer_name: host_name, peer_fingerprint: host_fp })
}

/// Host-side state of an open pairing window.
#[derive(Debug, Clone)]
pub struct PairingWindow {
    pub pin: String,
    pub opened: Instant,
    pub attempts: u32,
}

impl PairingWindow {
    pub fn new() -> Self {
        Self { pin: generate_pin(), opened: Instant::now(), attempts: 0 }
    }
    pub fn expired(&self) -> bool {
        self.opened.elapsed() > PIN_LIFETIME || self.attempts >= MAX_ATTEMPTS
    }
    pub fn remaining(&self) -> Duration {
        PIN_LIFETIME.saturating_sub(self.opened.elapsed())
    }
}

/// Outcome of a host-side pairing attempt.
pub enum HostPairOutcome {
    Paired(Paired),
    WrongPin,
    Refused(String),
}

/// Host side. `pin` is `None` if no pairing window is open.
pub async fn pair_as_host(conn: &quinn::Connection, own_fp: &str, name: &str, pin: Option<&str>) -> Result<HostPairOutcome> {
    let client_fp = crate::transport::peer_fingerprint(conn)?;
    let (mut send, mut recv) = conn.accept_bi().await?;
    let (client_name, their) = match read_msg::<PairMsg>(&mut recv).await? {
        Some(PairMsg::Hello { version, name, spake }) => {
            if version != crate::PROTOCOL_VERSION {
                let reason = format!("protocol version mismatch (host {}, client {version})", crate::PROTOCOL_VERSION);
                write_msg(&mut send, &PairMsg::Refused { reason: reason.clone() }).await?;
                flush(send).await;
                return Ok(HostPairOutcome::Refused(reason));
            }
            (name, spake)
        }
        other => bail!("unexpected pairing message: {other:?}"),
    };
    let Some(pin) = pin else {
        let reason = "host is not in pairing mode (run `rl-host pair` on the desktop)".to_string();
        write_msg(&mut send, &PairMsg::Refused { reason: reason.clone() }).await?;
        flush(send).await;
        return Ok(HostPairOutcome::Refused(reason));
    };
    let (state, msg) = spake_start(pin);
    write_msg(&mut send, &PairMsg::Reply { name: name.into(), spake: msg }).await?;
    let key = state.finish(&their).map_err(|e| anyhow!("SPAKE2 failed: {e:?}"))?;
    let tag = match read_msg::<PairMsg>(&mut recv).await? {
        Some(PairMsg::Confirm { mac }) => mac,
        other => bail!("unexpected pairing message: {other:?}"),
    };
    if !verify(&key, b"client", &client_fp, own_fp, &tag) {
        write_msg(&mut send, &PairMsg::Refused { reason: "wrong PIN".into() }).await?;
        flush(send).await;
        return Ok(HostPairOutcome::WrongPin);
    }
    write_msg(&mut send, &PairMsg::Accepted { mac: mac(&key, b"host", &client_fp, own_fp) }).await?;
    flush(send).await;
    Ok(HostPairOutcome::Paired(Paired { peer_name: client_name, peer_fingerprint: client_fp }))
}

/// Finish a stream and wait (briefly) until the peer has received everything,
/// so closing the connection right after doesn't discard the last message.
async fn flush(mut send: quinn::SendStream) {
    let _ = send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
}
