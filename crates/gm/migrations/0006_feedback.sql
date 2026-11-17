-- Triage feedback (workstream 2C, decisions N3, N8, N9): the owner's 👍 / 👎 per event, last tap
-- wins (by tap time, so redelivered or reordered jobs can't overwrite a newer answer).
CREATE TABLE triage_feedback (
    event_id   BIGINT PRIMARY KEY REFERENCES events (id) ON DELETE CASCADE,
    label      TEXT NOT NULL,              -- up | down
    rule       TEXT,                       -- the event's rule when rated (`model` = model triage)
    via        TEXT NOT NULL,              -- telegram | web
    tapped_at  TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
