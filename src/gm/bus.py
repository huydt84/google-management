"""Durable pub/sub on top of Postgres.

publish(topic) inserts one `jobs` row per subscriber of that topic, inside the caller's
transaction (outbox pattern): if the business write commits, the delivery is guaranteed;
if it rolls back, nothing is delivered. Workers claim rows with FOR UPDATE SKIP LOCKED, so
several workers (or processes) can run safely. Delivery is at-least-once: handlers must
be idempotent. Failed jobs are retried with exponential backoff, then marked `dead`.

The interface (publish/subscribe) is small on purpose so it can later be backed by
NATS JetStream without touching handlers.
"""

import asyncio
import logging
import traceback
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from typing import Any

from sqlalchemy import func, or_, select, update
from sqlalchemy.ext.asyncio import AsyncSession, async_sessionmaker

from gm.db import Job

log = logging.getLogger(__name__)


@dataclass(frozen=True)
class JobContext:
    job_id: int
    topic: str
    subscriber: str
    attempt: int
    max_attempts: int

    @property
    def is_last_attempt(self) -> bool:
        return self.attempt >= self.max_attempts


Handler = Callable[[dict[str, Any], JobContext], Awaitable[None]]


@dataclass(frozen=True)
class Subscription:
    name: str
    topic: str
    handler: Handler
    max_attempts: int


def backoff_delay(attempt: int, base_s: float = 5.0, cap_s: float = 900.0) -> timedelta:
    return timedelta(seconds=min(cap_s, base_s * (2 ** (attempt - 1))))


class EventBus:
    def __init__(
        self,
        sessionmaker: async_sessionmaker,
        *,
        default_max_attempts: int = 5,
        idle_poll_s: float = 1.0,
        stale_after: timedelta = timedelta(minutes=10),
    ):
        self._sm = sessionmaker
        self._subs: dict[str, Subscription] = {}
        self._wake = asyncio.Event()
        self._default_max_attempts = default_max_attempts
        self._idle_poll_s = idle_poll_s
        self._stale_after = stale_after

    # ---------------------------------------------------------------- subscribe / publish

    def subscribe(self, topic: str, name: str, handler: Handler, max_attempts: int | None = None):
        if name in self._subs:
            raise ValueError(f"subscriber {name!r} already registered")
        self._subs[name] = Subscription(
            name, topic, handler, max_attempts or self._default_max_attempts
        )

    def subscribers_of(self, topic: str) -> list[Subscription]:
        return [s for s in self._subs.values() if s.topic == topic]

    def publish(self, session: AsyncSession, topic: str, payload: dict[str, Any]) -> int:
        """Stage deliveries in `session`. They become visible when the caller commits."""
        subs = self.subscribers_of(topic)
        if not subs:
            log.warning("publish to %s with no subscribers; dropped", topic)
        for s in subs:
            session.add(
                Job(topic=topic, subscriber=s.name, payload=payload, max_attempts=s.max_attempts)
            )
        self._wake.set()
        return len(subs)

    # ---------------------------------------------------------------- worker

    async def run_worker(self, worker_id: int = 0) -> None:
        log.info("bus worker %d started", worker_id)
        while True:
            try:
                processed = await self.run_once()
            except asyncio.CancelledError:
                raise
            except Exception:
                log.exception("bus worker %d loop error", worker_id)
                processed = False
            if not processed:
                self._wake.clear()
                try:
                    await asyncio.wait_for(self._wake.wait(), timeout=self._idle_poll_s)
                except TimeoutError:
                    pass

    async def run_once(self) -> bool:
        """Claim and process one job. Returns False when the queue had nothing ready."""
        claimed = await self._claim()
        if claimed is None:
            return False
        job_id, sub, payload, ctx = claimed
        try:
            await sub.handler(payload, ctx)
        except asyncio.CancelledError:
            # Shutting down mid-job: give it back without burning an attempt.
            await self._release(job_id)
            raise
        except Exception as exc:
            err = "".join(traceback.format_exception(exc))[-4000:]
            dead = ctx.is_last_attempt
            log.warning(
                "job %d (%s/%s) attempt %d/%d failed%s: %s",
                job_id, ctx.topic, sub.name, ctx.attempt, ctx.max_attempts,
                " -> dead" if dead else "", exc,
            )
            await self._fail(job_id, err, dead, backoff_delay(ctx.attempt))
        else:
            await self._finish(job_id)
        return True

    async def _claim(self):
        names = list(self._subs)
        if not names:
            return None
        async with self._sm() as s, s.begin():
            stmt = (
                select(Job)
                .where(
                    Job.status == "pending",
                    Job.run_after <= func.now(),
                    Job.subscriber.in_(names),
                )
                .order_by(Job.id)
                .limit(1)
                .with_for_update(skip_locked=True)
            )
            job = (await s.execute(stmt)).scalar_one_or_none()
            if job is None:
                return None
            job.status = "running"
            job.attempts += 1
            job.locked_at = datetime.now(UTC)
            ctx = JobContext(job.id, job.topic, job.subscriber, job.attempts, job.max_attempts)
            return job.id, self._subs[job.subscriber], dict(job.payload), ctx

    async def _finish(self, job_id: int) -> None:
        async with self._sm() as s, s.begin():
            await s.execute(
                update(Job).where(Job.id == job_id).values(status="done", locked_at=None)
            )

    async def _fail(self, job_id: int, err: str, dead: bool, delay: timedelta) -> None:
        async with self._sm() as s, s.begin():
            await s.execute(
                update(Job)
                .where(Job.id == job_id)
                .values(
                    status="dead" if dead else "pending",
                    last_error=err,
                    locked_at=None,
                    run_after=datetime.now(UTC) + delay,
                )
            )

    async def _release(self, job_id: int) -> None:
        async with self._sm() as s, s.begin():
            await s.execute(
                update(Job)
                .where(Job.id == job_id)
                .values(status="pending", locked_at=None, attempts=Job.attempts - 1)
            )

    async def requeue_stale(self, older_than: timedelta | None = None) -> int:
        """Jobs stuck in `running` (process crashed mid-handler) go back to pending.

        At startup of a single-process deployment pass older_than=timedelta(0): anything
        still `running` belongs to a previous, dead process.
        """
        cutoff = datetime.now(UTC) - (self._stale_after if older_than is None else older_than)
        async with self._sm() as s, s.begin():
            res = await s.execute(
                update(Job)
                .where(
                    Job.status == "running",
                    or_(Job.locked_at.is_(None), Job.locked_at < cutoff),
                )
                .values(status="pending", locked_at=None)
            )
            n = res.rowcount or 0
        if n:
            log.warning("requeued %d stale running jobs", n)
        return n

    async def stats(self) -> dict[str, int]:
        async with self._sm() as s:
            rows = await s.execute(select(Job.status, func.count()).group_by(Job.status))
            return {status: count for status, count in rows.all()}
