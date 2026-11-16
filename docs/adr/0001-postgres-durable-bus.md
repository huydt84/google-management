# 0001. Durable event bus on Postgres

- Status: Accepted
- Date: 2026-11-16

## Context

The pipeline is a chain of steps (ingest → triage → summarize → notify, later instructions and
approvals). Steps call slow or flaky services (Gmail, a local LLM, Telegram), so they must run
asynchronously, be retried, and never lose or duplicate work across crashes and sleep. The original
idea called for both a pub/sub system and a durable queue.

There is one user, one machine, and a few hundred events per day at most. Postgres is already needed
for state (accounts, cursors, events).

## Decision

Implement pub/sub and the durable queue as one small library on a Postgres table (`jobs`):

- `publish()` inserts one row per subscriber **inside the producer's transaction** (outbox pattern),
  so business state and messages commit or roll back together.
- Workers claim rows with `SELECT … FOR UPDATE SKIP LOCKED`, run the handler, and mark `done`, or
  reschedule with exponential backoff, or mark `dead` after the last attempt.
- Delivery is at-least-once; handlers are idempotent by checking the state of the row they act on.
- The interface (`publish`, `subscribe`, `handler(payload, ctx)`) is kept small so the backend can be
  replaced.

## Consequences

- One fewer service to run, monitor and back up; transactional guarantees come for free.
- Throughput is bounded by Postgres polling (idle poll every 1 s, plus an in-process wake-up on
  publish). That is far above this system's needs.
- `jobs` grows and must be pruned (Phase 2 retention job).
- Handlers must be written idempotently — documented as an architecture invariant.

## Alternatives considered

- **NATS JetStream:** excellent durable streams, but a separate process, and losing atomicity between
  the database write and the publish would require an outbox anyway. Kept as optional workstream 2F,
  backed by the same outbox.
- **Redis Streams:** another service, weaker durability defaults, same outbox problem.
- **Kafka / RabbitMQ:** operationally heavy for a single-user desktop system.
- **Celery / arq / Dramatiq:** need a broker (Redis/RabbitMQ) and don't give transactional publish.
