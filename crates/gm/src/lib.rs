//! Google Management: ingests Gmail, triages with rules, summarizes with a local LLM, notifies on
//! Telegram. See `docs/README.md` for the big picture.

pub mod app;
pub mod bus;
pub mod clock;
pub mod config;
pub mod crypto;
pub mod db;
pub mod gmail;
pub mod google;
pub mod i18n;
pub mod llm;
pub mod notify;
pub mod pipeline;
pub mod rules;
pub mod secrets;
pub mod user_settings;
pub mod web;

/// Bus topics. See `docs/spec/events.md`.
pub mod topics {
    pub const GMAIL_MESSAGE_RECEIVED: &str = "gmail.message.received";
    pub const GMAIL_MESSAGE_SUMMARIZE: &str = "gmail.message.summarize";
    pub const NOTIFY_EMAIL: &str = "notify.email";
}
