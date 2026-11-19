# Spec: interfaces

External and human-facing interfaces: HTTP routes, Telegram messages, the CLI, configuration, the
rules file and the LLM contracts.

## HTTP

Code: [`src/web/mod.rs`](../../crates/gm/src/web/mod.rs), [`src/web/pages.rs`](../../crates/gm/src/web/pages.rs), [`src/web/auth.rs`](../../crates/gm/src/web/auth.rs), [`templates/`](../../crates/gm/templates/).

Served on `127.0.0.1:${GM_PORT}` (8080). Code: `crates/gm/src/web/`.

Every signed-in page shares one layout ([`templates/base.html`](../../crates/gm/templates/base.html),
`web::Nav`): a sidebar (a scrolling top bar on narrow screens) linking Overview, Tasks (with the
count of pending approvals), Digest, Model, Ignored senders, Mail rules, Jobs and Settings, plus the owner's address and
Sign out. Colour encodes the route only: notify, digest and ignore each have one hue, used for the
route key on the dashboard and the edge of each mail row. An event's route comes from its
status (`Pipeline::status_action` in [`pipeline.rs`](../../crates/gm/src/pipeline.rs)): `triaged`,
`ready`, `notified` → notify; `digest` → digest; `ignored` → ignore; `new` and `classify` are
still being sorted. Calendar events are not sorted and show as Calendar. Each mail row shows its
status as read-only text (hover or focus it for what its route does). The 👍 / 👎 and "Add to eval"
buttons appear when the row is hovered or focused, and always on touch screens; a given 👍 / 👎
and eval membership stay visible as notes under the status. Pages use system fonts only and load nothing from outside gm.

The dashboard's recent event list shows 50 events per page, newest ID first, including Gmail and
Calendar. Older and Newer links above and below the list use exclusive `before` and `after` event
IDs in the query string. Newer selects the nearest 50 IDs above its cursor and displays them newest
first. New arrivals do not shift an older page, and a cursor still works after its event is deleted.
Route counts describe the mail on the displayed page. Controls show the number of events on the
page, disable unavailable directions, and offer Latest events while browsing history. A single page
needs no controls; an empty cursor page offers a return to the latest events. Navigation targets the
list heading. Cursors must be positive signed 64-bit integers; supplying both directions or an invalid
cursor returns 400. Feedback and eval form actions carry the current cursor in their query string
and preserve it on redirect, along with the event anchor and any eval outcome.
Code: [`src/db.rs`](../../crates/gm/src/db.rs), [`src/web/pages.rs`](../../crates/gm/src/web/pages.rs),
[`templates/index.html`](../../crates/gm/templates/index.html).

Guards on every request (`web::guard`):

- `Host` must be `localhost`, `127.0.0.1`, or the host of `GM_BASE_URL` / `GM_PUBLIC_URL`;
  otherwise 400 (DNS-rebinding defense).
- A POST with an `Origin` header must come from one of those hosts; otherwise 403 (CSRF defense on top
  of `SameSite=Lax` cookies).

### Implemented (Phase 1)

