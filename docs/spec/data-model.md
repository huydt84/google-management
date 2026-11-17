# Spec: data model

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
| `settings` | jsonb | `{summary_language, translation_language, ui_language, digest_at}`; missing keys fall back to `GM_DEFAULT_SUMMARY_LANGUAGE` / `GM_DEFAULT_UI_LANGUAGE` |

Validation (`user_settings.rs`): language names are 2–40 characters of letters, marks, spaces,
`( ) ' -`; `ui_language` is one of `en`, `vi`, `ja`.

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
`done`/`dead`, and from `llm_calls`. The local eval set (`~/.local/share/gm/eval/`, outside the
database) is exempt ([ADR-0017](../adr/0017-eval-data-handling.md)).

## Planned

| Phase | Table | Purpose |
|-------|-------|---------|
| 2 | `digests` | one row per sent digest: date, Telegram message id (for later edits), item ids |
| 2 | `triage_feedback` | `event_id`, label (👍/👎, should have notified), created_at |
| 3 | `tasks`, `approvals`, `chat_messages`, `audit_log`, `kv_state` | instructions, immutable approvals with payload hash, Telegram message mapping, audit trail, poller offset |
| 4 | `calendar_state` | sync token per calendar |

Each comes as a new numbered migration. Parallel sessions must keep one linear migration sequence
(rename yours to the next free number when rebasing).

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
| `name` | text PK | `digest:<owner>`, `residency` |
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

Machine-wide settings edited in the app (not per user).

| Key | Value | Notes |
|-----|-------|-------|
| `model_residency` | bool | residency controller switch (decision N10); missing = off |
| `telegram_offset` | int | next `getUpdates` offset (2C) |

### `triage_feedback` (`0006_feedback.sql`)

| Column | Type | Notes |
|--------|------|-------|
| `event_id` | bigint PK FK → `events` (cascade) | one answer per event |
| `label` | text | `up`, `down` |
| `rule` | text | the event's rule when rated (`model` = model triage → eval candidate) |
| `via` | text | `telegram`, `web` |
| `tapped_at` | timestamptz | the newest tap wins |
