//! Read-only Calendar v3 calls. Sync queries keep the same parameters across every page.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::{GoogleError, accounts::AccountStore};

#[derive(Clone)]
pub struct CalendarClient {
    http: reqwest::Client,
    base: String,
    accounts: Arc<AccountStore>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventPage {
    #[serde(default)]
    pub items: Vec<Value>,
    pub next_page_token: Option<String>,
    pub next_sync_token: Option<String>,
}

impl CalendarClient {
    pub fn new(http: reqwest::Client, base: &str, accounts: Arc<AccountStore>) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            accounts,
        }
    }

    pub async fn events(
        &self,
        email: &str,
        calendar_id: &str,
        query: &[(&str, String)],
    ) -> Result<EventPage, GoogleError> {
        let mut query = query.to_vec();
        let mut items = Vec::new();
        loop {
            let mut page = self.events_page(email, calendar_id, &query).await?;
            items.append(&mut page.items);
            match page.next_page_token {
                Some(token) => {
                    query.retain(|(key, _)| *key != "pageToken");
                    query.push(("pageToken", token));
                }
                None => return Ok(EventPage { items, ..page }),
            }
        }
    }

    async fn events_page(
        &self,
        email: &str,
        calendar_id: &str,
        query: &[(&str, String)],
    ) -> Result<EventPage, GoogleError> {
        let mut url =
            url::Url::parse(&format!("{}/calendars/", self.base)).map_err(anyhow::Error::from)?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Calendar base URL must support path segments"))?
            .pop_if_empty()
            .push(calendar_id)
            .push("events");
        let value = super::client::request(
            &self.http,
            &self.accounts,
            email,
            url.as_str(),
            reqwest::Method::GET,
            query,
            None,
        )
        .await?;
        serde_json::from_value(value).map_err(|e| GoogleError::Other(e.into()))
    }
}
