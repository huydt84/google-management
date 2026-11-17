# Spec: events and topics

The messages that flow through the event bus: names, payloads, producers, consumers and delivery
rules. Bus design: [architecture §8](../architecture.md#8-event-bus). Code: `crates/gm/src/bus.rs`,
topic constants in `crates/gm/src/lib.rs` (`topics`).

## Delivery semantics (all topics)

| Property | Rule |
|----------|------|
| Publish | `bus.publish(&mut conn, topic, &payload)` inside the producer's transaction. Messages exist only if it commits. |
| Fan-out | One `jobs` row per subscriber. A topic with no subscribers logs a warning and drops the message. |
| Delivery | At-least-once. Handlers are idempotent: they check the target row's state first. |
| Ordering | Best effort by job id; handlers must not rely on order. |
| Outcome | `Done`; `Defer(t)` puts the job back to `pending` at `t` without counting an attempt (used for model hours); an error retries. |
| Retry | Delay 5 s × 2^(attempt−1), capped at 900 s; `GM_JOB_MAX_ATTEMPTS` (5). |
| Dead letter | After the last failed attempt the job is `dead` with `last_error`. Handlers may check `ctx.is_last_attempt()` to degrade instead of failing. |
| Stale jobs | `running` jobs are requeued at startup, and every 5 min if locked for more than 10 min. |
| Payload | JSON with ids, not content. Mail bodies are re-fetched by the handler. |

## Naming

`<domain>.<entity>.<event>` in lowercase: past tense for facts (`gmail.message.received`), imperative
for commands to a stage (`gmail.message.summarize`, `notify.email`).

## Topic catalog

### Phase 1 (implemented)

| Topic | Producer | Subscriber | Payload | Idempotency check |
|-------|----------|------------|---------|-------------------|
| `gmail.message.received` | `GmailIngestor::commit` | `triage` | `{event_id, account, message_id, thread_id}` | event status `new` (row locked) |
| `gmail.message.summarize` | `triage` (notify/triage with summary) | `summarize` | same | event status `triaged` |
| `notify.email` | `triage` (no summary) or `summarize` | `notify` | `{event_id}` | event status `ready` |

### Phase 2 (implemented)

| Topic | Producer | Subscriber | Payload | Idempotency check |
|-------|----------|------------|---------|-------------------|
| `gmail.message.classify` | `triage` (action `triage`) | `classify` (model triage, notify lane) | as `gmail.message.received` | event status `classify` (row locked) |

### Phase 2 (planned)

| Topic | Producer | Subscriber | Payload |
|-------|----------|------------|---------|
| `digest.item.summarize` | `triage` (digest with `summarize: true`) | `digest_summarize` (batch lane) | `{event_id}` |
| `schedule.due` | scheduler | digest builder, residency controller | `{kind, at}` |
| `feedback.received` | Telegram poller | feedback handler | `{event_id, label}` |

### Phase 3 (planned)

| Topic | Producer | Subscriber | Payload |
|-------|----------|------------|---------|
| `user.instruction` | Telegram poller | task runtime | `{event_id?, task_id?, text?, intent?, args, chat_message_id?}` |
| `task.reply` | task runtime | Telegram sender | `{task_id, text, buttons?, reply_to_event_id?}` |
| `approval.decision` | Telegram poller, web | task runtime | `{approval_id, decision, via}` |

### Phase 4 (planned)

| Topic | Producer | Subscriber | Payload |
|-------|----------|------------|---------|
| `calendar.event.invitation` / `.changed` / `.cancelled` | Calendar source | `notify_calendar` | `{event_id, account, calendar_event_id}` |

## Event record (`events` table)

Each Google object becomes one row, keyed by `(source, external_id)`.

| source | kind | external_id | Phase |
|--------|------|-------------|-------|
| `gmail` | `message.received` | Gmail message id | 1 |
| `calendar` | `event.*` | `<calendar event id>:<updated>` | 4 |

`events.data` for Gmail, written by `triage`: `from`, `from_addr`, `from_name`, `subject`, `date`,
`snippet`, `labels`, `thread_id`, `attachments`, `link`, and `verdict`
(`{rule, action, priority, summarize, section, message, extracted}`). The full body is never stored.

`events.summary`: the summary JSON (`summary`, `importance`, `category`, `suggested_actions`,
`language`, `parse_ok`). Nothing is notified without it: an unavailable model defers the job every
`GM_LLM_UNAVAILABLE_RETRY_S` (300 s), and other errors leave the job `dead`.

Statuses: `new → ignored | digest | classify | triaged → ready → notified`, `new → ready`
(rule-only), `classify → ignored | digest | triaged | ready` (model triage, as if a rule had said
that action), `new | classify | triaged → gone`.

Model triage (`classify`, `src/triage_model.rs`, `prompts/triage.md`): headers and the first ~1,500
tokens of the cleaned body, notify lane, `/no_think`, at most 80 output tokens, JSON
`{action: notify|digest|ignore, reason}` parsed tolerantly with one retry. Unusable output → `notify`
(fail open). Outside model hours it defers to the next opening, while the model is unavailable every
`GM_LLM_UNAVAILABLE_RETRY_S`. The verdict is `{rule: "model", priority: normal, summarize: action !=
ignore, section: "model" for digest, reason}`. Diagram: [architecture §6](../architecture.md#6-state-machines).

## Gmail ingestion contract

| Step | Behavior |
|------|----------|
| Bootstrap | On connect: `users.getProfile` → store `historyId`; only later changes are processed. |
| Triggers | Pub/Sub notification (mode `pubsub`), the poll loop, and `POST /api/gmail/sync`. The poll loop runs every `GM_GMAIL_POLL_INTERVAL_S` (60 s) in `poll` mode or while Pub/Sub is unhealthy, and every `GM_GMAIL_SAFETY_POLL_INTERVAL_S` (900 s) while Pub/Sub is healthy. |
| Sync | `history.list(startHistoryId, historyTypes=messageAdded)`, all pages; keep messages whose labels intersect `GM_GMAIL_LABEL_IDS` (default `INBOX`). |
| Commit | One transaction: insert events (`ON CONFLICT DO NOTHING`), publish `gmail.message.received` for new rows, `history_id = GREATEST(old, new)`, clear `last_error`. |
| Expired cursor (404) | Re-anchor at the profile `historyId`; list `in:inbox newer_than:2d` (max 50), oldest first; dedupe applies. |
| Errors | Stored in `gmail_state.last_error` and shown on the dashboard; `invalid_grant` marks the account `needs_reconnect`. |
| Concurrency | One lock per account; Pub/Sub, poll and manual syncs never overlap. |
| Pub/Sub provisioning | With the token of an account that granted the `pubsub` scope: create topic `GM_GMAIL_PUBSUB_TOPIC` in `GM_GOOGLE_PROJECT_ID` (409 = exists), add `roles/pubsub.publisher` for `serviceAccount:gmail-api-push@system.gserviceaccount.com` to its IAM policy if missing, create pull subscription `GM_GMAIL_PUBSUB_SUBSCRIPTION` (ack deadline 60 s, retention 7 days). |
| Watch | `users.watch` on each active mailbox for `GM_GMAIL_LABEL_IDS`; renewed when less than 6 days remain (checked before every pull); expiration in `gmail_state.watch_expiration`. |
| Pull | Up to 20 messages per pull (about a minute of waiting). Data is base64 JSON `{emailAddress, historyId}`; sync each named active mailbox; acknowledge the whole batch only if every sync succeeded (otherwise it is redelivered after the ack deadline). Unknown or malformed messages are acknowledged and dropped. |
| Pub/Sub failure | Error shown on the dashboard, poll interval back to 60 s, retry with backoff 5 s doubling to 5 min. A missing `pubsub` scope asks the owner to reconnect Gmail. |
