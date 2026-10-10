//! Exercises the GUI pairing flow against a running host: discovery, a
//! pairing window via the control socket, and client pairing.
use std::time::Duration;
use rl_common::{client, discovery, hostctl};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let st = hostctl::status().await.expect("host running");
    println!("host status: {} on {}", st.name, st.listen);
    let mut win = hostctl::PairWindow::open().await?;
    println!("pin {} ({}s)", win.pin, win.expires_in);
    let found = discovery::discover(Duration::from_millis(900)).await?;
    println!("discovered: {found:?}");
    let target = found.iter().find(|d| d.fingerprint == st.fingerprint).expect("host not discovered");
    assert!(target.pairing, "host should advertise pairing");
    let (pin, addr) = (win.pin.clone(), target.address.clone());
    let pair = tokio::spawn(async move { client::pair(&addr, &pin, "ui-test-laptop").await });
    let ev = tokio::time::timeout(Duration::from_secs(5), win.next()).await?;
    println!("host event: {ev:?}");
    println!("client got: {:?}", pair.await?.map(|h| (h.name, h.address)));
    let st = hostctl::status().await.unwrap();
    println!("paired list: {:?}", st.paired.iter().map(|p| &p.name).collect::<Vec<_>>());
    println!("unpaired: {}", hostctl::unpair("ui-test-laptop").await?);
    Ok(())
}