| Method | Path | Auth | Purpose |
|--------|------|------|---------|
| GET | `/` | session, else the sign-in page | Dashboard: Gmail connection, cursor, last sync and error, ingestion mode and Pub/Sub status, model state and hours, queue counts, health panel (Phase 2), 50 events per page (`?before=<id>` or `?after=<id>`) with their route, a key of the three routes (notify, digest, ignore) with how many of the displayed mails took each, and a closed "How mail is sorted" disclosure explaining them (its toggle stays at the right end of the heading row; the panel opens as the full-width row below). A mail row shows its summary and its suggested actions as a list, a linked action with its host; a calendar row shows the calendar notification text with its links and emphasis, and its title links to the event |
| GET | `/auth/login` | — | Start sign-in (OIDC, PKCE, signed `gm_oauth` state cookie) |
| GET | `/auth/callback` | — | Finish sign-in; 403 on state mismatch or unless the verified email equals `GM_ALLOWED_EMAIL` |
| POST | `/auth/logout` | — | Clear the session |
| GET | `/connect/google` | session | Start API consent (`gmail.readonly` and `gmail.compose`, plus `pubsub` in Pub/Sub mode; offline, `prompt=consent`). The dashboard warns when the stored grant lacks a required scope |
| GET | `/connect/google/callback` | session | Check owner and scopes, store the encrypted refresh token, bootstrap the Gmail cursor |
| GET | `/settings[?saved=1]` | session (else redirect to `/`) | The user's settings, rendered in their UI language |
| POST | `/settings` | session | Form `summary_language`, `translation_language`, `ui_language`, `digest_at` (HH:MM inside model hours), `trusted_link_domains` (domain names separated by new lines, spaces or commas; at most 50); 400 with a localized reason, else redirect `/settings?saved=1` |
| GET | `/model` | session (else redirect to `/`) | Model tab. Readiness, served model, power controls, editable settings, effective token budgets and the last completed eval |
| POST | `/model` | session | Save one model setting by its `setting` form value. Redirect to the setting with localized feedback. Model hours must include the owner's digest time |
| POST | `/model/power` | session | `action=rest` stops the local model and holds it off. `action=wakeup` starts it and waits for readiness. Both share the Telegram commands' state |
| POST | `/model/eval` | session | Start an evaluation in the background when the local model is ready and cases exist. An existing run stays in progress. Redirect to `/model#evaluation` |
| POST | `/api/gmail/sync` | session | Sync the owner's mailbox now, redirect to `/` |
| GET | `/api/events` | session (401 JSON otherwise) | Last 100 events as JSON |
| GET | `/healthz` | — | `{"ok":true}`, or 503 if the database is unreachable |
| GET | `/metrics` | `Authorization: Bearer <metrics-token>` | Prometheus text; 404 when no token is configured, 401 on a wrong token |

Metrics: `gm_events{status}`, `gm_jobs{status}`, `gm_llm_calls_24h{task,lane}`,
`gm_llm_prompt_tokens_24h`, `gm_llm_completion_tokens_24h`, `gm_llm_latency_ms_sum_24h` (per task and lane),
`gm_ingest_lag_seconds`, `gm_pubsub_healthy` (Pub/Sub mode), `gm_health_failing{check}` (Phase 2). Counts only, no content. Model-server metrics come from
mlx-air's own `/metrics`.

### Model tab

The serving model ID comes from `/ready`. The editable checkpoint ID applies at the next mlx-air
start. Power actions and edits to the checkpoint, automatic management, idle timer and stop grace
are available for a local instance. Model hours still govern background work with a remote model.

Each form updates its own setting. The `setting` values are `checkpoint` (`model`), `automatic`
(`manage` checkbox), `hours` (`on`, `off` in HH:MM), `idle` (`minutes`, 1–720), `grace` (`seconds`,
0–1800), `input-tokens` and `summary-tokens` (`tokens`, 1–32,768, or blank for the environment
default). The input limit caps email text in summary prompts. The summary answer limit applies to
clients that use the Qwen3 setting. Other answer limits and all reasoning allowances come from the
model client. The effective per-task limits are for the serving model, or the configured checkpoint
when the server omits its model ID. Token limit edits take effect on new requests.

Evaluation uses the saved local cases, the owner's summary language and the same local inference
profile and queue as production calls. Loopback inference calls connect directly, with redirects
disabled. One web evaluation runs at a time. The last completed run
shows its model, timestamp and routing score, with parse rates, latency and token usage in the full
report. A failed run leaves the previous result available. `gm eval` also saves a completed report
in its eval directory. Code is in [`web/model.rs`](../../crates/gm/src/web/model.rs),
[`model.html`](../../crates/gm/templates/model.html) and [`eval.rs`](../../crates/gm/src/eval.rs).

### Implemented (Phase 2)

| Method | Path | Auth | Purpose |
|--------|------|------|---------|
| GET | `/digest[?id=]` | session | The last 60 digests (date, items, status) and the text of the selected one (default: latest) |
| GET | `/jobs[?status=dead|pending|running|done]` | session | Jobs by status (default `dead`, latest 200) with their last error |
| POST | `/events/{id}/feedback` | session | Form `label=up|down`: 👍/👎 from the dashboard (2C); redirect to the event row |
| POST | `/events/{id}/eval` | session | Form `expected=notify|digest|ignore`: fetch the raw mail from Gmail and add it to the local eval set, or update its expected action (2F). Redirect to `/?eval=added|updated|full|gone#event-{id}` |
| POST | `/jobs/{id}/retry`, `/jobs/retry-dead` | session | A dead job (or all of them) back to `pending` with attempts reset; redirect to `/jobs` |
| GET | `/senders` | session | Ignored mail of the last 30 days grouped by sender address: count, last subject, the rule(s) that ignored it |
| GET | `/rules` | session | Mail rules in matching order, with their conditions, the owner's selected route, the file's default for each rule, and the fallback for unmatched mail |
| POST | `/rules` | session | Form `rule=<configured name>`, `action=notify\|digest\|ignore` or an empty action to use the rule's default. Redirect to the saved row, or render a localized error with 400 for an unknown rule or action |

