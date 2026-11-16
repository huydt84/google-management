//! OAuth 2.0 authorization-code flow with PKCE against Google, used for two flows:
//! `login` (OpenID: who is signing in to the web UI) and `connect` (API scopes + offline access).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use rand::Rng;
use rand::distributions::Alphanumeric;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Flow {
    Login,
    Connect,
}

impl Flow {
    pub fn redirect_uri(&self, cfg: &Config) -> String {
        match self {
            Flow::Login => format!("{}/auth/callback", cfg.base_url),
            Flow::Connect => format!("{}/connect/google/callback", cfg.base_url),
        }
    }
}

pub fn random_token(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

pub fn pkce_challenge(verifier: &str) -> String {
    B64URL.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn authorize_url(cfg: &Config, flow: Flow, state: &str, verifier: &str) -> String {
    let mut scopes = vec!["openid", "email"];
    let mut url = url::Url::parse(&cfg.google_auth_url).expect("valid GM_GOOGLE_AUTH_URL");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", &cfg.google_client_id)
            .append_pair("redirect_uri", &flow.redirect_uri(cfg))
            .append_pair("response_type", "code")
            .append_pair("state", state)
            .append_pair("code_challenge", &pkce_challenge(verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("login_hint", &cfg.allowed_email);
        match flow {
            Flow::Login => {
                scopes.push("profile");
                q.append_pair("prompt", "select_account");
            }
            Flow::Connect => {
                scopes.extend(cfg.required_scopes());
                // prompt=consent forces Google to (re)issue a refresh token.
                q.append_pair("access_type", "offline")
                    .append_pair("prompt", "consent")
                    .append_pair("include_granted_scopes", "true");
            }
        }
        q.append_pair("scope", &scopes.join(" "));
    }
    url.into()
}

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
}

pub async fn exchange_code(
    http: &reqwest::Client,
    cfg: &Config,
    flow: Flow,
    code: &str,
    verifier: &str,
) -> anyhow::Result<TokenResponse> {
    let resp = http
        .post(&cfg.google_token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &flow.redirect_uri(cfg)),
            ("client_id", &cfg.google_client_id),
            ("client_secret", &cfg.google_client_secret),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("token request failed: {}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        // The error body contains no secrets (Google returns {"error": "..."}).
        let body: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect();
        anyhow::bail!("token exchange failed: {status} {body}");
    }
    Ok(resp.json().await?)
}

#[derive(Debug, Deserialize)]
pub struct IdClaims {
    pub email: String,
    #[serde(default)]
    pub email_verified: bool,
    pub aud: String,
    pub iss: String,
    pub exp: i64,
}

/// Read the ID token returned by the token endpoint. Its signature is not checked: per OpenID
/// Connect Core 3.1.3.7 the TLS connection to Google's token endpoint authenticates it, since we
/// received it directly in exchange for our client secret. Audience, issuer and expiry are checked.
pub fn verify_id_token(id_token: &str, client_id: &str, now_unix: i64) -> anyhow::Result<IdClaims> {
    let payload = id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("malformed id_token"))?;
    let claims: IdClaims = serde_json::from_slice(&B64URL.decode(payload.trim_end_matches('='))?)?;
    anyhow::ensure!(claims.aud == client_id, "id_token audience mismatch");
    anyhow::ensure!(
        claims.iss == "https://accounts.google.com" || claims.iss == "accounts.google.com",
        "id_token issuer mismatch"
    );
    anyhow::ensure!(claims.exp > now_unix, "id_token expired");
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn fake_id_token(email: &str, verified: bool, aud: &str, exp: i64) -> String {
        let payload = serde_json::json!({
            "email": email, "email_verified": verified, "aud": aud,
            "iss": "https://accounts.google.com", "exp": exp
        });
        format!("e30.{}.sig", B64URL.encode(payload.to_string()))
    }

    #[test]
    fn id_token_checks() {
        let t = fake_id_token("a@b.c", true, "cid", 2_000_000_000);
        let c = verify_id_token(&t, "cid", 1_000).unwrap();
        assert_eq!(c.email, "a@b.c");
        assert!(c.email_verified);
        assert!(verify_id_token(&t, "other", 1_000).is_err());
        assert!(verify_id_token(&t, "cid", 2_000_000_001).is_err());
    }

    #[test]
    fn authorize_url_has_pkce_and_scopes() {
        let cfg = Config::for_tests();
        let u = authorize_url(&cfg, Flow::Connect, "st", "verifier");
        assert!(u.contains("code_challenge_method=S256"));
        assert!(u.contains("access_type=offline"));
        assert!(u.contains("gmail.readonly"));
        assert!(u.contains("state=st"));
    }
}
