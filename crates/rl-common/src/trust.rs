//! Trust stores: which clients a host accepts, and which hosts a client knows.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::{config_dir, write_private};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AuthorizedClient {
    pub name: String,
    pub fingerprint: String,
    pub paired_at: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct KnownHost {
    pub name: String,
    /// "ip:port" or "hostname:port" used to reach the host.
    pub address: String,
    pub fingerprint: String,
    pub paired_at: u64,
    /// Unix time of the last successful connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct AuthorizedClients {
    #[serde(default, rename = "client")]
    pub clients: Vec<AuthorizedClient>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct KnownHosts {
    #[serde(default, rename = "host")]
    pub hosts: Vec<KnownHost>,
}

fn load<T: Default + for<'de> Deserialize<'de>>(path: &PathBuf) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn save<T: Serialize>(path: &PathBuf, v: &T) -> Result<()> {
    write_private(path, toml::to_string_pretty(v)?.as_bytes(), 0o600)
}

impl AuthorizedClients {
    pub fn path() -> PathBuf {
        config_dir().join("authorized_clients.toml")
    }
    pub fn load() -> Result<Self> {
        load(&Self::path())
    }
    pub fn save(&self) -> Result<()> {
        save(&Self::path(), self)
    }
    pub fn find_fingerprint(&self, fp: &str) -> Option<&AuthorizedClient> {
        self.clients.iter().find(|c| c.fingerprint == fp)
    }
    /// Add or replace (by fingerprint, then by name).
    pub fn upsert(&mut self, c: AuthorizedClient) {
        self.clients.retain(|o| o.fingerprint != c.fingerprint && o.name != c.name);
        self.clients.push(c);
    }
    /// Remove by name or fingerprint prefix; returns whether anything was removed.
    pub fn remove(&mut self, key: &str) -> bool {
        let before = self.clients.len();
        self.clients.retain(|c| c.name != key && !(key.len() >= 8 && c.fingerprint.starts_with(key)));
        before != self.clients.len()
    }
}

impl KnownHosts {
    pub fn path() -> PathBuf {
        config_dir().join("known_hosts.toml")
    }
    pub fn load() -> Result<Self> {
        load(&Self::path())
    }
    pub fn save(&self) -> Result<()> {
        save(&Self::path(), self)
    }
    pub fn find(&self, name: &str) -> Option<&KnownHost> {
        self.hosts.iter().find(|h| h.name == name).or_else(|| self.hosts.iter().find(|h| h.address == name))
    }
    pub fn upsert(&mut self, h: KnownHost) {
        self.hosts.retain(|o| o.fingerprint != h.fingerprint && o.name != h.name);
        self.hosts.push(h);
    }
    pub fn remove(&mut self, key: &str) -> bool {
        let before = self.hosts.len();
        self.hosts.retain(|h| h.name != key && h.address != key);
        before != self.hosts.len()
    }
    /// Record a successful connection.
    pub fn touch(&mut self, fingerprint: &str) {
        if let Some(h) = self.hosts.iter_mut().find(|h| h.fingerprint == fingerprint) {
            h.last_seen = Some(crate::unix_now());
        }
    }
}
