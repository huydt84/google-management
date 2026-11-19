# Google Management

A personal assistant that runs on your Mac. It watches your Gmail, decides which mail matters using
a tested rules file, summarizes it with a local LLM served by mlx-server-runtime, and pings
you on Telegram. Exactly one Google account (`GM_ALLOWED_EMAIL`) can sign in. Mail never leaves the
machine.

A single Rust binary. Gmail → rules engine v2 (+ model triage) → local summary → Telegram, with a
morning digest, health alerts, a night policy, Keychain secrets, launchd, backups and metrics
(Phases 1–2). Phase 3 adds two-way Telegram: **Translate**, **Analyze** and **Reply** under every mail
notification, or reply to a notification in your own words ("tell them Tuesday works"). A reply is
written as a Gmail draft in the thread and sent only after you tap **Approve**; every send is in an
audit log.

Phase 4 watches the primary calendar for invitations and event changes within 30 days. Declined
events are excluded; nighttime notifications are silent. Today's live agenda starts the morning
digest. Reconnect Google to grant `calendar.readonly` after enabling the Calendar API.

Start with the [documentation index](docs/README.md): PRD, architecture, ADRs, specs and the phased
plan.

```
Gmail ──► Pub/Sub ──(pull)──► GmailIngestor ◄──(safety poll 15 min)
                                       │  events + job + cursor: one transaction
                                       ▼
                         gmail.message.received ──► triage (rules.yaml)
                              ignore / digest ◄──────┤
                                                     │ notify / triage
                                                     ▼
                         gmail.message.summarize ──► LlmClient (lanes, 09:00–19:00)
                                                     │
                                                     ▼
                         notify.email ─────────────► Telegram (silent / normal / urgent)
```

- **Event bus:** durable pub/sub on Postgres (`crates/gm/src/bus.rs`): one `jobs` row per subscriber,
  claimed with `FOR UPDATE SKIP LOCKED`, published inside the caller's transaction, at-least-once,
  exponential backoff, then `dead`.
- **Event states:** `new → ignored | digest | triaged → ready → notified`, or `gone`.
- **Night and outage policy:** rule-only pings (card amount, Rakuten login notice) go out at any hour;
  everything that needs the model waits for 09:00 and for the model to be running (checked every
  5 min), urgent included. No ping is sent without its summary.
- **No lost mail:** the cursor and the events are written together; Pub/Sub messages are
  acknowledged only after the sync committed; a safety poll catches dropped notifications; after
  sleep it catches up; an expired cursor recovers the last 2 days.

## Setup

### 1. Toolchain and database

```bash
docker compose up -d
```

```bash
cargo build --release
```

### 2. Google Cloud (one time)

1. Create a project at <https://console.cloud.google.com> and enable the **Gmail API**, **Google Calendar API** and
   **Cloud Pub/Sub API**. Note the project id for `GM_GOOGLE_PROJECT_ID`.
2. **OAuth consent screen**: External; add your email as a test user.
3. **Credentials → Create OAuth client ID → Web application** with redirect URIs
   `http://localhost:8080/auth/callback` and `http://localhost:8080/connect/google/callback`.
4. **Branding**: home page `https://google-management-bay.vercel.app/`, privacy policy
   `https://google-management-bay.vercel.app/privacy.html`, authorized domain
   `google-management-bay.vercel.app`. The pages are in `deploy/oauth-pages/`; redeploy with
   `npx vercel deploy --prod` from that folder.
5. **Publish the app** ("In production"). In "Testing", refresh tokens expire after 7 days. No
   verification needed; click *Advanced → Go to …* on the warning.

### 3. Secrets (macOS Keychain) and configuration

```bash
./target/release/gm gen-key
```

Run it twice and store one value as `master-key` and the other as `session-secret`; macOS prompts for
each value, so it never appears in your shell history:

```bash
./target/release/gm keychain set master-key
```

```bash
./target/release/gm keychain set session-secret
```

```bash
./target/release/gm keychain set google-client-secret
```

```bash
./target/release/gm keychain set telegram-bot-token
```

```bash
cp .env.example .env
```

Fill in `GM_ALLOWED_EMAIL`, `GM_GOOGLE_CLIENT_ID`, `GM_GOOGLE_PROJECT_ID` and `GM_TELEGRAM_CHAT_ID` (send your bot a message,
then read `chat.id` from `https://api.telegram.org/bot<TOKEN>/getUpdates`). `.env` holds no secrets.

Run the model with mlx-server-runtime (`mlx-air serve --model mlx-community/Qwen3-8B-4bit`, port
8000).

