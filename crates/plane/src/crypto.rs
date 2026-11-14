//! Secret encryption at rest (AES-256-GCM) with key rotation, and token helpers.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct Crypto {
    /// (key id, key). The first entry encrypts; all entries decrypt.
    keys: Vec<(String, Vec<u8>)>,
}

impl Crypto {
    pub fn from_spec(spec: &str) -> Result<Self> {
        let mut keys = vec![];
        for part in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (kid, b64) = part.split_once(':').unwrap_or(("k1", part));
            let key = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .or_else(|_| hex::decode(b64.trim()))
                .with_context(|| format!("encryption key {kid} is not base64 or hex"))?;
            if key.len() != 32 {
                bail!("encryption key {kid} must be 32 bytes, got {}", key.len());
            }
            keys.push((kid.to_string(), key));
        }
        if keys.is_empty() {
            bail!("no encryption keys");
        }
        Ok(Crypto { keys })
    }

    /// Deterministic secret derived from the active key: HMAC-SHA256(label).
    pub fn derive(&self, label: &str) -> String {
        use hmac::{Hmac, Mac};
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.keys[0].1).expect("hmac key");
        mac.update(label.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }

    pub fn active_key_id(&self) -> &str {
        &self.keys[0].0
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<(String, Vec<u8>, Vec<u8>)> {
        let (kid, key) = &self.keys[0];
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| anyhow!("{e}"))?;
        let nonce_bytes = random_bytes(12);
        let nonce = Nonce::try_from(&nonce_bytes[..]).map_err(|_| anyhow!("nonce"))?;
        let ct = cipher
            .encrypt(&nonce, plaintext)
            .map_err(|e| anyhow!("encrypt: {e}"))?;
        Ok((kid.clone(), nonce_bytes, ct))
    }

    pub fn decrypt(&self, kid: &str, nonce: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
        let key = self
            .keys
            .iter()
            .find(|(k, _)| k == kid)
            .map(|(_, k)| k)
            .ok_or_else(|| anyhow!("unknown encryption key id {kid}"))?;
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| anyhow!("{e}"))?;
        let nonce = Nonce::try_from(nonce).map_err(|_| anyhow!("nonce"))?;
        cipher.decrypt(&nonce, ct).map_err(|e| anyhow!("decrypt: {e}"))
    }
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        out.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    out.truncate(n);
    out
}

/// A random URL-safe token with a prefix, e.g. `frt_...`.
pub fn new_token(prefix: &str) -> String {
    format!(
        "{prefix}_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes(32))
    )
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Shown instead of a secret after entry: never more than the last 4 characters.
pub fn hint(secret: &str) -> String {
    let n = secret.chars().count();
    if n <= 8 {
        "…".to_string()
    } else {
        let tail: String = secret.chars().skip(n - 4).collect();
        format!("…{tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_rotation() {
        let k1 = base64::engine::general_purpose::STANDARD.encode([1u8; 32]);
        let k2 = base64::engine::general_purpose::STANDARD.encode([2u8; 32]);
        let old = Crypto::from_spec(&format!("k1:{k1}")).unwrap();
        let (kid, n, ct) = old.encrypt(b"hello").unwrap();
        assert_eq!(kid, "k1");
        let rotated = Crypto::from_spec(&format!("k2:{k2},k1:{k1}")).unwrap();
        assert_eq!(rotated.decrypt(&kid, &n, &ct).unwrap(), b"hello");
        let (kid2, _, _) = rotated.encrypt(b"x").unwrap();
        assert_eq!(kid2, "k2");
        assert!(rotated.decrypt("k1", &n, b"garbage").is_err());
    }
    #[test]
    fn hints_never_reveal() {
        assert_eq!(hint("short"), "…");
        assert_eq!(hint("sk-abcdefghijkl1234"), "…1234");
    }
}
