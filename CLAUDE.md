# Google Management

Personal, single-user web server on the owner's Mac: ingests Google events (Gmail now; Calendar and
Drive later), triages them, asks a local LLM (mlx-server-runtime, later via LiteLLM) to summarize, and
notifies via Telegram. Only `GM_ALLOWED_EMAIL` can log in. Python 3.13 + FastAPI + SQLAlchemy async +
Postgres. Docs and UI text are in Vietnamese.

## Commands

```bash
docker compose up -d          # Postgres on 127.0.0.1:5433
uv sync                       # add --extra pubsub for Gmail Pub/Sub mode
uv run gm serve               # web + background workers (reads .env)
uv run pytest                 # needs Postgres; creates DB gm_test
uv run ruff check src tests
```

## Where things are

- `src/gm/main.py`: app factory, `build_deps`, background tasks.
- `src/gm/bus.py`: durable pub/sub on Postgres.
- `src/gm/pipeline.py`: triage → summarize → notify handlers.
- `src/gm/gmail/`: Gmail ingestion and message parsing.
- `tests/conftest.py`: fakes (`FakeGmail`, `RecordingNotifier`) and DB fixtures.

## Before you change anything

**Read [docs/plan/README.md](docs/plan/README.md).** It has:

- The phase roadmap and the workstream status table.
- The rules for running workstreams in parallel (file ownership, one Alembic head).
- The Definition of Done.
- The **architecture invariants** you must not break: idempotent handlers with transactional publish;
  Google content is untrusted; external side effects only through approvals; minimal scopes; secrets
  never logged; bind localhost only; all LLM calls through `LLMClient` with no reliance on
  `tools`/`response_format`.

Per-phase plans: [phase-1.md](docs/plan/phase-1.md), [phase-2.md](docs/plan/phase-2.md),
[phase-3.md](docs/plan/phase-3.md).

Tests must never call real Google, Telegram, or LLM endpoints.
