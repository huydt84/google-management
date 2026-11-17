-- Health incidents (workstream 2E, ADR-0016). One row per failing check; deleted on
-- recovery. An alert goes out once a check has failed for 15 minutes (alerted_at set), and a
-- recovery message when it passes again.
CREATE TABLE health_state (
    check_name    TEXT PRIMARY KEY,   -- dead_jobs | ingest_lag | model_down | needs_reconnect
    failing_since TIMESTAMPTZ NOT NULL,
    alerted_at    TIMESTAMPTZ,
    detail        TEXT NOT NULL DEFAULT '',
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
