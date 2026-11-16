# 0015. Notification policy, night behavior and the 09:05 digest

- Status: Accepted
- Date: 2026-11-16

## Context

The owner wants few interruptions: important mail immediately, everything else once a day. The model
is resident 09:00–19:00 only ([0010](0010-llm-client-lanes-residency.md)). Card fraud and account
security can't wait until morning; most mail can.

## Decision

- **Priorities:** `urgent`, `high`, `normal` (sound) and `silent` (no sound). Any priority is delivered
  at any hour if the rule needs no LLM (`summarize: false`); otherwise in model hours. Telegram silent delivery uses `disable_notification`.
- **Night (19:00–09:00):** rule-only notifications (no LLM needed: card usage amounts, the Rakuten
  login notice) are delivered. Anything that needs the LLM is deferred to the 09:00 backlog,
  urgent included. *Amended 2026-11-16 (decisions P4, L12):* originally urgent rules notified at
  night with sender and subject only; the owner dropped that because such a ping duplicates the Gmail
  app. The same applies while the model is unavailable: summaries are queued (retried every 5 min)
  and no ping is sent without one.
- **Digest at 09:05** (user timezone, `digest_at` setting): sections for card usage, orders, coupons
  and points, newsletters, job alerts (deduplicated), account notices, items model triage put in the
  digest, pending approvals, and system health. Sent on Telegram and shown on the dashboard.
- **Batch timing:** daytime digest items are summarized on arrival in the `batch` lane and stored.
  The overnight backlog runs right after the model loads. Items unfinished at 09:05 appear as titles,
  and the digest message is edited when they finish.
- **Roadmap consequence:** the digest is built right after the Rust port, before reply/actions.

## Consequences

- Digest items need storage until sent (`digest_items` table) and the scheduler needs a
  "missed run executes once on wake" rule.
- Quiet-hours preferences (later) build on the same priority model.

## Alternatives considered

- **Notify everything important at any hour:** noisy at night.
- **One morning batch only:** a busy night could delay the digest.
