# Phase 4: Calendar

**Goal:** notify the owner about invitations and changes to upcoming events, and start the morning
digest with today's calendar.

Prerequisite: Phase 2 digest and scheduler.

## 4A Calendar source

Status: implemented.

Code and contracts: [Calendar spec](../spec/calendar.md).

- Poll each connected account's primary calendar every five minutes with `calendar.readonly`.
- Bootstrap a sync token without replaying historical events. Recover from HTTP 410 by rebuilding
  the baseline. Commit snapshots, change events, notification jobs and the next token atomically.
- Notify about events within the next 30 days, excluding declined events. Send silently outside
  configured model hours, normally 09:00–19:00 in the Mac's timezone.
- Render new events, invitations, updates and cancellations with localized templates. Compare
  fields only when a previous snapshot exists. Use native Telegram formatting entities for
  emphasis, compact local time ranges and a Google Calendar link.
- Fetch today's agenda live at digest time, including recurring instances and all-day events.

Implementation files:

- [`calendar/`](../../crates/gm/src/calendar/): ingestion, snapshots, notification rendering and agenda.
- [`google/calendar.rs`](../../crates/gm/src/google/calendar.rs): read-only Calendar API client.
- [`0008_calendar.sql`](../../crates/gm/migrations/0008_calendar.sql): cursor and snapshot tables.
- [`digest.rs`](../../crates/gm/src/digest.rs): agenda integration.
- [`locales/`](../../crates/gm/locales/): English, Vietnamese and Japanese strings.

## Checks

[`tests/calendar.rs`](../../crates/gm/tests/calendar.rs) covers bootstrap, pagination, cursor recovery,
rollback, duplicate versions, event filtering, recurring events, cancellation and agenda integration.
[`tests/calendar_serve.rs`](../../crates/gm/tests/calendar_serve.rs) starts the server binary with
isolated Postgres and fake Google and Telegram APIs. It checks database state, dashboard behavior
and the outgoing Telegram payload. Formatter tests in
[`calendar/message.rs`](../../crates/gm/src/calendar/message.rs) check Unicode spans, message limits
and Calendar link validation.

Run the automated checks:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

For a configured installation, run `cargo run --example verify_calendar`. See
[manual live verification](../spec/calendar.md#manual-live-verification) for prerequisites, command
behavior and event-specific checks. Automated tests never call real Google or Telegram endpoints.
