-- Google Management schema (Rust port). See docs/spec/data-model.md.

CREATE TABLE google_accounts (
    email             TEXT PRIMARY KEY,
    owner_email       TEXT NOT NULL,              -- the gm user who connected it (multi-user ready)
    refresh_token_enc TEXT NOT NULL,              -- AES-256-GCM, key in the Keychain
    scopes            JSONB NOT NULL DEFAULT '[]',
    status            TEXT NOT NULL DEFAULT 'active', -- active | needs_reconnect
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE gmail_state (
    email        TEXT PRIMARY KEY,
    history_id   BIGINT,
    last_sync_at TIMESTAMPTZ,
    last_error   TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE user_settings (
    email      TEXT PRIMARY KEY,
    settings   JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- new -> ignored | digest | triaged -> ready -> notified ; gone (deleted before triage)
CREATE TABLE events (
    id          BIGSERIAL PRIMARY KEY,
    account     TEXT NOT NULL,
    source      TEXT NOT NULL,
    kind        TEXT NOT NULL,
    external_id TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'new',
    rule        TEXT,
    data        JSONB NOT NULL DEFAULT '{}',
    summary     JSONB,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT uq_events_source_ext UNIQUE (source, external_id)
);
CREATE INDEX ix_events_created ON events (created_at);
CREATE INDEX ix_events_status ON events (status);

CREATE TABLE jobs (
    id           BIGSERIAL PRIMARY KEY,
    topic        TEXT NOT NULL,
    subscriber   TEXT NOT NULL,
    payload      JSONB NOT NULL,
    status       TEXT NOT NULL DEFAULT 'pending',  -- pending | running | done | dead
    attempts     INT NOT NULL DEFAULT 0,
    max_attempts INT NOT NULL DEFAULT 5,
    run_after    TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_at    TIMESTAMPTZ,
    last_error   TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_jobs_claim ON jobs (status, run_after, id);

-- One row per LLM request: tokens and latency per task and lane (ADR-0010).
CREATE TABLE llm_calls (
    id                BIGSERIAL PRIMARY KEY,
    task              TEXT NOT NULL,
    lane              TEXT NOT NULL,
    model             TEXT NOT NULL,
    prompt_tokens     INT,
    completion_tokens INT,
    latency_ms        INT NOT NULL,
    ok                BOOLEAN NOT NULL,
    error             TEXT,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_llm_calls_created ON llm_calls (created_at);
