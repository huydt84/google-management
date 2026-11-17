# Google Management

Personal, single-user server on the owner's Mac: ingests Gmail (Calendar later), triages it with a
tested rules file, summarizes with a local LLM (mlx-server-runtime, Qwen3-8B 4-bit, called directly;
the model process is run by mlx-air, never by gm), and notifies via Telegram. Only `GM_ALLOWED_EMAIL` can log in. Rust (one binary `gm`) + axum + sqlx +
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

The full module map, one row per file, is in [docs/architecture.md §4](docs/architecture.md#4-modules-inside-the-gm-binary).

- `crates/gm/src/app.rs`: wiring (`build`), background tasks, `serve`.
- `crates/gm/src/bus.rs`: durable pub/sub on Postgres.
- `crates/gm/src/pipeline.rs`: triage → classify → summarize → notify handlers, `route`, and
  `model_step` (the shared steps of every model job).
- `crates/gm/src/rules.rs` + `rules.yaml`: rules engine v2.
- `crates/gm/src/llm/`: the only way to call a model (lanes, prompt layout, parsing).
- `crates/gm/src/digest.rs`, `scheduler.rs`, `health.rs`, `feedback.rs`, `telegram.rs`,
  `triage_model.rs`, `eval.rs`: Phase 2 (digest, schedules, health alerts, 👍/👎, model triage,
  `gm eval`).
- `crates/gm/src/gmail/`, `crates/gm/src/google/`: ingestion, parsing, OAuth, Gmail client.
- `crates/gm/migrations/`: schema. `crates/gm/locales/`: UI strings. `crates/gm/prompts/`: prompts.
- `crates/gm/tests/common/mod.rs`: `test_app`, wiremock helpers, `RecordingNotifier`, `FixedClock`.
- `deploy/`: launchd plists and service setup; `scripts/backup.sh`.

## Before you change anything

**Never decide silently.** Any product or technical choice the docs don't already settle is asked
first, with explained options and a recommendation. Record the answer as plain wording in the doc it
governs: the spec for behavior and contracts, an ADR (in `docs/adr/`) for choices that are expensive
to reverse, the PRD for scope. No decision IDs or conversation references: a reader must understand
the doc without having seen the conversation. History goes in commit messages, not in docs. A
default you pick without asking goes under "Open questions" in
[docs/plan/README.md](docs/plan/README.md#open-questions) until the owner confirms it.

For the big picture, see [docs/README.md](docs/README.md): PRD, architecture, ADRs, specs and plan.

**No duplication.** Before writing code, search for something that already does the job and reuse
it: a module or helper in this repo (e.g. `Pipeline::model_step`, `Pipeline::route`,
`Action::as_str`/`parse_final`, `UserSettingsStore::tr`) or an external tool the owner already runs
(mlx-air owns the model process, launchd owns restarts, Postgres owns scheduling state). Don't build
a parallel implementation, a second copy of a mapping, or a workaround around an existing component;
extend or fix the original instead. When you find duplication, remove it.

**Docs follow the code.** Any change to behavior, data model, topics, config, routes, CLI or modules
updates the docs in the same commit, and deletes docs for removed code. Docs point to the files that
implement them (Hugging Face style: a `Code:` line or a link per section, e.g.
[`pipeline.rs`](crates/gm/src/pipeline.rs)), so the next agent can open the code and check the doc
against it. `cargo test` checks this (`crates/gm/tests/docs.rs`): every route, `GM_*` variable,
topic, table and CLI command in the code must appear in `docs/spec/`, and every relative doc link must
resolve.

**Turn every mistake into a hard check.** When an error is found, by the owner or by you, in code,
docs or process, fix it, then ask whether a machine could have caught it. If a lint, clippy setting,
test, CI step or startup check could prevent it, add that check in the same change so the mistake
can't come back. Prefer checks that fail `cargo test` / `cargo clippy` over written rules agents must
remember. Only fall back to a rule in this file when no automatic check is practical, and say why.
Examples already in place: `tests/docs.rs` (docs drifting from code, decision IDs in docs), the locale
key-parity test (a string missing in one language), `gm rules` and the rule examples (a rules edit
that stops matching), and the config refusing a non-loopback bind.

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
