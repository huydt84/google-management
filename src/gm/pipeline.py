"""Phase 0 event pipeline, wired as bus subscribers:

gmail.message.received --triage--> gmail.message.summarize --summarize--> notify.email --notify-->

Every handler is idempotent: it checks the event's status before doing work, because the bus
delivers at-least-once.
"""

import asyncio
import logging
from typing import Any

from googleapiclient.errors import HttpError
from sqlalchemy.ext.asyncio import async_sessionmaker

from gm.bus import EventBus, JobContext
from gm.db import Event
from gm.gmail.ingest import TOPIC_RECEIVED
from gm.gmail.message import ParsedMessage, parse_message
from gm.llm import LLMClient
from gm.notify.telegram import Notifier, format_email_notification
from gm.triage import RuleFile, evaluate

log = logging.getLogger(__name__)

TOPIC_SUMMARIZE = "gmail.message.summarize"
TOPIC_NOTIFY = "notify.email"

STATUS_BY_ACTION = {"ignore": "ignored", "log": "logged", "notify": "triaged"}


class Pipeline:
    def __init__(self, *, sessionmaker: async_sessionmaker, bus: EventBus, gmail_factory,
                 rules: RuleFile, llm: LLMClient, notifier: Notifier):
        self._sm = sessionmaker
        self._bus = bus
        self._gmail = gmail_factory
        self._rules = rules
        self._llm = llm
        self._notifier = notifier

    def register(self) -> None:
        self._bus.subscribe(TOPIC_RECEIVED, "triage", self.triage)
        self._bus.subscribe(TOPIC_SUMMARIZE, "summarize", self.summarize)
        self._bus.subscribe(TOPIC_NOTIFY, "notify", self.notify)

    async def _fetch(self, account: str, message_id: str) -> ParsedMessage:
        svc = await self._gmail(account)
        raw = await asyncio.to_thread(
            lambda: svc.users().messages().get(userId="me", id=message_id, format="full").execute()
        )
        return parse_message(raw)

    async def _status(self, event_id: int) -> str | None:
        async with self._sm() as s:
            ev = await s.get(Event, event_id)
            return ev.status if ev else None

    # ------------------------------------------------------------------ triage

    async def triage(self, p: dict[str, Any], ctx: JobContext) -> None:
        if await self._status(p["event_id"]) != "new":
            return
        try:
            msg = await self._fetch(p["account"], p["message_id"])
        except HttpError as e:
            if e.resp.status == 404:  # deleted before we got to it
                await self._set_status(p["event_id"], "new", "gone")
                return
            raise
        verdict = evaluate(self._rules.get(), msg)
        async with self._sm() as s, s.begin():
            ev = await s.get(Event, p["event_id"], with_for_update=True)
            if ev is None or ev.status != "new":
                return
            ev.data = {**ev.data, **msg.brief(), "link": msg.gmail_link(p["account"])}
            ev.rule = verdict.rule
            ev.status = STATUS_BY_ACTION[verdict.action]
            if verdict.action == "notify":
                self._bus.publish(s, TOPIC_SUMMARIZE, p)
        log.info("event %s triaged: %s (%s)", p["event_id"], verdict.action, verdict.rule)

    # ------------------------------------------------------------------ summarize

    async def summarize(self, p: dict[str, Any], ctx: JobContext) -> None:
        if await self._status(p["event_id"]) != "triaged":
            return
        try:
            msg = await self._fetch(p["account"], p["message_id"])
            summary: dict[str, Any] = (await self._llm.summarize_email(msg)).model_dump()
        except Exception as e:
            if not ctx.is_last_attempt:
                raise
            # Out of retries: still tell the owner the email arrived, just without a summary.
            log.error("summary for event %s failed permanently: %s", p["event_id"], e)
            summary = {"error": f"{type(e).__name__}: {e}"[:300]}
        async with self._sm() as s, s.begin():
            ev = await s.get(Event, p["event_id"], with_for_update=True)
            if ev is None or ev.status != "triaged":
                return
            ev.summary = summary
            ev.status = "summarized"
            self._bus.publish(s, TOPIC_NOTIFY, {"event_id": p["event_id"]})

    # ------------------------------------------------------------------ notify

    async def notify(self, p: dict[str, Any], ctx: JobContext) -> None:
        async with self._sm() as s:
            ev = await s.get(Event, p["event_id"])
        if ev is None or ev.status != "summarized":
            return
        summary = ev.summary or {}
        text = format_email_notification(
            sender=ev.data.get("from", "?"),
            subject=ev.data.get("subject", "(no subject)"),
            link=ev.data.get("link", ""),
            summary=None if "error" in summary else summary,
            error=summary.get("error"),
        )
        await self._notifier.send(text)
        await self._set_status(p["event_id"], "summarized", "notified")

    async def _set_status(self, event_id: int, expected: str, new: str) -> None:
        async with self._sm() as s, s.begin():
            ev = await s.get(Event, event_id, with_for_update=True)
            if ev is not None and ev.status == expected:
                ev.status = new
