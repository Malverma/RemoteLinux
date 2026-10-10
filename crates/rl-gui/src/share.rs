//! "Share this PC": run the RemoteLinux host on this machine, show the pairing
//! code, and manage paired laptops. Talks to the rl-host daemon through its
//! control socket (`rl_common::hostctl`).

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Layout, RichText};
use rl_common::hostctl::{self, HostStatus, PairEvent, PairWindow};
use tokio::sync::oneshot;

use crate::theme::*;

const REFRESH: Duration = Duration::from_secs(2);

enum Msg {
    Status(Option<HostStatus>),
    /// A setup action finished: Ok(message) or Err(error).
    Done(Result<String, String>),
    PairOpened(Result<(String, u64), String>),
    Pair(PairEvent),
}

struct Pairing {
    pin: Option<String>,
    expires_at: Instant,
    note: Option<String>,
    cancel: Option<oneshot::Sender<()>>,
}

pub struct Share {
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    status: Option<HostStatus>,
    checked_once: bool,
    last_refresh: Option<Instant>,
    refreshing: bool,
    host_bin: Option<PathBuf>,
    enabled: bool,
    uinput_ok: bool,
    ufw: bool,
    firewall_opened: bool,
    busy: Option<&'static str>,
    message: Option<(String, bool)>,
    pairing: Option<Pairing>,
    started_at: Option<Instant>,
    confirm_remove: Option<String>,
}

impl Share {
    pub fn new(rt: tokio::runtime::Handle, ctx: egui::Context) -> Self {
        let (tx, rx) = channel();
        let mut s = Self {
            rt,
            ctx,
            tx,
            rx,
            status: None,
            checked_once: false,
            last_refresh: None,
            refreshing: false,
            host_bin: None,
            enabled: false,
            uinput_ok: true,
            ufw: false,
            firewall_opened: false,
            busy: None,
            message: None,
            pairing: None,
            started_at: None,
            confirm_remove: None,
        };
        s.recheck_system();
        s
    }

    /// Whether this machine is set up as a host (used to pick the start screen).
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn recheck_system(&mut self) {
        self.host_bin = hostctl::host_binary();
        self.enabled = hostctl::service_enabled();
        self.uinput_ok = hostctl::uinput_writable();
        self.ufw = hostctl::ufw_active();
        self.firewall_opened = hostctl::firewall_opened();
    }

