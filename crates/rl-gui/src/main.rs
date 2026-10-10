//! RemoteLinux GUI for the laptop: pick a paired desktop and connect, pair a
//! new one, and change stream settings. The viewer itself is the
//! `rl-client connect --json` process (see `session.rs`).

mod session;
mod theme;

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Layout, RichText};
use rl_common::client::ProbeResult;
use rl_common::config::ClientConfig;
use rl_common::trust::{KnownHost, KnownHosts};
use serde_json::{json, Value};

use session::{Session, State};
use theme::*;

const PROBE_EVERY: Duration = Duration::from_secs(10);

pub enum Msg {
    Probed(Vec<ProbeResult>),
    Paired(Result<KnownHost, String>),
    Viewer(Value),
    ViewerExited,
}

#[derive(PartialEq, Clone, Copy)]
enum Screen {
    Home,
    Pair,
    Settings,
}

/// Things a click asks for, applied after drawing (avoids borrowing `self` twice).
enum Action {
    Go(Screen),
    Connect(String),
    Forget(String),
    AskForget(Option<String>),
    Viewer(&'static str),
    Disconnect,
    Probe,
}

struct App {
    rt: tokio::runtime::Runtime,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    screen: Screen,
    hosts: Vec<KnownHost>,
    probes: HashMap<String, ProbeResult>,
    probing: bool,
    last_probe: Option<Instant>,
    cfg: ClientConfig,
    /// Settings screen edits, saved on "Save".
    draft: ClientConfig,
    pair_addr: String,
    pair_pin: String,
    pairing: bool,
    pair_error: Option<String>,
    session: Option<Session>,
    /// Message at the top of the home screen: (text, is_error).
    banner: Option<(String, bool)>,
    confirm_forget: Option<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        theme::apply(&cc.egui_ctx);
        let (tx, rx) = channel();
        let mut banner = None;
        let cfg = ClientConfig::load().unwrap_or_else(|e| {
            banner = Some((format!("Could not read client.toml: {e:#}"), true));
            ClientConfig::default()
        });
        let mut app = Self {
            rt: tokio::runtime::Runtime::new().expect("tokio runtime"),
            tx,
            rx,
            ctx: cc.egui_ctx.clone(),
            screen: Screen::Home,
            hosts: Vec::new(),
            probes: HashMap::new(),
            probing: false,
            last_probe: None,
            draft: cfg.clone(),
            cfg,
            pair_addr: String::new(),
            pair_pin: String::new(),
            pairing: false,
            pair_error: None,
            session: None,
            banner,
            confirm_forget: None,
        };
        app.reload_hosts();
        if app.hosts.is_empty() {
            app.screen = Screen::Pair;
        }
        app
    }

    fn reload_hosts(&mut self) {
        match KnownHosts::load() {
            Ok(kh) => {
                self.hosts = kh.hosts;
                // Most recently used first.
                self.hosts.sort_by_key(|h| std::cmp::Reverse(h.last_seen.unwrap_or(h.paired_at)));
            }
            Err(e) => self.banner = Some((format!("Could not read known_hosts.toml: {e:#}"), true)),
        }
    }

