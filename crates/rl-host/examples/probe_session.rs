//! Acts as a paired client: opens a session and sends pointer motion, then
//! prints where the compositor put the cursor. Uses the client identity in
//! $XDG_DATA_HOME. Usage: probe_session <host:port> <monitor>
use std::time::Duration;

use rl_common::identity::{Identity, Role};
use rl_common::proto::{read_msg, write_msg, ClientMsg, HostMsg, Motion, StreamSettings};
use rl_common::transport;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let addr = transport::resolve(&args[1])?;
    let id = Identity::load_or_create(Role::Client)?;
    let ep = transport::client_endpoint()?;
    let conn = ep.connect_with(transport::client_config(&id, transport::ALPN_SESSION)?, addr, "remotelinux")?.await?;
    let (mut s, mut r) = conn.open_bi().await?;
    let settings = StreamSettings { monitor: Some(args[2].clone()), ..Default::default() };
    write_msg(&mut s, &ClientMsg::Hello { version: rl_common::PROTOCOL_VERSION, client_name: "probe".into(), settings }).await?;
    loop {
        match read_msg::<HostMsg>(&mut r).await? {
            Some(HostMsg::StreamInfo { monitor, width, height, .. }) => {
                println!("stream {monitor} {width}x{height}");
                break;
            }
            Some(_) => {}
            None => anyhow::bail!("closed"),
        }
    }
    for (i, (x, y)) in [(0.25f32, 0.25f32), (0.5, 0.5), (0.999, 0.999)].into_iter().enumerate() {
        conn.send_datagram(Motion::Abs { seq: i as u32 + 1, x, y }.encode().into())?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let out = std::process::Command::new("hyprctl").arg("cursorpos").output()?;
        println!("({x},{y}) -> {}", String::from_utf8_lossy(&out.stdout).trim());
    }
    // Stale motion (older seq) must be ignored.
    conn.send_datagram(Motion::Abs { seq: 1, x: 0.0, y: 0.0 }.encode().into())?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let out = std::process::Command::new("hyprctl").arg("cursorpos").output()?;
    println!("stale seq ignored -> {}", String::from_utf8_lossy(&out.stdout).trim());
    write_msg(&mut s, &ClientMsg::Key { code: 54, pressed: true }).await?; // KEY_RIGHTSHIFT
    write_msg(&mut s, &ClientMsg::Key { code: 54, pressed: false }).await?;
    write_msg(&mut s, &ClientMsg::Bye).await?;
    let _ = s.finish();
    tokio::time::sleep(Duration::from_millis(200)).await;
    println!("ok");
    Ok(())
}
