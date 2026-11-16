# Google Management

Personal, single-user server on the owner's Mac: ingests Gmail (Calendar later), triages it with a
tested rules file, summarizes with a local LLM (mlx-server-runtime, Qwen3-8B 4-bit, called directly),
and notifies via Telegram. Only `GM_ALLOWED_EMAIL` can log in. Rust (one binary `gm`) + axum + sqlx +
Postgres. Docs, code, comments and UI source strings are in English; the UI is localized (en / vi /
ja) and LLM output follows each user's language setting (`crates/gm/src/user_settings.rs`).

## Commands

```bash
docker compose up -d                       # Postgres on 127.0.0.1:5433
cargo run -- serve                         # web + background workers (reads .env, secrets from Keychain)
cargo test                                 # needs Postgres; fresh DB per test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
cargo run -- rules rules.yaml              # validate rules and their examples
```

## Where things are

- `crates/gm/src/app.rs`: wiring (`build`), background tasks, `serve`.
- `crates/gm/src/bus.rs`: durable pub/sub on Postgres.
- `crates/gm/src/pipeline.rs`: triage → summarize → notify handlers, night policy.
- `crates/gm/src/rules.rs` + `rules.yaml`: rules engine v2.
- `crates/gm/src/llm/`: the only way to call a model (lanes, prompt layout, parsing).
- `crates/gm/src/gmail/`, `crates/gm/src/google/`: ingestion, parsing, OAuth, Gmail client.
- `crates/gm/migrations/`: schema. `crates/gm/locales/`: UI strings. `crates/gm/prompts/`: prompts.
- `crates/gm/tests/common/mod.rs`: `test_app`, wiremock helpers, `RecordingNotifier`, `FixedClock`.
- `deploy/`: launchd plists and service setup; `scripts/backup.sh`.

## Before you change anything

**Never decide silently.** Any product or technical choice not already in
[docs/decisions.md](docs/decisions.md) is asked first, with explained options and a recommendation,
then recorded there.

For the big picture, see [docs/README.md](docs/README.md): decision log, PRD, architecture, specs and
ADRs. Keep them in sync when your change alters behavior, data model, topics, config or interfaces.

**Read [docs/plan/README.md](docs/plan/README.md).** It has the phase roadmap and workstream status,
the rules for parallel work (file ownership, linear migration numbering), the Definition of Done, and
the **architecture invariants** you must not break: idempotent handlers with transactional publish;
Google content is untrusted; external side effects only through approvals; minimal scopes; secrets
only in the Keychain and never logged; bind localhost only; all LLM calls through `LlmClient` with a
lane and no reliance on `tools`/`response_format`; model work deferred outside model hours; Telegram
plain text; every rule has redacted examples.

Per-phase plans: [phase-1.md](docs/plan/phase-1.md) (done), [phase-2.md](docs/plan/phase-2.md),
[phase-3.md](docs/plan/phase-3.md), [phase-4.md](docs/plan/phase-4.md).

Tests must never call real Google, Telegram, or LLM endpoints. Raw mail never goes into git.
