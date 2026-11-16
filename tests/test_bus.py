from datetime import timedelta

from sqlalchemy import select, update

from gm.bus import EventBus, backoff_delay
from gm.db import Job


async def test_publish_is_transactional_and_fans_out(sm):
    bus = EventBus(sm)
    got: list[tuple[str, dict]] = []

    async def a(p, ctx):
        got.append(("a", p))

    async def b(p, ctx):
        got.append(("b", p))

    bus.subscribe("t", "a", a)
    bus.subscribe("t", "b", b)

    # rolled back -> nothing delivered
    async with sm() as s:
        async with s.begin():
            bus.publish(s, "t", {"n": 0})
            await s.rollback()
    assert not await bus.run_once()

    async with sm() as s, s.begin():
        assert bus.publish(s, "t", {"n": 1}) == 2
    while await bus.run_once():
        pass
    assert sorted(got) == [("a", {"n": 1}), ("b", {"n": 1})]
    assert await bus.stats() == {"done": 2}


async def test_retry_with_backoff_then_dead(sm):
    bus = EventBus(sm)
    attempts = []

    async def flaky(p, ctx):
        attempts.append(ctx.attempt)
        raise RuntimeError("boom")

    bus.subscribe("t", "flaky", flaky, max_attempts=2)
    async with sm() as s, s.begin():
        bus.publish(s, "t", {})

    assert await bus.run_once()
    async with sm() as s:
        job = (await s.execute(select(Job))).scalar_one()
        assert job.status == "pending" and job.attempts == 1 and "boom" in job.last_error
        # Not ready yet because of backoff.
    assert not await bus.run_once()

    async with sm() as s, s.begin():
        await s.execute(update(Job).values(run_after=job.created_at))
    assert await bus.run_once()
    assert attempts == [1, 2]
    assert await bus.stats() == {"dead": 1}


async def test_requeue_stale_running_jobs(sm):
    bus = EventBus(sm)

    async def ok(p, ctx):
        pass

    bus.subscribe("t", "ok", ok)
    async with sm() as s, s.begin():
        bus.publish(s, "t", {})
    async with sm() as s, s.begin():
        await s.execute(update(Job).values(status="running"))
    assert await bus.requeue_stale(older_than=timedelta(0)) == 1
    assert await bus.run_once()


def test_backoff_is_capped():
    assert backoff_delay(1) == timedelta(seconds=5)
    assert backoff_delay(3) == timedelta(seconds=20)
    assert backoff_delay(50) == timedelta(seconds=900)
