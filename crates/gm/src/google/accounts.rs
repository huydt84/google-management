//! Connected Google accounts: encrypted refresh tokens and short-lived access tokens.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use sqlx::PgPool;
use tokio::sync::Mutex;

use super::GoogleError;
use crate::config::Config;
use crate::crypto::SecretBox;
use crate::db::GoogleAccount;

pub struct AccountStore {
    pool: PgPool,
    secret_box: SecretBox,
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    token_url: String,
    cache: Mutex<HashMap<String, (String, DateTime<Utc>)>>,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    expires_in: Option<i64>,
}

impl AccountStore {
    pub fn new(
        pool: PgPool,
        secret_box: SecretBox,
        http: reqwest::Client,
        cfg: &Config,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            secret_box,
            http,
            client_id: cfg.google_client_id.clone(),
            client_secret: cfg.google_client_secret.clone(),
            token_url: cfg.google_token_url.clone(),
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub async fn save(
        &self,
        owner: &str,
        email: &str,
        refresh_token: &str,
        scopes: &[String],
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO google_accounts (email, owner_email, refresh_token_enc, scopes, status)
             VALUES ($1, $2, $3, $4, 'active')
             ON CONFLICT (email) DO UPDATE SET owner_email = EXCLUDED.owner_email,
                 refresh_token_enc = EXCLUDED.refresh_token_enc, scopes = EXCLUDED.scopes,
                 status = 'active', updated_at = now()",
        )
        .bind(email.to_lowercase())
        .bind(owner.to_lowercase())
        .bind(self.secret_box.encrypt(refresh_token))
        .bind(sqlx::types::Json(scopes))
        .execute(&self.pool)
        .await?;
        self.cache.lock().await.remove(&email.to_lowercase());
        Ok(())
    }

    pub async fn get(&self, email: &str) -> sqlx::Result<Option<GoogleAccount>> {
        sqlx::query_as::<_, GoogleAccount>(
            "SELECT email, owner_email, refresh_token_enc, scopes, status FROM google_accounts WHERE email = $1",
        )
        .bind(email.to_lowercase())
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn active_emails(&self) -> sqlx::Result<Vec<String>> {
        sqlx::query_scalar(
            "SELECT email FROM google_accounts WHERE status = 'active' ORDER BY email",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// The gm user who owns a connected account (for per-user settings).
    pub async fn owner_of(&self, email: &str) -> sqlx::Result<String> {
        let owner: Option<String> =
            sqlx::query_scalar("SELECT owner_email FROM google_accounts WHERE email = $1")
                .bind(email.to_lowercase())
                .fetch_optional(&self.pool)
                .await?;
        Ok(owner.unwrap_or_else(|| email.to_lowercase()))
    }

    pub async fn mark_needs_reconnect(&self, email: &str) -> sqlx::Result<()> {
        sqlx::query("UPDATE google_accounts SET status = 'needs_reconnect', updated_at = now() WHERE email = $1")
            .bind(email.to_lowercase())
            .execute(&self.pool)
            .await?;
        self.cache.lock().await.remove(&email.to_lowercase());
        tracing::error!(
            account = email,
            "Google account needs to be reconnected (refresh token rejected)"
        );
        Ok(())
    }

    pub async fn invalidate(&self, email: &str) {
        self.cache.lock().await.remove(&email.to_lowercase());
    }

    /// A valid access token, refreshed from the stored refresh token when needed.
    pub async fn access_token(&self, email: &str) -> Result<String, GoogleError> {
        let email = email.to_lowercase();
        let mut cache = self.cache.lock().await;
        if let Some((tok, exp)) = cache.get(&email)
            && *exp - Duration::seconds(60) > Utc::now()
        {
            return Ok(tok.clone());
        }
        let acc = self.get(&email).await.map_err(anyhow::Error::from)?;
        let acc = match acc {
            Some(a) if a.status == "active" => a,
            _ => return Err(GoogleError::NotConnected(email)),
        };
        let refresh = self.secret_box.decrypt(&acc.refresh_token_enc)?;
        let resp = self
            .http
            .post(&self.token_url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.as_str()),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
            ])
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 400 || status.as_u16() == 401 {
            let body = resp.text().await.unwrap_or_default();
            if body.contains("invalid_grant") {
                drop(cache);
                self.mark_needs_reconnect(&email)
                    .await
                    .map_err(anyhow::Error::from)?;
                return Err(GoogleError::NeedsReconnect(email));
            }
            return Err(GoogleError::Http {
                status: status.as_u16(),
                body: body.chars().take(300).collect(),
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GoogleError::Http {
                status: status.as_u16(),
                body: body.chars().take(300).collect(),
            });
        }
        let r: RefreshResponse = resp.json().await?;
        let exp = Utc::now() + Duration::seconds(r.expires_in.unwrap_or(3600));
        cache.insert(email, (r.access_token.clone(), exp));
        Ok(r.access_token)
    }
}
