# Spec: data model

Code: [`migrations/`](../../crates/gm/migrations/), [`src/db.rs`](../../crates/gm/src/db.rs). Each table names its migration; the migrations are the source of truth.

Postgres 17 (Docker, `127.0.0.1:5433`, db/user/password `gm`). Timestamps are `timestamptz`. The
schema is managed by sqlx migrations in `crates/gm/migrations/`, embedded in the binary and applied by
`gm serve` and `gm migrate`. Queries are written with `sqlx::query` / `query_as` (checked at runtime
and by the test suite). Diagram: [architecture §7](../architecture.md#7-data-model).

## Phase 1 (implemented, `0001_init.sql`, `0002_gmail_watch.sql`)

### `google_accounts`

| Column | Type | Notes |
|--------|------|-------|
| `email` | text PK | the Google account, lowercased |
| `owner_email` | text | the gm user who connected it (multi-user ready, [ADR-0008](../adr/0008-per-user-settings.md)) |
| `refresh_token_enc` | text | AES-256-GCM, base64(nonce ‖ ciphertext); key = Keychain `master-key` |
| `scopes` | jsonb | scopes granted at the last consent |
| `status` | text | `active`, `needs_reconnect` |

### `gmail_state`

| Column | Type | Notes |
|--------|------|-------|
| `email` | text PK | |
| `history_id` | bigint | last processed Gmail `historyId` (only moves forward) |
| `last_sync_at` | timestamptz | last successful sync |
| `last_error` | text | last sync error, cleared on success |
| `watch_expiration` | timestamptz | Pub/Sub mode: when the Gmail watch expires (`0002_gmail_watch.sql`) |

### `user_settings`

| Column | Type | Notes |
|--------|------|-------|
| `email` | text PK | the gm user |
| `settings` | jsonb | `{summary_language, translation_language, ui_language, digest_at, trusted_link_domains, rule_routes}`; missing keys use configured or built-in defaults |

Validation (`user_settings.rs`): language names are 2–40 characters of letters, marks, spaces,
`( ) ' -`; `ui_language` is one of `en`, `vi`, `ja`; `trusted_link_domains` holds at most 50
lowercase domain names of two or more labels (no scheme, path or wildcard), default empty.
`rule_routes` maps configured rule names to `notify`, `digest` or `ignore`. A missing entry
inherits the rule's configured action. Each rule change updates its own entry, and general
settings saves preserve the map. Code: [`user_settings.rs`](../../crates/gm/src/user_settings.rs).

### `events`

| Column | Type | Notes |
|--------|------|-------|
| `id` | bigserial PK | |
| `account` | text | Google account email |
| `source`, `kind`, `external_id` | text | unique `(source, external_id)` |
| `status` | text | `new`, `ignored`, `digest`, `classify` (waiting for model triage), `triaged`, `ready`, `notified`, `gone` |
| `rule` | text | name of the matching rule, `model` (model triage), or `default` |
| `data` | jsonb | headers brief, link and verdict ([events spec](events.md#event-record-events-table)) |
| `summary` | jsonb | summary JSON (set before the event becomes `ready`); digest items: `{line}` or `{jobs}` |
| `digest_id` | bigint FK → `digests` | the digest the event was sent in (`0003_digest.sql`); NULL = not yet |

Indexes: `created_at`, `status`, `digest_id`.

### `jobs`

| Column | Type | Notes |
|--------|------|-------|
| `id` | bigserial PK | |
| `topic`, `subscriber` | text | |
| `payload` | jsonb | |
| `status` | text | `pending`, `running`, `done`, `dead` |
| `attempts`, `max_attempts` | int | |
| `run_after` | timestamptz | next eligible run (backoff or defer) |
| `locked_at` | timestamptz | set when claimed |
| `last_error` | text | |

Index `ix_jobs_claim (status, run_after, id)`.

### `llm_calls`

One row per model request ([ADR-0010](../adr/0010-llm-client-lanes-residency.md)).

| Column | Type | Notes |
|--------|------|-------|
| `task` | text | `summary`, later `triage`, `digest`, `translate`, `reply` … |
| `lane` | text | `interactive`, `notify`, `batch` |
| `model` | text | |
| `prompt_tokens`, `completion_tokens` | int | from the response `usage` |
| `latency_ms` | int | |
| `ok`, `error` | bool, text | |

## Retention

Every 6 hours, rows older than `GM_RETENTION_DAYS` (60) are deleted from `events`, from `jobs` in
`done`/`dead`, from `llm_calls`, from `tasks` (their `approvals` go with them) and from
`chat_messages`. Calendar snapshots expire by `updated_at`; upcoming snapshots are refreshed by polling. `audit_log` is kept forever: it holds metadata only (action, recipients, ids, no mail
body). The local eval set (`~/.local/share/gm/eval/`, outside the
database) is exempt ([ADR-0017](../adr/0017-eval-data-handling.md)).

## Phase 2

### `digests` (`0003_digest.sql`)

One row per user and local date (unique `(owner_email, digest_date)`), so a digest is built and sent
once per day.

| Column | Type | Notes |
|--------|------|-------|
| `id` | bigserial PK | |
| `owner_email`, `digest_date` | text, date | |
| `status` | text | `building` (events claimed, not sent yet), `sent`, `empty` (nothing to say, no message) |
| `item_count` | int | |
| `message_ids` | jsonb | Telegram message ids, one per 4096-character part |
| `text` | text | last rendered text (for `/digest` and to skip no-op edits) |
| `sent_at`, `edited_at` | timestamptz | |

Deleted after `GM_RETENTION_DAYS` like events.

### `schedule_runs` (`0003_digest.sql`)

| Column | Type | Notes |
|--------|------|-------|
| `name` | text PK | `digest:<owner>` |
| `last_at` | timestamptz | last occurrence fired (or recorded on first sight) |

### `health_state` (`0004_health.sql`)

One row per failing health check, deleted when it passes again.

| Column | Type | Notes |
|--------|------|-------|
| `check_name` | text PK | `dead_jobs`, `ingest_lag`, `model_down`, `needs_reconnect` |
| `failing_since` | timestamptz | first failing check of this incident |
| `alerted_at` | timestamptz | when the Telegram alert went out (after 15 min); a recovery message follows only if set |
| `detail` | text | the localized problem text (reused in the recovery message) |

### `system_settings` (`0005_system_settings.sql`)

Machine-wide key-value state (not per user), read and written with `db::system_get` /
`db::system_set` ([`db.rs`](../../crates/gm/src/db.rs)).

| Key | Value | Notes |
|-----|-------|-------|
| `telegram_offset` | int | next `getUpdates` offset ([`telegram/poller.rs`](../../crates/gm/src/telegram/poller.rs)) |
| `model_control` | object | the model controls edited on the Settings page: `on`, `off`, `manage`, `idle_min`, `grace_s`, `model` ([`model.rs`](../../crates/gm/src/model.rs)); absent until first saved, then it replaces the `GM_MODEL_*` defaults |
| `notifications_paused` | bool | `true` while `/pause` holds mail notifications ([`telegram/mod.rs`](../../crates/gm/src/telegram/mod.rs)) |

### `triage_feedback` (`0006_feedback.sql`)

| Column | Type | Notes |
|--------|------|-------|
| `event_id` | bigint PK FK → `events` (cascade) | one answer per event |
| `label` | text | `up`, `down` |
| `rule` | text | the event's rule when rated (`model` = model triage → eval candidate) |
| `via` | text | `telegram`, `web` |
| `tapped_at` | timestamptz | the newest tap wins |

## Phase 3 (`0007_actions.sql`)

Code: [`tasks/runtime.rs`](../../crates/gm/src/tasks/runtime.rs), [`tasks/approvals.rs`](../../crates/gm/src/tasks/approvals.rs), [`telegram/mod.rs`](../../crates/gm/src/telegram/mod.rs).

### `tasks`

One owner request: a button tap or a sentence about a mail.

| Column | Type | Notes |
|--------|------|-------|
| `id` | bigserial PK | |
| `origin_key` | text unique | the update that created it (`cb:<callback id>`, `msg:<message id>`) |
| `event_id` | bigint FK → `events` (set null) | the mail |
| `origin` | text | `telegram` |
| `instruction` | text | the owner's words (trusted); answers to a question are appended |
| `intent` | text | `translate`, `analyze`, `reply`, `unknown` (NULL until routed) |
| `args` | jsonb | e.g. `{language}` for a translation, `{edit}` for a change to a reply draft |
| `status` | text | `pending → running → done \| needs_input \| awaiting_approval \| failed`; `awaiting_approval → executing → done \| failed`; `awaiting_approval → rejected \| expired`; `needs_input \| awaiting_approval → pending` when the owner answers or asks for a change |
| `result` | jsonb | the answer text, the reply draft id and body, `last_origin`, `waiting_told` |
| `error` | text | |
| `reply_to` | bigint | the Telegram message answers reply to |

### `approvals`

A proposed external action. The payload never changes after insert.

| Column | Type | Notes |
|--------|------|-------|
| `id` | bigserial PK | |
| `task_id` | bigint FK → `tasks` (cascade) | |
| `action` | text | `gmail.send_draft` |
| `payload` | jsonb | `{account, draft_id, thread_id, to, subject, body_sha256}` |
| `payload_hash` | text | sha256 of the payload as canonical JSON (sorted keys, no spaces); checked again before executing |
| `preview` | text | the text the owner saw |
| `status` | text | `pending → approved → executed \| failed`; `pending → rejected \| expired`; a pending approval also expires when the owner asks for a change, and fails when its hash no longer matches or the draft changed in Gmail |
| `expires_at` | timestamptz | creation + `GM_APPROVAL_TTL_H` |
| `decided_at`, `decided_via` | timestamptz, text | `telegram`, `web` |
| `executing_job` | bigint | the `approval.decision` job that approved it, the only one allowed to run the executor |
| `error` | text | |

### `chat_messages`

Which mail, task or approval a Telegram message belongs to, so a reply to it can be routed, and which
`task.reply` sent it.

| Column | Type | Notes |
|--------|------|-------|
| `chat_id`, `message_id` | text, bigint | unique together |
| `event_id`, `task_id`, `approval_id` | bigint | what the message is about (no foreign keys: the map outlives nothing it points to) |
| `direction` | text | `in` (from the owner, recorded once: a re-polled update is skipped), `out` |
| `reply_key` | text | the `task.reply` key that sent it |

### `audit_log`

Every executed or failed external action. Kept forever.

| Column | Type | Notes |
|--------|------|-------|
| `ts` | timestamptz | |
| `actor` | text | `owner` |
| `action` | text | `gmail.send_draft` |
| `target` | text | recipients |
| `task_id` | bigint | (no foreign key, so the row survives task retention) |
| `details` | jsonb | executed: `{approval_id, via, outcome, draft_id, thread_id, subject, message_id, note}` (`note: "already sent"` when the draft had already gone out); failed: `{approval_id, via, outcome, error}`. Never a mail body |

## Calendar state and snapshots

Code: [`0008_calendar.sql`](../../crates/gm/migrations/0008_calendar.sql),
[`calendar/mod.rs`](../../crates/gm/src/calendar/mod.rs), [`db.rs`](../../crates/gm/src/db.rs).

- `calendar_state`: primary key `(email, calendar_id)`, foreign key `email` to `google_accounts`,
  nullable `sync_token`, `last_sync_at`, `last_error`, `updated_at`.
- `calendar_events`: primary key `(email, calendar_id, event_id)`, `data` JSON, `updated_at`.
  Its foreign key to `calendar_state` cascades on deletion. `data` contains minimal metadata and a
  description hash, without raw descriptions or attendee lists. Snapshots without updates expire
  under `GM_RETENTION_DAYS`; upcoming snapshots are refreshed each poll.

Calendar `events` use `ready → notified`, or `ignored` when no notification is relevant. Their data
contains `calendar_id`, `event`, `previous`, and `changes`. See the [Calendar spec](calendar.md).
