//! Long-term device identity: a self-signed Ed25519 certificate.

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Host,
    Client,
}

impl Role {
    fn dir(self) -> &'static str {
        match self {
            Role::Host => "host",
            Role::Client => "client",
        }
    }
}

pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivatePkcs8KeyDer<'static>,
    pub fingerprint: String,
}

impl Identity {
    /// Load this device's identity for `role`, creating it on first use.
    pub fn load_or_create(role: Role) -> Result<Self> {
        let dir = crate::paths::data_dir().join("identity").join(role.dir());
        let cert_path = dir.join("cert.der");
        let key_path = dir.join("key.der");
        if cert_path.exists() && key_path.exists() {
            let cert = std::fs::read(&cert_path).context("reading identity certificate")?;
            let key = std::fs::read(&key_path).context("reading identity key")?;
            return Ok(Self::from_der(cert, key));
        }
        let id = Self::generate()?;
        crate::paths::write_private(&key_path, id.key.secret_pkcs8_der(), 0o600)?;
        crate::paths::write_private(&cert_path, &id.cert, 0o600)?;
        Ok(id)
    }

    /// Generate a fresh, unsaved identity.
    pub fn generate() -> Result<Self> {
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
        let params = rcgen::CertificateParams::new(vec!["remotelinux".to_string()])?;
        let cert = params.self_signed(&key_pair)?;
        Ok(Self::from_der(cert.der().to_vec(), key_pair.serialize_der()))
    }

    fn from_der(cert: Vec<u8>, key: Vec<u8>) -> Self {
        let fingerprint = fingerprint(&cert);
        Self { cert: CertificateDer::from(cert), key: PrivatePkcs8KeyDer::from(key), fingerprint }
    }

    pub fn key_der(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }
}

/// Hex SHA-256 of a DER certificate.
pub fn fingerprint(cert_der: &[u8]) -> String {
    hex::encode(Sha256::digest(cert_der))
}

/// Short, human-friendly form of a fingerprint for display.
pub fn short_fingerprint(fp: &str) -> String {
    fp.chars()
        .take(16)
        .collect::<Vec<_>>()
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(":")
}
