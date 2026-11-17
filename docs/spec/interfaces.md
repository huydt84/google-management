# Spec: interfaces

External and human-facing interfaces: HTTP routes, Telegram messages, the CLI, configuration, the
rules file and the LLM contracts.

## HTTP

Served on `127.0.0.1:${GM_PORT}` (8080). Code: `crates/gm/src/web/`.

Guards on every request (`web::guard`):

- `Host` must be `localhost`, `127.0.0.1`, or the host of `GM_BASE_URL` / `GM_PUBLIC_URL`;
  otherwise 400 (DNS-rebinding defense).
- A POST with an `Origin` header must come from one of those hosts; otherwise 403 (CSRF defense on top
  of `SameSite=Lax` cookies).

### Implemented (Phase 1)

| Method | Path | Auth | Purpose |
|--------|------|------|---------|
| GET | `/` | session, else the sign-in page | Dashboard: Gmail connection, cursor, last sync and error, ingestion mode and Pub/Sub status, model hours, queue counts, health panel (Phase 2), last 50 events |
| GET | `/auth/login` | — | Start sign-in (OIDC, PKCE, signed `gm_oauth` state cookie) |
| GET | `/auth/callback` | — | Finish sign-in; 403 on state mismatch or unless the verified email equals `GM_ALLOWED_EMAIL` |
| POST | `/auth/logout` | — | Clear the session |
| GET | `/connect/google` | session | Start API consent (`gmail.readonly`, plus `pubsub` in Pub/Sub mode; offline, `prompt=consent`) |
| GET | `/connect/google/callback` | session | Check owner and scopes, store the encrypted refresh token, bootstrap the Gmail cursor |
| GET | `/settings` | session (else redirect to `/`) | The user's settings, rendered in their UI language |
| POST | `/settings` | session | Form `summary_language`, `translation_language`, `ui_language`, `digest_at` (HH:MM inside model hours); 400 with a localized reason, else redirect `/settings?saved=1` |
| POST | `/api/gmail/sync` | session | Sync the owner's mailbox now, redirect to `/` |
| GET | `/api/events` | session (401 JSON otherwise) | Last 100 events as JSON |
| GET | `/healthz` | — | `{"ok":true}`, or 503 if the database is unreachable |
| GET | `/metrics` | `Authorization: Bearer <metrics-token>` | Prometheus text; 404 when no token is configured, 401 on a wrong token |

Metrics: `gm_events{status}`, `gm_jobs{status}`, `gm_llm_calls_24h{task,lane}`,
`gm_llm_prompt_tokens_24h`, `gm_llm_completion_tokens_24h`, `gm_llm_latency_ms_sum_24h` (per task and lane),
`gm_ingest_lag_seconds`, `gm_pubsub_healthy` (Pub/Sub mode), `gm_health_failing{check}` (Phase 2). Counts only, no content.

### Implemented (Phase 2)

| Method | Path | Auth | Purpose |
|--------|------|------|---------|
| GET | `/digest[?id=]` | session | The last 60 digests (date, items, status) and the text of the selected one (default: latest) |
| GET | `/jobs[?status=dead|pending|running|done]` | session | Jobs by status (default `dead`, latest 200) with their last error |
| POST | `/jobs/{id}/retry`, `/jobs/retry-dead` | session | A dead job (or all of them) back to `pending` with attempts reset; redirect to `/jobs` |
| GET | `/senders` | session | Ignored mail of the last 30 days grouped by sender address: count, last subject, the rule(s) that ignored it |

### Planned

| Method | Path | Phase | Purpose |
|--------|------|-------|---------|
| GET | `/tasks`, POST `/approvals/{id}/approve` · `/reject` | 3 | Tasks and web approvals |

## Telegram

The bot sends to exactly one chat, `GM_TELEGRAM_CHAT_ID`. Messages are plain text (no `parse_mode`),
≤ 4096 characters, link previews disabled. Strings are localized with the owner's `ui_language`;
summaries use their `summary_language`.

