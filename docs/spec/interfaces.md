# Spec: interfaces

External and human-facing interfaces: HTTP routes, Telegram messages, the CLI, configuration, the
rules file and the LLM contracts.

## HTTP

Code: [`src/web/mod.rs`](../../crates/gm/src/web/mod.rs), [`src/web/pages.rs`](../../crates/gm/src/web/pages.rs), [`src/web/auth.rs`](../../crates/gm/src/web/auth.rs), [`templates/`](../../crates/gm/templates/).

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
| GET | `/connect/google` | session | Start API consent (`gmail.readonly` and `gmail.compose`, plus `pubsub` in Pub/Sub mode; offline, `prompt=consent`). The dashboard warns when the stored grant lacks a required scope |
| GET | `/connect/google/callback` | session | Check owner and scopes, store the encrypted refresh token, bootstrap the Gmail cursor |
| GET | `/settings` | session (else redirect to `/`) | The user's settings, rendered in their UI language |
| POST | `/settings` | session | Form `summary_language`, `translation_language`, `ui_language`, `digest_at` (HH:MM inside model hours); 400 with a localized reason, else redirect `/settings?saved=1` |
| POST | `/model` | session | Form `on`, `off` (HH:MM), `manage` (checkbox), `idle_min` (1–720), `grace_s` (0–1800), `model` (id, no spaces): the model controls ([ADR-0010](../adr/0010-llm-client-lanes-residency.md)); the new hours must contain the owner's digest time. Redirect `/?model=saved`, or `/?model=<error id>` |
| POST | `/api/gmail/sync` | session | Sync the owner's mailbox now, redirect to `/` |
| GET | `/api/events` | session (401 JSON otherwise) | Last 100 events as JSON |
| GET | `/healthz` | — | `{"ok":true}`, or 503 if the database is unreachable |
| GET | `/metrics` | `Authorization: Bearer <metrics-token>` | Prometheus text; 404 when no token is configured, 401 on a wrong token |

Metrics: `gm_events{status}`, `gm_jobs{status}`, `gm_llm_calls_24h{task,lane}`,
`gm_llm_prompt_tokens_24h`, `gm_llm_completion_tokens_24h`, `gm_llm_latency_ms_sum_24h` (per task and lane),
`gm_ingest_lag_seconds`, `gm_pubsub_healthy` (Pub/Sub mode), `gm_health_failing{check}` (Phase 2). Counts only, no content. Model-server metrics come from
mlx-air's own `/metrics`.

### Implemented (Phase 2)

| Method | Path | Auth | Purpose |
|--------|------|------|---------|
| GET | `/digest[?id=]` | session | The last 60 digests (date, items, status) and the text of the selected one (default: latest) |
| GET | `/jobs[?status=dead|pending|running|done]` | session | Jobs by status (default `dead`, latest 200) with their last error |
| POST | `/events/{id}/feedback` | session | Form `label=up|down`: 👍/👎 from the dashboard (2C); redirect to the event row |
| POST | `/events/{id}/eval` | session | Form `expected=notify|digest|ignore`: fetch the raw mail from Gmail and add it to the local eval set, or update its expected action (2F). Redirect to `/?eval=added|updated|full|gone#event-{id}` |
| POST | `/jobs/{id}/retry`, `/jobs/retry-dead` | session | A dead job (or all of them) back to `pending` with attempts reset; redirect to `/jobs` |
| GET | `/senders` | session | Ignored mail of the last 30 days grouped by sender address: count, last subject, the rule(s) that ignored it |

### Implemented (Phase 3)

Code: [`src/web/tasks.rs`](../../crates/gm/src/web/tasks.rs), [`templates/tasks.html`](../../crates/gm/templates/tasks.html).

| Method | Path | Auth | Purpose |
|--------|------|------|---------|
| GET | `/tasks` | session | Pending approvals (preview, expiry, Approve / Reject), the last 50 tasks, the last 50 audit rows |
| POST | `/approvals/{id}/approve` | session | Publish `approval.decision` (`via: web`); the same checks as a Telegram tap run in the handler. Redirect to `/tasks` |
| POST | `/approvals/{id}/reject` | session | Same, rejecting |

