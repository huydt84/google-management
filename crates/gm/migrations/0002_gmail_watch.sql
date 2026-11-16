-- Pub/Sub ingest mode (ADR-0002, decision A7): when the Gmail watch for this mailbox expires.
ALTER TABLE gmail_state ADD COLUMN watch_expiration TIMESTAMPTZ;
