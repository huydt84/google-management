//! Authenticated Google JSON requests, shared by Gmail and Calendar.

use serde_json::Value;

use super::{GoogleError, accounts::AccountStore};

pub(super) async fn request(
    http: &reqwest::Client,
    accounts: &AccountStore,
    email: &str,
    url: &str,
    method: reqwest::Method,
    query: &[(&str, String)],
    body: Option<&Value>,
) -> Result<Value, GoogleError> {
    for attempt in 0..2 {
        let token = accounts.access_token(email).await?;
        let mut req = http
            .request(method.clone(), url)
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
                accounts.invalidate(email).await;
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
