import base64
import os
from pathlib import Path

import asyncpg
import httplib2
import pytest
from cryptography.fernet import Fernet
from googleapiclient.errors import HttpError

from gm.config import Settings
from gm.db import Base, make_engine, make_sessionmaker

TEST_DB = os.environ.get(
    "GM_TEST_DATABASE_URL", "postgresql+asyncpg://gm:gm@localhost:5433/gm_test"
)
ROOT = Path(__file__).resolve().parents[1]
OWNER = "owner@example.com"


def make_settings(**kw) -> Settings:
    base = dict(
        session_secret="s" * 48,
        encryption_key=Fernet.generate_key().decode(),
        allowed_email=OWNER,
        google_client_id="cid",
        google_client_secret="csecret",
        database_url=TEST_DB,
        base_url="http://localhost",
        rules_path=ROOT / "rules.yaml",
        run_background=False,
        llm_base_url="http://llm.test/v1",
    )
    base.update(kw)
    return Settings(_env_file=None, **base)  # type: ignore[call-arg]


@pytest.fixture
def settings() -> Settings:
    return make_settings()


async def _ensure_test_db() -> None:
    dsn = TEST_DB.replace("+asyncpg", "")
    admin_dsn, dbname = dsn.rsplit("/", 1)
    try:
        conn = await asyncpg.connect(admin_dsn + "/postgres", timeout=3)
    except Exception as e:  # noqa: BLE001
        pytest.skip(f"Postgres not reachable ({e}); run `docker compose up -d`")
    try:
        if not await conn.fetchval("select 1 from pg_database where datname=$1", dbname):
            await conn.execute(f'create database "{dbname}"')
    finally:
        await conn.close()


@pytest.fixture
async def engine():
    await _ensure_test_db()
    eng = make_engine(TEST_DB)
    async with eng.begin() as conn:
        await conn.run_sync(Base.metadata.drop_all)
        await conn.run_sync(Base.metadata.create_all)
    yield eng
    await eng.dispose()


@pytest.fixture
def sm(engine):
    return make_sessionmaker(engine)


# ---------------------------------------------------------------- fake Gmail API


def b64(s: str) -> str:
    return base64.urlsafe_b64encode(s.encode()).decode().rstrip("=")


def gmail_message(mid: str, *, subject="Hello", sender="Alice <alice@example.com>",
                  body="Hi there", labels=("INBOX", "CATEGORY_PERSONAL"), extra_headers=()):
    headers = [{"name": "From", "value": sender}, {"name": "To", "value": OWNER},
               {"name": "Subject", "value": subject},
               {"name": "Date", "value": "Mon, 16 Nov 2026 10:00:00 +0700"}, *extra_headers]
    return {
        "id": mid, "threadId": f"t-{mid}", "labelIds": list(labels), "snippet": body[:50],
        "payload": {"mimeType": "text/plain", "headers": headers, "body": {"data": b64(body)}},
    }


class _Exec:
    def __init__(self, fn):
        self._fn = fn

    def execute(self):
        return self._fn()


def http_error(status: int) -> HttpError:
    return HttpError(httplib2.Response({"status": status}), b"error")


class FakeGmail:
    """Just enough of googleapiclient's Gmail resource for the ingestor and pipeline."""

    def __init__(self, history_id: int = 100):
        self.history_id = history_id
        self.messages_by_id: dict[str, dict] = {}
        self.records: list[tuple[int, dict]] = []  # (history_id, message)
        self.history_expired = False
        self.watch_calls = 0

    # mailbox mutation used by tests
    def deliver(self, msg: dict) -> None:
        self.history_id += 1
        self.messages_by_id[msg["id"]] = msg
        self.records.append((self.history_id, msg))

    # resource chain
    def users(self):
        return self

    def getProfile(self, userId):
        return _Exec(lambda: {"emailAddress": OWNER, "historyId": str(self.history_id)})

    def watch(self, userId, body):
        self.watch_calls += 1
        return _Exec(lambda: {"historyId": str(self.history_id), "expiration": "4102444800000"})

    def messages(self):
        fake = self

        class _Messages:
            def get(self, userId, id, format):
                def run():
                    if id not in fake.messages_by_id:
                        raise http_error(404)
                    return fake.messages_by_id[id]
                return _Exec(run)

            def list(self, userId, labelIds, q, maxResults):
                return _Exec(lambda: {"messages": [
                    {"id": m["id"], "threadId": m["threadId"]}
                    for _, m in reversed(fake.records)
                ][:maxResults]})

        return _Messages()

    def history(self):
        fake = self

        class _History:
            def list(self, userId, startHistoryId, historyTypes, pageToken=None):
                def run():
                    if fake.history_expired:
                        raise http_error(404)
                    recs = [
                        {"id": str(h), "messagesAdded": [{"message": {
                            "id": m["id"], "threadId": m["threadId"], "labelIds": m["labelIds"]}}]}
                        for h, m in fake.records if h > int(startHistoryId)
                    ]
                    return {"history": recs, "historyId": str(fake.history_id)}
                return _Exec(run)

        return _History()


class RecordingNotifier:
    def __init__(self):
        self.sent: list[str] = []

    async def send(self, text: str) -> None:
        self.sent.append(text)

    async def aclose(self) -> None:
        pass