### Notification (Phase 1)

With a summary:

```
🟡 📧 <subject>
From: <name> <address>

<2–4 sentence summary>

Suggested: <action> · <action>

https://mail.google.com/mail/?authuser=<account>#all/<thread id>
```

With a rule `message` (no summary), for example the card flash:

```
🔕 💳 Rakuten Card ¥1,234 (2026/01/01)
From: 楽天カード株式会社 <info@mail.rakuten-card.co.jp>

https://mail.google.com/…
```

- Icon: 🚨 urgent, 🔕 silent, 🔴 high; for normal priority it follows the summary importance
  (🔴 high, 🟡 medium, ⚪ low).
- Priority `silent` sends with `disable_notification` (no sound).
- Rule-only notifications (`summarize: false`) go out at any hour; their text is the rule `message`
  or the subject, sender and link.
- A notification that needs a summary is never sent without it: it waits for model hours and for the
  model to answer (decisions P4, L12).

### Digest (Phase 2)

One silent message per user and day at their `digest_at` (default 09:05), split at 4096 characters
on line boundaries. Header `☀️ Morning digest · <date>`, then the sections in `rules.yaml` order of
first appearance (titles from `digest-section-<name>` in the locale files, else the raw name), then
"📨 Other mail" (model triage) last. Lines:

- rule `message` if any, else `• <sender>: <summary line>`, else the extracted values, else
  `• <sender>: <subject>`; `⏳` marks items whose summary isn't ready (with a note at the end);
- jobs: one line per posting `• <title> — <company> · <location>` plus the link, deduplicated by
  (title, company) across the digest;
- model items: `[n] <sender>: <line>` and `↳ <reason>`, with a `👍 n` / `👎 n` row each (at most 8);
- `⚠️ System` with the current health problems, only when there are any.

Nothing is sent when there is nothing to say. While model hours last that day, the message is edited
(`editMessageText`) when pending summaries arrive.

### Health alerts (Phase 2)

Checked every minute (`src/health.rs`): `dead_jobs` (any job `dead`), `ingest_lag` (an active
account not synced for more than 10 min; in Pub/Sub mode while it works, the safety-poll interval +
10 min), `model_down` (`GM_LLM_READY_URL` not 2xx within 5 s; only checked during model hours, an open
incident stays open outside them), `needs_reconnect` (an account whose refresh token was rejected).
After 15 minutes of the same failure: one message `🩺 Problem for <n> min: <problem>`, and
`✅ Resolved: <problem>` when it passes. Silent 19:00–09:00, with sound by day. A failed send is
retried the next minute. Incidents live in `health_state`.

### Planned

| Feature | Phase |
|---------|-------|
| Feedback buttons `f:<event_id>:up/down` under notifications and model-triaged items | 2 |
| **[Translate]**, **[Analyze]**, **[Reply]** buttons (`i:<event_id>:<intent>`); approvals `a:<approval_id>:y/n` | 3 |
| Replies to notifications as instructions; `/status`, `/pause`, `/resume`, `/language` | 3 |

`callback_data` is limited to 64 bytes, so it carries ids only. Updates from other chats are ignored.

## CLI

`gm <command>` (`crates/gm/src/main.rs`):

| Command | Purpose |
|---------|---------|
| `gm serve` | Run migrations, the web server and the background tasks |
| `gm migrate` | Apply migrations and exit |
| `gm gen-key` | Print a random 32-byte key (base64) for `master-key` / `session-secret` |
| `gm keychain set <name>` | Store a secret in the Keychain (macOS prompts for the value) |
| `gm keychain list` | Show which known secrets are present (never the values) |
| `gm rules [path]` | Validate a rules file and check every rule's examples |
| `gm eval` (Phase 2) | Run the local eval set against the local model |

## Configuration

Non-secret settings come from the environment or `.env` (prefix `GM_`); see
[.env.example](../../.env.example). Secrets come from the Keychain (service `google-management`), or
from `GM_SECRET_<NAME>` in tests and CI.

