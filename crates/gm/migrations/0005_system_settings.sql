-- Machine-wide settings (not per user), edited in the app. Keys: `model_residency` (bool, decision
-- N10: gm starts/stops the mlx launchd agent; off by default), `telegram_offset` (Telegram
-- getUpdates offset, 2C).
CREATE TABLE system_settings (
    key        TEXT PRIMARY KEY,
    value      JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
