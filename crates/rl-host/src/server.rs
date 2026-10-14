//! QUIC server: accepts paired clients, runs sessions, handles pairing.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use bytes::Bytes;
use rl_common::config::HostConfig;
use rl_common::identity::{Identity, Role};
use rl_common::pairing::{self, HostPairOutcome, PairingWindow};
use rl_common::proto::{read_msg, write_msg, ClientMsg, HostMsg, Motion};
use rl_common::transport::{self, ALPN_PAIR, ALPN_PROBE, ALPN_SESSION};
use rl_common::trust::{AuthorizedClient, AuthorizedClients};
use rl_common::video::{fragment, FrameMeta};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};

use crate::input::Injector;
use crate::stream::{self, StreamControl, StreamEvent};

/// Release held input if the client is silent this long.
const INPUT_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(1);

/// QUIC application close codes.
pub mod close {
    pub const NORMAL: u32 = 0;
    pub const NOT_PAIRED: u32 = 1;
    pub const REPLACED: u32 = 2;
    pub const VERSION: u32 = 3;
    pub const ERROR: u32 = 4;
    pub const SHUTDOWN: u32 = 5;
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PairEvent {
    Paired { name: String, fingerprint: String },
    WrongPin { attempts_left: u32 },
    Expired,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientStatus {
    pub name: String,
    pub fingerprint: String,
    pub address: String,
    pub connected_secs: u64,
    pub monitor: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub rtt_ms: f64,
}

struct Session {
    id: u64,
    conn: quinn::Connection,
    name: String,
    fingerprint: String,
    started: Instant,
    stream: Option<(String, u32, u32, u32)>,
    bytes_sent: Arc<AtomicU64>,
    last_rate: (Instant, u64, u32),
}

pub struct Host {
    pub config: HostConfig,
    pub identity: Identity,
    pub name: String,
    pub listen: SocketAddr,
    pairing: Mutex<Option<PairingWindow>>,
    pub pair_events: broadcast::Sender<PairEvent>,
    session: Mutex<Option<Session>>,
    injector: Mutex<Option<Injector>>,
    next_id: AtomicU64,
    endpoint: Mutex<Option<quinn::Endpoint>>,
}

impl Host {
    pub fn new(config: HostConfig) -> Result<Arc<Self>> {
        let identity = Identity::load_or_create(Role::Host)?;
        let listen: SocketAddr = config.listen.parse().with_context(|| format!("invalid listen address {:?}", config.listen))?;
        Ok(Arc::new(Self {
            name: config.device_name(),
            config,
            identity,
            listen,
            pairing: Mutex::new(None),
            pair_events: broadcast::channel(16).0,
            session: Mutex::new(None),
            injector: Mutex::new(None),
            next_id: AtomicU64::new(1),
            endpoint: Mutex::new(None),
        }))
    }

    /// Create the virtual input devices up front: the compositor takes a moment
    /// to pick up new devices, which would otherwise eat the first keystrokes.
    pub fn init_input(&self) -> Result<()> {
        *self.injector.lock().unwrap() = Some(Injector::new()?);
        Ok(())
    }

    /// Open a pairing window; returns the PIN.
    pub fn open_pairing(&self) -> PairingWindow {
        let w = PairingWindow::new();
        *self.pairing.lock().unwrap() = Some(w.clone());
        tracing::info!("pairing window open for {}s", w.remaining().as_secs());
        w
    }

    pub fn close_pairing(&self) {
        *self.pairing.lock().unwrap() = None;
    }

    pub fn pairing_open(&self) -> bool {
        let mut p = self.pairing.lock().unwrap();
        if p.as_ref().is_some_and(|w| w.expired()) {
            *p = None;
            let _ = self.pair_events.send(PairEvent::Expired);
        }
        p.is_some()
    }

    pub fn client_status(&self) -> Option<ClientStatus> {
        let mut s = self.session.lock().unwrap();
        let s = s.as_mut()?;
        let now = Instant::now();
        let bytes = s.bytes_sent.load(Ordering::Relaxed);
        let (t0, b0, kbps) = s.last_rate;
        let dt = now.duration_since(t0).as_secs_f64();
        let kbps = if dt >= 1.0 {
            let k = ((bytes - b0) as f64 * 8.0 / 1000.0 / dt) as u32;
            s.last_rate = (now, bytes, k);
            k
        } else {
            kbps
        };
        Some(ClientStatus {
            name: s.name.clone(),
            fingerprint: s.fingerprint.clone(),
            address: s.conn.remote_address().to_string(),
            connected_secs: s.started.elapsed().as_secs(),
            monitor: s.stream.as_ref().map(|x| x.0.clone()),
            width: s.stream.as_ref().map_or(0, |x| x.1),
            height: s.stream.as_ref().map_or(0, |x| x.2),
            fps: s.stream.as_ref().map_or(0, |x| x.3),
            kbps,
            rtt_ms: s.conn.rtt().as_secs_f64() * 1000.0,
        })
    }

    pub fn disconnect_client(&self) -> bool {
        if let Some(s) = self.session.lock().unwrap().as_ref() {
            s.conn.close(close::NORMAL.into(), b"disconnected by host");
            return true;
        }
        false
    }

    pub fn shutdown(&self) {
        if let Some(ep) = self.endpoint.lock().unwrap().as_ref() {
            ep.close(close::SHUTDOWN.into(), b"host shutting down");
        }
        if let Some(i) = self.injector.lock().unwrap().as_mut() {
            i.release_all();
        }
    }

    /// Accept connections until the endpoint closes.
    pub async fn serve(self: &Arc<Self>) -> Result<()> {
        let ep = transport::server_endpoint(&self.identity, self.listen)?;
        tracing::info!("listening on {} as {:?} (fingerprint {})", ep.local_addr()?, self.name, rl_common::identity::short_fingerprint(&self.identity.fingerprint));
        *self.endpoint.lock().unwrap() = Some(ep.clone());
        while let Some(incoming) = ep.accept().await {
            let host = self.clone();
            tokio::spawn(async move {
                let remote = incoming.remote_address();
                match incoming.await {
                    Ok(conn) => {
                        if let Err(e) = host.handle(conn).await {
                            tracing::warn!("{remote}: {e:#}");
                        }
                    }
                    Err(e) => tracing::debug!("{remote}: handshake failed: {e}"),
                }
            });
        }
        Ok(())
    }

    async fn handle(self: &Arc<Self>, conn: quinn::Connection) -> Result<()> {
        let fp = transport::peer_fingerprint(&conn)?;
        match transport::alpn(&conn).as_deref() {
            Some(ALPN_PAIR) => self.handle_pair(conn).await,
            Some(ALPN_PROBE) => {
                conn.close(close::NORMAL.into(), b"ok");
                Ok(())
            }
            Some(ALPN_SESSION) => {
                let Some(client) = AuthorizedClients::load()?.find_fingerprint(&fp).cloned() else {
                    tracing::warn!("rejecting unpaired client {} ({})", conn.remote_address(), rl_common::identity::short_fingerprint(&fp));
                    conn.close(close::NOT_PAIRED.into(), b"not paired with this host");
                    return Ok(());
                };
                self.handle_session(conn, client).await
            }
            _ => {
                conn.close(close::VERSION.into(), b"unknown protocol");
                Ok(())
            }
        }
    }

    async fn handle_pair(self: &Arc<Self>, conn: quinn::Connection) -> Result<()> {
        let pin = if self.pairing_open() { self.pairing.lock().unwrap().as_ref().map(|w| w.pin.clone()) } else { None };
        let outcome = pairing::pair_as_host(&conn, &self.identity.fingerprint, &self.name, pin.as_deref()).await?;
        match outcome {
            HostPairOutcome::Paired(p) => {
                let mut store = AuthorizedClients::load()?;
                store.upsert(AuthorizedClient { name: p.peer_name.clone(), fingerprint: p.peer_fingerprint.clone(), paired_at: rl_common::unix_now() });
                store.save()?;
                self.close_pairing();
                tracing::info!("paired with {:?}", p.peer_name);
                let _ = self.pair_events.send(PairEvent::Paired { name: p.peer_name, fingerprint: p.peer_fingerprint });
            }
            HostPairOutcome::WrongPin => {
                let mut g = self.pairing.lock().unwrap();
                if let Some(w) = g.as_mut() {
                    w.attempts += 1;
                    let left = pairing::MAX_ATTEMPTS.saturating_sub(w.attempts);
                    tracing::warn!("wrong pairing PIN from {} ({left} attempts left)", conn.remote_address());
                    let _ = self.pair_events.send(PairEvent::WrongPin { attempts_left: left });
                    if left == 0 {
                        *g = None;
                        let _ = self.pair_events.send(PairEvent::Expired);
                    }
                }
            }
            HostPairOutcome::Refused(r) => tracing::info!("pairing refused: {r}"),
        }
        conn.close(close::NORMAL.into(), b"");
        Ok(())
    }

    async fn handle_session(self: &Arc<Self>, conn: quinn::Connection, client: AuthorizedClient) -> Result<()> {
        let (mut send, mut recv) = conn.accept_bi().await?;
        let (settings, client_name) = match read_msg::<ClientMsg>(&mut recv).await? {
            Some(ClientMsg::Hello { version, client_name, settings }) => {
                if version != rl_common::PROTOCOL_VERSION {
                    let text = format!("protocol version mismatch: host {} client {version}", rl_common::PROTOCOL_VERSION);
                    write_msg(&mut send, &HostMsg::Error { text }).await?;
                    conn.close(close::VERSION.into(), b"version mismatch");
                    return Ok(());
                }
                (settings, client_name)
            }
            other => anyhow::bail!("expected Hello, got {other:?}"),
        };
        let monitors: Vec<_> = crate::monitors::list().map(|m| m.into_iter().map(|m| m.info).collect()).unwrap_or_default();
        write_msg(&mut send, &HostMsg::Welcome { version: rl_common::PROTOCOL_VERSION, host_name: self.name.clone(), monitors }).await?;

        // One client at a time: a new connection from a paired client replaces
        // the old one (typically the same laptop reconnecting after a drop).
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let bytes_sent = Arc::new(AtomicU64::new(0));
        {
            let mut s = self.session.lock().unwrap();
            if let Some(old) = s.take() {
                tracing::info!("replacing session with {:?}", old.name);
                old.conn.close(close::REPLACED.into(), b"replaced by a new connection");
            }
            *s = Some(Session {
                id,
                conn: conn.clone(),
                name: client.name.clone(),
                fingerprint: client.fingerprint.clone(),
                started: Instant::now(),
                stream: None,
                bytes_sent: bytes_sent.clone(),
                last_rate: (Instant::now(), 0, 0),
            });
        }
        tracing::info!("session started: {:?} ({}) from {}", client.name, client_name, conn.remote_address());
        if self.config.notify_on_connect {
            notify(&format!("{} connected", client.name));
        }
        if let Some(i) = self.injector.lock().unwrap().as_mut() {
            i.new_session();
        }

        let result = self.run_session(&conn, send, recv, settings, id, bytes_sent).await;

        if let Some(i) = self.injector.lock().unwrap().as_mut() {
            i.release_all();
        }
        {
            let mut s = self.session.lock().unwrap();
            if s.as_ref().is_some_and(|s| s.id == id) {
                *s = None;
            }
        }
        tracing::info!("session ended: {:?} ({})", client.name, match &result { Ok(()) => "closed".to_string(), Err(e) => format!("{e:#}") });
        if self.config.notify_on_connect {
            notify(&format!("{} disconnected", client.name));
        }
        result
    }

    async fn run_session(
        self: &Arc<Self>,
        conn: &quinn::Connection,
        send: quinn::SendStream,
        mut recv: quinn::RecvStream,
        settings: rl_common::proto::StreamSettings,
        id: u64,
        bytes_sent: Arc<AtomicU64>,
    ) -> Result<()> {
        let ctrl = Arc::new(StreamControl::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        stream::spawn(self.config.clone(), settings, ctrl.clone(), tx);
        struct StopOnDrop(Arc<StreamControl>);
        impl Drop for StopOnDrop {
            fn drop(&mut self) {
                self.0.stop.store(true, Ordering::Relaxed);
            }
        }
        let _stop = StopOnDrop(ctrl.clone());

        // Single writer for the control stream.
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<HostMsg>();
        let writer = async move {
            let mut send = send;
            while let Some(m) = out_rx.recv().await {
                write_msg(&mut send, &m).await?;
            }
            anyhow::Ok(())
        };

        let audio_bytes = bytes_sent.clone();
        let video = {
            let conn = conn.clone();
            let out_tx = out_tx.clone();
            let host = self.clone();
            async move {
                let mut frame_id: u32 = 0;
                while let Some(ev) = rx.recv().await {
                    match ev {
                        StreamEvent::Info { monitor, width, height, fps, target } => {
                            if let Some(i) = host.injector.lock().unwrap().as_mut() {
                                i.set_target(target);
                            }
                            if let Some(s) = host.session.lock().unwrap().as_mut().filter(|s| s.id == id) {
                                s.stream = Some((monitor.clone(), width, height, fps));
                            }
                            let _ = out_tx.send(HostMsg::StreamInfo { monitor, width, height, fps });
                        }
                        StreamEvent::Frame { data, keyframe, capture_us, host_us } => {
                            let max = conn.max_datagram_size().unwrap_or(1200);
                            let meta = FrameMeta { frame_id, keyframe, capture_us, host_us };
                            frame_id = frame_id.wrapping_add(1);
                            for d in fragment(meta, &data, max) {
                                bytes_sent.fetch_add(d.len() as u64, Ordering::Relaxed);
                                if let Err(e) = conn.send_datagram(Bytes::from(d)) {
                                    anyhow::bail!("sending video: {e}");
                                }
                            }
                        }
                        StreamEvent::Error(text) => {
                            let _ = out_tx.send(HostMsg::Error { text: text.clone() });
                            anyhow::bail!("stream failed: {text}");
                        }
                    }
                }
                anyhow::Ok(())
            }
        };

        // Desktop audio. Optional: a failure is reported, never fatal.
        let audio = {
            let conn = conn.clone();
            let out_tx = out_tx.clone();
            let enabled = self.config.audio;
            let bytes_sent = audio_bytes;
            async move {
                if !enabled {
                    return std::future::pending().await;
                }
                let (tx, mut rx) = mpsc::unbounded_channel();
                let (err_tx, mut err_rx) = mpsc::unbounded_channel();
                crate::audio::spawn(tx, err_tx);
                let mut seq: u32 = 0;
                loop {
                    tokio::select! {
                        Some(p) = rx.recv() => {
                            let d = rl_common::proto::encode_audio(seq, &p);
                            seq = seq.wrapping_add(1);
                            bytes_sent.fetch_add(d.len() as u64, Ordering::Relaxed);
                            if let Err(e) = conn.send_datagram(Bytes::from(d)) {
                                anyhow::bail!("sending audio: {e}");
                            }
                        }
                        Some(e) = err_rx.recv() => {
                            let _ = out_tx.send(HostMsg::Notice { text: format!("No audio: {e}") });
                        }
                        else => return std::future::pending().await,
                    }
                }
            }
        };

        let motion = {
            let conn = conn.clone();
            let host = self.clone();
            async move {
                loop {
                    let d = conn.read_datagram().await?;
                    if let Some(m) = Motion::decode(&d) {
                        if let Some(i) = host.injector.lock().unwrap().as_mut() {
                            let r = match m {
                                Motion::Abs { seq, x, y } => i.motion_abs(seq, x, y),
                                Motion::Rel { seq, dx, dy } => i.motion_rel(seq, dx, dy),
                            };
                            if let Err(e) = r {
                                tracing::warn!("pointer injection: {e}");
                            }
                        }
                    }
                }
                #[allow(unreachable_code)]
                anyhow::Ok(())
            }
        };

        let control = {
            let host = self.clone();
            let ctrl = ctrl.clone();
            async move {
                loop {
                    // Watchdog: the client pings every 250 ms. If it goes
                    // quiet, release held keys long before QUIC gives up.
                    let msg = match tokio::time::timeout(INPUT_WATCHDOG, read_msg::<ClientMsg>(&mut recv)).await {
                        Ok(r) => match r? {
                            Some(m) => m,
                            None => break,
                        },
                        Err(_) => {
                            if let Some(i) = host.injector.lock().unwrap().as_mut() {
                                i.release_all();
                            }
                            continue;
                        }
                    };
                    let inject = |f: &mut dyn FnMut(&mut Injector) -> Result<()>| {
                        if let Some(i) = host.injector.lock().unwrap().as_mut() {
                            if let Err(e) = f(i) {
                                tracing::warn!("input injection: {e}");
                            }
                        }
                    };
                    match msg {
                        ClientMsg::Key { code, pressed } => inject(&mut |i| i.key(code, pressed)),
                        ClientMsg::Button { code, pressed } => inject(&mut |i| i.button(code, pressed)),
                        ClientMsg::Scroll { v120_x, v120_y } => inject(&mut |i| i.scroll(v120_x, v120_y)),
                        ClientMsg::ReleaseAll => inject(&mut |i| {
                            i.release_all();
                            Ok(())
                        }),
                        ClientMsg::RequestKeyframe => ctrl.keyframe.store(true, Ordering::Relaxed),
                        ClientMsg::UpdateSettings(s) => *ctrl.settings.lock().unwrap() = Some(s),
                        ClientMsg::Ping { t_client_us } => {
                            let _ = out_tx.send(HostMsg::Pong { t_client_us, t_host_us: rl_common::clock::now_us() });
                        }
                        ClientMsg::Bye => break,
                        ClientMsg::Hello { .. } => tracing::warn!("duplicate Hello ignored"),
                    }
                }
                anyhow::Ok(())
            }
        };

        let res = tokio::select! {
            r = writer => r,
            r = video => r,
            r = audio => r,
            r = motion => r,
            r = control => r,
        };
        match res {
            Err(e) if is_closed(&e) => Ok(()),
            Err(e) => {
                let mut reason = format!("{e:#}");
                reason.truncate(500);
                conn.close(close::ERROR.into(), reason.as_bytes());
                Err(e)
            }
            Ok(()) => {
                conn.close(close::NORMAL.into(), b"session ended");
                Ok(())
            }
        }
    }
}

fn is_closed(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<quinn::ConnectionError>().is_some()
            || c.downcast_ref::<quinn::ReadExactError>().is_some()
            || c.downcast_ref::<quinn::WriteError>().is_some()
    })
}

fn notify(text: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "RemoteLinux", "-i", "network-wired", "RemoteLinux", text])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
