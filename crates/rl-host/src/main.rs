use std::sync::Arc;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use rl_common::config::HostConfig;
use rl_common::trust::AuthorizedClients;
use rl_host::{control, server::Host};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Parser)]
#[command(name = "rl-host", version, about = "RemoteLinux host daemon (run on the desktop)")]
struct Cli {
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in the foreground (default).
    Run {
        /// Override the listen address, e.g. 0.0.0.0:47800.
        #[arg(long)]
        listen: Option<String>,
    },
    /// Show a PIN and wait for a laptop to pair.
    Pair,
    /// List paired clients.
    List,
    /// Remove a paired client by name.
    Unpair { name: String },
    /// Show daemon status.
    Status,
    /// Disconnect the current client.
    Disconnect,
    /// List monitors that can be streamed.
    Monitors,
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("RL_LOG").unwrap_or_else(|_| "info,quinn=warn".into()))
        .with_writer(std::io::stderr)
        .init();
}

/// When started from systemd the session environment may be missing; find the
/// Wayland socket and Hyprland instance in the runtime dir.
fn fix_env() {
    let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from) else { return };
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        if let Some(sock) = std::fs::read_dir(&rt).ok().and_then(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
                .min()
        }) {
            std::env::set_var("WAYLAND_DISPLAY", sock);
        }
    }
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        let newest = std::fs::read_dir(rt.join("hypr")).ok().and_then(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| e.path().join(".socket.sock").exists())
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        });
        if let Some(e) = newest {
            std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", e.file_name());
        }
    }
}

