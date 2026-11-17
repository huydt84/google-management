# 0009. Rust single binary replaces the Python implementation

- Status: Accepted
- Date: 2026-11-16
- Amends: [0005](0005-single-process-localhost-deployment.md) (process model kept, language changed)

## Context

Phase 0 was written in Python (FastAPI, SQLAlchemy, Authlib). The owner chose Rust for the long-lived
system: one compiled binary, strong typing across the bus payloads and the database, and low idle
resource use on a Mac that also runs the LLM.

## Decision

- A Cargo workspace at the repository root. One binary `gm` (subcommands `serve`, `migrate`, `gen-key`,
  `keychain`, `rules`, `eval`; see [`main.rs`](../../crates/gm/src/main.rs)) runs everything as tokio
  tasks: the axum web server, Gmail ingestion, bus workers, the scheduler and health checks.
- Libraries: axum (web), sqlx (Postgres, embedded migrations; queries via `sqlx::query`/`query_as`,
  checked at runtime and by the test suite, confirmed by the owner on 2026-11-16), reqwest with thin hand-written clients for Google OAuth/Gmail and Telegram, tracing,
  askama + htmx (see [0014](0014-web-ui-askama-htmx-fluent.md)), fluent (i18n), serde, chrono.
- Python Phase 0 is ported **to parity** first (same behavior, same states and topics, same tests
  rewritten in Rust). After parity is shown, the Python code is deleted in its own commit.
- The Postgres schema is recreated by sqlx migrations. There is no data migration from the Python
  schema: Phase 0 stored only re-ingestable events. You reconnect Google once.

## Consequences

- One artifact to install and run under launchd; no virtualenv.
- Hand-written Google clients mean more code, but exact control over scopes, retries and logging
  (secrets never in logs).
- Compile times are longer than Python edits; `cargo watch` and askama keep the loop acceptable.

## Alternatives considered

- **Stay on Python:** working code, faster iteration; rejected by the owner.
- **Several Rust services:** more isolation, more moving parts for one user.
- **google-apis-rs generated clients:** large dependency, awkward for incremental-sync semantics.
