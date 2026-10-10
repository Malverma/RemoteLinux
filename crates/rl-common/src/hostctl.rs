//! Managing the local host daemon from a frontend: talking to its control
//! socket, installing the systemd user service, and the one-time system
//! setup (firewall, uinput access) through pkexec.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

pub const SERVICE: &str = "rl-host.service";

/// Status reported by a running host daemon.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HostStatus {
    pub name: String,
    pub listen: String,
    pub port: u16,
    pub fingerprint: String,
    pub pairing_open: bool,
    pub client: Option<ConnectedClient>,
    #[serde(default)]
    pub paired: Vec<PairedClient>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConnectedClient {
    pub name: String,
    pub address: String,
    pub connected_secs: u64,
    pub monitor: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub rtt_ms: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PairedClient {
    pub name: String,
    pub fingerprint: String,
    pub fingerprint_short: String,
    pub paired_at: u64,
}

async fn connect() -> Result<(Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf)> {
    let sock = UnixStream::connect(crate::paths::host_control_socket()).await.context("RemoteLinux host is not running")?;
    let (r, w) = sock.into_split();
    Ok((BufReader::new(r).lines(), w))
}

/// Send one request to the running daemon and return its JSON reply.
pub async fn request(req: Value) -> Result<Value> {
    let (mut lines, mut w) = connect().await?;
    w.write_all(format!("{req}\n").as_bytes()).await?;
    let line = lines.next_line().await?.ok_or_else(|| anyhow!("host closed the control connection"))?;
    Ok(serde_json::from_str(&line)?)
}

/// `None` if the daemon is not running.
pub async fn status() -> Option<HostStatus> {
    let v = request(json!({"cmd": "status"})).await.ok()?;
    serde_json::from_value(v).ok()
}

pub async fn unpair(name: &str) -> Result<bool> {
    let v = request(json!({"cmd": "unpair", "name": name})).await?;
    Ok(v["removed"].as_bool().unwrap_or(false))
}

pub async fn disconnect_client() -> Result<()> {
    request(json!({"cmd": "disconnect"})).await.map(|_| ())
}

/// An open pairing window. Dropping it closes the window on the host.
pub struct PairWindow {
    pub pin: String,
    pub expires_in: u64,
    lines: Lines<BufReader<OwnedReadHalf>>,
    _w: OwnedWriteHalf,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PairEvent {
    Paired { name: String },
    WrongPin { attempts_left: u32 },
    Expired,
    Cancelled,
}

impl PairWindow {
    pub async fn open() -> Result<Self> {
        let (mut lines, mut w) = connect().await?;
        w.write_all(b"{\"cmd\":\"pair\"}\n").await?;
        let line = lines.next_line().await?.ok_or_else(|| anyhow!("host closed the control connection"))?;
        let v: Value = serde_json::from_str(&line)?;
        let pin = v["pin"].as_str().ok_or_else(|| anyhow!("host did not return a PIN: {v}"))?.to_string();
        Ok(Self { pin, expires_in: v["expires_in"].as_u64().unwrap_or(120), lines, _w: w })
    }

    /// Wait for the next pairing event. Returns `Expired` if the host goes away.
    pub async fn next(&mut self) -> PairEvent {
        loop {
            let Ok(Some(line)) = self.lines.next_line().await else { return PairEvent::Expired };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            return match v["event"].as_str() {
                Some("paired") => PairEvent::Paired { name: v["name"].as_str().unwrap_or("").to_string() },
                Some("wrong_pin") => PairEvent::WrongPin { attempts_left: v["attempts_left"].as_u64().unwrap_or(0) as u32 },
                Some("cancelled") => PairEvent::Cancelled,
                Some("expired") => PairEvent::Expired,
                _ => continue,
            };
        }
    }
}

// ---- Service management -------------------------------------------------

/// The rl-host binary: next to the current executable, else on PATH.
pub fn host_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join("rl-host"))).filter(|p| p.exists()) {
        return Some(p);
    }
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| Path::new(d).join("rl-host")).find(|p| p.exists())
}