    fn probe(&mut self) {
        if self.probing || self.hosts.is_empty() {
            return;
        }
        self.probing = true;
        self.last_probe = Some(Instant::now());
        let (hosts, tx, ctx) = (self.hosts.clone(), self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let r = rl_common::client::probe(&hosts, Duration::from_secs(2)).await;
            let _ = tx.send(Msg::Probed(r));
            ctx.request_repaint();
        });
    }

    fn start_pairing(&mut self) {
        self.pairing = true;
        self.pair_error = None;
        let (addr, pin, name) = (self.pair_addr.trim().to_string(), self.pair_pin.clone(), self.cfg.device_name());
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let r = rl_common::client::pair(&addr, &pin, &name).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Paired(r));
            ctx.request_repaint();
        });
    }

    fn save_settings(&mut self) {
        match self.draft.save() {
            Ok(()) => {
                self.cfg = self.draft.clone();
                // Apply stream settings to a running session straight away. The
                // viewer treats "native" / 0 as "back to the host default".
                if let Some(s) = &mut self.session {
                    let c = &self.cfg;
                    let res = if c.resolution.is_empty() { "native" } else { c.resolution.as_str() };
                    s.send(json!({ "cmd": "settings", "resolution": res, "fps": c.fps, "bitrate_kbps": c.bitrate_kbps }));
                }
                self.banner = Some(("Settings saved.".into(), false));
                self.screen = Screen::Home;
            }
            Err(e) => self.banner = Some((format!("Could not save settings: {e:#}"), true)),
        }
    }

    fn handle_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Probed(r) => {
                    self.probing = false;
                    self.probes = r.into_iter().map(|p| (p.name.clone(), p)).collect();
                }
                Msg::Paired(Ok(host)) => {
                    self.pairing = false;
                    self.pair_pin.clear();
                    self.pair_addr.clear();
                    self.banner = Some((format!("Paired with {}. You can connect now.", host.name), false));
                    self.screen = Screen::Home;
                    self.reload_hosts();
                    self.last_probe = None;
                }
                Msg::Paired(Err(e)) => {
                    self.pairing = false;
                    self.pair_error = Some(e);
                }
                Msg::Viewer(v) => {
                    if let Some(s) = &mut self.session {
                        s.handle(&v);
                    }
                }
                Msg::ViewerExited => {
                    if let Some(s) = self.session.take() {
                        self.banner = match &s.state {
                            State::Failed(m) => Some((format!("Could not connect to {}: {m}", s.host), true)),
                            _ if !s.ever_connected => Some((format!("The viewer for {} closed before connecting.", s.host), true)),
                            _ => None,
                        };
                    }
                    self.reload_hosts();
                    self.last_probe = None;
                }
            }
        }
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::Go(s) => {
                if s == Screen::Settings {
                    self.draft = self.cfg.clone();
                }
                self.screen = s;
            }
            Action::Connect(name) => {
                self.banner = None;
                match Session::start(&name, &self.ctx, self.tx.clone()) {
                    Ok(s) => self.session = Some(s),
                    Err(e) => self.banner = Some((format!("Could not start rl-client: {e}. Is it installed?"), true)),
                }
            }
            Action::Forget(name) => {
                self.confirm_forget = None;
                let res = KnownHosts::load().and_then(|mut kh| {
                    kh.remove(&name);
                    kh.save()
                });
                match res {
                    Ok(()) => self.banner = Some((format!("Forgot {name}. Pair again to reconnect."), false)),
                    Err(e) => self.banner = Some((format!("Could not forget {name}: {e:#}"), true)),
                }
                self.reload_hosts();
            }
            Action::AskForget(name) => self.confirm_forget = name,
            Action::Viewer(cmd) => {
                if let Some(s) = &mut self.session {
                    s.command(cmd);
                }
            }
            Action::Disconnect => self.session = None, // Drop disconnects cleanly.
            Action::Probe => {
                self.last_probe = None;
                self.probe();
            }
        }
    }

    // ---------- Screens ----------

    fn home(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let mut action = None;
        ui.horizontal(|ui| {
            ui.vertical(|ui| heading(ui, "Your computers", "Pick a computer to control from this laptop."));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if secondary(ui, "+  Pair new computer").clicked() {
                    action = Some(Action::Go(Screen::Pair));
                }
                if ui.add_enabled(!self.probing, egui::Button::new("⟳").frame(false)).on_hover_text("Check which computers are online").clicked() {
                    action = Some(Action::Probe);
                }
            });
        });

        if self.hosts.is_empty() {
            card(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No computers yet").size(20.0).strong());
                    ui.label(muted("Pair your desktop once, then connect with one click."));
                    ui.add_space(8.0);
                    if primary(ui, "Pair a computer", true).clicked() {
                        action = Some(Action::Go(Screen::Pair));
                    }
                });
            });
        }

        for (i, host) in self.hosts.iter().enumerate() {
            if let Some(a) = self.host_card(ui, host, i == 0) {
                action = Some(a);
            }
            ui.add_space(12.0);
        }

        ui.add_space(4.0);
        egui::Frame::new().fill(SURFACE).corner_radius(12).inner_margin(egui::Margin::symmetric(20, 14)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("🔑").color(WARN));
                ui.label(muted("While connected, every key goes to the remote computer. Press"));
                ui.label(RichText::new(&self.cfg.release_chord).monospace().background_color(SURFACE2));
                ui.label(muted("to release it."));
            });
        });
        action
    }

    fn host_card(&self, ui: &mut egui::Ui, host: &KnownHost, first: bool) -> Option<Action> {
        let mut action = None;
        let session = self.session.as_ref().filter(|s| s.host == host.name);
        let busy_elsewhere = self.session.is_some() && session.is_none();
        let probe = self.probes.get(&host.name);
        let online = probe.is_some_and(|p| p.online);

        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("🖥").size(30.0).color(if online || session.is_some() { ACCENT } else { MUTED }));
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(&host.name).size(22.0).strong().color(TEXT));
                    ui.horizontal(|ui| {
                        match probe {
                            _ if session.is_some() => pill(ui, "In use", ACCENT, SURFACE2),
                            Some(p) if p.online => pill(ui, &format!("Online · {:.0} ms", p.rtt_ms.unwrap_or_default()), SUCCESS, SUCCESS_SOFT),
                            Some(_) => pill(ui, "Offline", MUTED, SURFACE2),
                            None if self.probing => pill(ui, "Checking…", MUTED, SURFACE2),
                            None => {}
                        }
                        let seen = match host.last_seen {
                            Some(t) => format!("last used {}", ago(t)),
                            None => format!("paired {}", ago(host.paired_at)),
                        };
                        ui.label(muted(format!("{}  ·  {seen}", host.address)).size(13.0));
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if session.is_some() {
                        if danger(ui, "Disconnect").clicked() {
                            action = Some(Action::Disconnect);
                        }
                    } else {
                        let label = if online { "▶  Connect" } else { "Connect" };
                        let r = primary(ui, label, !busy_elsewhere);
                        let r = if busy_elsewhere { r.on_disabled_hover_text("Disconnect the other computer first") } else { r };
                        if r.clicked() {
                            action = Some(Action::Connect(host.name.clone()));
                        }
                        ui.menu_button(RichText::new("…").size(20.0).color(MUTED), |ui| {
                            if ui.button("Forget this computer…").clicked() {
                                action = Some(Action::AskForget(Some(host.name.clone())));
                                ui.close();
                            }
                        });
                    }
                });
            });

            if let Some(s) = session {
                ui.separator();
                if let Some(a) = session_panel(ui, s) {
                    action = Some(a);
                }
            } else if self.confirm_forget.as_deref() == Some(&host.name) {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Forget {}? You will need to pair again.", host.name)).color(TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if secondary(ui, "Cancel").clicked() {
                            action = Some(Action::AskForget(None));
                        }
                        if danger(ui, "Forget").clicked() {
                            action = Some(Action::Forget(host.name.clone()));
                        }
                    });
                });
            } else if first {
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    let c = &self.cfg;
                    for (k, v) in [
                        ("Resolution", if c.resolution.is_empty() { "Default".into() } else { c.resolution.clone() }),
                        ("Frame rate", if c.fps == 0 { "Default".into() } else { format!("{} fps", c.fps) }),
                        ("Quality", if c.bitrate_kbps == 0 { "Default".into() } else { format!("{} Mbps", c.bitrate_kbps / 1000) }),
                        ("Fullscreen", if c.fullscreen { "On".into() } else { "Off".into() }),
                    ] {
                        ui.label(muted(format!("{k}:")).size(13.0));
                        ui.label(RichText::new(v).strong().size(13.0));
                        ui.add_space(10.0);
                    }
                    if ui.link("Change…").clicked() {
                        action = Some(Action::Go(Screen::Settings));
                    }
                });
            }
        });
        action
    }

    fn pair(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let mut action = None;
        if !self.hosts.is_empty() && ui.button("‹ Back").clicked() {
            action = Some(Action::Go(Screen::Home));
        }
        heading(ui, "Pair a new computer", "You only need to do this once. After pairing, connecting takes one click.");
        card(ui, |ui| {
            step(ui, "1", "On the desktop, run  rl-host pair  in a terminal.", "It shows a 6-digit code that works for 2 minutes.");
            ui.add_space(12.0);
            step(ui, "2", "Type the desktop's name or IP address.", "");
            ui.add_enabled(
                !self.pairing,
                egui::TextEdit::singleline(&mut self.pair_addr).hint_text("desktop.local or 192.168.1.20").desired_width(f32::INFINITY).margin(egui::vec2(12.0, 10.0)),
            );
            ui.add_space(12.0);
            step(ui, "3", "Enter the code shown on the desktop.", "");
            let pin = ui.add_enabled(
                !self.pairing,
                egui::TextEdit::singleline(&mut self.pair_pin)
                    .hint_text(RichText::new("000000").monospace().size(28.0))
                    .char_limit(6)
                    .font(egui::FontId::monospace(28.0))
                    .desired_width(180.0)
                    .margin(egui::vec2(14.0, 10.0)),
            );
            if pin.changed() {
                self.pair_pin.retain(|c| c.is_ascii_digit());
            }
            let ready = !self.pair_addr.trim().is_empty() && self.pair_pin.len() == 6 && !self.pairing;
            ui.add_space(8.0);
            if let Some(e) = &self.pair_error {
                ui.label(RichText::new(e).color(DANGER));
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("🛡").color(SUCCESS));
                ui.label(muted("Encrypted. Only paired devices can connect.").size(13.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let enter = ready && pin.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if primary(ui, "Pair", ready).clicked() || enter {
                        self.start_pairing();
                    }
                    if self.pairing {
                        ui.spinner();
                        ui.label(muted("Pairing…"));
                    }
                });
            });
        });
        action
    }

    fn settings(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let mut action = None;
        if ui.button("‹ Back").clicked() {
            action = Some(Action::Go(Screen::Home));
        }
        heading(ui, "Settings", "Lower settings feel faster. Picture sharpness matters least.");
        let d = &mut self.draft;
        card(ui, |ui| {
            row(ui, "Resolution", "720p is fastest. Use Native only on a wired connection.", |ui| {
                segmented(ui, &mut d.resolution, &[
                    ("".into(), "Default"),
                    ("720p".into(), "720p"),
                    ("900p".into(), "900p"),
                    ("1080p".into(), "1080p"),
                    ("native".into(), "Native"),
                ]);
            });
            row(ui, "Frame rate", "Follows the desktop monitor, up to this limit.", |ui| {
                segmented(ui, &mut d.fps, &[(0, "Default"), (30, "30"), (60, "60"), (120, "120")]);
            });
            row(ui, "Quality", "Lower this if Wi-Fi stutters.", |ui| {
                segmented(ui, &mut d.bitrate_kbps, &[(0, "Default"), (5_000, "5 Mbps"), (10_000, "10"), (20_000, "20"), (40_000, "40")]);
            });
            row(ui, "Start in fullscreen", "Toggle any time with Ctrl+Alt+Shift+F.", |ui| {
                ui.checkbox(&mut d.fullscreen, "");
            });
            row(ui, "Show stats overlay", "Latency, frame rate and dropped frames in the corner.", |ui| {
                ui.checkbox(&mut d.show_stats, "");
            });
            row(ui, "Video decoder", "Auto uses your GPU when it can.", |ui| {
                egui::ComboBox::from_id_salt("decoder").selected_text(decoder_label(&d.decoder)).show_ui(ui, |ui| {
                    for v in ["auto", "vaapi", "software"] {
                        ui.selectable_value(&mut d.decoder, v.to_string(), decoder_label(v));
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("This laptop's name").strong().color(TEXT));
                    ui.label(muted("Shown on the desktop when pairing.").size(13.0));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add(egui::TextEdit::singleline(&mut d.name).hint_text(rl_common::hostname()).desired_width(220.0));
                });
            });
        });
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            if secondary(ui, "Reset to defaults").clicked() {
                self.draft = ClientConfig { name: self.draft.name.clone(), ..Default::default() };
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if primary(ui, "Save", true).clicked() {
                    self.save_settings();
                }
            });
        });
        action
    }
}

