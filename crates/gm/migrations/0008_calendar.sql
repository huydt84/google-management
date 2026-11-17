-- Calendar cursors and minimal snapshots for diffs and sparse cancellation records.
CREATE TABLE calendar_state (
    email TEXT NOT NULL REFERENCES google_accounts(email) ON DELETE CASCADE,
    calendar_id TEXT NOT NULL,
    sync_token TEXT,
    last_sync_at TIMESTAMPTZ,
    last_error TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (email, calendar_id)
);

CREATE TABLE calendar_events (
    email TEXT NOT NULL,
    calendar_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    data JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (email, calendar_id, event_id),
    FOREIGN KEY (email, calendar_id) REFERENCES calendar_state(email, calendar_id) ON DELETE CASCADE
);
CREATE INDEX ix_calendar_events_updated ON calendar_events(updated_at);
