//! User configuration files (`~/.config/remotelinux/{host,client}.toml`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::config_dir;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default)]
pub struct HostConfig {
    /// Address to listen on.
    pub listen: String,
    /// Monitor to stream: "auto" (focused monitor) or a name such as "DP-1".
    pub monitor: String,
    /// "native", or a height like "720p", "900p", "1080p".
    pub resolution: String,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// "auto", "nvenc" or "x264".
    pub encoder: String,
    pub show_cursor_in_stream: bool,
    pub notify_on_connect: bool,
    /// Forward desktop audio (the default output) to the client.
    pub audio: bool,
    /// Device name presented to clients. Empty = hostname.
    pub name: String,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            listen: format!("0.0.0.0:{}", crate::DEFAULT_PORT),
            monitor: "auto".into(),
            resolution: "native".into(),
            fps: 60,
            bitrate_kbps: 15_000,
            encoder: "auto".into(),
            show_cursor_in_stream: false,
            notify_on_connect: true,
            audio: true,
            name: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default)]
pub struct ClientConfig {
    /// Host to connect to when none is given (name from known_hosts).
    pub default_host: String,
    pub fullscreen: bool,
    /// Key chord that releases the keyboard grab (never forwarded).
    pub release_chord: String,
    /// "auto", "vaapi" or "software".
    pub decoder: String,
    /// Optional overrides of host defaults. Empty / 0 = host default.
    pub resolution: String,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub monitor: String,
    /// Show the stats overlay when the viewer opens.
    pub show_stats: bool,
    /// Device name presented to hosts. Empty = hostname.
    pub name: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            default_host: String::new(),
            fullscreen: false,
            release_chord: "Ctrl+Alt+Shift+Q".into(),
            decoder: "auto".into(),
            resolution: String::new(),
            fps: 0,
            bitrate_kbps: 0,
            monitor: String::new(),
            show_stats: false,
            name: String::new(),
        }
    }
}

fn load<T: Default + for<'de> Deserialize<'de>>(file: &str) -> Result<T> {
    let path = config_dir().join(file);
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

impl HostConfig {
    pub fn load() -> Result<Self> {
        load("host.toml")
    }
    pub fn save(&self) -> Result<()> {
        crate::paths::write_private(&config_dir().join("host.toml"), toml::to_string_pretty(self)?.as_bytes(), 0o644)
    }
    pub fn device_name(&self) -> String {
        if self.name.is_empty() { crate::hostname() } else { self.name.clone() }
    }
}

impl ClientConfig {
    pub fn load() -> Result<Self> {
        load("client.toml")
    }
    pub fn save(&self) -> Result<()> {
        crate::paths::write_private(&config_dir().join("client.toml"), toml::to_string_pretty(self)?.as_bytes(), 0o644)
    }
    pub fn device_name(&self) -> String {
        if self.name.is_empty() { crate::hostname() } else { self.name.clone() }
    }
}

/// Parse "720p" / "1080" / "native" into an optional max height.
pub fn parse_resolution(s: &str) -> Result<Option<u32>> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() || s == "native" {
        return Ok(None);
    }
    let n: u32 = s.trim_end_matches('p').parse().with_context(|| format!("invalid resolution {s:?}"))?;
    anyhow::ensure!((144..=4320).contains(&n), "resolution {n}p out of range");
    Ok(Some(n))
}

#[cfg(test)]
mod tests {
    #[test]
    fn resolution() {
        assert_eq!(super::parse_resolution("720p").unwrap(), Some(720));
        assert_eq!(super::parse_resolution("native").unwrap(), None);
        assert!(super::parse_resolution("abc").is_err());
    }
}