fn print(json_mode: bool, v: &Value, human: impl FnOnce(&Value)) {
    if json_mode {
        println!("{v}");
    } else {
        human(v);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging();
    fix_env();
    match cli.cmd.unwrap_or(Cmd::Run { listen: None }) {
        Cmd::Run { listen } => run(listen).await,
        Cmd::Pair => pair(cli.json).await,
        Cmd::List => {
            let v = json!({ "paired": control::paired_json() });
            print(cli.json, &v, |v| {
                let list = v["paired"].as_array().cloned().unwrap_or_default();
                if list.is_empty() {
                    println!("No paired clients. Run `rl-host pair`.");
                }
                for c in list {
                    println!("{:<20} {}", c["name"].as_str().unwrap_or(""), c["fingerprint_short"].as_str().unwrap_or(""));
                }
            });
            Ok(())
        }
        Cmd::Unpair { name } => {
            let mut a = AuthorizedClients::load()?;
            let removed = a.remove(&name);
            if removed {
                a.save()?;
            }
            print(cli.json, &json!({ "removed": removed, "name": name }), |_| {
                println!("{}", if removed { format!("Removed {name}.") } else { format!("No paired client named {name}.") })
            });
            if !removed && !cli.json {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Status => {
            let v = match request(json!({"cmd": "status"})).await {
                Some(v) => v,
                None => json!({ "running": false, "paired": control::paired_json() }),
            };
            print(cli.json, &v, |v| {
                if v["running"] != json!(true) {
                    println!("Daemon not running.");
                    return;
                }
                println!("Host {:?} listening on {}", v["name"].as_str().unwrap_or(""), v["listen"].as_str().unwrap_or(""));
                match v["client"].as_object() {
                    Some(c) => println!(
                        "Connected: {} from {} — {} {}x{} @{}fps, {} kbps, rtt {:.1} ms",
                        c["name"].as_str().unwrap_or(""), c["address"].as_str().unwrap_or(""), c["monitor"].as_str().unwrap_or("?"),
                        c["width"], c["height"], c["fps"], c["kbps"], c["rtt_ms"].as_f64().unwrap_or(0.0)
                    ),
                    None => println!("No client connected."),
                }
            });
            Ok(())
        }
        Cmd::Disconnect => {
            let v = request(json!({"cmd": "disconnect"})).await.unwrap_or(json!({"running": false}));
            print(cli.json, &v, |v| println!("{}", if v["disconnected"] == json!(true) { "Disconnected." } else { "No client connected." }));
            Ok(())
        }
        Cmd::Monitors => {
            let mons = rl_host::monitors::list()?;
            let v = json!(mons.iter().map(|m| &m.info).collect::<Vec<_>>());
            print(cli.json, &v, |_| {
                for m in &mons {
                    println!("{:<10} {}x{} @{:.0}Hz at {},{}{}  {}", m.info.name, m.info.width, m.info.height, m.info.refresh_hz, m.info.x, m.info.y,
                        if m.info.focused { " (focused)" } else { "" }, m.info.description);
                }
            });
            Ok(())
        }
    }
}

async fn run(listen: Option<String>) -> Result<()> {
    let mut cfg = HostConfig::load()?;
    if let Some(l) = listen {
        cfg.listen = l;
    }
    if control::connect().await.is_some() {
        bail!("rl-host is already running (control socket {})", rl_common::paths::host_control_socket().display());
    }
    let host = Host::new(cfg)?;
    if let Err(e) = host.init_input() {
        tracing::error!("input injection disabled: {e:#}");
    }
    tokio::spawn(control::serve(host.clone()));
    let h = host.clone();
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
        tracing::info!("shutting down");
        h.shutdown();
    });
    host.serve().await?;
    let _ = std::fs::remove_file(rl_common::paths::host_control_socket());
    Ok(())
}

/// Send one request to the running daemon and read one response.
async fn request(req: Value) -> Option<Value> {
    let sock = control::connect().await?;
    let (r, mut w) = sock.into_split();
    w.write_all(format!("{req}\n").as_bytes()).await.ok()?;
    let line = BufReader::new(r).lines().next_line().await.ok()??;
    serde_json::from_str(&line).ok()
}

async fn pair(json_mode: bool) -> Result<()> {
    let emit = |v: &Value| {
        if json_mode {
            println!("{v}");
        }
    };
    let human = |s: String| {
        if !json_mode {
            println!("{s}");
        }
    };
    // Preferred: ask the running daemon to open a pairing window.
    if let Some(sock) = control::connect().await {
        let (r, mut w) = sock.into_split();
        w.write_all(b"{\"cmd\":\"pair\"}\n").await?;
        let mut lines = BufReader::new(r).lines();
        let mut ok = false;
        while let Some(line) = lines.next_line().await? {
            let v: Value = serde_json::from_str(&line)?;
            emit(&v);
            if let Some(pin) = v["pin"].as_str() {
                human(format!("Pairing PIN: {} {}\nOn the laptop run:  rl-client pair <this-desktop-ip>\n(expires in {}s)", &pin[..3], &pin[3..], v["expires_in"]));
            }
            match v["event"].as_str() {
                Some("paired") => {
                    human(format!("Paired with {}.", v["name"].as_str().unwrap_or("?")));
                    ok = true;
                    break;
                }
                Some("wrong_pin") => human(format!("Wrong PIN entered ({} attempts left).", v["attempts_left"])),
                Some("expired") => {
                    human("Pairing window closed.".into());
                    break;
                }
                _ => {}
            }
        }
        if !ok {
            std::process::exit(1);
        }
        return Ok(());
    }
    // No daemon: run a temporary listener just for pairing.
    let host: Arc<Host> = Host::new(HostConfig::load()?)?;
    let mut events = host.pair_events.subscribe();
    let win = host.open_pairing();
    emit(&json!({ "pin": win.pin, "expires_in": win.remaining().as_secs() }));
    human(format!(
        "Pairing PIN: {} {}\nOn the laptop run:  rl-client pair <this-desktop-ip>\n(expires in {}s; daemon not running, using a temporary listener)",
        &win.pin[..3], &win.pin[3..], win.remaining().as_secs()
    ));
    let h = host.clone();
    tokio::spawn(async move { h.serve().await });
    let deadline = tokio::time::sleep(win.remaining());
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => {
                emit(&json!({"event": "expired"}));
                human("Pairing window closed.".into());
                std::process::exit(1);
            }
            ev = events.recv() => {
                let Ok(ev) = ev else { std::process::exit(1) };
                emit(&serde_json::to_value(&ev)?);
                match ev {
                    rl_host::server::PairEvent::Paired { name, .. } => {
                        human(format!("Paired with {name}."));
                        // Let the final message reach the client.
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        host.shutdown();
                        return Ok(());
                    }
                    rl_host::server::PairEvent::WrongPin { attempts_left } => human(format!("Wrong PIN entered ({attempts_left} attempts left).")),
                    rl_host::server::PairEvent::Expired => { human("Pairing window closed.".into()); std::process::exit(1); }
                }
            }
        }
    }
}
