//! Local control socket for the CLI and UI: JSON lines over a Unix socket at
//! `$XDG_RUNTIME_DIR/remotelinux/host.sock`.
//!
//! Requests: `{"cmd":"status"}`, `{"cmd":"pair"}`, `{"cmd":"cancel_pair"}`,
//! `{"cmd":"list"}`, `{"cmd":"unpair","name":"..."}`, `{"cmd":"disconnect"}`.
//! Each request gets one JSON response line; `pair` additionally streams
//! events until pairing succeeds, fails or expires.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use anyhow::Result;
use rl_common::trust::AuthorizedClients;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::server::{Host, PairEvent};

#[derive(Deserialize)]
struct Request {
    cmd: String,
    #[serde(default)]
    name: Option<String>,
}

pub async fn serve(host: Arc<Host>) -> Result<()> {
    let path = rl_common::paths::host_control_socket();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::set_permissions(path.parent().unwrap(), std::fs::Permissions::from_mode(0o700))?;
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    loop {
        let (sock, _) = listener.accept().await?;
        let host = host.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(host, sock).await {
                tracing::debug!("control client: {e:#}");
            }
        });
    }
}

pub fn status_json(host: &Host) -> Value {
    let paired = paired_json();
    json!({
        "running": true,
        "name": host.name,
        "listen": host.listen.to_string(),
        "port": host.listen.port(),
        "fingerprint": host.identity.fingerprint,
        "pairing_open": host.pairing_open(),
        "client": host.client_status(),
        "paired": paired,
    })
}

pub fn paired_json() -> Value {
    match AuthorizedClients::load() {
        Ok(a) => json!(a
            .clients
            .iter()
            .map(|c| json!({
                "name": c.name,
                "fingerprint": c.fingerprint,
                "fingerprint_short": rl_common::identity::short_fingerprint(&c.fingerprint),
                "paired_at": c.paired_at,
            }))
            .collect::<Vec<_>>()),
        Err(e) => json!({ "error": format!("{e:#}") }),
    }
}

async fn send(w: &mut tokio::net::unix::OwnedWriteHalf, v: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(v)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    Ok(())
}

async fn handle(host: Arc<Host>, sock: UnixStream) -> Result<()> {
    let (r, mut w) = sock.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                send(&mut w, &json!({ "error": format!("bad request: {e}") })).await?;
                continue;
            }
        };
        match req.cmd.as_str() {
            "status" => send(&mut w, &status_json(&host)).await?,
            "list" => send(&mut w, &json!({ "paired": paired_json() })).await?,
            "unpair" => {
                let name = req.name.unwrap_or_default();
                let mut a = AuthorizedClients::load()?;
                let removed = a.remove(&name);
                if removed {
                    a.save()?;
                }
                send(&mut w, &json!({ "removed": removed, "name": name })).await?;
            }
            "disconnect" => send(&mut w, &json!({ "disconnected": host.disconnect_client() })).await?,
            "cancel_pair" => {
                host.close_pairing();
                send(&mut w, &json!({ "cancelled": true })).await?;
            }
            "pair" => {
                let mut events = host.pair_events.subscribe();
                let win = host.open_pairing();
                send(&mut w, &json!({ "pin": win.pin, "expires_in": win.remaining().as_secs() })).await?;
                let deadline = tokio::time::sleep(win.remaining());
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        _ = &mut deadline => {
                            host.close_pairing();
                            send(&mut w, &json!({ "event": "expired" })).await?;
                            break;
                        }
                        ev = events.recv() => match ev {
                            Ok(ev) => {
                                send(&mut w, &serde_json::to_value(&ev)?).await?;
                                if matches!(ev, PairEvent::Paired { .. } | PairEvent::Expired) { break; }
                            }
                            Err(_) => break,
                        },
                        // Requester went away: close the window.
                        l = lines.next_line() => {
                            if matches!(l, Ok(None) | Err(_)) { host.close_pairing(); return Ok(()); }
                            if let Ok(Some(l)) = l {
                                if l.contains("cancel_pair") {
                                    host.close_pairing();
                                    send(&mut w, &json!({ "event": "cancelled" })).await?;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            other => send(&mut w, &json!({ "error": format!("unknown command {other:?}") })).await?,
        }
    }
    Ok(())
}

/// Client side, used by the `rl-host` CLI: connect to a running daemon.
pub async fn connect() -> Option<UnixStream> {
    UnixStream::connect(rl_common::paths::host_control_socket()).await.ok()
}
