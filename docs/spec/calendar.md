# Spec: Calendar

Code: [`calendar/mod.rs`](../../crates/gm/src/calendar/mod.rs),
[`calendar/message.rs`](../../crates/gm/src/calendar/message.rs),
[`google/calendar.rs`](../../crates/gm/src/google/calendar.rs),
[`google/client.rs`](../../crates/gm/src/google/client.rs),
[`app.rs`](../../crates/gm/src/app.rs), [`digest.rs`](../../crates/gm/src/digest.rs).

## Consent and polling

Watch the primary calendar of each connected account. `calendar.readonly` is required by
`Config::required_scopes`; the owner reconnects through the dashboard to grant it. Accounts without
that scope keep their mail functionality and are skipped by Calendar. The existing dashboard's
missing-scope notice asks for reconnection. Enable the Calendar API in the OAuth client's Google
Cloud project before reconnecting.

The supervised Calendar loop polls immediately on startup and every five minutes. An initial full
`events.list` establishes a sync token and minimal snapshots without replaying existing events.
Incremental requests use that token with `singleEvents=false` and `showDeleted=true`. All pages
must succeed before snapshots, new events, bus jobs and the next token commit together. An advisory
transaction lock serializes syncs of the same account and calendar across processes. HTTP 410
rebuilds the baseline without notifications. Other failures preserve the old cursor and record
`calendar_state.last_error`; the next poll retries.

Google's contract: [incremental synchronization](https://developers.google.com/workspace/calendar/api/guides/sync)
and [`events.list`](https://developers.google.com/workspace/calendar/api/v3/reference/events/list).
Authenticated requests and the one retry after HTTP 401 are shared with Gmail.

## Changes and notifications

The notification window is the next 30 days, including events already in progress. Declined events
are excluded. A separate live `events.list` with `singleEvents=true`, `timeMin` and `timeMax` finds
the next instance of recurring series, including series whose first event is in the past. Sync
requests never combine a sync token with these time filters.

Each changed resource creates an `events` row with `source=calendar` and an external id of
`<event id>:<updated>`. A sparse cancellation without `updated` uses the returned next sync token
as its version. An unseen event organized by someone else is `event.invitation`; a cancellation is
`event.cancelled`; other records are `event.changed`. Existing rows suppress duplicate deliveries.
Only relevant changes publish a notification job. Out-of-window, declined, and metadata-only
changes have status `ignored`. A move out of the window is relevant when the previous time was
inside it. A recurring series moved out of the window says that no occurrence remains in the next
30 days instead of displaying its original, past start time. Cancellation uses the last snapshot
when Google omits the title or dates.

Notification topics are `calendar.event.invitation`, `calendar.event.changed`, and
`calendar.event.cancelled`. The `notify_calendar_invitation`, `notify_calendar_changed`, and `notify_calendar_cancelled`
subscribers share one handler that locks the event, checks `ready`, sends
the notification, and marks it `notified`. As with mail, a crash after Telegram accepts a send but
before the database commits can resend it. Normal retries and overlapping workers check the stored
state. No model is called and no Calendar object is modified.

Templates in en, vi and ja distinguish new events, invitations, updates and cancellations. New
personal events have a "New event" heading and no change list. Only an existing snapshot can produce
changed fields. A bold heading and title lead the message, followed by a compact local time range,
organizer ("You" for the owner), optional location, then actual changes. Old values are struck
through and new values are bold. A safe Google Calendar link appears when available.
Title, location and time edits show old and new values. Description edits show only that the
description changed. All-day event end dates are exclusive in the API and inclusive in the text.
Messages use literal text and explicit Telegram entities for emphasis, strikethrough and the
Calendar link. Neither HTML nor Markdown is parsed. Entity offsets and lengths use UTF-16 units;
long messages are clipped with their formatting spans kept inside the result. Google text remains
literal, and links must use HTTPS on Google's Calendar website. Messages have no mail-action buttons.
Notifications are silent outside configured model hours, normally 09:00–19:00 in the Mac's timezone.
The existing `/pause` command continues to hold mail notifications only.

## Today's digest

At digest creation, query the local day's midnight-to-midnight interval live with `events.list`.
Follow every page, expand recurring instances, and omit declined and cancelled events. Show a
"Today" section immediately after the digest title, before mail sections, ordered by start time.
All-day events and events spanning midnight are included. A calendar-only digest is sent even with
no mail; an empty agenda adds no section. The digest's `item_count` still counts mail items.

An API failure leaves the digest job retryable instead of presenting an incomplete agenda. Existing
accounts without Calendar consent do not block the mail digest. A same-day late-summary edit also
fetches the agenda live.

## Stored data

`calendar_state` holds the sync token, last success and last error for each account and calendar.
`calendar_events` holds minimal snapshots for change detection, including recurring-series metadata
and the next occurrence's times. Snapshots omit descriptions and attendee lists. Description hashes
detect edits; one boolean records whether the owner declined. `events.data` holds the current and
previous snapshot and changed field names. Event rows follow normal retention. Snapshots outside the
window expire after the configured retention period without an update; snapshots in the window
are refreshed by polling. No raw Calendar content belongs in git.

The dashboard displays Calendar titles and notification details. Mail feedback, local mail eval,
ignored-sender counts and mail recipes accept only Gmail events. Calendar rows offer no mail
feedback or eval controls.

## Manual live verification

Code: [`examples/verify_calendar.rs`](../../crates/gm/examples/verify_calendar.rs).

Run `cargo run --example verify_calendar` from the repository on the owner's Mac. This manual command
uses the app's saved Google account and existing client, checks Calendar consent, runs normal
primary-calendar sync, queries today's live agenda and the next 30 days, and checks the authenticated
local dashboard. It prints counts and sync health, without tokens or private event details. It
updates local sync state like an ordinary poll; legitimate new changes can be delivered by the
running service. It does not modify Google Calendar or send synthetic Telegram messages. Do not
run it from automated tests.

Pass `--event-id <Google event id>` to restrict the change-state report to one event.
The command also checks whether that event is present in Google's upcoming results and in the live
agenda for its date. This option does not create, rename or delete an event.
