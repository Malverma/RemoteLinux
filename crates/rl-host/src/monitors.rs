//! Monitor layout from Hyprland (`hyprctl -j monitors`).

use anyhow::{Context, Result};
use rl_common::proto::MonitorInfo;
use serde::Deserialize;

use crate::input::Target;

#[derive(Deserialize)]
struct HyprMonitor {
    name: String,
    #[serde(default)]
    description: String,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    #[serde(default = "one")]
    scale: f64,
    #[serde(default)]
    transform: u32,
    #[serde(default, rename = "refreshRate")]
    refresh_rate: f32,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    disabled: bool,
}

fn one() -> f64 {
    1.0
}

/// Monitor with its logical (layout) size.
#[derive(Debug, Clone)]
pub struct Monitor {
    pub info: MonitorInfo,
    pub logical_w: u32,
    pub logical_h: u32,
}

pub fn list() -> Result<Vec<Monitor>> {
    let out = std::process::Command::new("hyprctl")
        .args(["-j", "monitors"])
        .output()
        .context("running hyprctl (is Hyprland running?)")?;
    anyhow::ensure!(out.status.success(), "hyprctl failed: {}", String::from_utf8_lossy(&out.stderr));
    parse(&out.stdout)
}

fn parse(json: &[u8]) -> Result<Vec<Monitor>> {
    let mons: Vec<HyprMonitor> = serde_json::from_slice(json).context("parsing hyprctl output")?;
    Ok(mons
        .into_iter()
        .filter(|m| !m.disabled)
        .map(|m| {
            let rotated = m.transform % 2 == 1;
            let (pw, ph) = if rotated { (m.height, m.width) } else { (m.width, m.height) };
            let s = if m.scale > 0.0 { m.scale } else { 1.0 };
            Monitor {
                logical_w: (pw as f64 / s).round() as u32,
                logical_h: (ph as f64 / s).round() as u32,
                info: MonitorInfo {
                    name: m.name,
                    description: m.description,
                    x: m.x,
                    y: m.y,
                    width: pw,
                    height: ph,
                    refresh_hz: m.refresh_rate,
                    focused: m.focused,
                },
            }
        })
        .collect())
}

/// Pick a monitor by name, falling back to the focused one, then the first.
pub fn select<'a>(mons: &'a [Monitor], want: Option<&str>) -> Option<&'a Monitor> {
    want.filter(|w| !w.is_empty() && *w != "auto")
        .and_then(|w| mons.iter().find(|m| m.info.name == w))
        .or_else(|| mons.iter().find(|m| m.info.focused))
        .or_else(|| mons.first())
}

pub fn target(mons: &[Monitor], m: &Monitor) -> Target {
    let x0 = mons.iter().map(|m| m.info.x).min().unwrap_or(0);
    let y0 = mons.iter().map(|m| m.info.y).min().unwrap_or(0);
    let x1 = mons.iter().map(|m| m.info.x + m.logical_w as i32).max().unwrap_or(0);
    let y1 = mons.iter().map(|m| m.info.y + m.logical_h as i32).max().unwrap_or(0);
    Target {
        layout_x: x0,
        layout_y: y0,
        layout_w: (x1 - x0).max(1) as u32,
        layout_h: (y1 - y0).max(1) as u32,
        mon_x: m.info.x,
        mon_y: m.info.y,
        mon_w: m.logical_w,
        mon_h: m.logical_h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_builds_target() {
        let j = br#"[{"name":"DP-1","width":1920,"height":1080,"x":1920,"y":-1080,"scale":1,"transform":0,"refreshRate":60.0,"focused":false},
                    {"name":"HDMI-A-1","width":3840,"height":2160,"x":0,"y":0,"scale":2,"transform":0,"refreshRate":60.0,"focused":true}]"#;
        let mons = parse(j).unwrap();
        assert_eq!(mons[1].logical_w, 1920);
        let m = select(&mons, None).unwrap();
        assert_eq!(m.info.name, "HDMI-A-1");
        let t = target(&mons, m);
        assert_eq!((t.layout_x, t.layout_y, t.layout_w, t.layout_h), (0, -1080, 3840, 2160));
        assert_eq!(select(&mons, Some("DP-1")).unwrap().info.name, "DP-1");
    }
}
