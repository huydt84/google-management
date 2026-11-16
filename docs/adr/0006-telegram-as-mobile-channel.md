# 0006. Telegram as the mobile interaction channel

- Status: Accepted (amended 2026-11-16: Telegram sits behind a `Notifier` channel interface; Google Chat needs Workspace)
- Date: 2026-11-16

## Context

The owner wants to be notified on their phone and answer in a sentence or a tap. The server is not
reachable from the internet.

## Decision

Use a Telegram bot:

- Outbound messages via `sendMessage`, plain text only.
- Inbound (Phase 1) via **long polling** `getUpdates` — no webhook, so no public endpoint.
- Inline keyboard buttons for intents, approvals, feedback and undo; `callback_data` carries ids only
  (≤ 64 bytes).
- Only the configured chat id is served; everything else is ignored.
- Without Telegram configured, notifications go to the log.

## Consequences

- Free, cross-platform, push notifications, buttons, and replies with no app to build.
- Messages transit Telegram's servers (not end-to-end encrypted for bots): summaries and previews
  leave the machine in this form. Accepted; the owner can choose to keep notifications terse.
- The bot token is a secret and must never be logged.
- The channel is behind a small interface, so web push or another messenger can be added later.

## Alternatives considered

- **Web push / PWA:** needs HTTPS and a reachable origin for good mobile support.
- **Email to self:** slow, no buttons, and it feeds back into the mailbox being triaged.
- **Signal / WhatsApp:** no official, simple bot API for personal use.
