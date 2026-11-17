-- Machine-wide key-value state (not per user). Keys: `telegram_offset` (Telegram getUpdates
-- offset, 2C).
CREATE TABLE system_settings (
    key        TEXT PRIMARY KEY,
    value      JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
