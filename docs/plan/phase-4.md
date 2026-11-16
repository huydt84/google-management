# Phase 4: Calendar

**Goal:** the owner hears about new invitations and changes to upcoming events, and the morning
digest starts with today's calendar.

Prerequisite: Phase 2 (digest, scheduler). Before starting, interview the owner on the open choices
(which calendars, how far ahead counts as "upcoming", whether declined events matter, quiet handling
at night).

## 4A Calendar source

**Owns:** `src/google/calendar.rs`, `src/calendar/` (new), migration `000N_calendar.sql`, calendar
strings in the locale files.

1. Scope `calendar.readonly` added to `REQUIRED_SCOPES` (owner reconnects).
2. `calendar_state(email, calendar_id, sync_token, updated_at)`. Bootstrap: a full list to get a sync
   token without creating events (old data is not replayed). Poll every 5 minutes with `syncToken`;
   `410 Gone` → re-bootstrap.
3. Each changed event becomes an `events` row with `source = calendar`,
   `external_id = <event id>:<updated>`, `kind` = `event.invitation` / `event.changed` /
   `event.cancelled`, and publishes the matching topic.
4. `notify_calendar`: template notification (no model needed): title, time in local time, organizer,
   what changed. Calendar descriptions are untrusted, like mail.
5. Digest section "Today": today's events from a live `events.list` at digest time.

Tests: wiremock Calendar API; bootstrap creates nothing; a change creates one event and one
notification; 410 recovery.
