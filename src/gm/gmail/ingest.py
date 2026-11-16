"""Gmail ingestion: turn mailbox changes into `gmail.message.received` events.

Both modes funnel into `sync()`, which is idempotent:
- poll:   call sync() every N seconds.
- pubsub: users.watch() makes Gmail publish {emailAddress, historyId} to a Pub/Sub topic.
          We *pull* that subscription (no public URL needed), then call sync(). A slow safety
          poll also runs because Gmail may delay or drop notifications.

The cursor (history_id), the new `events` rows and the bus deliveries are written in one
transaction, so a crash can never lose or double-publish a message.
"""

import asyncio
import json
import logging
from collections.abc import Awaitable, Callable
from datetime import UTC, datetime, timedelta
from typing import Any

from google.auth.exceptions import RefreshError
from googleapiclient.errors import HttpError
from sqlalchemy.dialects.postgresql import insert
from sqlalchemy.ext.asyncio import async_sessionmaker

from gm.bus import EventBus
from gm.config import Settings
from gm.db import Event, GmailState
from gm.google_creds import AccountNotConnected, GoogleCredentialStore

log = logging.getLogger(__name__)

TOPIC_RECEIVED = "gmail.message.received"
RECOVERY_QUERY = "newer_than:2d"


class GmailIngestor:
    def __init__(
        self,
        settings: Settings,
        sessionmaker: async_sessionmaker,
        bus: EventBus,
        creds: GoogleCredentialStore,
        gmail_factory: Callable[[str], Awaitable[Any]] | None = None,
    ):
        self._s = settings
        self._sm = sessionmaker
        self._bus = bus
        self._creds = creds
        self._gmail = gmail_factory or creds.gmail
        self._locks: dict[str, asyncio.Lock] = {}

    # ------------------------------------------------------------------ helpers

    async def _call(self, email: str, fn: Callable[[Any], Any]) -> Any:
        svc = await self._gmail(email)
        return await asyncio.to_thread(lambda: fn(svc).execute())

    def _wanted(self, label_ids: list[str]) -> bool:
        return bool(set(label_ids) & set(self._s.gmail_label_ids))

    async def _state(self, email: str) -> GmailState | None:
        async with self._sm() as s:
            return await s.get(GmailState, email)

    async def _record_error(self, email: str, err: str) -> None:
        async with self._sm() as s, s.begin():
            st = await s.get(GmailState, email)
            if st is None:
                st = GmailState(email=email)
                s.add(st)
            st.last_error = err[:2000]

    # ------------------------------------------------------------------ bootstrap / watch

    async def bootstrap(self, email: str) -> None:
        """Start the cursor at "now": only mail arriving after connecting is processed."""
        profile = await self._call(email, lambda g: g.users().getProfile(userId="me"))
        stmt = insert(GmailState).values(email=email, history_id=int(profile["historyId"]))
        stmt = stmt.on_conflict_do_nothing(index_elements=[GmailState.email])
        async with self._sm() as s, s.begin():
            await s.execute(stmt)
        log.info("gmail %s bootstrapped at historyId %s", email, profile["historyId"])
        if self._s.gmail_ingest_mode == "pubsub":
            await self.renew_watch(email)

    async def renew_watch(self, email: str) -> None:
        if not self._s.gmail_pubsub_topic:
            raise RuntimeError("GM_GMAIL_PUBSUB_TOPIC is required in pubsub mode")
        body = {
            "topicName": self._s.gmail_pubsub_topic,
            "labelIds": self._s.gmail_label_ids,
            "labelFilterBehavior": "include",
        }
        resp = await self._call(email, lambda g: g.users().watch(userId="me", body=body))
        exp = datetime.fromtimestamp(int(resp["expiration"]) / 1000, UTC)
        async with self._sm() as s, s.begin():
            st = await s.get(GmailState, email)
            if st is None:
                st = GmailState(email=email, history_id=int(resp["historyId"]))
                s.add(st)
            st.watch_expiration = exp
        log.info("gmail watch for %s renewed until %s", email, exp.isoformat())

    # ------------------------------------------------------------------ sync

    async def sync(self, email: str) -> int:
        """Fetch changes since the stored cursor. Returns the number of new events."""
        lock = self._locks.setdefault(email, asyncio.Lock())
        async with lock:
            try:
                return await self._sync_locked(email)
            except RefreshError as e:
                await self._creds.mark_needs_reconnect(email)
                await self._record_error(email, f"refresh token rejected: {e}")
                raise
            except AccountNotConnected:
                raise
            except Exception as e:
                await self._record_error(email, f"{type(e).__name__}: {e}")
                raise

    async def _sync_locked(self, email: str) -> int:
        state = await self._state(email)
        if state is None or state.history_id is None:
            await self.bootstrap(email)
            return 0

        start = state.history_id
        found: dict[str, str] = {}  # message_id -> thread_id (ordered, deduped)
        latest = start
        try:
            page_token = None
            while True:
                kwargs = {"userId": "me", "startHistoryId": start,
                          "historyTypes": ["messageAdded"]}
                if page_token:
                    kwargs["pageToken"] = page_token
                resp = await self._call(email, lambda g, kw=kwargs: g.users().history().list(**kw))
                for record in resp.get("history", []):
                    for added in record.get("messagesAdded", []):
                        m = added["message"]
                        if self._wanted(m.get("labelIds", [])):
                            found.setdefault(m["id"], m.get("threadId", m["id"]))
                latest = max(latest, int(resp.get("historyId", latest)))
                page_token = resp.get("nextPageToken")
                if not page_token:
                    break
        except HttpError as e:
            if e.resp.status != 404:
                raise
            # startHistoryId is too old (Gmail keeps ~1 week). Re-anchor and recover recent mail;
            # event dedupe makes re-publishing already-seen messages harmless.
            log.warning("gmail %s history %s expired; recovering recent messages", email, start)
            found, latest = await self._recover(email)

        return await self._commit(email, found, latest)

    async def _recover(self, email: str) -> tuple[dict[str, str], int]:
        profile = await self._call(email, lambda g: g.users().getProfile(userId="me"))
        resp = await self._call(
            email,
            lambda g: g.users().messages().list(
                userId="me", labelIds=self._s.gmail_label_ids, q=RECOVERY_QUERY, maxResults=50
            ),
        )
        found = {m["id"]: m.get("threadId", m["id"]) for m in reversed(resp.get("messages", []))}
        return found, int(profile["historyId"])

    async def _commit(self, email: str, found: dict[str, str], latest: int) -> int:
        published = 0
        async with self._sm() as s, s.begin():
            for message_id, thread_id in found.items():
                stmt = (
                    insert(Event)
                    .values(
                        account=email,
                        source="gmail",
                        kind="message.received",
                        external_id=message_id,
                        status="new",
                        data={"thread_id": thread_id},
                    )
                    .on_conflict_do_nothing(constraint="uq_events_source_ext")
                    .returning(Event.id)
                )
                event_id = (await s.execute(stmt)).scalar_one_or_none()
                if event_id is None:
                    continue  # already seen
                self._bus.publish(s, TOPIC_RECEIVED, {
                    "event_id": event_id, "account": email,
                    "message_id": message_id, "thread_id": thread_id,
                })
                published += 1
            st = await s.get(GmailState, email)
            if st is None:
                st = GmailState(email=email)
                s.add(st)
            st.history_id = max(st.history_id or 0, latest)
            st.last_sync_at = datetime.now(UTC)
            st.last_error = None
        if published:
            log.info("gmail %s: %d new message(s)", email, published)
        return published

    async def sync_all(self) -> None:
        for email in await self._creds.active_emails():
            try:
                await self.sync(email)
            except AccountNotConnected:
                pass
            except Exception:
                log.exception("gmail sync failed for %s", email)

    # ------------------------------------------------------------------ background loops

    async def poll_loop(self, interval_s: int) -> None:
        log.info("gmail poll loop every %ss", interval_s)
        while True:
            await self.sync_all()
            await asyncio.sleep(interval_s)

    async def watch_renew_loop(self, check_every: timedelta = timedelta(hours=6)) -> None:
        while True:
            for email in await self._creds.active_emails():
                try:
                    st = await self._state(email)
                    exp = st.watch_expiration if st else None
                    if exp is None or exp - datetime.now(UTC) < timedelta(days=2):
                        await self.renew_watch(email)
                except Exception:
                    log.exception("gmail watch renewal failed for %s", email)
            await asyncio.sleep(check_every.total_seconds())

    async def pubsub_loop(self) -> None:
        """Pull Gmail notifications. Ack only after sync() committed (at-least-once)."""
        try:
            from google.cloud import pubsub_v1
        except ImportError as e:
            raise RuntimeError("pubsub mode needs: uv sync --extra pubsub") from e
        sub_path = self._s.gmail_pubsub_subscription
        if not sub_path:
            raise RuntimeError("GM_GMAIL_PUBSUB_SUBSCRIPTION is required in pubsub mode")

        client = pubsub_v1.SubscriberClient()  # Application Default Credentials
        log.info("gmail pubsub pulling %s", sub_path)
        while True:
            try:
                resp = await asyncio.to_thread(
                    client.pull,
                    request={"subscription": sub_path, "max_messages": 20},
                    timeout=60,
                )
            except asyncio.CancelledError:
                raise
            except Exception as e:
                if "DeadlineExceeded" not in type(e).__name__:
                    log.warning("pubsub pull failed: %s", e)
                    await asyncio.sleep(5)
                continue

            ack_ids: list[str] = []
            emails: set[str] = set()
            active = set(await self._creds.active_emails())
            for rm in resp.received_messages:
                try:
                    data = json.loads(rm.message.data.decode())
                    email = str(data.get("emailAddress", "")).lower()
                except (ValueError, UnicodeDecodeError):
                    email = ""
                if email in active:
                    emails.add(email)
                ack_ids.append(rm.ack_id)  # unknown/malformed: ack and drop

            failed = False
            for email in emails:
                try:
                    await self.sync(email)
                except Exception:
                    failed = True
                    log.exception("gmail sync after notification failed for %s", email)
            if ack_ids and not failed:
                await asyncio.to_thread(
                    client.acknowledge,
                    request={"subscription": sub_path, "ack_ids": ack_ids},
                )
