# Phase 1: Rust port, rules engine v2, operations

**Goal:** replace the Python Phase 0 with one Rust binary at feature parity, add the triage policy
from the mail analysis (rules engine v2), and give the system what it needs to run unattended
(Keychain secrets, launchd, backups, logs, metrics, CI).

**Status:** implemented. Remaining manual steps for the owner: store secrets with
`gm keychain set`, install the launchd agents ([deploy/README.md](../../deploy/README.md)), push to a
private GitHub repository so CI runs, and set up `tailscale serve`.

Decisions: [decision log](../decisions.md) A1–A10, R1, O1–O5, L1–L9; ADRs
[0009](../adr/0009-rust-single-binary.md), [0010](../adr/0010-llm-client-lanes-residency.md),
[0012](../adr/0012-rules-engine-v2.md), [0013](../adr/0013-ops-launchd-keychain-tailscale.md),
[0014](../adr/0014-web-ui-askama-htmx-fluent.md), [0015](../adr/0015-notification-policy-and-digest.md),
[0016](../adr/0016-operations-baseline.md).

## What was built

| Area | Code | Notes |
|------|------|-------|
| Core | `config.rs`, `secrets.rs`, `crypto.rs`, `db.rs`, `migrations/0001_init.sql`, `clock.rs` | Keychain secrets with env fallback for tests; AES-256-GCM token encryption; runtime-checked sqlx queries (decision A3); model hours. |
| Bus | `bus.rs` | Outbox + `jobs`, SKIP LOCKED claim in one statement, backoff, `Outcome::Defer`, stale requeue. |
| Google | `google/oauth.rs`, `google/accounts.rs`, `google/gmail.rs` | Manual PKCE flow; id_token checks; access-token cache; `invalid_grant` → `needs_reconnect`; 401 retry; 404 → `NotFound`. |
| Gmail | `gmail/ingest.rs`, `gmail/pubsub.rs`, `google/pubsub.rs`, `gmail/message.rs` | History paging, label filter, 2-day recovery, per-account lock; Pub/Sub pull with self-provisioning and watch renewal, safety poll; MIME with encoding_rs charsets, HTML → text. |
| Rules v2 | `rules.rs`, `rules.yaml` | Matchers, extractors, message templates, priorities, sections, examples, hot reload; 22 rules from the mail analysis (T1–T16). |
| LLM | `llm/mod.rs`, `llm/clean.rs`, `prompts/` | Lanes with admission control, `llm_calls` accounting, stable prefix, body cleaning and token cap, one JSON retry. |
| Pipeline | `pipeline.rs` | `triage` → `summarize` → `notify` with the night policy; unmatched mail = notify with summary until Phase 2. |
| Notifications | `notify.rs`, `locales/` | Telegram plain text, silent flag, icons by priority, localized strings. |
| Web | `web/`, `templates/` | askama pages, Fluent en/vi/ja, host and origin guards, settings, `/metrics`. |
| CLI | `main.rs` | `serve`, `migrate`, `gen-key`, `keychain set`, `keychain list`, `rules`. |
| Ops | `deploy/launchd/*.plist`, `deploy/README.md`, `scripts/backup.sh`, `.github/workflows/ci.yml`, `docker-compose.yml` | Files only; installing them is the owner's step. |
| Tests | `crates/gm/tests/*.rs` | 53 tests (15 unit, 38 integration): bus, Pub/Sub, message parsing, rules (incl. every `rules.yaml` example), pipeline end to end, LLM client, notifier, web/auth flows. |

## Changes confirmed by the owner after the port

- **Pub/Sub pull** added (decision A7): `src/google/pubsub.rs` (provisioning, pull, ack),
  `src/gmail/pubsub.rs` (watch renewal, pull loop), safety poll in `GmailIngestor::poll_loop`,
  migration `0002_gmail_watch.sql`, `tests/pubsub.rs`.
- **No install command** (O1): the launchd agents are registered once.
- **Health panel and Telegram health alert** (O3) are built in Phase 2 (workstream 2E).
- **Unmatched mail** is notified with a summary until model triage (T16).

## Defaults chosen during the port (not covered by the interview)

`GM_LLM_MAX_INFLIGHT=2`, `GM_LLM_SUMMARY_MAX_TOKENS=400`, retention sweep every 6 h, stale-job
requeue every 5 min for jobs older than 10 min, log files kept 7 days, backup at 03:30, launchd labels
`com.gm.*`, eval folder `~/.local/share/gm/eval/`. Each is a config value or a one-line change.