fn systemctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("systemctl").arg("--user").args(args).output().context("running systemctl")
}

/// Whether the service is enabled to start with the desktop session.
pub fn service_enabled() -> bool {
    systemctl(&["is-enabled", SERVICE]).map(|o| o.status.success()).unwrap_or(false)
}

/// Install (or update) the user service for `bin` and start it now and at every login.
pub fn enable_service(bin: &Path) -> Result<()> {
    let dir = crate::paths::config_dir().parent().unwrap_or(Path::new(".")).join("systemd/user");
    let unit = format!(
        "[Unit]\nDescription=RemoteLinux host (low-latency remote desktop server)\nAfter=graphical-session.target\nPartOf=graphical-session.target\n\n\
         [Service]\nExecStart={} run\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=graphical-session.target\n",
        bin.display()
    );
    crate::paths::write_private(&dir.join(SERVICE), unit.as_bytes(), 0o644)?;
    for args in [&["daemon-reload"][..], &["enable", SERVICE], &["restart", SERVICE]] {
        let o = systemctl(args)?;
        if !o.status.success() {
            bail!("systemctl --user {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim());
        }
    }
    Ok(())
}

/// Stop the service and keep it from starting at login.
pub fn disable_service() -> Result<()> {
    let o = systemctl(&["disable", "--now", SERVICE])?;
    if !o.status.success() {
        bail!("{}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}

/// Recent service log lines, for showing why the host failed to start.
pub fn service_log(lines: usize) -> String {
    Command::new("journalctl")
        .args(["--user", "-u", SERVICE, "-n", &lines.to_string(), "--no-pager", "-o", "cat"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

// ---- One-time system setup (needs root via pkexec) -------------------------

pub fn uinput_writable() -> bool {
    std::fs::OpenOptions::new().write(true).open("/dev/uinput").is_ok()
}

pub fn ufw_active() -> bool {
    Command::new("systemctl").args(["is-active", "-q", "ufw"]).status().map(|s| s.success()).unwrap_or(false)
}

fn marker() -> PathBuf {
    crate::paths::data_dir().join("firewall-opened")
}

/// Whether we already added the firewall rules (ufw status itself needs root).
pub fn firewall_opened() -> bool {
    marker().exists()
}

const UDEV_RULE: &str = r#"KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess", OPTIONS+="static_node=uinput""#;

fn pkexec(script: &str) -> Result<()> {
    let o = Command::new("pkexec").args(["sh", "-c", script]).output().context("running pkexec (is polkit installed?)")?;
    match o.status.code() {
        Some(0) => Ok(()),
        Some(126) | Some(127) => bail!("authorisation was cancelled"),
        _ => bail!("{}", String::from_utf8_lossy(&o.stderr).trim()),
    }
}

/// Allow RemoteLinux's UDP port from private LAN ranges in ufw. (Discovery
/// uses mDNS, which ufw allows by default.)
pub fn open_firewall() -> Result<()> {
    let mut script = String::from("set -e;");
    for net in ["192.168.0.0/16", "10.0.0.0/8", "172.16.0.0/12"] {
        script.push_str(&format!("ufw allow proto udp from {net} to any port {} comment RemoteLinux >/dev/null;", crate::DEFAULT_PORT));
    }
    pkexec(&script)?;
    crate::paths::write_private(&marker(), b"1", 0o644)
}

/// Install the udev rule that lets the logged-in user create virtual input devices.
pub fn fix_uinput() -> Result<()> {
    pkexec(&format!(
        "set -e; printf '%s\\n' '{UDEV_RULE}' > /etc/udev/rules.d/70-remotelinux.rules; udevadm control --reload; udevadm trigger --name-match=uinput; modprobe uinput || true"
    ))
}
