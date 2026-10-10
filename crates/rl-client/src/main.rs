mod decode;
mod events;
mod keymap;
mod net;
mod bench;
mod viewer;

use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use rl_common::config::{parse_resolution, ClientConfig};
use rl_common::identity::short_fingerprint;
use rl_common::proto::StreamSettings;
use rl_common::trust::{KnownHost, KnownHosts};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(name = "rl-client", version, about = "RemoteLinux client (run on the laptop)")]
struct Cli {
    /// Print machine-readable JSON (one object per line).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pair with a desktop running `rl-host pair`.
    Pair {
        /// Desktop address: IP, hostname, or host:port.
        host: String,
        /// The 6-digit PIN shown on the desktop (prompted if omitted).
        #[arg(long)]
        pin: Option<String>,
        /// Name this laptop presents to the desktop.
        #[arg(long)]
        name: Option<String>,
    },
    /// Connect to a paired desktop and open the viewer.
    Connect {
        /// Host name from `rl-client list` (default: config default_host, or the only host).
        host: Option<String>,
        /// Encoded resolution: native, 720p, 900p, 1080p.
        #[arg(long)]
        res: Option<String>,
        #[arg(long)]
        fps: Option<u32>,
        /// Bitrate, e.g. 15M or 8000k.
        #[arg(long)]
        bitrate: Option<String>,
        /// Monitor to stream, e.g. DP-1.
        #[arg(long)]
        monitor: Option<String>,
        #[arg(long)]
        fullscreen: bool,
        /// Show the stats overlay from the start.
        #[arg(long)]
        stats: bool,
        /// Watch only: never forward keyboard or mouse input.
        #[arg(long)]
        view_only: bool,
    },
    /// List paired desktops.
    List {
        /// Also check which are online.
        #[arg(long)]
        probe: bool,
    },
    /// Check whether a paired desktop is reachable.
    Probe { host: Option<String> },
    /// Forget a paired desktop.
    Forget { host: String },
    /// Connect without a window, decode, and report latency statistics.
    Bench {
        host: Option<String>,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        #[arg(long)]
        res: Option<String>,
    },
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("RL_LOG").unwrap_or_else(|_| "info,quinn=warn".into()))
        .with_writer(std::io::stderr)
        .init();
}

fn main() {
    let cli = Cli::parse();
    events::set_json(cli.json);
    init_logging();
    if let Err(e) = real_main(cli) {
        if events::json() {
            println!("{}", json!({ "event": "error", "message": format!("{e:#}"), "fatal": true }));
        } else {
            eprintln!("error: {e:#}");
        }
        std::process::exit(1);
    }
}

fn real_main(cli: Cli) -> Result<()> {
    let cfg = ClientConfig::load()?;
    match cli.cmd {
        Cmd::Pair { host, pin, name } => pair(&cfg, &host, pin, name),
        Cmd::Connect { host, res, fps, bitrate, monitor, fullscreen, stats, view_only } => {
            let target = pick_host(&cfg, host.as_deref())?;
            let res = res.or_else(|| Some(cfg.resolution.clone()).filter(|s| !s.is_empty()));
            let settings = StreamSettings {
                max_height: match res {
                    Some(r) => parse_resolution(&r)?,
                    None => None,
                },
                fps: fps.or((cfg.fps > 0).then_some(cfg.fps)),
                bitrate_kbps: match bitrate {
                    Some(b) => Some(parse_bitrate(&b)?),
                    None => (cfg.bitrate_kbps > 0).then_some(cfg.bitrate_kbps),
                },
                monitor: monitor.or_else(|| Some(cfg.monitor.clone()).filter(|s| !s.is_empty())),
                show_cursor: None,
            };
            let commands = spawn_stdin_commands();
            viewer::run(
                viewer::Options {
                    host: target,
                    settings,
                    client_name: cfg.device_name(),
                    fullscreen: fullscreen || cfg.fullscreen,
                    show_stats: stats || cfg.show_stats,
                    view_only,
                    release_chord: cfg.release_chord.clone(),
                },
                commands,
            )
        }
        Cmd::List { probe } => list(probe),
        Cmd::Bench { host, seconds, res } => {
            let h = pick_host(&cfg, host.as_deref())?;
            let max_height = match res {
                Some(r) => parse_resolution(&r)?,
                None => None,
            };
            bench::run(h, StreamSettings { max_height, ..Default::default() }, cfg.device_name(), Duration::from_secs(seconds))
        }
        Cmd::Probe { host } => {
            let h = pick_host(&cfg, host.as_deref())?;
            let res = probe_hosts(std::slice::from_ref(&h)).pop().unwrap();
            if events::json() {
                println!("{}", serde_json::to_value(&res)?);
            } else if res.online {
                println!("{} is online ({:.1} ms)", h.name, res.rtt_ms.unwrap_or(0.0));
            } else {
                println!("{} is offline: {}", h.name, res.error.as_deref().unwrap_or(""));
            }
            if !res.online {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Forget { host } => {
            let mut kh = KnownHosts::load()?;
            let removed = kh.remove(&host);
            if removed {
                kh.save()?;
            }
            if events::json() {
                println!("{}", json!({ "removed": removed, "host": host }));
            } else if removed {
                println!("Forgot {host}.");
            } else {
                bail!("no paired host named {host}");
            }
            Ok(())
        }
    }
}

fn parse_bitrate(s: &str) -> Result<u32> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mul) = if let Some(n) = s.strip_suffix('m') {
        (n, 1000.0)
    } else if let Some(n) = s.strip_suffix('k') {
        (n, 1.0)
    } else {
        (s.as_str(), 1.0)
    };
    let v: f64 = num.parse().with_context(|| format!("invalid bitrate {s:?}"))?;
    Ok((v * mul) as u32)
}