/// Live status of a running viewer, shown inside its host card.
fn session_panel(ui: &mut egui::Ui, s: &Session) -> Option<Action> {
    let mut action = None;
    ui.horizontal(|ui| {
        match &s.state {
            State::Starting => {
                ui.spinner();
                ui.label("Opening viewer…");
            }
            State::Connecting { attempt } => {
                ui.spinner();
                ui.label(if *attempt > 1 { format!("Connecting… (try {attempt})") } else { "Connecting…".into() });
            }
            State::Connected => {
                dot(ui, SUCCESS);
                ui.label(RichText::new("Connected").color(SUCCESS).strong());
                if let Some(st) = &s.stats {
                    let lat = st.latency_ms.map(|l| format!("{l:.0} ms latency")).unwrap_or_else(|| format!("{:.0} ms round trip", st.rtt_ms));
                    ui.label(muted(format!("{lat}  ·  {:.0} fps  ·  {:.1} Mbps  ·  {} dropped", st.fps, st.kbps / 1000.0, st.dropped_total)).size(13.0));
                }
            }
            State::Reconnecting { retry_at, reason } => {
                ui.spinner();
                let secs = retry_at.saturating_duration_since(Instant::now()).as_secs_f32();
                ui.label(RichText::new(format!("Reconnecting in {secs:.0} s…")).color(WARN));
                ui.label(muted(reason).size(13.0));
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            }
            State::Disconnected { reason } => {
                ui.label(RichText::new("Disconnected").color(WARN));
                ui.label(muted(format!("{reason}. Held keys were released on the desktop.")).size(13.0));
            }
            State::Failed(m) => {
                ui.label(RichText::new(m).color(DANGER));
            }
        }
    });
    if let Some(n) = &s.notice {
        ui.label(muted(n).size(13.0));
    }
    if matches!(s.state, State::Connected) {
        ui.horizontal(|ui| {
            if !s.forwarding && primary(ui, "Resume control", true).clicked() {
                action = Some(Action::Viewer("resume"));
            }
            if secondary(ui, if s.stats_visible { "Hide stats" } else { "Show stats" }).clicked() {
                action = Some(Action::Viewer("toggle_stats"));
            }
            if secondary(ui, if s.fullscreen { "Exit fullscreen" } else { "Fullscreen" }).clicked() {
                action = Some(Action::Viewer("fullscreen"));
            }
        });
    }
    action
}

