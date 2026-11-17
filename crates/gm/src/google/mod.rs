//! Thin hand-written Google clients (ADR-0009): OAuth (login + API consent), access tokens from
//! stored refresh tokens, the Gmail REST API and Cloud Pub/Sub.

pub mod accounts;
pub mod gmail;
pub mod oauth;
pub mod pubsub;

/// Scopes. Which ones are required depends on the config ([`crate::config::Config::required_scopes`]);
/// adding one means the owner reconnects (invariant 4).
pub const GMAIL_READONLY: &str = "https://www.googleapis.com/auth/gmail.readonly";
/// Only requested in Pub/Sub ingest mode (ADR-0002): lets gm create its topic and subscription
/// and pull notifications with the same encrypted refresh token.
pub const PUBSUB: &str = "https://www.googleapis.com/auth/pubsub";

#[derive(Debug, thiserror::Error)]
pub enum GoogleError {
    #[error("Google account {0} is not connected")]
    NotConnected(String),
    #[error("Google account {0} needs to be reconnected (refresh token rejected)")]
    NeedsReconnect(String),
    #[error("not found")]
    NotFound,
    #[error("Google API error {status}: {body}")]
    Http { status: u16, body: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<reqwest::Error> for GoogleError {
    fn from(e: reqwest::Error) -> Self {
        // reqwest errors can include the URL; Google URLs carry no secrets (tokens go in headers
        // or form bodies), so this is safe to log.
        GoogleError::Other(anyhow::Error::new(e.without_url()))
    }
}
