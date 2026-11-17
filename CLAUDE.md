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
- `crates/gm/src/digest.rs`, `scheduler.rs`, `health.rs`, `feedback.rs`, `triage_model.rs`,
  `eval.rs`: Phase 2 (digest, schedules, health alerts, 👍/👎, model triage, `gm eval`).
- `crates/gm/src/telegram/`, `tasks/`, `recipes/`, `google/compose.rs`, `web/tasks.rs`: Phase 3
  (Telegram poller and commands, owner tasks, router, approvals, audit, translate / analyze / reply).
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

## Verification and mistakes

**Prove it works on the real system.** Passing `cargo test` is not "done". Before you report a change
as working, run it the way the owner will: start `gm serve` (or the CLI command), drive the changed
flow (dashboard over `curl` or a browser, a real `gm` command, the local model at `GM_LLM_BASE_URL`),
and look at the result: page content, DB rows, logs. In the final report, separate what you
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

Show that a new check fails on the real past mistake. Add the lesson to the table below in the same
change. If a lesson with only a written rule happens again, move it up a layer. If two fixes built on
the same premise fail, stop and question the premise instead of trying a third.

| Mistake that happened | Enforcement |
|-----------------------|-------------|
| Docs drifted from the code (stale ADR, module table, routes, settings) | `crates/gm/tests/docs.rs` fails `cargo test` |
| Docs cited decision IDs that only made sense next to a conversation | `crates/gm/tests/docs.rs` |
| A string present in one language only | locale key-parity test |
| A `rules.yaml` edit that stops matching its intended mail | rule examples, checked by `gm rules` and `cargo test` |
| A fixed test clock fell in the past and deferred jobs looped | structural: `test_day()` is always a week ahead |
| Built gm-side model start/stop that duplicated mlx-air | rule only: "Check before you claim or build" (no check possible inside this repo) |
| Claimed the services weren't installed without looking at the Mac | lever: `gm status` reads launchd, the running build, `/healthz`, model `/ready`, migrations and the queue from the live Mac |
| A docs fix edited an already-applied migration (sqlx refuses to start on a changed checksum) | `gm status` fails on an applied migration whose file changed |
| A test set a time with Postgres `now()` and checked it with the host clock; the Docker VM clock runs ~1 ms ahead, so "16 min" read as 15 | rule only: in tests, take times that gm compares from the clock gm reads (`Utc::now()` or the `FixedClock`), not from `now()` in SQL |
| Called Phase 2 done on tests alone, never run against the real model or Gmail | rule only, for now: "Prove it works on the real system" |
| `gm status` said "restart to run it" right after the documented upgrade, because `ps` start times are whole seconds | unit test `a_build_in_the_same_second_as_the_start_is_not_stale` in `status.rs` |

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

## Commit Style
- Each commit must be able stand standalone - which means take a random commit and the app still run, all the checks are clean, all the verifications are passed
- Commit message: `<type>[optional scope]: <description>` follow by 3-5 bullet points. description and each bullet points are no more than 20 words each. No "Co-Authored-By..."
