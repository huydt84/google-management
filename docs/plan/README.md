# Implementation plan: overview

For sessions (people or agents) that pick up a piece of work and do it independently. Read this file,
then the file for your phase. For *what* and *why*, read the [PRD](../prd.md), the
[decision log](../decisions.md) and the [architecture](../architecture.md) first.

**Never decide silently.** If your workstream needs a choice the decision log doesn't cover (a new
setting, a default, a library, a behavior the owner will see), stop and ask the owner with explained
options and a recommendation, then record the answer in `docs/decisions.md`.

| Phase | File | Goal |
|-------|------|------|
| 0 | (done in Python, commit `145de43`, removed) | Gmail → rules → summary → Telegram |
| 1 | [phase-1.md](phase-1.md) | Rust port to parity, rules engine v2, operations baseline |
| 2 | [phase-2.md](phase-2.md) | Morning digest, model triage + feedback, residency controller, `gm eval`, health alerts |
| 3 | [phase-3.md](phase-3.md) | Two-way Telegram: Translate / Analyze / Reply with approvals and audit |
| 4 | [phase-4.md](phase-4.md) | Calendar notifications and today's calendar in the digest |

## Workstream status

Update this table in the same commit that completes the workstream.

| ID | Name | Depends on | Status |
|----|------|------------|--------|
| 1A | Rust core: config, secrets, crypto, db, bus, clock | — | done |
| 1B | Google OAuth, accounts, Gmail client, ingestion (poll + Pub/Sub), parser | 1A | done |
| 1C | Rules engine v2 + `rules.yaml` from the mail analysis | 1A | done |
| 1D | LLM client (lanes, cleaning, prompts) + pipeline + notifier | 1A–1C | done |
| 1E | Web (askama, Fluent en/vi/ja, guards), settings, metrics | 1A, 1B | done |
| 1F | Ops files: launchd plists, backup script, CI, `.env.example` | — | done (not installed) |
| 1G | Remove the Python code | 1A–1E | done |
| 1H | Health panel on the dashboard | 1E | done (in 2E) |
| 2A | Digest builder + scheduler + digest lane | Phase 1 | done |
| 2B | Model triage | Phase 1 | done |
| 2C | Feedback buttons (needs Telegram callbacks from 3A) | 2B, 3A-min | todo |
| 2D | Residency controller | Phase 1 | done |
| 2E | Health alerts, health panel, dead-letter page, ignored-sender counts | Phase 1 | done |
| 2F | `gm eval` and the local eval set | 2B | todo |
| 3A | Telegram client + poller (callbacks, replies, commands) | Phase 1 | todo |
| 3B | Task runtime, intent router, approvals, audit | 3A | todo |
| 3C | Recipes: translate, analyze, reply (+ `gmail.compose`) | 3B interface | todo |
| 3Z | Phase 3 integration + prompt-injection end-to-end test | 3A–3C | todo |
| 4A | Calendar source + notifications + digest section | Phase 2 | todo |

Workstreams in the same phase run in parallel when they own different files (listed in each plan).

## How to pick up a workstream

1. Read `CLAUDE.md`, this file, and your workstream's section.
2. Branch `phase<N>/<id>-<slug>`, for example `phase2/2a-digest`.
3. Edit only the files your workstream owns. Shared files (`lib.rs`, `app.rs`, `config.rs`, `db.rs`,
   `web/mod.rs`, `locales/*/main.ftl`, `templates/base.html`) get minimal changes, called out in the PR.
4. Schema changes: a new file in `crates/gm/migrations/` with the next number (`0002_…sql`). When
   rebasing, renumber yours if another branch took that number; never edit a migration that has been
   merged (sqlx stores checksums).
5. Done = the Definition of Done below; then update the status table and the specs you touched.

## Definition of Done (every workstream)

- `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` are clean
  (tests need Postgres on `127.0.0.1:5433`: `docker compose up -d`).
- `cargo run -- rules rules.yaml` passes if rules changed.
- New logic has tests that **never call real networks**: use `wiremock` for Google, Telegram and the
  LLM, and the helpers in `crates/gm/tests/common/mod.rs` (`test_app`, `mock_message`, `mock_llm`,
  `drain`, `RecordingNotifier`, `FixedClock`).
- A migration if the schema changed; the app starts on an empty DB and on the previous DB.
- New config is in `.env.example` and `docs/spec/interfaces.md`; new user-visible strings are in all
  three locale files (a test checks key parity).
- Specs in `docs/spec/` match the code; new decisions are in `docs/decisions.md`.
- None of the invariants below is broken.

## Architecture invariants (must not break)

1. **At-least-once + idempotent.** Handlers check state before acting; a state change and its
   `bus.publish(&mut tx, …)` happen in the same transaction.
2. **Google content is untrusted data.** It reaches a model only inside `<email>` delimiters
   (`clean::sanitize_delimiters`), and never a model call that can cause external effects. Targets of
   actions (addresses, threads, calendars) never come from model output.
3. **Every external effect goes through an approval** (send mail, change calendar, share, delete):
   immutable payload with a stored hash, re-validated before execution, written to the audit log.
4. **Minimal scopes.** A new scope goes in `google::REQUIRED_SCOPES`; the owner reconnects.
5. **Secrets** live in the Keychain (`secrets::get`), never in `.env`, logs, argv or error messages.
   Telegram URLs contain the bot token: never log them; strip URLs from reqwest errors.
6. **Bind to loopback only.** Remote access through Tailscale; no public webhooks.
7. **Every model call goes through `llm::LlmClient`** with a lane. No reliance on `tools` or
   `response_format`; ask for JSON and parse tolerantly. Keep the stable system prefix (system.md +
   task + `/no_think`) and put the mail last.
8. **Model hours and outages.** Work that needs the model returns `Outcome::Defer` outside
   09:00–19:00 and while the model is unavailable (`LlmUnavailable`). No notification is sent without
   its summary; rule-only notifications go out at any hour.
9. **Telegram messages are plain text** (no `parse_mode`).
10. **Rules are tested.** Every rule has redacted examples (no names, real amounts, IPs or order
    numbers); raw mail never goes into git.

## Environment facts (verified)

- Rust stable (edition 2024), one crate `crates/gm` in a workspace at the repo root.
- Postgres 17 in Docker at `127.0.0.1:5433` (user/password/db `gm`); `#[sqlx::test]` creates a fresh
  database per test from `DATABASE_URL` (set in `.cargo/config.toml`).
- `mlx-server-runtime` (sibling repo `../mlx-server-runtime`, CLI `mlx-air serve --model …`):
  OpenAI-style `POST /v1/chat/completions` with `usage`; one model per instance; no `tools`,
  `response_format` or JSON mode; `429` when its queue is full (the bus retries).
- Google: the OAuth app must be "In production" (otherwise refresh tokens expire after 7 days).
- Commands:
  ```bash
  docker compose up -d
  cargo run -- serve
  cargo test
  cargo clippy --all-targets -- -D warnings
  cargo run -- rules rules.yaml
  ```
