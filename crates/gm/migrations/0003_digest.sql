-- Morning digest (workstream 2A, ADR-0015). See docs/spec/data-model.md.

-- One row per user and local date: idempotent by (owner_email, digest_date).
CREATE TABLE digests (
    id          BIGSERIAL PRIMARY KEY,
    owner_email TEXT NOT NULL,
    digest_date DATE NOT NULL,
    status      TEXT NOT NULL DEFAULT 'building', -- building | sent | empty
    item_count  INT NOT NULL DEFAULT 0,
    message_ids JSONB NOT NULL DEFAULT '[]',      -- Telegram message ids, one per 4096-char part
    text        TEXT NOT NULL DEFAULT '',         -- last rendered text (parts joined)
    sent_at     TIMESTAMPTZ,
    edited_at   TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT uq_digests_owner_date UNIQUE (owner_email, digest_date)
);

-- The digest an event was included in (NULL = not yet sent in a digest).
ALTER TABLE events ADD COLUMN digest_id BIGINT REFERENCES digests (id) ON DELETE SET NULL;
CREATE INDEX ix_events_digest ON events (digest_id);

-- Scheduler bookkeeping: the last occurrence fired per schedule, so a time missed while the Mac
-- slept fires once on wake and never twice.
CREATE TABLE schedule_runs (
    name       TEXT PRIMARY KEY,
    last_at    TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
