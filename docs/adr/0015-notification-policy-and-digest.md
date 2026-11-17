# 0015. Notification policy, night behavior and the 09:05 digest

- Status: Accepted
- Date: 2026-11-16

## Context

The owner wants few interruptions: important mail immediately, everything else once a day. gm runs
model work 09:00–19:00 only ([0010](0010-llm-client-lanes-residency.md)). Card fraud and account
security can't wait until morning; most mail can. Measured volume: about 20 mails a day, most of them
digest material.

## Decision

- **Priorities:** `urgent`, `high`, `normal` (sound) and `silent` (no sound). Any priority is delivered
  at any hour if the rule needs no LLM (`summarize: false`); otherwise in model hours. Telegram silent delivery uses `disable_notification`.
- **Night (19:00–09:00):** rule-only notifications (no LLM needed: card usage amounts, the Rakuten
  login notice) are delivered. Anything that needs the LLM is deferred to the 09:00 backlog,
  urgent included. *Amended 2026-11-16:* originally urgent rules notified at night with sender and
  subject only; the owner dropped that because such a ping duplicates the Gmail app. The same
  applies while the model is unavailable: summaries are queued (retried every 5 min) and no ping is
  sent without one. Answers to the owner's own requests (translate, analyze, reply) are not
  notifications and go out at any hour.
- **Pause:** `/pause` in Telegram holds mail notifications (they wait, nothing is dropped) until
  `/resume`, which sends what was held at once. The digest, health alerts and answers to the owner's
  requests are not paused.
- **Digest at 09:05** by default; each user can move it in Settings (`digest_at`, must fall inside
  model hours). One **silent** Telegram message (nothing in it is urgent), also shown on `/digest`.
  Sections follow `rules.yaml`; mail the model put in the digest comes last as "Other mail" with the
  model's reason, so its decisions are easy to check. A "System" section appears only while a health
  check is failing. An empty digest sends nothing. The first digest only covers the last 24 hours.
- **Feedback in the digest:** model items are numbered, with one 👍/👎 row each under the message (at
  most 8; the rest are rated on the dashboard).
- **Batch timing:** daytime digest items are summarized on arrival in the `batch` lane and stored.
  The overnight backlog runs from 09:00. Items unfinished at send time appear as titles, and the
  message is edited as they finish, until model hours end that day.
- **Roadmap consequence:** the digest is built right after the Rust port, before reply/actions.

Code: [`digest.rs`](../../crates/gm/src/digest.rs), [`scheduler.rs`](../../crates/gm/src/scheduler.rs),
[`pipeline.rs`](../../crates/gm/src/pipeline.rs) (night policy); message format in the
[interfaces spec](../spec/interfaces.md#digest-phase-2).

## Consequences

- Digest runs are stored in `digests`, and each event points at its digest (`events.digest_id`);
  the scheduler fires a time missed during sleep once on wake (`schedule_runs`).
- Quiet-hours preferences (later) build on the same priority model.

## Alternatives considered

- **Notify everything important at any hour:** noisy at night.
- **One morning batch only:** a busy night could delay the digest.
