//! Gmail REST API (v1): read-only calls used by ingestion and the pipeline, `users.watch` (push
//! notifications to Pub/Sub, allowed by `gmail.readonly`), and drafts (`gmail.compose`) for the
//! reply recipe. Only an approved executor sends a draft; `messages.send` is never called.

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
        super::client::request(
            &self.http,
            &self.accounts,
            email,
            &format!("{}/users/me/{path}", self.base),
            method,
            query,
            body,
        )
        .await
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

    /// Thread with every message (`format=full`).
    pub async fn get_thread(&self, email: &str, id: &str) -> Result<Value, GoogleError> {
        self.get(
            email,
            &format!("threads/{id}"),
            &[("format", "full".to_string())],
        )
        .await
    }

    /// `drafts.create` in `thread_id` from a base64url RFC 5322 message; returns the draft id.
    pub async fn create_draft(
        &self,
        email: &str,
        raw: &str,
        thread_id: &str,
    ) -> Result<String, GoogleError> {
        let body = serde_json::json!({"message": {"raw": raw, "threadId": thread_id}});
        let v = self
            .request(email, reqwest::Method::POST, "drafts", &[], Some(&body))
            .await?;
        draft_id(&v)
    }

    /// `drafts.update`: replace the draft's message; returns the draft id.
    pub async fn update_draft(
        &self,
        email: &str,
        id: &str,
        raw: &str,
        thread_id: &str,
    ) -> Result<String, GoogleError> {
        let body = serde_json::json!({"id": id, "message": {"raw": raw, "threadId": thread_id}});
        let v = self
            .request(
                email,
                reqwest::Method::PUT,
                &format!("drafts/{id}"),
                &[],
                Some(&body),
            )
            .await?;
        draft_id(&v)
    }

    /// `drafts.get` (`format=full`): `{id, message}`.
    pub async fn get_draft(&self, email: &str, id: &str) -> Result<Value, GoogleError> {
        self.get(
            email,
            &format!("drafts/{id}"),
            &[("format", "full".to_string())],
        )
        .await
    }

    /// `drafts.send`: returns the sent message (`{id, threadId, labelIds}`).
    pub async fn send_draft(&self, email: &str, id: &str) -> Result<Value, GoogleError> {
        let body = serde_json::json!({"id": id});
        self.request(
            email,
            reqwest::Method::POST,
            "drafts/send",
            &[],
            Some(&body),
        )
        .await
    }
}

fn draft_id(v: &Value) -> Result<String, GoogleError> {
    v["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| GoogleError::Other(anyhow::anyhow!("draft response without id")))
}

/// Gmail returns 64-bit ids as JSON strings.
pub fn parse_i64(v: &Value) -> Option<i64> {
    v.as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v.as_i64())
}