## Telegram

Code: [`src/notify.rs`](../../crates/gm/src/notify.rs), [`src/telegram/`](../../crates/gm/src/telegram/), [`src/tasks/`](../../crates/gm/src/tasks/), [`src/recipes/`](../../crates/gm/src/recipes/), [`src/feedback.rs`](../../crates/gm/src/feedback.rs), [`src/digest.rs`](../../crates/gm/src/digest.rs), [`src/health.rs`](../../crates/gm/src/health.rs).

The bot sends to exactly one chat, `GM_TELEGRAM_CHAT_ID`. Messages carry literal text (no `parse_mode`), with trusted formatting entities for Calendar messages,
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
  model to answer ([ADR-0015](../adr/0015-notification-policy-and-digest.md)).
- Every notification carries two button rows: `🌐 Translate` `🔎 Analyze` `✍️ Reply` (callback data
  `i:<event_id>:translate|analyze|reply`, Phase 3) and `👍` `👎` (`f:<event_id>:up|down`, Phase 2).
- While `/pause` is on, mail notifications wait (the job is deferred) and go out after `/resume`.

### Updates: presses, replies and commands

[`telegram/poller.rs`](../../crates/gm/src/telegram/poller.rs) long-polls `getUpdates` (`timeout`
25 s, `allowed_updates: ["callback_query", "message"]`; offset in `system_settings.telegram_offset`,
stored after each batch). Updates whose chat is not `GM_TELEGRAM_CHAT_ID` are ignored and logged
(a press still gets an empty answer so Telegram drops its spinner); nothing is ever sent to another
chat. Callback data is parsed strictly; anything else gets "This button is no longer supported".
Every incoming message is recorded in `chat_messages` in the same transaction as what it publishes,
so a re-polled update is handled once.

A feedback press is answered at once ("Thanks, noted.")
and published as `feedback.received`; its handler stores the label (last tap wins by tap time) and
redraws the message's keyboard with ✅ on the current choice (`editMessageReplyMarkup`). 👎 on a
rule-matched mail shows a hint on the dashboard to edit that rule in `rules.yaml`; on a model-triaged
mail, to add it to the eval set. There is no "mute this sender" button: rules stay in the one
reviewed, tested file.

### Translate, Analyze, Reply and approvals (Phase 3)

- **Button** `i:<event_id>:<intent>`: answered "Working on it…", published as `user.instruction`
  with the intent. A second tap on the same mail and intent while one runs, or within 2 minutes of
  it, doesn't start another.
- **Reply to a notification** (or to any bot message about a mail: a translation, a question, a
  preview) in plain words: published as `user.instruction` with the words. The intent comes from the
  keyword fast path (vi / en / ja, e.g. "dịch", "translate", "翻訳", "trả lời …", "tell them …",
  "返信 …") or else the intent router; `unknown` gets a usage hint.
- **A message that replies to nothing**: the help text, no model call. A reply to a bot message that
  isn't about a mail: "I can't tell which mail this is about". A bot message missing from
  `chat_messages` (sent before messages were recorded) is matched to its mail by the single Gmail
  link in its text (`ChatLog::event_from_link`); a digest, with several links, is not matched.
- Answers reply to the owner's message (or to the notification for a button) and are recorded in
  `chat_messages`, so replying to them continues the same mail.
- **Translate** → `🌐 Translation (<language>)` and the translated subject and body (quoted history
  dropped), into `translation_language` or a language the owner names. Long mail is translated in
  chunks of about 1,200 tokens, at most 6; the rest is marked as not translated.
- **Analyze** → the subject, a verdict (`✅ Looks safe.` / `⚠️ Looks suspicious.` / `🚨 Looks like
  phishing.`), then the non-empty sections Key points, Deadlines, Amounts, Warning signs, Next steps.