### 4. Run

```bash
./target/release/gm serve
```

Open <http://localhost:8080>, sign in, and click **Connect Gmail** (the consent screen asks for Gmail
read access, draft access for replies, and Pub/Sub). gm then creates its Pub/Sub topic and subscription and starts the Gmail
watch by itself; the dashboard shows the ingestion status. Only mail that arrives after you connect
is processed. Set `GM_GMAIL_INGEST_MODE=poll` to skip Pub/Sub and poll every 60 s instead.

**Settings** sets the summary language, the Translate target and the UI language
(English, Tiếng Việt, 日本語).

In Telegram: tap a button under a notification, or reply to it in plain words (Vietnamese, English
or Japanese). Commands: `/status`, `/pause` (hold notifications), `/resume`, `/language [name]`,
`/help`. Pending approvals, recent tasks and the audit log are also on **Tasks** in the dashboard.

To run it as a service (start on login, nightly backups) and reach it from your phone over Tailscale,
follow [deploy/README.md](deploy/README.md).

## Customizing the filter

Edit [`rules.yaml`](rules.yaml); it reloads on save, and an invalid edit keeps the previous rules.
The header of the file documents every field. Check it before saving:

```bash
./target/release/gm rules rules.yaml
```

The first matching rule wins. Actions: `ignore`, `digest`, `notify` (priority `silent`, `normal`,
`high`, `urgent`) and `triage`. Rules can extract values (for example a card amount) into a short
notification message. Every rule has redacted examples that CI checks.

## Security

- Sign-in: Google OIDC with PKCE; the verified email must equal `GM_ALLOWED_EMAIL`. Cookies are
  HMAC-signed, `HttpOnly`, `SameSite=Lax`; a host allowlist blocks DNS rebinding and foreign-origin
  POSTs are refused.
- Secrets live in the macOS Keychain. Refresh tokens are AES-256-GCM-encrypted in the database with a
  key that never leaves the Keychain. Google scopes: `calendar.readonly`, `gmail.readonly`, `gmail.compose` (drafts; sent only after your approval), plus
  `pubsub` in Pub/Sub mode.
- Mail is untrusted data: it reaches the model only inside delimiters it can't close, the model has no
  tools, and Telegram messages are plain text. Reply recipients and threading come from the mail's
  headers, never from the model; the bot answers only your chat.

Full threat model: [docs/spec/security.md](docs/spec/security.md).

## Development

```bash
cargo test
```

```bash
cargo clippy --all-targets -- -D warnings
```

```bash
cargo fmt --all
```

Tests need Postgres from `docker compose` (`DATABASE_URL` is set in `.cargo/config.toml`); each test
gets a fresh database. Google, Telegram and the LLM are faked with wiremock.

```
crates/gm/
  src/main.rs          CLI: serve, migrate, gen-key, keychain, rules, status, eval
  src/app.rs           wiring, background tasks, tracing
  src/config.rs        GM_* settings; secrets.rs Keychain; crypto.rs AES-GCM
  src/bus.rs           durable pub/sub on Postgres
  src/db.rs            pool, queries, retention   (migrations/ holds the schema)
  src/google/          OAuth (PKCE), encrypted accounts, Gmail and Calendar REST clients
  src/gmail/           ingestion and MIME parsing
  src/rules.rs         rules engine v2
  src/llm/             LlmClient: lanes, prompt building, cleaning, JSON parsing
  src/pipeline.rs      triage → summarize → notify
  src/notify.rs        notifier trait, log notifier, formatting
  src/telegram/        Bot API client, getUpdates poller (taps, replies, commands)
  src/tasks/           owner tasks: intent router, approvals, audit, runtime
  src/recipes/         translate, analyze, reply + the send executor
  src/user_settings.rs per-user settings;  i18n.rs + locales/ (en, vi, ja)
  src/web/             axum routes, guards, sessions, pages (templates/)
  prompts/             system and task prompts
  tests/               integration tests
```

## Roadmap

See [docs/plan/README.md](docs/plan/README.md).

- **Phase 2:** morning digest at 09:05, model triage for unmatched mail with 👍/👎 feedback, model
  loaded only 09:00–19:00, `gm eval`, health alerts.
- **Phase 3 (built):** Translate / Analyze / Reply from Telegram; sending needs your approval.
- **Phase 4:** Primary-calendar notifications and today's agenda in the digest. Reconnect Google to
  grant `calendar.readonly`, with the Calendar API enabled in your Google Cloud project.
