from datetime import datetime
from typing import Any

from sqlalchemy import (
    BigInteger,
    DateTime,
    Index,
    Integer,
    String,
    Text,
    UniqueConstraint,
    func,
)
from sqlalchemy.dialects.postgresql import JSONB
from sqlalchemy.ext.asyncio import AsyncEngine, async_sessionmaker, create_async_engine
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column


class Base(DeclarativeBase):
    pass


class Timestamps:
    created_at: Mapped[datetime] = mapped_column(
        DateTime(timezone=True), server_default=func.now()
    )
    updated_at: Mapped[datetime] = mapped_column(
        DateTime(timezone=True), server_default=func.now(), onupdate=func.now()
    )


class GoogleAccount(Timestamps, Base):
    """A connected Google account. The refresh token is Fernet-encrypted."""

    __tablename__ = "google_accounts"

    email: Mapped[str] = mapped_column(String(320), primary_key=True)
    refresh_token_enc: Mapped[str] = mapped_column(Text)
    scopes: Mapped[list[str]] = mapped_column(JSONB, default=list)
    status: Mapped[str] = mapped_column(String(32), default="active")  # active | needs_reconnect


class GmailState(Timestamps, Base):
    """Incremental sync cursor for one mailbox."""

    __tablename__ = "gmail_state"

    email: Mapped[str] = mapped_column(String(320), primary_key=True)
    history_id: Mapped[int | None] = mapped_column(BigInteger)
    watch_expiration: Mapped[datetime | None] = mapped_column(DateTime(timezone=True))
    last_sync_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True))
    last_error: Mapped[str | None] = mapped_column(Text)


class Event(Timestamps, Base):
    """One thing that happened in a Google service, deduplicated by (source, external_id)."""

    __tablename__ = "events"
    __table_args__ = (UniqueConstraint("source", "external_id", name="uq_events_source_ext"),)

    id: Mapped[int] = mapped_column(BigInteger, primary_key=True, autoincrement=True)
    account: Mapped[str] = mapped_column(String(320))
    source: Mapped[str] = mapped_column(String(32))  # gmail | calendar | drive ...
    kind: Mapped[str] = mapped_column(String(64))  # message.received ...
    external_id: Mapped[str] = mapped_column(String(256))
    # new -> ignored | logged | triaged -> summarized -> notified ; gone (deleted)
    status: Mapped[str] = mapped_column(String(32), default="new")
    rule: Mapped[str | None] = mapped_column(String(128))
    data: Mapped[dict[str, Any]] = mapped_column(JSONB, default=dict)
    summary: Mapped[dict[str, Any] | None] = mapped_column(JSONB)


class Job(Timestamps, Base):
    """Durable queue row: one delivery of a published message to one subscriber."""

    __tablename__ = "jobs"
    __table_args__ = (Index("ix_jobs_claim", "status", "run_after", "id"),)

    id: Mapped[int] = mapped_column(BigInteger, primary_key=True, autoincrement=True)
    topic: Mapped[str] = mapped_column(String(128))
    subscriber: Mapped[str] = mapped_column(String(128))
    payload: Mapped[dict[str, Any]] = mapped_column(JSONB)
    status: Mapped[str] = mapped_column(String(16), default="pending")  # pending|running|done|dead
    attempts: Mapped[int] = mapped_column(Integer, default=0)
    max_attempts: Mapped[int] = mapped_column(Integer, default=5)
    run_after: Mapped[datetime] = mapped_column(DateTime(timezone=True), server_default=func.now())
    locked_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True))
    last_error: Mapped[str | None] = mapped_column(Text)


def make_engine(url: str) -> AsyncEngine:
    return create_async_engine(url, pool_pre_ping=True)


def make_sessionmaker(engine: AsyncEngine) -> async_sessionmaker:
    return async_sessionmaker(engine, expire_on_commit=False)


async def create_schema(engine: AsyncEngine) -> None:
    # Phase 0: create tables on startup. Switch to Alembic once the schema starts changing.
    async with engine.begin() as conn:
        await conn.run_sync(Base.metadata.create_all)
