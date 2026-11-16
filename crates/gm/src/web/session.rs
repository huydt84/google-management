//! Signed cookies (HMAC-SHA256): the login session and the short-lived OAuth state. Values are
//! not secret (email, nonces) but must not be forgeable.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use hmac::{Hmac, Mac};
use serde::{Serialize, de::DeserializeOwned};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub const SESSION_COOKIE: &str = "gm_session";
pub const OAUTH_COOKIE: &str = "gm_oauth";

#[derive(Clone)]
pub struct Signer {
    key: Vec<u8>,
}

impl Signer {
    pub fn new(secret: &str) -> Self {
        Self {
            key: secret.as_bytes().to_vec(),
        }
    }

    fn mac(&self, purpose: &str, data: &str) -> Vec<u8> {
        let mut m = HmacSha256::new_from_slice(&self.key).expect("hmac key");
        m.update(purpose.as_bytes());
        m.update(b"\0");
        m.update(data.as_bytes());
        m.finalize().into_bytes().to_vec()
    }

    pub fn sign<T: Serialize>(&self, purpose: &str, value: &T) -> String {
        let data = B64URL.encode(serde_json::to_vec(value).expect("serializable"));
        let sig = B64URL.encode(self.mac(purpose, &data));
        format!("{data}.{sig}")
    }

    pub fn verify<T: DeserializeOwned>(&self, purpose: &str, token: &str) -> Option<T> {
        let (data, sig) = token.split_once('.')?;
        let sig = B64URL.decode(sig).ok()?;
        let mut m = HmacSha256::new_from_slice(&self.key).ok()?;
        m.update(purpose.as_bytes());
        m.update(b"\0");
        m.update(data.as_bytes());
        m.verify_slice(&sig).ok()?; // constant-time comparison
        serde_json::from_slice(&B64URL.decode(data).ok()?).ok()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub email: String,
    pub iat: i64,
    pub exp: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthState {
    pub flow: crate::google::oauth::Flow,
    pub state: String,
    pub verifier: String,
    pub exp: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_and_tamper() {
        let s = Signer::new("k1");
        let tok = s.sign(
            "session",
            &Session {
                email: "a@b.c".into(),
                iat: 1,
                exp: 2,
            },
        );
        let back: Session = s.verify("session", &tok).unwrap();
        assert_eq!(back.email, "a@b.c");
        assert!(
            s.verify::<Session>("oauth", &tok).is_none(),
            "purpose is bound"
        );
        assert!(
            Signer::new("k2")
                .verify::<Session>("session", &tok)
                .is_none()
        );
        let mut forged = tok.clone();
        forged.replace_range(0..1, if tok.starts_with('A') { "B" } else { "A" });
        assert!(s.verify::<Session>("session", &forged).is_none());
    }
}