- **Reply** without words (the button) asks "What should the reply say?"; the answer to that question
  continues the task. With words, gm writes a Gmail draft in the thread and sends a preview:

  ```
  ✉️ Reply draft
  To: alice@example.com
  Subject: Re: Meeting

  <body>

  You can edit the draft in Gmail before approving, or answer this message with changes.
  [✅ Approve] [✖️ Reject]          (callback data a:<approval_id>:y|n)
  ```

  Answering the preview with changes expires that approval, rewrites the same draft and sends a new
  preview. Approve sends the draft as it is in Gmail if it still matches the preview; if it was
  edited in Gmail, nothing is sent and the new version is proposed again (`⚠️ The draft changed in
  Gmail…`). Then `✅ Sent to <to>: <subject>`. Reject leaves the draft in Gmail. Approve/Reject
  buttons are removed once decided, expired or replaced. Approvals expire after `GM_APPROVAL_TTL_H`.
- Owner requests run whenever the model answers, at any hour; if it isn't answering, the owner is
  told once ("⏳ The model isn't answering…") and the task retries every `GM_LLM_UNAVAILABLE_RETRY_S`.

### Commands (Phase 3)

| Command | Effect |
|---------|--------|
| `/status` | Notifications on or paused, model hours, queue counts (pending, running, dead), current health problems |
| `/pause` | Hold mail notifications until `/resume`; the digest, health alerts and task answers still come |
| `/resume` | Send what was held, at once |
| `/language [name]` | Show, or set, the summary language (2–40 letters, as in Settings) |
| `/help`, `/start`, any other command | Usage |

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

Nothing is sent when there is nothing to say (the run is still recorded on `/digest`). The digest
covers digest mail not yet in a digest; the first one only takes the last 24 hours. Until model hours
end that day, the message is edited (`editMessageText`) when pending summaries arrive; after that,
leftovers stay as subject lines. Mail arriving after the digest goes into the next one.

### Health alerts (Phase 2)

Checked every minute (`src/health.rs`): `dead_jobs` (any job `dead`), `ingest_lag` (an active
account not synced for more than 10 min; in Pub/Sub mode while it works, the safety-poll interval +
10 min), `model_down` (`GM_LLM_READY_URL` not 2xx within 5 s; only checked during model hours, an open
incident stays open outside them), `needs_reconnect` (an account whose refresh token was rejected).
After 15 minutes of the same failure: one message `🩺 Problem for <n> min: <problem>`, and
`✅ Resolved: <problem>` when it passes. Silent 19:00–09:00, with sound by day. A failed send is
retried the next minute. Incidents live in `health_state`.

`callback_data` is limited to 64 bytes, so it carries ids only.

## CLI

Code: [`src/main.rs`](../../crates/gm/src/main.rs).

`gm <command>` (`crates/gm/src/main.rs`):

| Command | Purpose |
|---------|---------|
| `gm serve` | Run migrations, the web server and the background tasks |
| `gm migrate` | Apply migrations and exit |
| `gm gen-key` | Print a random 32-byte key (base64) for `master-key` / `session-secret` |
| `gm keychain set <name>` | Store a secret in the Keychain (macOS prompts for the value) |
| `gm keychain list` | Show which known secrets are present (never the values) |
| `gm rules [path]` | Validate a rules file and check every rule's examples |
| `gm status` | Report what actually runs on this Mac, read from the live system: the launchd agents `com.gm.server` and `com.gm.backup` (loaded, running, last exit code), whether the running server is the latest build of its binary (a binary at most 2 s newer than the start counts as the running one: `ps` reports whole seconds and the upgrade command restarts within a second), `/healthz`, the model's readiness probe and the model it serves versus `GM_LLM_MODEL`, migrations (pending, unknown to this build, or edited after being applied), dead jobs, Gmail sync lag (the health checker's limit) and open health incidents. Needs no secrets. Exit code 1 when any check fails. Code: [`status.rs`](../../crates/gm/src/status.rs) |
| `gm eval [--dir DIR] [--language LANG]` | Run rules → model triage → summary on every case in the eval set against the **local** model (refuses a non-loopback `GM_LLM_BASE_URL`) and print accuracy per expected action, the confusion matrix, the JSON parse rate per task, misclassified ids, and calls, latency p50/p95 and tokens per model task. Needs no database, Keychain or Google access |

