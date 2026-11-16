//! Encrypts secrets (OAuth refresh tokens) before they reach the database: AES-256-GCM with a
//! random 96-bit nonce, stored as base64(nonce || ciphertext).

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

#[derive(Clone)]
pub struct SecretBox {
    cipher: Aes256Gcm,
}

impl SecretBox {
    /// `key_b64` is 32 random bytes, base64 encoded (`gm gen-key`).
    pub fn new(key_b64: &str) -> anyhow::Result<Self> {
        let key = B64.decode(key_b64.trim())?;
        anyhow::ensure!(
            key.len() == 32,
            "master key must be 32 bytes (base64), got {}",
            key.len()
        );
        Ok(Self {
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key)),
        })
    }

    pub fn generate_key() -> String {
        B64.encode(Aes256Gcm::generate_key(OsRng))
    }

    pub fn encrypt(&self, plaintext: &str) -> String {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = self
            .cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .expect("aes-gcm encrypt");
        let mut out = nonce.to_vec();
        out.extend(ct);
        B64.encode(out)
    }

    pub fn decrypt(&self, token: &str) -> anyhow::Result<String> {
        let raw = B64.decode(token)?;
        anyhow::ensure!(raw.len() > 12, "ciphertext too short");
        let (nonce, ct) = raw.split_at(12);
        let pt = self
            .cipher
            .decrypt(Nonce::from_slice(nonce), ct)
            .map_err(|_| anyhow::anyhow!("decryption failed (wrong master key?)"))?;
        Ok(String::from_utf8(pt)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_wrong_key() {
        let a = SecretBox::new(&SecretBox::generate_key()).unwrap();
        let b = SecretBox::new(&SecretBox::generate_key()).unwrap();
        let ct = a.encrypt("refresh-token");
        assert_ne!(ct, a.encrypt("refresh-token"), "nonce must be random");
        assert_eq!(a.decrypt(&ct).unwrap(), "refresh-token");
        assert!(b.decrypt(&ct).is_err());
        assert!(SecretBox::new("c2hvcnQ=").is_err());
    }
}
