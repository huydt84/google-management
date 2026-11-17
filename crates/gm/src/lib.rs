//! Google Management: ingests Gmail, triages with rules, summarizes with a local LLM, notifies on
//! Telegram. See `docs/README.md` for the big picture.

pub mod app;
pub mod bus;
pub mod clock;
pub mod config;
pub mod crypto;
pub mod db;
pub mod digest;
pub mod eval;
pub mod feedback;
pub mod gmail;
pub mod google;
pub mod health;
pub mod i18n;
pub mod llm;
pub mod notify;
pub mod pipeline;
pub mod rules;
pub mod scheduler;
pub mod secrets;
pub mod telegram;
pub mod triage_model;
pub mod user_settings;
pub mod web;

/// Bus topics. See `docs/spec/events.md`.
pub mod topics {
    pub const GMAIL_MESSAGE_RECEIVED: &str = "gmail.message.received";
    pub const GMAIL_MESSAGE_CLASSIFY: &str = "gmail.message.classify";
    pub const GMAIL_MESSAGE_SUMMARIZE: &str = "gmail.message.summarize";
    pub const NOTIFY_EMAIL: &str = "notify.email";
    pub const DIGEST_ITEM_SUMMARIZE: &str = "digest.item.summarize";
    pub const DIGEST_REFRESH: &str = "digest.refresh";
    pub const SCHEDULE_DUE: &str = "schedule.due";
    pub const FEEDBACK_RECEIVED: &str = "feedback.received";
}