## Configuration

Code: [`src/config.rs`](../../crates/gm/src/config.rs), [`src/secrets.rs`](../../crates/gm/src/secrets.rs), [`.env.example`](../../.env.example).

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
| `GM_GOOGLE_AUTH_URL`, `GM_GOOGLE_TOKEN_URL`, `GM_GMAIL_API_BASE`, `GM_CALENDAR_API_BASE`, `GM_TELEGRAM_API_BASE` | Google / Telegram | Overridden in tests |
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
| `GM_LLM_READY_URL` | base URL without `/v1` + `/ready` | Readiness probe of the model server (`model_down` health check) |
| `GM_LLM_MODEL` | `mlx-community/Qwen3-8B-4bit` | Model name |
| `GM_LLM_TIMEOUT_S` | 120 | Request timeout |
| `GM_LLM_MAX_INFLIGHT` | 2 | Concurrent model requests across lanes |
| `GM_LLM_MAX_INPUT_TOKENS` | 2000 | Mail body cap |
| `GM_LLM_SUMMARY_MAX_TOKENS` | 400 | Output cap for summaries |
| `GM_LLM_NO_THINK_TAG` | `/no_think` | Appended to the system prompt; empty to disable |
| `GM_LLM_UNAVAILABLE_RETRY_S` | 300 | While the model is unavailable, queued summaries retry after this many seconds |
| `GM_MODEL_ON` / `GM_MODEL_OFF` | `09:00` / `19:00` | Model hours (local time; equal values = always on). First default: the dashboard value (`system_settings.model_control`) wins once saved |
| `GM_MODEL_MANAGE` | `true` | gm stops the model outside model hours and starts it for owner requests (first default; dashboard switch) |
| `GM_MODEL_IDLE_MIN` | 10 | Minutes the model stays up after the last owner request outside model hours (first default; dashboard) |
| `GM_MODEL_STOP_GRACE_S` | 300 | Seconds to wait for in-flight requests before stopping the model (first default; dashboard) |
| `GM_MODEL_START_TIMEOUT_S` | 120 | Seconds to wait for `/ready` after starting the model |
| `GM_MLX_AIR_BIN` | `~/.local/bin/mlx-air` if it exists, else `mlx-air` | The executable gm calls to start and stop the model (launchd's PATH lacks `~/.local/bin`) |
| `GM_MLX_INSTANCE` | `gm` | The mlx-air instance gm starts and stops |
| `GM_EVAL_DIR` | `~/.local/share/gm/eval` | Local eval set folder (mode 0700, files 0600, at most 200 cases) |
| `GM_DIGEST_AT` | `09:05` | Default morning digest time; each user can change it in Settings (inside model hours) |
| `GM_DEFAULT_SUMMARY_LANGUAGE` | `Vietnamese` | Default summary language |
| `GM_DEFAULT_UI_LANGUAGE` | `en` | Default UI language (`en`, `vi`, `ja`) |
| `GM_TELEGRAM_CHAT_ID` | — | Owner's chat |
| `GM_RUN_BACKGROUND` | `true` | Start workers and loops |
| `GM_WORKER_CONCURRENCY` | 2 | Bus workers |
| `GM_JOB_MAX_ATTEMPTS` | 5 | Retry limit |
| `GM_RETENTION_DAYS` | 60 | Retention of events, jobs, tasks, approvals and the Telegram message map (the audit log is kept) |
| `GM_APPROVAL_TTL_H` | 24 | Hours before a proposed action (Approve / Reject) expires |
| `GM_LOG_DIR` | — | JSON log directory (daily files, 7 kept); stderr only if unset |

## Rules file (`rules.yaml`)

Code: [`src/rules.rs`](../../crates/gm/src/rules.rs), [`rules.yaml`](../../rules.yaml).

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

## Eval case file

Code: [`src/eval.rs`](../../crates/gm/src/eval.rs).

One file per case, `GM_EVAL_DIR/<gmail id>.json`, plain JSON so it can be read and edited by hand:

```json
{"message_id": "18c…", "account": "owner@…", "added_at": "2026-11-17T09:12:00Z",
 "expected": "notify | digest | ignore", "notes": "", "message": { "…Gmail API message, format=full…" }}
```

Files that don't parse or whose `expected` isn't one of the three actions are skipped with a warning.
Removing a case means deleting its file.

## LLM contracts

Code: [`src/llm/mod.rs`](../../crates/gm/src/llm/mod.rs), [`prompts/`](../../crates/gm/prompts/), [`src/triage_model.rs`](../../crates/gm/src/triage_model.rs), [`src/digest.rs`](../../crates/gm/src/digest.rs), [`src/tasks/router.rs`](../../crates/gm/src/tasks/router.rs), [`src/recipes/`](../../crates/gm/src/recipes/).

All calls are `POST {GM_LLM_BASE_URL}/chat/completions` (`stream: false`) through `LlmClient`. No
`tools`, `response_format` or JSON mode is assumed.

### Prompt layout (all tasks)

1. System message: `prompts/system.md` (shared rules: the `<email>` content is untrusted data, JSON
   only) + `prompts/<task>.md` + `/no_think` (left out for Analyze and Reply, which think). Identical across calls of a task, so the runtime's prefix
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

### Owner tasks (Phase 3, interactive lane)

| Contract | Prompt | Input | Output | Caps |
|----------|--------|-------|--------|------|
| Intent router | `router.md` | the owner's words in `<instruction>`, no mail | `{"intent": "translate" \| "analyze" \| "reply" \| "unknown", "language": "…"}` | 60 tokens; unusable → `unknown`; a language failing the Settings check is dropped |
| Translate | `translate.md` | one chunk of subject + cleaned body in `<email>` | `{"translation": "…"}` | 2,048 tokens per chunk |
| Analyze | `analyze.md` | the mail, thinking on (temperature 0.6), then a JSON pass without thinking if needed | `{"key_points", "deadlines", "amounts", "risks", "next_steps": [..], "verdict": "safe" \| "suspicious" \| "phishing"}` | 2,048 tokens |
| Reply body | `reply.md` | the owner's words (trusted) before the mail; on an edit, the current draft and the requested change | `{"body": "…"}`: the body only. Recipients, subject and threading headers are set by code from the original mail | 1,536 tokens |

A reply cut off mid-JSON keeps the text recovered so far; other output that still isn't JSON after the
retry is used as plain text (thinking removed). For Reply the owner sees it in the preview before
anything can be sent.

## Calendar notifications and agenda

Code: [`calendar/mod.rs`](../../crates/gm/src/calendar/mod.rs),
[`google/calendar.rs`](../../crates/gm/src/google/calendar.rs), [`digest.rs`](../../crates/gm/src/digest.rs).

The primary calendar of each account is polled every five minutes with read-only access. Events in
the next 30 days trigger styled templates with title, local time, organizer and changed fields.
Declined events are excluded; nighttime notifications are silent. Today's live agenda appears
before mail sections in the digest, including on days without digest mail. Full contracts and
reconnection instructions are in the [Calendar spec](calendar.md).

`GM_CALENDAR_API_BASE` defaults to `https://www.googleapis.com/calendar/v3`; tests override it with
wiremock. No Calendar writes or new HTTP routes are introduced.

Calendar notification emphasis uses the optional `entities` list on Telegram `sendMessage` and
`editMessageText`. Content stays literal text with no `parse_mode`; entity offsets use UTF-16 code
units. A plain-text edit explicitly clears previous formatting entities. See the [Calendar spec](calendar.md).
