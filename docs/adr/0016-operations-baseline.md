# 0016. Operations baseline: CI, tests, observability, backups

- Status: Accepted
- Date: 2026-11-16

## Context

The system runs unattended, and several sessions may work on it in parallel. Tests must never call real
Google, Telegram or LLM endpoints.

## Decision

- **Hosting/CI:** private GitHub repository. A GitHub Actions workflow on Linux runs `cargo fmt
  --check`, `cargo clippy -D warnings`, `cargo test` against a Postgres 17 service container, and
  `gm rules rules.yaml` (`.github/workflows/ci.yml`).
- **Tests:** `#[sqlx::test]` gives each test a fresh database with migrations applied; `wiremock`
  serves fake Google, Telegram and LLM HTTP; a few end-to-end tests drive ingest → triage → summary →
  notify. Rule examples from `rules.yaml` are tests too.
- **Observability:** `tracing` JSON logs to `~/Library/Logs/gm/` with daily rotation (7 files);
  Prometheus text at `/metrics` behind a bearer token (events by state, jobs by status, LLM latency and
  tokens per lane, ingest lag, model health); a health panel on the dashboard; a Telegram alert
  when health stays bad for 15 minutes (dead jobs, ingest lag over 10 minutes, model down during
  model hours, Google token invalid).
- **Backups:** nightly `pg_dump -Fc` via launchd to `~/Backups/google-management/`, keep 14, with a
  documented `pg_restore` procedure. The master key is in the Keychain, so a dump alone cannot decrypt
  tokens.

## Consequences

- Rust compile + Postgres in CI takes a few minutes per push. Queries are runtime-checked, so no
  sqlx offline data has to be refreshed; SQL mistakes surface in the test suite instead of at compile
  time.

## Alternatives considered

- **Local-only repo:** no off-machine copy, no PR gate.
- **Trait mocks only / testcontainers:** respectively, no real SQL behavior / slower and Docker-bound.
- **Prometheus + Grafana locally:** nicer graphs, two more containers.
