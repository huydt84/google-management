# 0002. Ingest Gmail by polling or pulling, no public webhooks

- Status: Accepted
- Date: 2026-11-16
- Note (2026-11-16): in the Rust port ([0009](0009-rust-single-binary.md)) the owner chose Pub/Sub
  mode as the default setup (decision A7). gm creates the topic, subscription and the publisher
  permission for `gmail-api-push@system.gserviceaccount.com` itself, using the Gmail consent with the
  added `pubsub` scope (no gcloud, no Application Default Credentials). The safety poll runs every
  15 minutes while Pub/Sub is healthy and every 60 s while it isn't. Watches are renewed when less
  than 6 days remain.

## Context

Gmail announces mailbox changes through Cloud Pub/Sub (`users.watch`). A Pub/Sub *push* subscription
— and Calendar/Drive push channels — need a public HTTPS endpoint. gm runs on a personal Mac behind
NAT and must not be exposed to the internet. Notifications only carry a `historyId`, never the mail
itself.

## Decision

- Default mode **poll**: call `history.list` from the stored cursor every 60 s.
- Optional mode **pubsub**: `users.watch` publishes to a topic; gm **pulls** the subscription, then
  runs the same sync; acknowledges only after the sync committed; also runs a 15-minute safety poll.
- Both modes share one idempotent `sync()` that writes events, bus messages and the new cursor in one
  transaction.
- Calendar and Drive (Phase 2) use incremental `syncToken` / `changes` polling for the same reason.

## Consequences

- No inbound connectivity, no tunnel, no certificates.
- Poll mode adds up to 60 s latency; Pub/Sub mode reduces it to seconds at the cost of a Google
  Cloud project setup and Application Default Credentials.
- Downtime is recoverable: the cursor resumes, Pub/Sub retains messages for 7 days, and an expired
  history cursor triggers a 2-day recovery.
- The watch must be renewed (gm renews when < 2 days remain).

## Alternatives considered

- **Push subscription through a tunnel** (Cloudflare Tunnel, ngrok): exposes an endpoint and adds a
  dependency; rejected for security and simplicity.
- **IMAP IDLE:** real-time, but needs IMAP access and broader credentials, and loses Gmail labels and
  history semantics.
