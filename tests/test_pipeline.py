"""End to end: fake Gmail -> ingest -> bus -> triage -> summarize -> notify."""

import httpx
import pytest
from conftest import OWNER, FakeGmail, RecordingNotifier, gmail_message, make_settings
from sqlalchemy import select

from gm.db import Event, GmailState
from gm.llm import LLMClient
from gm.main import build_deps

LLM_JSON = '{"summary": "Alice hỏi lịch họp", "importance": "high", ' \
           '"suggested_actions": ["trả lời"], "category": "work", "language": "en"}'


def llm(ok: bool = True) -> httpx.MockTransport:
    def handler(req):
        if not ok:
            return httpx.Response(503, json={"error": "busy"})
        return httpx.Response(200, json={"choices": [{"message": {"content": LLM_JSON}}]})
    return httpx.MockTransport(handler)


@pytest.fixture
def fake():
    return FakeGmail()


def make(engine, fake, *, llm_ok=True, **settings_kw):
    settings = make_settings(**settings_kw)
    notifier = RecordingNotifier()

    async def factory(email):
        return fake

    deps = build_deps(settings, engine=engine, gmail_factory=factory, notifier=notifier,
                      llm=LLMClient(settings, transport=llm(llm_ok)))
    return deps, notifier


async def drain(deps):
    while await deps.bus.run_once():
        pass


async def events(deps) -> dict[str, Event]:
    async with deps.sm() as s:
        return {e.external_id: e for e in (await s.execute(select(Event))).scalars()}


async def test_full_flow_with_dedupe(engine, fake):
    deps, notifier = make(engine, fake)
    await deps.creds.save(OWNER, "refresh", ["https://www.googleapis.com/auth/gmail.readonly"])
    fake.deliver(gmail_message("old"))  # before connecting: must not be processed
    await deps.gmail.bootstrap(OWNER)

    fake.deliver(gmail_message("m1", subject="Họp thứ 2?"))
    fake.deliver(gmail_message("promo", labels=["INBOX", "CATEGORY_PROMOTIONS"]))
    fake.deliver(gmail_message("sent", labels=["SENT"]))  # not in INBOX: skipped
    assert await deps.gmail.sync(OWNER) == 2
    assert await deps.gmail.sync(OWNER) == 0  # cursor advanced, nothing new
    await drain(deps)

    evs = await events(deps)
    assert set(evs) == {"m1", "promo"}
    assert evs["promo"].status == "ignored"
    assert evs["m1"].status == "notified" and evs["m1"].summary["importance"] == "high"
    assert len(notifier.sent) == 1
    assert "Họp thứ 2?" in notifier.sent[0] and "Alice hỏi lịch họp" in notifier.sent[0]


async def test_llm_down_still_notifies_on_last_attempt(engine, fake):
    deps, notifier = make(engine, fake, llm_ok=False, job_max_attempts=1)
    await deps.creds.save(OWNER, "refresh", [])
    await deps.gmail.bootstrap(OWNER)
    fake.deliver(gmail_message("m1"))
    await deps.gmail.sync(OWNER)
    await drain(deps)
    ev = (await events(deps))["m1"]
    assert ev.status == "notified" and "error" in ev.summary
    assert "Không tóm tắt được" in notifier.sent[0]


async def test_expired_history_recovers_recent_messages(engine, fake):
    deps, _ = make(engine, fake)
    await deps.creds.save(OWNER, "refresh", [])
    await deps.gmail.bootstrap(OWNER)
    fake.deliver(gmail_message("a"))
    await deps.gmail.sync(OWNER)
    fake.deliver(gmail_message("b"))
    fake.history_expired = True
    assert await deps.gmail.sync(OWNER) == 1  # "a" deduped, "b" recovered
    async with deps.sm() as s:
        st = await s.get(GmailState, OWNER)
    assert st.history_id == fake.history_id and st.last_error is None


async def test_message_deleted_before_triage(engine, fake):
    deps, notifier = make(engine, fake)
    await deps.creds.save(OWNER, "refresh", [])
    await deps.gmail.bootstrap(OWNER)
    fake.deliver(gmail_message("gone"))
    await deps.gmail.sync(OWNER)
    del fake.messages_by_id["gone"]
    await drain(deps)
    assert (await events(deps))["gone"].status == "gone" and not notifier.sent


async def test_pubsub_bootstrap_registers_watch(engine, fake):
    deps, _ = make(engine, fake, gmail_ingest_mode="pubsub",
                   gmail_pubsub_topic="projects/p/topics/t")
    await deps.creds.save(OWNER, "refresh", [])
    await deps.gmail.bootstrap(OWNER)
    async with deps.sm() as s:
        st = await s.get(GmailState, OWNER)
    assert fake.watch_calls == 1 and st.watch_expiration.year == 2100
