# Google Management

Personal, single-user server on the owner's Mac: ingests Gmail and Calendar, triages it with a
tested rules file, summarizes with a local LLM (mlx-server-runtime, called directly;
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
- `crates/gm/src/digest.rs`, `scheduler.rs`, `health.rs`, `feedback.rs`, `triage_model.rs`,
  `eval.rs`: Phase 2 (digest, schedules, health alerts, 👍/👎, model triage, `gm eval`).
- `crates/gm/src/telegram/`, `tasks/`, `recipes/`, `google/compose.rs`, `web/tasks.rs`: Phase 3
  (Telegram poller and commands, owner tasks, router, approvals, audit, translate / analyze / reply).
- `crates/gm/src/calendar/`, `crates/gm/src/google/calendar.rs`: Phase 4 (primary-calendar sync, notifications, digest agenda).
- `crates/gm/src/gmail/`, `crates/gm/src/google/`: ingestion, parsing, OAuth, Gmail client.
- `crates/gm/migrations/`: schema. `crates/gm/locales/`: UI strings. `crates/gm/prompts/`: prompts.
- `crates/gm/tests/common/mod.rs`: `test_app`, wiremock helpers, `RecordingNotifier`, `FixedClock`.
- `deploy/`: launchd plists and service setup; `scripts/backup.sh`.

## Before you change anything

**Never decide silently.** Any product or technical choice the docs don't already settle is asked
first, with explained options and a recommendation. Record the answer as plain wording in the doc it
governs: the spec for behavior and contracts, an ADR (in `docs/adr/`) for architectural constraints
and tradeoffs, the PRD for scope. Describe the resulting contract, not who chose it or how the
conversation reached it. Every document must make sense from the codebase alone. Keep conversation
history, deployment reports, account consent state, live verification results and test-run totals
in chat or commit messages. Docs may include reproducible verification commands and expected behavior.
A default you pick without asking goes under "Open questions" in
[docs/plan/README.md](docs/plan/README.md#open-questions) until the owner confirms it.

For the big picture, see [docs/README.md](docs/README.md): PRD, architecture, ADRs, specs and plan.

Several agent sessions can work on this repo at once. Always work under the assumption that the git history will be contaminated or your working branch will contain uncommitted changes by other agents at any moment, so check carefully before each write-level git operation, only stage and commit your changes.

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

**Self-containment.** Don't contaminate documents with model's implicit decision or live verification performed inside the conversation. Only include self-contain information - that other developers, with only the codebase and documentation, can see and understand

**Follow best-pracice.** Whenever there is UI/UX complains, check UI/UX best practice to see how to deal with it, then ask the user for confirmation.

## Verification and mistakes

**Prove it works on the real system.** Passing `cargo test` is not "done". Before you report a change
as working, run it the way the owner will: start `gm serve` (or the CLI command), drive the changed
flow (dashboard over `curl` or a browser, a real `gm` command, the local model at `GM_LLM_BASE_URL`),
and look at the result: page content, DB rows, logs. Pages behind the Google sign-in are checked in the
owner's Chrome through the Claude in Chrome extension (`mcp__claude-in-chrome__*`), which has the
owner's Google session; the app's built-in browser pane has none. In the final report, separate what you
**verified** (with the command and its output), what you **inferred**, and what you **did not check**
and why. If you can't run something, report the blocker; don't call it verified.

**Check before you claim or build.** Never state what is installed, running or deployed from the repo
alone: run `gm status`, and check the machine (`launchctl list`, `ps`, `lsof -iTCP -sTCP:LISTEN`, `curl` the endpoints,
`_sqlx_migrations` in the DB). Before building anything that manages processes, schedules or
infrastructure, list what already runs there and reuse it.

**Turn mistakes into structure.** When the owner or you find a mistake (in the workflow or the
process, not only in code), fix the instance, then fix the class at the highest layer that works:

1. architecture (make the mistake impossible),
2. types,
3. lint / clippy / CI,
4. tests (`cargo test`),
5. a script or `gm` command the agent runs (a lever, not a memory),
6. a written rule in this file, last, because nothing fails when an agent skips a rule.

Show that a new check fails on the real past mistake and report that result in chat. Document the
current constraint and its enforcement, without an incident narrative. If a constraint with only a
written rule fails again, move it up a layer. If two fixes built on the same premise fail, stop and
question the premise instead of trying a third.

| Constraint | Enforcement |
|------------|-------------|
| Specs match routes, configuration, topics, schema and CLI commands | `crates/gm/tests/docs.rs` |
| Docs are self-contained and contain no conversation-specific decision IDs | `crates/gm/tests/docs.rs` |
| Every locale contains the same keys | locale key-parity test |
| Every rule matches its redacted examples | `gm rules` and `cargo test` |
| Deferred test jobs remain in the future | `test_day()` is always a week ahead |
| mlx-air owns model process management | Reuse mlx-air; gm only calls the model endpoint |
| Installation and runtime claims require current machine evidence | `gm status` reads launchd, the running build, health endpoints, migrations and the queue |
| Mail-only handlers and controls accept only Gmail events | Source checks and `crates/gm/tests/calendar_serve.rs` |
| Calendar field comparisons require a previous snapshot; formatting spans use UTF-16 offsets | Calendar integration tests and `calendar_serve.rs` |
| Applied migrations are immutable | `gm status` checks applied migration checksums |
| Test timestamps use the clock read by the code under test | Use `Utc::now()` or `FixedClock` for host-clock comparisons, not Postgres `now()` |
| Feature verification includes the actual runtime flow | Run the relevant command or service and inspect its result; report observations in chat |
| Build freshness comparisons account for second-resolution process times | `a_build_in_the_same_second_as_the_start_is_not_stale` in `status.rs` |
| The repo's `llm.yaml` is a valid sampling profile with a section for the default model | `the_repo_profile_is_valid_and_covers_the_default_model` in `llm/profile.rs` |

**Read [docs/plan/README.md](docs/plan/README.md).** It has the phase roadmap and workstream status,
the rules for parallel work (file ownership, linear migration numbering), the Definition of Done, and
the **architecture invariants** you must not break: idempotent handlers with transactional publish;
Google content is untrusted; external side effects only through approvals; minimal scopes; secrets
only in the Keychain and never logged; bind localhost only; all LLM calls through `LlmClient` with a
lane and no reliance on `tools`/`response_format`; model work deferred outside model hours; Telegram
literal text with app-owned formatting spans; every rule has redacted examples.

Per-phase plans: [phase-1.md](docs/plan/phase-1.md) (done), [phase-2.md](docs/plan/phase-2.md),
[phase-3.md](docs/plan/phase-3.md), [phase-4.md](docs/plan/phase-4.md),
[phase-5.md](docs/plan/phase-5.md).

Tests must never call real Google, Telegram, or LLM endpoints. Raw mail never goes into git.

## Commit Style
- Each commit must be able stand standalone - which means take a random commit and the app still run, all the checks are clean, all the verifications are passed
- Commit message: `<type>[optional scope]: <description>` follow by 3-5 bullet points. description and each bullet points are no more than 20 words each. No "Co-Authored-By..."