The Mail rules sidebar page changes the route for the signed-in user's mail at the point of sorting.
The first matching rule still wins. A change to a different route uses [that route's defaults](../adr/0012-rules-engine-v2.md).
The match conditions and unmatched-mail fallback come from the rules file. Mail already sorted
keeps its recorded route. Code: [`src/web/rules.rs`](../../crates/gm/src/web/rules.rs),
[`templates/rules.html`](../../crates/gm/templates/rules.html), [`src/pipeline.rs`](../../crates/gm/src/pipeline.rs).

### Implemented (Phase 3)

Code: [`src/web/tasks.rs`](../../crates/gm/src/web/tasks.rs), [`templates/tasks.html`](../../crates/gm/templates/tasks.html).

| Method | Path | Auth | Purpose |
|--------|------|------|---------|
| GET | `/tasks` | session | Pending approvals (preview, expiry, Approve / Reject), the last 50 tasks, the last 50 audit rows |
| POST | `/approvals/{id}/approve` | session | Publish `approval.decision` (`via: web`); the same checks as a Telegram tap run in the handler. Redirect to `/tasks` |
| POST | `/approvals/{id}/reject` | session | Same, rejecting |

## Telegram

Code: [`src/notify.rs`](../../crates/gm/src/notify.rs), [`src/telegram/`](../../crates/gm/src/telegram/), [`src/tasks/`](../../crates/gm/src/tasks/), [`src/recipes/`](../../crates/gm/src/recipes/), [`src/feedback.rs`](../../crates/gm/src/feedback.rs), [`src/digest.rs`](../../crates/gm/src/digest.rs), [`src/health.rs`](../../crates/gm/src/health.rs).

The bot sends to exactly one chat, `GM_TELEGRAM_CHAT_ID`. Messages carry literal text (no `parse_mode`), with trusted formatting entities for mail and Calendar notifications (built by `notify::RichText`),
≤ 4096 characters, link previews disabled. Strings are localized with the owner's `ui_language`;
summaries use their `summary_language`.

### Notification (Phase 1)

With a summary:

```
🟡 📧 <subject>
From: <name> <address>

<2–4 sentence summary>

Suggested:
• <action>
• <linked action> (<host>)

Open in Gmail
```

With a rule `message` (no summary), for example the card flash:

```
🔕 💳 Rakuten Card ¥1,234 (2026/01/01)
From: 楽天カード株式会社 <info@mail.rakuten-card.co.jp>

Open in Gmail
```

- Icon: 🚨 urgent, 🔕 silent, 🔴 high; for normal priority it follows the summary importance
  (🔴 high, 🟡 medium, ⚪ low).
- Priority `silent` sends with `disable_notification` (no sound).
- `Open in Gmail` is a `text_link` span to `https://mail.google.com/mail/?authuser=<account>#all/<thread id>`,
  built by code. A reply to a notification missing from `chat_messages` finds its mail from a Gmail
  link in the replied message's text or in its link spans.
- Each suggested action is one `• ` line. An action with a URL (see [Summary](#summary-implemented))
  is a `text_link` span on its text, followed by the URL's host in parentheses.
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
  The headings follow the UI language; the items are in the summary language.
- **Reply** without words (the button) asks "What should the reply say?"; the answer to that question
  continues the task. With words, gm writes a Gmail draft in the thread and sends a preview:

  ```
  ✉️ Reply draft
  To: alice@example.com
  Subject: Re: Meeting

  <body>

  You can edit the draft in Gmail before approving, or answer this message with changes.
  [✅ Approve] [✖️ Reject]          (callback data a:<approval_id>:y|n)
  [📝 Use my exact words]           (callback data a:<approval_id>:w)
  ```

  The body keeps the owner's words when they already read as the message in the reply's language;
  otherwise the model writes the short message they describe. It adds no thanks, promises, questions
  or facts the owner did not give, and a greeting or closing only when the owner's words have one or
  the mail is formal and the body was written from a description. Answering the preview with changes
  supersedes that approval, rewrites the same draft and sends a new preview headed `🔄 Updated draft.
  The previous one can no longer be approved.` **Use my exact words** does the same without the
  model: the owner's latest message for this reply (the change request, else the original words)
  becomes the body as written. Approve sends the draft as it is in Gmail if it still matches the preview; if it was
  edited in Gmail, nothing is sent and the new version is proposed again (`⚠️ The draft changed in
  Gmail…`). Then `✅ Sent to <to>: <subject>`. Reject leaves the draft in Gmail. The preview's
  buttons are removed once it is decided, expired or superseded. Approvals expire after `GM_APPROVAL_TTL_H`.
- Owner requests run whenever the model answers, at any hour; if it isn't answering, the owner is
  told once ("⏳ The model isn't answering…") and the task retries every `GM_LLM_UNAVAILABLE_RETRY_S`.

### Commands (Phase 3)

| Command | Effect |
|---------|--------|
| `/status` | Notifications on or paused, model hours and manual rest status, queue counts (pending, running, dead), current health problems other than Gmail sync lag |
| `/sync` | Check the owner's Gmail now, at any hour, and reply with the number of newly ingested emails or a short connection/error message. Uses the normal ingestion lock, deduplication and mail notification rules; does not start the model or bypass `/pause` |
| `/pause` | Hold mail notifications until `/resume`; the digest, health alerts and task answers still come |
| `/resume` | Send what was held, at once |
| `/rest` | Stop the local mlx-air instance immediately to free its RAM. Keep it off across gm restarts until `/wakeup`. Model-dependent work defers and rule-only notifications can still go out. |
| `/wakeup` | Start the local instance and confirm when it answers `/ready`. Clear manual rest on success and resume the usual schedule, including the idle timer outside model hours. |
| `/language [name]` | Show, or set, the summary language (2–40 letters, as in Settings) |
| `/help`, `/start`, any other command | Usage |

Code for the model commands: [`telegram/poller.rs`](../../crates/gm/src/telegram/poller.rs)
and [`model.rs`](../../crates/gm/src/model.rs).

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
- `⚠️ System` with the current health problems other than Gmail sync lag, only when there are any.

Nothing is sent when there is nothing to say (the run is still recorded on `/digest`). The digest
covers digest mail not yet in a digest; the first one only takes the last 24 hours. Until model hours
end that day, the message is edited (`editMessageText`) when pending summaries arrive; after that,
leftovers stay as subject lines. Mail arriving after the digest goes into the next one.

### Health alerts (Phase 2)

Checked every minute (`src/health.rs`): `dead_jobs` (any job `dead`), `ingest_lag` (an active
account not synced for more than 10 min; in Pub/Sub mode while it works, the safety-poll interval +
10 min), `model_down` (`GM_LLM_READY_URL` not 2xx within 5 s; checked during model hours when the
model is awake, with open incidents retained while the check is skipped), `needs_reconnect` (an
account whose refresh token was rejected).
Gmail sync lag is diagnostic only: it stays on the dashboard, `/metrics`, `gm status` and in
`health_state`, without Telegram failure or recovery messages, including incidents previously
announced by an older build. It is excluded from the digest System section and Telegram `/status`.
The owner can explicitly check Gmail with `/sync`; only that command's response reports its result.
For the other checks, after 15 minutes of the same failure: one message `🩺 Problem for <n> min: <problem>`, and
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
| `gm status` | Report what actually runs on this Mac, read from the live system: the launchd agents `com.gm.server` and `com.gm.backup` (loaded, running, last exit code), whether the running server is the latest build of its binary (a binary at most 2 s newer than the start counts as the running one: `ps` reports whole seconds and the upgrade command restarts within a second), `/healthz`, the model's readiness or manual rest status and the model it serves versus `GM_LLM_MODEL`, the configured model's client and sampling profile (`!` on the built-in defaults, `✗` for an invalid file), migrations (pending, unknown to this build, or edited after being applied), dead jobs, Gmail sync lag (the health checker's limit) and open health incidents. Needs no secrets. Exit code 1 when any check fails. Code: [`status.rs`](../../crates/gm/src/status.rs) |
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
| `GM_LLM_MAX_INPUT_TOKENS` | 2000 | Default mail summary input cap. The Model tab can override it for `gm serve` |
| `GM_LLM_SUMMARY_MAX_TOKENS` | 400 | Default summary answer cap for Qwen3 and the fallback client. The Model tab can override it for `gm serve` |
| `GM_LLM_PROFILE` | `llm.yaml` | Sampling overrides per model ([below](#inference-profile)); missing file or section → built-in defaults, invalid file → `gm serve` does not start |
| `GM_LLM_UNAVAILABLE_RETRY_S` | 300 | While the model is unavailable, queued summaries retry after this many seconds |
| `GM_MODEL_ON` / `GM_MODEL_OFF` | `09:00` / `19:00` | Model hours (local time; equal values = always on). The Model tab value (`system_settings.model_control`) wins once saved |
| `GM_MODEL_MANAGE` | `true` | gm stops the model outside model hours and starts it for owner requests (first default; Model tab switch) |
| `GM_MODEL_IDLE_MIN` | 10 | Minutes the model stays up after the last owner request outside model hours (first default; Model tab) |
| `GM_MODEL_STOP_GRACE_S` | 300 | Seconds to wait for in-flight requests before a scheduled stop (first default; Model tab) |
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

The most recent completed report is replaced atomically in `GM_EVAL_DIR/.latest-report` with mode
0600. It contains the model ID, completion time, routing totals and formatted results. The report
may include misclassified case IDs. Raw case content stays in the case files.

## LLM contracts

Code: [`src/llm/mod.rs`](../../crates/gm/src/llm/mod.rs), [`prompts/`](../../crates/gm/prompts/), [`src/triage_model.rs`](../../crates/gm/src/triage_model.rs), [`src/digest.rs`](../../crates/gm/src/digest.rs), [`src/tasks/router.rs`](../../crates/gm/src/tasks/router.rs), [`src/recipes/`](../../crates/gm/src/recipes/).

All calls are `POST {GM_LLM_BASE_URL}/chat/completions` (`stream: false`) through `LlmClient`. No
`tools`, `response_format` or JSON mode is assumed.

### Prompt layout (all tasks)

1. System message: `prompts/system.md` (shared rules: the `<email>` content is untrusted data, JSON
   only) + `prompts/<task>.md` + the model client's optional control tag. This prefix can be reused
   across calls of that task and model.
2. User message: `Output language: <language>`, then the mail (headers, attachment names, cleaned and
   capped body) inside `<email>…</email>`. Any `<email` / `</email` in the content is neutralized.
   The summary task appends the mail's trusted links, numbered, in a `<links>` block after the mail.
3. Tasks that answer in the owner's language (summary, digest line, translate, analyze; `Task::in_owner_language`)
   end the user message by restating it: `Output language: <language>. Write every value in the JSON
   in <language>.` A thinking model otherwise tends to answer in the language it reasoned in.

### Inference profile

Code: [`model_client.rs`](../../crates/gm/src/llm/model_client.rs), [`profile.rs`](../../crates/gm/src/llm/profile.rs), [`llm.yaml`](../../llm.yaml) ([ADR-0018](../adr/0018-llm-inference-profile.md)).

Every task belongs to one tier, fixed in code:

| Tier | Tasks (`llm_calls.task`) |
|------|--------------------------|
| easy | `router`, `triage`, `digest-line` |
| medium | `summary`, `digest-jobs`, `translate` |
| hard | `analyze`, `reply` |

The client for the served model receives the exact task through its tier method. The client sets
reasoning, prompt formatting, response handling and retries, and can tune answer and reasoning tokens
for each task. Each client registers its supported IDs beside its implementation. At startup, gm indexes
the registrations and rejects duplicate IDs. If gm has no served model ID, it uses the configured
ID. An unregistered ID uses the Qwen3 fallback.

- Qwen3 sends `/no_think` on easy and medium tasks. Hard tasks add 1,024 tokens of reasoning to
  their answer cap, with a direct retry if the first response cannot be parsed. Direct attempts
  include a format-reminder retry when needed.
- The [Bonsai client](../../crates/gm/src/llm/model_client/bonsai.rs) has a separate answer cap
  for each task. An absolute path whose directory is named `Bonsai-27B-mlx-1bit-direct` selects the
  [direct checkpoint](../../deploy/README.md#prepare-bonsai-for-direct-answers) and uses those caps
  directly. Its directory name is also the sampling key.
- The stock Bonsai model uses a template that opens `<think>` in the prompt. Its client adds
  reasoning tokens to the answer cap, reads the answer after `</think>`, and retries at
  temperature 0 if the answer fails parsing or the language check. That check applies to summary,
  digest line, translate and analyze.

The request's `max_tokens` covers reasoning and answer tokens together. The runtime's completion
limit and `GM_LLM_TIMEOUT_S` both apply to each request.

`GM_LLM_PROFILE` holds sampling overrides by model ID. For example:

```yaml
models:
  prism-ml/Bonsai-27B-mlx-1bit:
    temperature: 0.7
    top_p: 0.95
  mlx-community/Qwen3-8B-4bit:
    temperature: 0.2
    thinking_temperature: 0.6
```

`temperature` applies to initial requests. `thinking_temperature` can override it for thinking
requests. Without an override, direct requests use 0.2 and thinking requests use 0.6. An omitted
`top_p` uses the server's default. Temperatures must be finite and non-negative. `top_p` must
be greater than 0 and at most 1. Legacy `tiers` and `no_think_tag` keys are ignored. Other unknown
keys are errors. A missing file or model entry uses built-in sampling defaults and is shown as
`!` by `gm status`.

The check ([`llm/language.rs`](../../crates/gm/src/llm/language.rs) `in_language`) recognizes
Vietnamese (at least a fifth of the words carry Vietnamese-only marks), Japanese (kana present, kana
and kanji at least half the letters), Korean, Chinese and Thai (their script at least half the
letters). Other output languages are not checked.

### Summary (implemented)

```json
{"summary": "2-4 short sentences in the output language",
 "importance": "low | medium | high",
 "category": "personal | work | finance | billing | shopping | travel | security | newsletter | jobs | other",
 "suggested_actions": [{"text": "a short next step", "link": 1}],
 "language": "language of the email"}
```

At most 3 actions; `link` is optional. Trusted links (`ParsedMessage::trusted_links`,
[`gmail/message.rs`](../../crates/gm/src/gmail/message.rs)) are the mail's https links (HTML `href`s
and URLs in text; default port, no user info, at most 10) whose host is a domain on the owner's
`trusted_link_domains` or its subdomain, when Gmail's own `Authentication-Results` (the topmost
header, authserv-id `mx.google.com`) reports `dmarc=pass` with a `header.from` under the same
trusted domain. `link: n` maps to the n-th listed link; any other value, or a mail with no trusted
links, leaves the action without a URL. The model never writes a URL. `events.summary` stores
`suggested_actions` as `[{"text", "url"?}]`; summaries stored as plain strings still render.

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
| Intent router | `router.md` | the owner's words in `<instruction>`, no mail | `{"intent": "translate" \| "analyze" \| "reply" \| "unknown", "language": "…"}` | 60 Qwen3 answer tokens; unusable → `unknown`; a language failing the Settings check is dropped |
| Translate | `translate.md` | one chunk of subject + cleaned body in `<email>` | `{"translation": "…"}` | 2,048 answer tokens per chunk |
| Analyze | `analyze.md` | the mail; a mail detected as a forward gets a `Forwarded: yes` line from code before `<email>` | `{"key_points", "deadlines", "amounts", "risks", "next_steps": [..], "verdict": "safe" \| "suspicious" \| "phishing"}` | 1,024 answer tokens |
| Reply body | `reply.md` | the owner's words (trusted) before the mail; on an edit, the current draft and the requested change; not called for **Use my exact words** | `{"body": "…"}`: the body only. Recipients, subject and threading headers are set by code from the original mail | 512 answer tokens |

A reply cut off mid-JSON keeps the text recovered so far; other output that still isn't JSON after the
retry is used as plain text (thinking removed). For Reply the owner sees it in the preview before
anything can be sent.

The caps shown in the task contracts are for Qwen3. The [model client](#inference-profile)
sets the cap, reasoning allowance, and language check for the active model.

Forward detection ([`gmail/message.rs`](../../crates/gm/src/gmail/message.rs) `is_forward`): a
`Fwd:` / `FW:` / `転送:` subject, or a forward marker line in the body (`---------- Forwarded message
---------`, `転送メッセージ` / `転送されたメッセージ`, `Thư được chuyển tiếp`, `Begin forwarded message:`).
The fact line carries no mail text; the original sender is read inside `<email>`.

`system.md` forbids guessing anyone's gender, age or relationship: Vietnamese output uses neutral
forms of address (`bạn`, `quý khách`, or the name with no title); Japanese honorifics (様, さん) appear
only in Japanese output. Analyze's `risks` lists only warning signs that are present.

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