| Secret | Required | Purpose |
|--------|----------|---------|
| `master-key` | yes | AES-256-GCM key for refresh tokens |
| `session-secret` | yes | HMAC key for cookies |
| `google-client-secret` | yes | OAuth client secret |
| `telegram-bot-token` | no | Without it notifications are only logged |
| `metrics-token` | no | Enables `/metrics` |

| Variable | Default | Purpose |
|----------|---------|---------|
| `GM_HOST` / `GM_PORT` | `127.0.0.1` / `8080` | Bind; non-loopback hosts are refused |
| `GM_BASE_URL` | `http://localhost:8080` | OAuth redirect base |
| `GM_PUBLIC_URL` | — | Tailnet URL, allowed by the host guard |
| `GM_SESSION_MAX_AGE_S` | 604800 | Session lifetime |
| `GM_ALLOWED_EMAIL` | required | The only account that can sign in |
| `GM_DATABASE_URL` | `postgres://gm:gm@127.0.0.1:5433/gm` | Database |
| `GM_GOOGLE_CLIENT_ID` | required | OAuth client (Web application) |
| `GM_GOOGLE_AUTH_URL` / `_TOKEN_URL`, `GM_GMAIL_API_BASE`, `GM_TELEGRAM_API_BASE` | Google / Telegram | Overridden in tests |
| `GM_GMAIL_INGEST_MODE` | `poll` | `poll` or `pubsub` (the owner's setup uses `pubsub`) |
| `GM_GOOGLE_PROJECT_ID` | required for `pubsub` | Google Cloud project that holds gm's topic and subscription |
| `GM_GMAIL_PUBSUB_TOPIC` / `GM_GMAIL_PUBSUB_SUBSCRIPTION` | `gm-gmail` / `gm-gmail-pull` | Created by gm if missing |
| `GM_GMAIL_SAFETY_POLL_INTERVAL_S` | 900 | Poll interval while Pub/Sub is healthy |
| `GM_PUBSUB_API_BASE` | Google | Overridden in tests |
| `GM_GMAIL_POLL_INTERVAL_S` | 60 | Poll interval in `poll` mode and while Pub/Sub is down |
| `GM_GMAIL_LABEL_IDS` | `INBOX` | Comma-separated labels to ingest |
| `GM_RULES_PATH` | `rules.yaml` | Rules file |
| `GM_LLM_BASE_URL` | `http://127.0.0.1:8000/v1` | OpenAI-compatible endpoint |
| `GM_LLM_API_KEY` | `not-needed` | Bearer token for it |
| `GM_LLM_READY_URL` | base URL without `/v1` + `/ready` | Readiness probe of mlx-server-runtime (health check, residency) |
| `GM_LLM_MODEL` | `mlx-community/Qwen3-8B-4bit` | Model name |
| `GM_LLM_TIMEOUT_S` | 120 | Request timeout |
| `GM_LLM_MAX_INFLIGHT` | 2 | Concurrent model requests across lanes |
| `GM_LLM_MAX_INPUT_TOKENS` | 2000 | Mail body cap |
| `GM_LLM_SUMMARY_MAX_TOKENS` | 400 | Output cap for summaries |
| `GM_LLM_NO_THINK_TAG` | `/no_think` | Appended to the system prompt; empty to disable |
| `GM_LLM_UNAVAILABLE_RETRY_S` | 300 | While the model is unavailable, queued summaries retry after this many seconds |
| `GM_MODEL_ON` / `GM_MODEL_OFF` | `09:00` / `19:00` | Model hours (local time; equal values = always on) |
| `GM_DIGEST_AT` | `09:05` | Default morning digest time; each user can change it in Settings (inside model hours) |
| `GM_DEFAULT_SUMMARY_LANGUAGE` | `Vietnamese` | Default summary language |
| `GM_DEFAULT_UI_LANGUAGE` | `en` | Default UI language (`en`, `vi`, `ja`) |
| `GM_TELEGRAM_CHAT_ID` | — | Owner's chat |
| `GM_RUN_BACKGROUND` | `true` | Start workers and loops |
| `GM_WORKER_CONCURRENCY` | 2 | Bus workers |
| `GM_JOB_MAX_ATTEMPTS` | 5 | Retry limit |
| `GM_RETENTION_DAYS` | 60 | Retention |
| `GM_LOG_DIR` | — | JSON log directory (daily files, 7 kept); stderr only if unset |

## Rules file (`rules.yaml`)

Specified in [ADR-0012](../adr/0012-rules-engine-v2.md); the header of [rules.yaml](../../rules.yaml)
is the reference. Summary:

- `default`: action for unmatched mail (`triage`).
- `rules`: ordered list; the first match wins. Each has a unique `name`, a `when` with at least one
  condition (`from` glob or list, `from_name` regex, `subject` regex, `body` regex on the first 4 KB,
  `labels_any`, `labels_none`, `list_unsubscribe`), an `action` (`ignore`, `digest`, `notify`,
  `triage`), and optional `priority` (`silent`, `normal`, `high`, `urgent`), `summarize`, `section`,
  `message` template, `extract` regexes and `examples`.
- Unknown keys are errors. The file is reloaded when its mtime changes; an invalid edit keeps the
  previous rules. `gm rules` and the test suite check every example.

## LLM contracts

All calls are `POST {GM_LLM_BASE_URL}/chat/completions` (`stream: false`) through `LlmClient`. No
`tools`, `response_format` or JSON mode is assumed.

### Prompt layout (all tasks)

1. System message: `prompts/system.md` (shared rules: the `<email>` content is untrusted data, JSON
   only) + `prompts/<task>.md` + `/no_think`. Identical across calls of a task, so the runtime's prefix
   cache can reuse it.
2. User message: `Output language: <language>`, then the mail (headers, attachment names, cleaned and
   capped body) inside `<email>…</email>`. Any `<email` / `</email` in the content is neutralized.

### Summary (implemented)

```json
{"summary": "2-4 short sentences in the output language",
 "importance": "low | medium | high",
 "category": "personal | work | finance | billing | shopping | travel | security | newsletter | jobs | other",
 "suggested_actions": ["at most 3"],
 "language": "language of the email"}
```

Parsing strips `<think>…</think>` and code fences and takes the outermost `{…}`. On failure the
client retries once with a reminder; then it keeps the raw text (≤ 1,200 characters) as the summary
with `importance: medium` and `parse_ok: false`. Connection errors, timeouts and HTTP 429/502/503/504
are `LlmUnavailable` (the job is queued); other HTTP errors fail the job.

### Model triage (implemented, `prompts/triage.md`)

`{"action": "notify" | "digest" | "ignore", "reason": "at most 12 words"}`. Notify lane, first ~1,500
body tokens, at most 80 output tokens, one retry; unusable output → `notify` (fail open).

### Digest item (implemented, `prompts/digest.md`, `prompts/jobs.md`)

Batch lane. Stored in `events.summary`.

- Line: `{"line": "1-2 sentences"}` (≤ 150 output tokens) → stored `{line, parse_ok}`; unusable output
  keeps the raw text (≤ 300 characters) with `parse_ok: false`.
- Job alerts (section `jobs`): `{"jobs": [{title, company, location, link}]}` (≤ 600 output tokens, at
  most 10 per mail) → stored `{jobs, parse_ok}`. A `link` is kept only if it is an `https://` URL that
  appears verbatim in the mail; the model can't introduce links.

### Planned

| Contract | Phase | Lane | Output |
|----------|-------|------|--------|
| Translate | 3 | interactive | text in `translation_language` |
| Analyze | 3 | interactive (thinking on) | key points, deadlines, amounts, phishing signs |
| Intent router | 3 | interactive | `{intent, args}` from the owner's words only |
| Reply body | 3 | interactive (thinking on) | body text only, never recipients |