    fn refresh(&mut self) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        self.last_refresh = Some(Instant::now());
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let _ = tx.send(Msg::Status(hostctl::status().await));
            ctx.request_repaint();
        });
    }

    /// Run a blocking setup step off the UI thread.
    fn run(&mut self, label: &'static str, f: impl FnOnce() -> anyhow::Result<String> + Send + 'static) {
        self.busy = Some(label);
        self.message = None;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Done(f().map_err(|e| format!("{e:#}"))));
            ctx.request_repaint();
        });
    }

    fn start_pairing(&mut self) {
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.pairing = Some(Pairing { pin: None, expires_at: Instant::now() + Duration::from_secs(120), note: None, cancel: Some(cancel_tx) });
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let send = |m| {
                let _ = tx.send(m);
                ctx.request_repaint();
            };
            let mut win = match PairWindow::open().await {
                Ok(w) => w,
                Err(e) => return send(Msg::PairOpened(Err(format!("{e:#}")))),
            };
            send(Msg::PairOpened(Ok((win.pin.clone(), win.expires_in))));
            tokio::pin!(cancel_rx);
            loop {
                tokio::select! {
                    _ = &mut cancel_rx => return, // dropping `win` closes the window on the host
                    ev = win.next() => {
                        let end = !matches!(ev, PairEvent::WrongPin { .. });
                        send(Msg::Pair(ev));
                        if end { return; }
                    }
                }
            }
        });
    }

    fn cancel_pairing(&mut self) {
        if let Some(mut p) = self.pairing.take() {
            if let Some(c) = p.cancel.take() {
                let _ = c.send(());
            }
        }
    }

    fn handle_messages(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Status(s) => {
                    self.refreshing = false;
                    self.checked_once = true;
                    if s.is_some() {
                        self.started_at = None;
                    }
                    self.status = s;
                }
                Msg::Done(r) => {
                    self.busy = None;
                    self.recheck_system();
                    self.last_refresh = None;
                    self.message = Some(match r {
                        Ok(m) => (m, false),
                        Err(e) => (e, true),
                    });
                }
                Msg::PairOpened(Ok((pin, secs))) => {
                    if let Some(p) = &mut self.pairing {
                        p.pin = Some(pin);
                        p.expires_at = Instant::now() + Duration::from_secs(secs);
                    }
                }
                Msg::PairOpened(Err(e)) => {
                    self.pairing = None;
                    self.message = Some((format!("Could not start pairing: {e}"), true));
                }
                Msg::Pair(PairEvent::WrongPin { attempts_left }) => {
                    if let Some(p) = &mut self.pairing {
                        p.note = Some(format!("Wrong code entered on the laptop. {attempts_left} tries left."));
                    }
                }
                Msg::Pair(PairEvent::Paired { name }) => {
                    self.pairing = None;
                    self.message = Some((format!("Paired with {name}. It can connect now."), false));
                    self.last_refresh = None;
                }
                Msg::Pair(PairEvent::Expired) | Msg::Pair(PairEvent::Cancelled) => {
                    if self.pairing.is_some() {
                        self.pairing = None;
                        self.message = Some(("The pairing code expired. Start again if you still need it.".into(), true));
                    }
                }
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.handle_messages();
        if self.last_refresh.is_none_or(|t| t.elapsed() >= REFRESH) {
            self.refresh();
        }
        ui.ctx().request_repaint_after(if self.pairing.is_some() { Duration::from_millis(500) } else { REFRESH });

        heading(ui, "Share this PC", "Let your paired laptops see and control this computer.");
        if let Some((text, err)) = &self.message {
            let (fg, bg) = if *err { (DANGER, DANGER_SOFT) } else { (SUCCESS, SUCCESS_SOFT) };
            egui::Frame::new().fill(bg).corner_radius(10).inner_margin(egui::Margin::symmetric(16, 10)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(text).color(fg));
            });
            ui.add_space(12.0);
        }

        self.status_card(ui);
        ui.add_space(12.0);
        if self.status.is_some() {
            self.setup_card(ui);
            self.pair_card(ui);
            ui.add_space(12.0);
            self.paired_card(ui);
        }
    }

    fn status_card(&mut self, ui: &mut egui::Ui) {
        let running = self.status.is_some();
        let starting = !running && self.started_at.is_some_and(|t| t.elapsed() < Duration::from_secs(8));
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("📡").size(30.0).color(if running { SUCCESS } else { MUTED }));
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(if running { "Remote access is on" } else { "Remote access is off" }).size(20.0).strong().color(TEXT));
                    match &self.status {
                        Some(s) => {
                            let ip = rl_common::discovery::local_ip().map(|i| i.to_string()).unwrap_or_else(|| "this PC".into());
                            ui.label(muted(format!("{}  ·  {ip}  ·  laptops on your network can find it automatically", s.name)).size(13.0));
                        }
                        None if starting => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(muted("Starting…"));
                            });
                        }
                        None if self.enabled && self.checked_once => {
                            ui.label(RichText::new("The service is enabled but not running.").color(WARN).size(13.0));
                        }
                        None => {
                            ui.label(muted("Turn it on to pair a laptop. It starts automatically when you log in.").size(13.0));
                        }
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(b) = self.busy {
                        ui.spinner();
                        ui.label(muted(b));
                    } else if running {
                        if secondary(ui, "Turn off").clicked() {
                            self.cancel_pairing();
                            self.run("Turning off…", || hostctl::disable_service().map(|_| "Remote access turned off.".into()));
                        }
                    } else {
                        let r = primary(ui, "Turn on", self.host_bin.is_some());
                        match self.host_bin.clone() {
                            None => {
                                r.on_disabled_hover_text("rl-host is not installed next to this app");
                            }
                            Some(bin) if r.clicked() => {
                                self.started_at = Some(Instant::now());
                                self.run("Turning on…", move || hostctl::enable_service(&bin).map(|_| "Remote access is on.".into()));
                            }
                            Some(_) => {}
                        }
                    }
                });
            });
            if self.host_bin.is_none() {
                ui.label(RichText::new("rl-host was not found next to this app or on PATH. Reinstall RemoteLinux with the full package.").color(DANGER));
            }
            if !running && self.enabled && self.checked_once && !starting {
                let log = hostctl::service_log(6);
                if !log.trim().is_empty() {
                    ui.separator();
                    ui.label(muted("Recent log:").size(13.0));
                    ui.label(RichText::new(log.trim()).monospace().size(11.0).color(MUTED));
                }
            }
            if let Some(c) = self.status.as_ref().and_then(|s| s.client.clone()) {
                ui.separator();
                ui.horizontal(|ui| {
                    dot(ui, SUCCESS);
                    ui.label(RichText::new(format!("{} is connected", c.name)).strong().color(SUCCESS));
                    ui.label(
                        muted(format!(
                            "{}  ·  {} {}×{} @ {} fps  ·  {:.1} Mbps  ·  {:.1} ms",
                            c.address.split(':').next().unwrap_or(""),
                            c.monitor.as_deref().unwrap_or(""),
                            c.width,
                            c.height,
                            c.fps,
                            c.kbps as f64 / 1000.0,
                            c.rtt_ms
                        ))
                        .size(13.0),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if danger(ui, "Disconnect").clicked() {
                            self.rt.spawn(async { hostctl::disconnect_client().await });
                            self.last_refresh = None;
                        }
                    });
                });
            }
        });
    }

    /// One-time system setup that needs an admin password.
    fn setup_card(&mut self, ui: &mut egui::Ui) {
        let need_input = !self.uinput_ok;
        let need_fw = self.ufw && !self.firewall_opened;
        if !need_input && !need_fw {
            return;
        }
        card(ui, |ui| {
            ui.label(RichText::new("Finish setup").strong().size(16.0).color(WARN));
            ui.label(muted("These need your password once.").size(13.0));
            ui.add_space(6.0);
            if need_input {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("⌨").color(WARN));
                    ui.label("Allow RemoteLinux to type and move the mouse on this PC");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary(ui, "Allow", self.busy.is_none()).clicked() {
                            self.run("Waiting for password…", || {
                                hostctl::fix_uinput()?;
                                // The daemon created its input devices at startup: restart it.
                                if let Some(b) = hostctl::host_binary() {
                                    hostctl::enable_service(&b)?;
                                }
                                Ok("Input access granted.".into())
                            });
                        }
                    });
                });
            }
            if need_fw {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("🛡").color(WARN));
                    ui.label("Let laptops on your home network through the firewall (ufw)");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary(ui, "Allow", self.busy.is_none()).clicked() {
                            self.run("Waiting for password…", || hostctl::open_firewall().map(|_| "Firewall opened for your local network.".into()));
                        }
                    });
                });
            }
        });
        ui.add_space(12.0);
    }

    fn pair_card(&mut self, ui: &mut egui::Ui) {
        let name = self.status.as_ref().map(|s| s.name.clone()).unwrap_or_default();
        let mut cancel = false;
        let mut start = false;
        card(ui, |ui| match &self.pairing {
            None => {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Pair a laptop").strong().size(18.0).color(TEXT));
                        ui.label(muted("Shows a code to type on the laptop. You only do this once per laptop.").size(13.0));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary(ui, "Show pairing code", true).clicked() {
                            start = true;
                        }
                    });
                });
            }
            Some(p) => {
                ui.vertical_centered(|ui| {
                    match &p.pin {
                        None => {
                            ui.spinner();
                        }
                        Some(pin) => {
                            ui.label(muted("On the laptop, open RemoteLinux → Pair new computer, choose"));
                            ui.label(RichText::new(&name).strong().color(TEXT));
                            ui.label(muted("and type this code:"));
                            ui.add_space(8.0);
                            ui.label(RichText::new(format!("{} {}", &pin[..3], &pin[3..])).monospace().size(56.0).strong().color(ACCENT));
                            let left = p.expires_at.saturating_duration_since(Instant::now()).as_secs();
                            ui.label(muted(format!("Waiting for the laptop…  code expires in {}:{:02}", left / 60, left % 60)).size(13.0));
                        }
                    }
                    if let Some(n) = &p.note {
                        ui.label(RichText::new(n).color(WARN));
                    }
                    ui.add_space(8.0);
                    if secondary(ui, "Cancel").clicked() {
                        cancel = true;
                    }
                });
            }
        });
        if start {
            self.message = None;
            self.start_pairing();
        }
        if cancel {
            self.cancel_pairing();
        }
    }

    fn paired_card(&mut self, ui: &mut egui::Ui) {
        let paired = self.status.as_ref().map(|s| s.paired.clone()).unwrap_or_default();
        card(ui, |ui| {
            ui.label(RichText::new("Paired laptops").strong().size(16.0).color(TEXT));
            if paired.is_empty() {
                ui.label(muted("None yet."));
            }
            for c in paired {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("💻").color(MUTED));
                    ui.label(RichText::new(&c.name).strong().color(TEXT));
                    ui.label(muted(&c.fingerprint_short).monospace().size(12.0));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if self.confirm_remove.as_deref() == Some(&c.name) {
                            if danger(ui, "Remove").clicked() {
                                let name = c.name.clone();
                                self.confirm_remove = None;
                                self.rt.spawn(async move { hostctl::unpair(&name).await });
                                self.last_refresh = None;
                            }
                            if secondary(ui, "Keep").clicked() {
                                self.confirm_remove = None;
                            }
                        } else if ui.button("Remove…").clicked() {
                            self.confirm_remove = Some(c.name.clone());
                        }
                    });
                });
            }
        });
    }
}
