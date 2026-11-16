import asyncio
import logging

from google.oauth2.credentials import Credentials
from googleapiclient.discovery import build
from sqlalchemy import select
from sqlalchemy.dialects.postgresql import insert
from sqlalchemy.ext.asyncio import async_sessionmaker

from gm.config import Settings
from gm.crypto import SecretBox
from gm.db import GoogleAccount

log = logging.getLogger(__name__)

TOKEN_URI = "https://oauth2.googleapis.com/token"

# Phase 0 only reads mail. Phase 1 adds gmail.compose (drafts) — still never full send scope
# without an approval step.
GMAIL_SCOPES = ["https://www.googleapis.com/auth/gmail.readonly"]


class AccountNotConnected(Exception):
    pass


class GoogleCredentialStore:
    def __init__(self, settings: Settings, sessionmaker: async_sessionmaker, box: SecretBox):
        self._settings = settings
        self._sm = sessionmaker
        self._box = box
        self._cache: dict[str, Credentials] = {}
        self._lock = asyncio.Lock()

    async def save(self, email: str, refresh_token: str, scopes: list[str]) -> None:
        email = email.lower()
        stmt = insert(GoogleAccount).values(
            email=email,
            refresh_token_enc=self._box.encrypt(refresh_token),
            scopes=scopes,
            status="active",
        )
        stmt = stmt.on_conflict_do_update(
            index_elements=[GoogleAccount.email],
            set_={
                "refresh_token_enc": stmt.excluded.refresh_token_enc,
                "scopes": stmt.excluded.scopes,
                "status": "active",
            },
        )
        async with self._sm() as s, s.begin():
            await s.execute(stmt)
        self._cache.pop(email, None)

    async def account(self, email: str) -> GoogleAccount | None:
        async with self._sm() as s:
            return await s.get(GoogleAccount, email.lower())

    async def active_emails(self) -> list[str]:
        async with self._sm() as s:
            rows = await s.execute(
                select(GoogleAccount.email).where(GoogleAccount.status == "active")
            )
            return list(rows.scalars())

    async def mark_needs_reconnect(self, email: str) -> None:
        async with self._sm() as s, s.begin():
            acc = await s.get(GoogleAccount, email.lower())
            if acc:
                acc.status = "needs_reconnect"
        self._cache.pop(email.lower(), None)
        log.error("Google account %s needs to be reconnected (refresh token rejected)", email)

    async def credentials(self, email: str) -> Credentials:
        email = email.lower()
        async with self._lock:
            if email in self._cache:
                return self._cache[email]
            acc = await self.account(email)
            if acc is None or acc.status != "active":
                raise AccountNotConnected(email)
            creds = Credentials(
                token=None,
                refresh_token=self._box.decrypt(acc.refresh_token_enc),
                token_uri=TOKEN_URI,
                client_id=self._settings.google_client_id,
                client_secret=self._settings.google_client_secret,
                scopes=acc.scopes,
            )
            self._cache[email] = creds
            return creds

    async def gmail(self, email: str):
        """A Gmail API client. httplib2 is not thread-safe, so build one per use."""
        creds = await self.credentials(email)
        return build("gmail", "v1", credentials=creds, cache_discovery=False)
