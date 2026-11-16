//! Gmail REST API (v1): read-only calls used by ingestion and the pipeline, plus `users.watch`
//! (push notifications to Pub/Sub, allowed by `gmail.readonly`).

use std::sync::Arc;

use serde_json::Value;

use super::GoogleError;
use super::accounts::AccountStore;

#[derive(Clone)]
pub struct GmailClient {
    http: reqwest::Client,
    base: String,
    accounts: Arc<AccountStore>,
}

impl GmailClient {
    pub fn new(http: reqwest::Client, base: &str, accounts: Arc<AccountStore>) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            accounts,
        }
    }

    pub fn accounts(&self) -> &Arc<AccountStore> {
        &self.accounts
    }

    async fn get(
        &self,
        email: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value, GoogleError> {
        self.request(email, reqwest::Method::GET, path, query, None)
            .await
    }

    async fn request(
        &self,
        email: &str,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value, GoogleError> {
        for attempt in 0..2 {
            let token = self.accounts.access_token(email).await?;
            let mut req = self
                .http
                .request(method.clone(), format!("{}/users/me/{path}", self.base))
                .bearer_auth(&token)
                .query(query);
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().await?;
            let status = resp.status().as_u16();
            match status {
                200..=299 => return Ok(resp.json().await?),
                404 => return Err(GoogleError::NotFound),
                401 if attempt == 0 => {
                    self.accounts.invalidate(email).await;
                    continue;
                }
                _ => {
                    let body: String = resp
                        .text()
                        .await
                        .unwrap_or_default()
                        .chars()
                        .take(300)
                        .collect();
                    return Err(GoogleError::Http { status, body });
                }
            }
        }
        unreachable!()
    }

    pub async fn profile_history_id(&self, email: &str) -> Result<i64, GoogleError> {
        let v = self.get(email, "profile", &[]).await?;
        parse_i64(&v["historyId"])
            .ok_or_else(|| GoogleError::Other(anyhow::anyhow!("profile without historyId")))
    }

    pub async fn history_page(
        &self,
        email: &str,
        start: i64,
        page_token: Option<&str>,
    ) -> Result<Value, GoogleError> {
        let mut q = vec![
            ("startHistoryId", start.to_string()),
            ("historyTypes", "messageAdded".to_string()),
        ];
        if let Some(t) = page_token {
            q.push(("pageToken", t.to_string()));
        }
        self.get(email, "history", &q).await
    }

    pub async fn list_messages(
        &self,
        email: &str,
        labels: &[String],
        q: &str,
        max: u32,
    ) -> Result<Value, GoogleError> {
        let mut query: Vec<(&str, String)> =
            labels.iter().map(|l| ("labelIds", l.clone())).collect();
        query.push(("q", q.to_string()));
        query.push(("maxResults", max.to_string()));
        self.get(email, "messages", &query).await
    }

    /// Start or renew push notifications for `labels` to a Pub/Sub topic. Returns the watch
    /// expiration (Gmail requires renewal at least every 7 days).
    pub async fn watch(
        &self,
        email: &str,
        topic_name: &str,
        labels: &[String],
    ) -> Result<chrono::DateTime<chrono::Utc>, GoogleError> {
        let body = serde_json::json!({
            "topicName": topic_name, "labelIds": labels, "labelFilterBehavior": "INCLUDE"
        });
        let v = self
            .request(email, reqwest::Method::POST, "watch", &[], Some(&body))
            .await?;
        parse_i64(&v["expiration"])
            .and_then(chrono::DateTime::from_timestamp_millis)
            .ok_or_else(|| GoogleError::Other(anyhow::anyhow!("watch response without expiration")))
    }

    pub async fn get_message(&self, email: &str, id: &str) -> Result<Value, GoogleError> {
        self.get(
            email,
            &format!("messages/{id}"),
            &[("format", "full".to_string())],
        )
        .await
    }
}

/// Gmail returns 64-bit ids as JSON strings.
pub fn parse_i64(v: &Value) -> Option<i64> {
    v.as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v.as_i64())
}
