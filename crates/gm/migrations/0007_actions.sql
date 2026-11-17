-- Act with approval (Phase 3): owner instructions, approvals, the Telegram message map and the
-- audit trail. See docs/spec/data-model.md. Machine-wide flags (`notifications_paused`) and the
-- Telegram offset live in `system_settings`.

-- One owner request (a button tap or a sentence). States: pending -> running -> done | needs_input
-- | awaiting_approval | failed; awaiting_approval -> executing -> done | failed;
-- awaiting_approval -> rejected | expired. needs_input / awaiting_approval -> pending when the owner
-- answers or asks for an edit.
CREATE TABLE tasks (
    id          BIGSERIAL PRIMARY KEY,
    origin_key  TEXT NOT NULL UNIQUE,      -- the update that created it (dedupes redelivery)
    event_id    BIGINT REFERENCES events (id) ON DELETE SET NULL,
    origin      TEXT NOT NULL,             -- telegram
    instruction TEXT,                      -- the owner's words (trusted), answers appended
    intent      TEXT,                      -- translate | analyze | reply | unknown
    args        JSONB NOT NULL DEFAULT '{}',
    status      TEXT NOT NULL DEFAULT 'pending',
    result      JSONB NOT NULL DEFAULT '{}', -- text, question, draft id, ...
    error       TEXT,
    reply_to    BIGINT,                    -- Telegram message the answers reply to
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_tasks_event ON tasks (event_id);
CREATE INDEX ix_tasks_created ON tasks (created_at);

-- A proposed external action. The payload never changes after insert; payload_hash is its sha256
-- and is checked again before the executor runs.
-- pending -> approved -> executed | failed ; pending -> rejected | expired
CREATE TABLE approvals (
    id            BIGSERIAL PRIMARY KEY,
    task_id       BIGINT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    action        TEXT NOT NULL,           -- gmail.send_draft
    payload       JSONB NOT NULL,
    payload_hash  TEXT NOT NULL,
    preview       TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'pending',
    expires_at    TIMESTAMPTZ NOT NULL,
    decided_at    TIMESTAMPTZ,
    decided_via   TEXT,                    -- telegram | web
    executing_job BIGINT,                  -- the approval.decision job that runs the executor
    error         TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_approvals_status ON approvals (status, expires_at);
CREATE INDEX ix_approvals_task ON approvals (task_id);

-- Which mail, task or approval a Telegram message belongs to, so a reply to it can be routed.
CREATE TABLE chat_messages (
    id          BIGSERIAL PRIMARY KEY,
    chat_id     TEXT NOT NULL,
    message_id  BIGINT NOT NULL,
    event_id    BIGINT,
    task_id     BIGINT,
    approval_id BIGINT,
    direction   TEXT NOT NULL,             -- in | out
    reply_key   TEXT,                      -- the task.reply that sent it (dedupes redelivery)
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT uq_chat_messages UNIQUE (chat_id, message_id)
);
CREATE INDEX ix_chat_messages_reply_key ON chat_messages (reply_key);
CREATE INDEX ix_chat_messages_approval ON chat_messages (approval_id);
CREATE INDEX ix_chat_messages_created ON chat_messages (created_at);

-- Every executed (or failed) external action. Kept forever: metadata only, never a mail body.
CREATE TABLE audit_log (
    id      BIGSERIAL PRIMARY KEY,
    ts      TIMESTAMPTZ NOT NULL DEFAULT now(),
    actor   TEXT NOT NULL,                 -- owner
    action  TEXT NOT NULL,                 -- gmail.send_draft
    target  TEXT NOT NULL,                 -- recipients
    task_id BIGINT,
    details JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX ix_audit_log_ts ON audit_log (ts);