fn step(ui: &mut egui::Ui, n: &str, title: &str, hint: &str) {
    ui.horizontal(|ui| {
        egui::Frame::new().fill(SURFACE2).corner_radius(14).inner_margin(egui::Margin::symmetric(9, 4)).show(ui, |ui| {
            ui.label(RichText::new(n).color(ACCENT).strong());
        });
        ui.vertical(|ui| {
            ui.label(RichText::new(title).strong().color(TEXT));
            if !hint.is_empty() {
                ui.label(muted(hint).size(13.0));
            }
        });
    });
}

/// One settings line: title and hint on the left, control on the right.
fn row(ui: &mut egui::Ui, title: &str, hint: &str, control: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).strong().color(TEXT));
            ui.label(muted(hint).size(13.0));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), control);
    });
    ui.separator();
}

fn decoder_label(v: &str) -> &'static str {
    match v {
        "vaapi" => "GPU (VA-API)",
        "software" => "Software",
        _ => "Auto",
    }
}

fn ago(unix: u64) -> String {
    let secs = rl_common::unix_now().saturating_sub(unix);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        86_400..172_800 => "yesterday".into(),
        _ => format!("{} days ago", secs / 86_400),
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_messages();
        if self.screen == Screen::Home && self.session.is_none() && self.last_probe.is_none_or(|t| t.elapsed() >= PROBE_EVERY) {
            self.probe();
        }
        ctx.request_repaint_after(PROBE_EVERY);

        egui::Panel::top("bar").frame(egui::Frame::new().fill(SURFACE).inner_margin(egui::Margin::symmetric(16, 10))).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("🖥").color(ACCENT).size(18.0));
                ui.label(RichText::new("RemoteLinux").strong().color(TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if self.screen != Screen::Settings && ui.add(egui::Button::new(RichText::new("⚙").size(18.0).color(MUTED)).frame(false)).on_hover_text("Settings").clicked() {
                        self.apply(Action::Go(Screen::Settings));
                    }
                });
            });
        });

        let mut action = None;
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                column(ui, 720.0, |ui| {
                    if let Some((text, err)) = &self.banner {
                        let (fg, bg) = if *err { (DANGER, DANGER_SOFT) } else { (SUCCESS, SUCCESS_SOFT) };
                        ui.add_space(16.0);
                        let close = egui::Frame::new().fill(bg).corner_radius(10).inner_margin(egui::Margin::symmetric(16, 10)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(text).color(fg));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.add(egui::Button::new(RichText::new("✕").color(fg)).frame(false)).clicked()).inner
                            })
                            .inner
                        });
                        if close.inner {
                            self.banner = None;
                        }
                    }
                    action = match self.screen {
                        Screen::Home => self.home(ui),
                        Screen::Pair => self.pair(ui),
                        Screen::Settings => self.settings(ui),
                    };
                    ui.add_space(24.0);
                });
            });
        });
        if let Some(a) = action {
            self.apply(a);
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RemoteLinux")
            .with_app_id("remotelinux")
            .with_inner_size([860.0, 640.0])
            .with_min_inner_size([560.0, 460.0]),
        ..Default::default()
    };
    eframe::run_native("RemoteLinux", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
