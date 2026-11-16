//! Cloud Pub/Sub REST client (v1) for Gmail push notifications (ADR-0002, decision A7). gm creates
//! its own topic, the publisher permission for Gmail's service account and a pull subscription, then
//! pulls and acknowledges messages. Calls use the owner's Google token with the `pubsub` scope.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};

use super::GoogleError;
use super::accounts::AccountStore;
use crate::config::PubSubConfig;

/// Gmail publishes change notifications as this service account.
pub const GMAIL_PUBLISHER: &str = "serviceAccount:gmail-api-push@system.gserviceaccount.com";
const PUBLISHER_ROLE: &str = "roles/pubsub.publisher";
const PULL_TIMEOUT: Duration = Duration::from_secs(60);

/// One pulled message: the ack id and Gmail's payload `{emailAddress, historyId}`, if parseable.
#[derive(Debug, Clone)]
pub struct Pulled {
    pub ack_id: String,
    pub email: Option<String>,
}

#[derive(Clone)]
pub struct PubSubClient {
    http: reqwest::Client,
    cfg: PubSubConfig,
    accounts: Arc<AccountStore>,
}

impl PubSubClient {
    pub fn new(http: reqwest::Client, cfg: PubSubConfig, accounts: Arc<AccountStore>) -> Self {
        Self {
            http,
            cfg,
            accounts,
        }
    }

    pub fn config(&self) -> &PubSubConfig {
        &self.cfg
    }

    /// `Ok(None)` for 409 Already Exists, so "create if missing" calls are idempotent.
    async fn call(
        &self,
        email: &str,
        method: reqwest::Method,
        path: &str,
        body: &Value,
        timeout: Option<Duration>,
    ) -> Result<Option<Value>, GoogleError> {
        for attempt in 0..2 {
            let token = self.accounts.access_token(email).await?;
            let mut req = self
                .http
                .request(method.clone(), format!("{}/{path}", self.cfg.api_base))
                .bearer_auth(&token);
            // GET methods (getIamPolicy) take no body; Google answers a POST there with a 404 page.
            if method != reqwest::Method::GET {
                req = req.json(body);
            }
            if let Some(t) = timeout {
                req = req.timeout(t);
            }
            let resp = req.send().await?;
            let status = resp.status().as_u16();
            match status {
                200..=299 => return Ok(Some(resp.json().await?)),
                409 => return Ok(None),
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

    /// Create the topic, let Gmail publish to it, and create the pull subscription (each only if
    /// missing).
    pub async fn provision(&self, email: &str) -> Result<(), GoogleError> {
        let topic = self.cfg.topic_name();
        self.call(email, reqwest::Method::PUT, &topic, &json!({}), None)
            .await?;

        let policy = self
            .call(
                email,
                reqwest::Method::GET,
                &format!("{topic}:getIamPolicy"),
                &json!({}),
                None,
            )
            .await?
            .unwrap_or_else(|| json!({}));
        if let Some(updated) = with_publisher(policy) {
            self.call(
                email,
                reqwest::Method::POST,
                &format!("{topic}:setIamPolicy"),
                &json!({ "policy": updated }),
                None,
            )
            .await?;
            tracing::info!(topic, "pubsub: granted Gmail publish permission");
        }

        self.call(
            email,
            reqwest::Method::PUT,
            &self.cfg.subscription_name(),
            &json!({
                "topic": topic,
                "ackDeadlineSeconds": 60,
                "messageRetentionDuration": "604800s",
            }),
            None,
        )
        .await?;
        Ok(())
    }

    /// Wait up to about a minute for messages. A client-side timeout counts as "no messages".
    pub async fn pull(&self, email: &str, max: u32) -> Result<Vec<Pulled>, GoogleError> {
        let path = format!("{}:pull", self.cfg.subscription_name());
        let resp = match self
            .call(
                email,
                reqwest::Method::POST,
                &path,
                &json!({ "maxMessages": max }),
                Some(PULL_TIMEOUT),
            )
            .await
        {
            Ok(v) => v.unwrap_or_default(),
            Err(GoogleError::Other(e)) if is_timeout(&e) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(resp["receivedMessages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let ack_id = m["ackId"].as_str()?.to_string();
                Some(Pulled {
                    ack_id,
                    email: decode_email(m["message"]["data"].as_str().unwrap_or_default()),
                })
            })
            .collect())
    }

    pub async fn ack(&self, email: &str, ack_ids: &[String]) -> Result<(), GoogleError> {
        if ack_ids.is_empty() {
            return Ok(());
        }
        let path = format!("{}:acknowledge", self.cfg.subscription_name());
        self.call(
            email,
            reqwest::Method::POST,
            &path,
            &json!({ "ackIds": ack_ids }),
            None,
        )
        .await?;
        Ok(())
    }
}

fn is_timeout(e: &anyhow::Error) -> bool {
    e.downcast_ref::<reqwest::Error>()
        .is_some_and(reqwest::Error::is_timeout)
}

/// Add the Gmail publisher binding to an IAM policy; `None` if it is already there.
pub fn with_publisher(mut policy: Value) -> Option<Value> {
    let bindings = policy
        .as_object_mut()?
        .entry("bindings")
        .or_insert_with(|| json!([]));
    let list = bindings.as_array_mut()?;
    if let Some(b) = list.iter_mut().find(|b| b["role"] == PUBLISHER_ROLE) {
        let members = b
            .as_object_mut()?
            .entry("members")
            .or_insert_with(|| json!([]))
            .as_array_mut()?;
        if members.iter().any(|m| m == GMAIL_PUBLISHER) {
            return None;
        }
        members.push(json!(GMAIL_PUBLISHER));
    } else {
        list.push(json!({ "role": PUBLISHER_ROLE, "members": [GMAIL_PUBLISHER] }));
    }
    Some(policy)
}

/// Gmail's notification data is base64 JSON `{"emailAddress": "...", "historyId": 123}`.
pub fn decode_email(data: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(data))
        .ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    Some(v["emailAddress"].as_str()?.trim().to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_binding_is_added_once() {
        let p = with_publisher(json!({"etag": "x"})).unwrap();
        assert_eq!(p["bindings"][0]["members"][0], GMAIL_PUBLISHER);
        assert!(with_publisher(p.clone()).is_none());
        let other = json!({"bindings": [{"role": PUBLISHER_ROLE, "members": ["user:a@b.c"]}]});
        let p = with_publisher(other).unwrap();
        assert_eq!(p["bindings"][0]["members"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn decodes_gmail_notification() {
        let data = base64::engine::general_purpose::STANDARD
            .encode(r#"{"emailAddress":"Owner@Example.com","historyId":9}"#);
        assert_eq!(decode_email(&data).as_deref(), Some("owner@example.com"));
        assert_eq!(decode_email("not base64!"), None);
    }
}
