//! Moves the pointer via the virtual absolute device and prints where Hyprland
//! put it, to validate the coordinate mapping.
use rl_host::{input::Injector, monitors};
fn main() -> anyhow::Result<()> {
    let mons = monitors::list()?;
    let mut inj = Injector::new()?;
    std::thread::sleep(std::time::Duration::from_millis(800)); // let compositor pick up device
    let mut seq = 0;
    for m in &mons {
        inj.set_target(monitors::target(&mons, m));
        for (x, y) in [(0.0, 0.0), (0.5, 0.5), (1.0, 1.0)] {
            seq += 1;
            inj.motion_abs(seq, x, y)?;
            std::thread::sleep(std::time::Duration::from_millis(150));
            let pos = std::process::Command::new("hyprctl").arg("cursorpos").output()?;
            println!("{} ({x},{y}) -> {}", m.info.name, String::from_utf8_lossy(&pos.stdout).trim());
        }
    }
    Ok(())
}
