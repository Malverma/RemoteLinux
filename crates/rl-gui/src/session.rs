//! A running viewer: the `rl-client connect <name> --json` process. Events are
//! read from its stdout on a thread, and commands are written to its stdin.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use eframe::egui;
use serde_json::{json, Value};

use crate::Msg;

pub enum State {
    Starting,
    Connecting { attempt: u64 },
    Connected,
    Reconnecting { retry_at: Instant, reason: String },
    Disconnected { reason: String },
    Failed(String),
}

#[derive(Default)]
pub struct Stats {
    pub latency_ms: Option<f64>,
    pub rtt_ms: f64,
    pub fps: f64,
    pub kbps: f64,
    pub dropped_total: u64,
}

pub struct Session {
    pub host: String,
    pub state: State,
    pub stats: Option<Stats>,
    /// Whether keyboard and mouse are being sent to the desktop.
    pub forwarding: bool,
    pub stats_visible: bool,
    pub fullscreen: bool,
    pub notice: Option<String>,
    pub ever_connected: bool,
    child: Child,
    stdin: Option<ChildStdin>,
}

/// `rl-client` next to this binary (cargo build / package), else from PATH.
fn viewer_binary() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| Some(p.parent()?.join("rl-client")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| "rl-client".into())
}

impl Session {
    pub fn start(host: &str, ctx: &egui::Context, tx: Sender<Msg>) -> std::io::Result<Self> {
        let mut child = Command::new(viewer_binary())
            .args(["connect", host, "--json"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if tx.send(Msg::Viewer(v)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            }
            let _ = tx.send(Msg::ViewerExited);
            ctx.request_repaint();
        });
        Ok(Self {
            host: host.to_string(),
            state: State::Starting,
            stats: None,
            forwarding: true,
            stats_visible: false,
            fullscreen: false,
            notice: None,
            ever_connected: false,
            stdin: child.stdin.take(),
            child,
        })
    }

    pub fn handle(&mut self, v: &Value) {
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        match v["event"].as_str().unwrap_or_default() {
            "connecting" => self.state = State::Connecting { attempt: v["attempt"].as_u64().unwrap_or(1) },
            "connected" => {
                self.state = State::Connected;
                self.ever_connected = true;
                self.notice = None;
            }
            "stats" => {
                self.stats = Some(Stats {
                    latency_ms: v["latency_ms"].as_f64(),
                    rtt_ms: v["rtt_ms"].as_f64().unwrap_or_default(),
                    fps: v["fps"].as_f64().unwrap_or_default(),
                    kbps: v["kbps"].as_f64().unwrap_or_default(),
                    dropped_total: v["dropped_total"].as_u64().unwrap_or_default(),
                })
            }
            "input" => self.forwarding = v["forwarding"].as_bool().unwrap_or(true),
            "disconnected" => {
                self.state = State::Disconnected { reason: s("reason") };
                self.stats = None;
            }
            "reconnecting" => {
                let ms = v["retry_in_ms"].as_u64().unwrap_or(0);
                self.state = State::Reconnecting { retry_at: Instant::now() + Duration::from_millis(ms), reason: s("reason") };
                self.stats = None;
            }
            "error" if v["fatal"].as_bool().unwrap_or(false) => self.state = State::Failed(s("message")),
            "error" | "notice" => self.notice = Some(if v["text"].is_string() { s("text") } else { s("message") }),
            "fullscreen" => self.fullscreen = v["fullscreen"].as_bool().unwrap_or(false),
            "stats_visible" => self.stats_visible = v["visible"].as_bool().unwrap_or(false),
            _ => {}
        }
    }

    pub fn send(&mut self, cmd: Value) {
        if let Some(stdin) = &mut self.stdin {
            let _ = writeln!(stdin, "{cmd}").and_then(|_| stdin.flush());
        }
    }

    pub fn command(&mut self, cmd: &str) {
        self.send(json!({ "cmd": cmd }));
    }
}

impl Drop for Session {
    /// Ask the viewer to disconnect cleanly (so the desktop releases keys), then
    /// make sure the process is gone.
    fn drop(&mut self) {
        self.command("disconnect");
        self.stdin = None;
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