fn pick_host(cfg: &ClientConfig, name: Option<&str>) -> Result<KnownHost> {
    let kh = KnownHosts::load()?;
    let want = name.map(str::to_string).or_else(|| Some(cfg.default_host.clone()).filter(|s| !s.is_empty()));
    match want {
        Some(n) => kh.find(&n).cloned().ok_or_else(|| anyhow!("no paired host named {n:?}; see `rl-client list`")),
        None => match kh.hosts.as_slice() {
            [] => bail!("no paired hosts; run `rl-host pair` on the desktop, then `rl-client pair <desktop-ip>`"),
            [one] => Ok(one.clone()),
            many => {
                // Most recently used.
                Ok(many.iter().max_by_key(|h| h.last_seen.unwrap_or(h.paired_at)).unwrap().clone())
            }
        },
    }
}

/// JSON commands on stdin, for a UI driving the viewer. Only enabled when
/// stdin is not a terminal.
fn spawn_stdin_commands() -> Option<std::sync::mpsc::Receiver<viewer::Command>> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            let cmd = match v["cmd"].as_str().unwrap_or("") {
                "toggle_stats" => viewer::Command::ToggleStats,
                "fullscreen" => viewer::Command::Fullscreen,
                "release" => viewer::Command::Release,
                "resume" => viewer::Command::Resume,
                "relative_mouse" => viewer::Command::Relative,
                "disconnect" => viewer::Command::Disconnect,
                // Partial update: only keys present change; "native", 0 or ""
                // reset a field to the host default.
                "settings" => viewer::Command::Settings(viewer::SettingsPatch {
                    max_height: v.get("resolution").map(|r| r.as_str().and_then(|r| parse_resolution(r).ok().flatten())),
                    fps: v.get("fps").map(|x| x.as_u64().filter(|&n| n > 0).map(|n| n as u32)),
                    bitrate_kbps: v.get("bitrate_kbps").map(|x| x.as_u64().filter(|&n| n > 0).map(|n| n as u32)),
                    monitor: v.get("monitor").map(|m| m.as_str().filter(|m| !m.is_empty()).map(str::to_string)),
                }),
                other => {
                    tracing::warn!("unknown stdin command {other:?}");
                    continue;
                }
            };
            if tx.send(cmd).is_err() {
                return;
            }
        }
        // The controlling process went away: close the viewer too.
        let _ = tx.send(viewer::Command::Disconnect);
    });
    Some(rx)
}

fn pair(cfg: &ClientConfig, host: &str, pin: Option<String>, name: Option<String>) -> Result<()> {
    let pin = match pin {
        Some(p) => p,
        None => {
            if events::json() {
                bail!("--pin is required with --json");
            }
            print!("Enter the PIN shown on the desktop: ");
            std::io::stdout().flush()?;
            let mut s = String::new();
            std::io::stdin().read_line(&mut s)?;
            s
        }
    };
    let name = name.unwrap_or_else(|| cfg.device_name());
    let rt = tokio::runtime::Runtime::new()?;
    let h = rt.block_on(rl_common::client::pair(host, &pin, &name))?;
    if events::json() {
        println!("{}", json!({ "paired": true, "host": h.name, "address": h.address, "fingerprint": h.fingerprint }));
    } else {
        println!("Paired with {} ({}). Connect with:  rl-client connect {}", h.name, short_fingerprint(&h.fingerprint), h.name);
    }
    Ok(())
}

fn probe_hosts(hosts: &[KnownHost]) -> Vec<rl_common::client::ProbeResult> {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(rl_common::client::probe(hosts, Duration::from_millis(1500)))
}

fn list(probe: bool) -> Result<()> {
    let kh = KnownHosts::load()?;
    let probes = if probe { Some(probe_hosts(&kh.hosts)) } else { None };
    let rows: Vec<Value> = kh
        .hosts
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let mut v = json!({
                "name": h.name,
                "address": h.address,
                "fingerprint": h.fingerprint,
                "fingerprint_short": short_fingerprint(&h.fingerprint),
                "paired_at": h.paired_at,
                "last_seen": h.last_seen,
            });
            if let Some(p) = &probes {
                v["online"] = json!(p[i].online);
                v["rtt_ms"] = json!(p[i].rtt_ms);
            }
            v
        })
        .collect();
    if events::json() {
        println!("{}", json!({ "hosts": rows }));
        return Ok(());
    }
    if rows.is_empty() {
        println!("No paired hosts. Run `rl-host pair` on the desktop, then `rl-client pair <desktop-ip>`.");
    }
    for r in rows {
        let status = match r["online"].as_bool() {
            Some(true) => format!("  online ({} ms)", r["rtt_ms"]),
            Some(false) => "  offline".to_string(),
            None => String::new(),
        };
        println!("{:<20} {:<24} {}{status}", r["name"].as_str().unwrap_or(""), r["address"].as_str().unwrap_or(""), r["fingerprint_short"].as_str().unwrap_or(""));
    }
    Ok(())
}
